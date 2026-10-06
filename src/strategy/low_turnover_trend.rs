use super::Strategy;
use crate::{
    data::{StockBar, StockSymbol},
    engine::{BtContext, Order},
    utils::parse_date,
};
use clap::{Args, Parser};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use time::{Date, Duration};

#[derive(Args, Clone, Debug)]
pub struct LowTurnoverTrendConfig {
    /// 动态股票池的指数代码
    #[arg(long, default_value = "399101.XSHE")]
    pub symbol: String,
    /// 持仓名单股票数
    #[arg(long, default_value_t = 6)]
    pub top_k: usize,
    /// 平均成交额窗口（日线根数）
    #[arg(long, default_value_t = 120)]
    pub liquidity_lookback: usize,
    /// 趋势均线窗口（日线根数）
    #[arg(long, default_value_t = 15)]
    pub trend_lookback: usize,
    /// 用于趋势中位数的合格股票数
    #[arg(long, default_value_t = 100)]
    pub breadth_count: usize,
    /// 趋势切换阈值，区间内维持原状态
    #[arg(long, default_value_t = 0.005)]
    pub trend_band: f64,
    /// 防御状态下，新增股票现金预算相对进攻状态的比例
    #[arg(long, default_value_t = 0.25)]
    pub defensive_fraction: f64,
    /// 最少上市自然日数
    #[arg(long, default_value_t = 120)]
    pub minimum_listed_days: u32,
    /// 历史预热最早查询日期，默认与 Tushare 历史 ST 数据起点一致
    #[arg(long, default_value = "20000101", value_parser = parse_date)]
    pub history_start: Date,
}

impl Default for LowTurnoverTrendConfig {
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            config: LowTurnoverTrendConfig,
        }
        Defaults::parse_from(["low-turnover-trend"]).config
    }
}

impl LowTurnoverTrendConfig {
    pub fn validate(&self) {
        assert!(!self.symbol.trim().is_empty(), "指数代码不能为空");
        assert!(
            self.top_k > 0 && self.breadth_count >= self.top_k,
            "股票数量必须满足 0 < top_k <= breadth_count"
        );
        assert!(
            self.liquidity_lookback > 0 && self.trend_lookback > 0,
            "历史窗口必须为正整数"
        );
        assert!(self.warmup_period() <= i32::MAX as usize, "历史窗口过大");
        assert!(
            (0.0..1.0).contains(&self.trend_band),
            "趋势阈值须在 [0, 1) 内"
        );
        assert!(
            (0.0..=1.0).contains(&self.defensive_fraction),
            "防御比例须在 [0, 1] 内"
        );
    }

    fn warmup_period(&self) -> usize {
        self.liquidity_lookback.max(self.trend_lookback)
    }
}

struct HistoryWindow {
    start: Date,
    end: Date,
    bars: VecDeque<StockBar>,
}

/// 从当时生效的指数成分选取低成交额股票。
/// 仅卖出落选股票、按现金权重买入新增股票，保留股票不再平衡。
/// 进攻/防御分别使用买入批次现金的 95% / (95% × defensive_fraction)。
pub struct LowTurnoverTrend {
    config: LowTurnoverTrendConfig,
    listed_dates: BTreeMap<StockSymbol, Date>,
    histories: BTreeMap<StockSymbol, HistoryWindow>,
    risk_on: bool,
    latest_selection: Vec<StockSymbol>,
}

impl LowTurnoverTrend {
    pub fn new(config: LowTurnoverTrendConfig) -> Self {
        config.validate();
        Self {
            config,
            listed_dates: BTreeMap::new(),
            histories: BTreeMap::new(),
            risk_on: false,
            latest_selection: Vec::new(),
        }
    }

    /// 缓存每只股票最近 N 根复权日线；不足时向前补拉，最多到历史下界/上市日。
    async fn update_history(&mut self, ctx: &BtContext<'_>, symbol: StockSymbol, listed: Date) {
        let end = ctx.date();
        let floor = listed.max(self.config.history_start);
        let count = self.config.warmup_period();
        let mut span = Duration::days(count as i64 * 2);
        if let Some(window) = self.histories.get_mut(&symbol) {
            assert!(end >= window.end, "策略日期不能倒退");
            if end > window.end {
                for bar in ctx
                    .stock_bars(symbol, window.end.next_day().unwrap(), end)
                    .await
                {
                    window.bars.push_back(bar.adjusted());
                    if window.bars.len() > count {
                        window.bars.pop_front();
                    }
                }
                window.end = end;
            }
        } else {
            let start = end.checked_sub(span).unwrap_or(Date::MIN).max(floor);
            let bars = ctx.stock_bars(symbol, start, end).await;
            let first = bars.len().saturating_sub(count);
            self.histories.insert(
                symbol,
                HistoryWindow {
                    start,
                    end,
                    bars: bars[first..]
                        .iter()
                        .copied()
                        .map(StockBar::adjusted)
                        .collect(),
                },
            );
        }
        let window = self.histories.get_mut(&symbol).unwrap();
        while window.bars.len() < count && window.start > floor {
            let start = window
                .start
                .checked_sub(span)
                .unwrap_or(Date::MIN)
                .max(floor);
            let bars = ctx
                .stock_bars(symbol, start, window.start.previous_day().unwrap())
                .await;
            for bar in bars.into_iter().rev().take(count - window.bars.len()) {
                window.bars.push_front(bar.adjusted());
            }
            window.start = start;
            span = span.saturating_mul(2);
        }
    }

    async fn selection(&mut self, ctx: &BtContext<'_>) -> Option<(Vec<StockSymbol>, f64)> {
        let date = ctx.date();
        if date < self.config.history_start {
            log::warn!("{date} 早于历史查询下界，跳过地量趋势信号");
            return None;
        }
        let hist = ctx.index_comp(&self.config.symbol, date, date).await;
        let comp = match hist.composition(date) {
            Ok(comp) => comp,
            Err(err) => {
                log::warn!("{date} 没有已生效的指数成分，跳过地量趋势信号: {err}");
                return None;
            }
        };
        // 排序使查询顺序和相同成交额下的选股结果可复现。
        let members: BTreeSet<_> = comp.weights().keys().copied().collect();
        let mut ranked = Vec::new();
        for symbol in members {
            let listed = if let Some(&listed) = self.listed_dates.get(&symbol) {
                listed
            } else {
                let listed = ctx.stock_info(symbol).await.listed;
                self.listed_dates.insert(symbol, listed);
                listed
            };
            if (date - listed).whole_days() < i64::from(self.config.minimum_listed_days) {
                continue;
            }
            self.update_history(ctx, symbol, listed).await;
            let bars = &self.histories[&symbol].bars;
            if bars.len() < self.config.warmup_period()
                || bars
                    .iter()
                    .any(|bar| !bar.turnover.is_finite() || bar.turnover <= 0.0)
            {
                continue;
            }
            let average = bars
                .iter()
                .rev()
                .take(self.config.liquidity_lookback)
                .map(|bar| bar.turnover / self.config.liquidity_lookback as f64)
                .sum::<f64>();
            let mean = bars
                .iter()
                .rev()
                .take(self.config.trend_lookback)
                .map(|bar| bar.close / self.config.trend_lookback as f64)
                .sum::<f64>();
            let trend = bars.back().unwrap().close / mean - 1.0;
            assert!(
                average.is_finite() && mean.is_finite() && mean > 0.0 && trend.is_finite(),
                "地量趋势指标计算溢出"
            );
            ranked.push((average, symbol, trend));
        }
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut eligible = Vec::new();
        for row in ranked {
            if ctx.is_tradable(row.1, date).await {
                eligible.push(row);
            }
            if eligible.len() == self.config.breadth_count {
                break;
            }
        }
        if eligible.len() < self.config.breadth_count {
            log::warn!(
                "{date} 合格风险观察池仅 {} 只，需要 {} 只，跳过地量趋势信号",
                eligible.len(),
                self.config.breadth_count
            );
            return None;
        }
        let mut trends: Vec<_> = eligible.iter().map(|row| row.2).collect();
        trends.sort_by(f64::total_cmp);
        let middle = trends.len() / 2;
        let trend = if trends.len().is_multiple_of(2) {
            trends[middle - 1] / 2.0 + trends[middle] / 2.0
        } else {
            trends[middle]
        };
        Some((
            eligible
                .iter()
                .take(self.config.top_k)
                .map(|row| row.1)
                .collect(),
            trend,
        ))
    }

    fn update_regime(&mut self, trend: f64) -> f64 {
        if trend > self.config.trend_band {
            self.risk_on = true;
        } else if trend < -self.config.trend_band {
            self.risk_on = false;
        }
        0.95 * if self.risk_on {
            1.0
        } else {
            self.config.defensive_fraction
        }
    }

    fn orders(
        &self,
        ctx: &BtContext<'_>,
        selection: &[StockSymbol],
        allocation: f64,
    ) -> Vec<Vec<Order>> {
        let held: BTreeSet<_> = ctx
            .positions
            .iter()
            .filter(|(_, p)| p.purchased_shares > 0)
            .map(|(&s, _)| s)
            .collect();
        let selected: BTreeSet<_> = selection.iter().copied().collect();
        let sells: Vec<_> = held
            .difference(&selected)
            .map(|&symbol| Order::SellAll { symbol })
            .collect();
        let added: Vec<_> = selected.difference(&held).copied().collect();
        let mut batches = Vec::new();
        if !sells.is_empty() {
            batches.push(sells);
        }
        if !added.is_empty() && allocation > 0.0 {
            let weight = allocation / added.len() as f64;
            batches.push(vec![Order::BuyWeights {
                weights: added.into_iter().map(|symbol| (symbol, weight)).collect(),
            }]);
        }
        batches
    }
}

#[async_trait::async_trait]
impl Strategy for LowTurnoverTrend {
    fn name(&self) -> &str {
        "low_turnover_trend"
    }

    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        let Some((selection, trend)) = self.selection(ctx).await else {
            return Vec::new();
        };
        let old_regime = self.risk_on;
        let allocation = self.update_regime(trend);
        let orders = self.orders(ctx, &selection, allocation);
        if selection != self.latest_selection || old_regime != self.risk_on {
            log::info!(
                "{} 地量趋势：中位数 {:.2}%，新增股票使用现金比例 {:.2}%，名单 {:?}",
                ctx.date(),
                trend * 100.0,
                allocation * 100.0,
                selection
            );
        }
        self.latest_selection = selection;
        orders
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn trend_band_retains_state_at_boundaries_and_starts_defensive() {
        let mut strategy = LowTurnoverTrend::new(LowTurnoverTrendConfig::default());
        assert_eq!(strategy.update_regime(0.0), 0.2375);
        assert_eq!(strategy.update_regime(0.005), 0.2375);
        assert_eq!(strategy.update_regime(0.006), 0.95);
        assert_eq!(strategy.update_regime(-0.005), 0.95);
        assert_eq!(strategy.update_regime(-0.006), 0.2375);
    }
}
