use super::Strategy;
use crate::{
    data::{StockBar, StockSymbol},
    engine::{BtContext, Order, OrderFailure},
    utils::{DateRange, parse_date},
};
use clap::{Args, Parser};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use time::{Date, Duration};

#[derive(Args, Clone, Debug)]
pub struct DynamicRotationConfig {
    /// 动态股票池的指数代码
    #[arg(long, default_value = "399101.XSHE")]
    pub symbol: String,
    /// 目标持仓股票数
    #[arg(long, default_value_t = 6)]
    pub top_k: usize,
    /// 按平均成交额由低到高选出的反转候选股票数
    #[arg(long, default_value_t = 20)]
    pub liquidity_pool: usize,
    /// 平均成交额窗口（日线根数）
    #[arg(long, default_value_t = 120)]
    pub liquidity_lookback: usize,
    /// 反转收益窗口；优先选择窗口内复权收益较低的股票
    #[arg(long, default_value_t = 20)]
    pub reversal_lookback: usize,
    /// 原始收盘低价排名权重；其余用于反转排名，相同因子值取平均名次
    #[arg(long, default_value_t = 0.5)]
    pub price_weight: f64,
    /// 趋势均线窗口（日线根数）
    #[arg(long, default_value_t = 5)]
    pub trend_lookback: usize,
    /// 用于趋势中位数的低成交额股票数
    #[arg(long, default_value_t = 100)]
    pub breadth_count: usize,
    /// 趋势超过此阈值时建仓；两阈值之间维持原状态
    #[arg(long, default_value_t = 0.005)]
    pub entry_band: f64,
    /// 趋势低于此阈值时清仓
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub exit_band: f64,
    /// 趋势达到此上限时退出，避免过热追涨
    #[arg(long, default_value = "0.032")]
    pub max_trend: Option<f64>,
    /// 强趋势确认阈值；达到此阈值进入全额仓位，否则保持谨慎建仓仓位
    #[arg(long, default_value_t = 0.011)]
    pub strong_trend_band: f64,
    /// 谨慎建仓阶段（弱趋势）新增股票合计使用买入批次可用现金的比例
    #[arg(long, default_value_t = 0.35)]
    pub conservative_allocation: f64,
    /// 强趋势状态下新增股票合计使用买入批次可用现金的比例
    #[arg(long, default_value_t = 0.95)]
    pub allocation: f64,
    /// 现有持仓排名不超过 top_k × 此倍数时优先保留
    #[arg(long, default_value_t = 2)]
    pub retain_buffer: usize,
    /// 正常调仓间隔（交易日）；风险关闭或重新开启立即处理
    #[arg(long, default_value_t = 1)]
    pub rebalance_days: usize,
    /// 最少上市自然日数
    #[arg(long, default_value_t = 120)]
    pub minimum_listed_days: u32,
    /// 历史预热最早查询日期
    #[arg(long, default_value = "20000101", value_parser = parse_date)]
    pub history_start: Date,
}

impl Default for DynamicRotationConfig {
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            config: DynamicRotationConfig,
        }
        Defaults::parse_from(["dynamic-rotation"]).config
    }
}

impl DynamicRotationConfig {
    pub fn validate(&self) {
        assert!(!self.symbol.trim().is_empty(), "指数代码不能为空");
        assert!(
            self.top_k > 0
                && self.top_k <= self.liquidity_pool
                && self.liquidity_pool <= self.breadth_count,
            "股票数量必须满足 0 < top_k <= liquidity_pool <= breadth_count"
        );
        assert!(
            self.liquidity_lookback > 0 && self.reversal_lookback > 0 && self.trend_lookback > 0,
            "历史窗口必须为正整数"
        );
        assert!(self.reversal_lookback < i32::MAX as usize, "反转窗口过大");
        assert!(self.warmup_period() <= i32::MAX as usize, "历史窗口过大");
        assert!(
            self.retain_buffer > 0 && self.top_k.checked_mul(self.retain_buffer).is_some(),
            "持仓缓冲倍数必须为正整数且不能溢出"
        );
        assert!(self.rebalance_days > 0, "调仓间隔必须为正整数");
        assert!(
            (0.0..1.0).contains(&self.entry_band)
                && (-1.0..1.0).contains(&self.exit_band)
                && self.exit_band <= self.entry_band,
            "趋势阈值须满足 -1 <= exit_band <= entry_band < 1 且 entry_band >= 0"
        );
        assert!(
            (0.0..1.0).contains(&self.strong_trend_band)
                && self.strong_trend_band >= self.entry_band,
            "强趋势阈值须在 [entry_band, 1) 内"
        );
        assert!(
            (0.0..=1.0).contains(&self.price_weight),
            "低价权重须在 [0, 1] 内"
        );
        assert!(
            self.max_trend.is_none_or(|ceiling| {
                ceiling.is_finite() && ceiling > self.strong_trend_band && ceiling < 1.0
            }),
            "趋势上限须为有限值且满足 strong_trend_band < max_trend < 1"
        );
        assert!(
            self.conservative_allocation.is_finite()
                && self.conservative_allocation > 0.0
                && self.conservative_allocation <= 1.0,
            "谨慎建仓现金比例须在 (0, 1] 内"
        );
        assert!(
            self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0,
            "强趋势现金比例须在 (0, 1] 内"
        );
        assert!(
            self.conservative_allocation <= self.allocation,
            "谨慎建仓比例不能超过全额仓位比例"
        );
    }

    fn warmup_period(&self) -> usize {
        self.liquidity_lookback
            .max(self.reversal_lookback + 1)
            .max(self.trend_lookback)
    }
}

struct HistoryWindow {
    start: Date,
    end: Date,
    bars: VecDeque<StockBar>,
    raw_close: Option<f64>,
}

#[derive(Clone, Copy)]
struct Candidate {
    symbol: StockSymbol,
    liquidity: f64,
    reversal: f64,
    trend: f64,
    raw_close: f64,
}

/// 自适应动态仓位轮动策略：
/// 1. 低成交额池内结合低价与反转排名选股，保留交集排名缓冲；
/// 2. 观察池 5 日趋势中位数监控市场风险，在弱趋势（0.005 ~ 0.011）下以 35% 谨慎仓位试仓；
/// 3. 趋势确认强劲（>= 0.011）时提升至 95% 全额仓位进攻；
/// 4. 趋势过热（>= 0.032）或趋势走弱（< 0.0）时立即清仓，有效规避剧烈回调。
pub struct DynamicRotation {
    config: DynamicRotationConfig,
    listed_dates: BTreeMap<StockSymbol, Date>,
    histories: BTreeMap<StockSymbol, HistoryWindow>,
    risk_on: bool,
    latest_selection: Vec<StockSymbol>,
    day_number: usize,
    last_rebalance: Option<usize>,
    retry_orders: bool,
}

impl DynamicRotation {
    pub fn new(config: DynamicRotationConfig) -> Self {
        config.validate();
        log::debug!("自适应动态仓位轮动参数: {config:?}");
        Self {
            config,
            listed_dates: BTreeMap::new(),
            histories: BTreeMap::new(),
            risk_on: false,
            latest_selection: Vec::new(),
            day_number: 0,
            last_rebalance: None,
            retry_orders: false,
        }
    }

    async fn update_history(
        &mut self,
        ctx: &BtContext<'_>,
        symbol: StockSymbol,
        bars: Vec<StockBar>,
    ) {
        let end = ctx.date();
        let floor = self.listed_dates[&symbol].max(self.config.history_start);
        let count = self.config.warmup_period();
        let mut span = Duration::days(count as i64 * 2);
        assert!(bars.iter().all(|bar| bar.date <= end), "日线包含未来数据");
        if let Some(window) = self.histories.get_mut(&symbol) {
            assert!(end >= window.end, "策略日期不能倒退");
            if end > window.end {
                for bar in bars {
                    assert!(bar.date > window.end, "日线增量区间重叠");
                    window.raw_close = Some(bar.close);
                    window.bars.push_back(bar.adjusted());
                    if window.bars.len() > count {
                        window.bars.pop_front();
                    }
                }
                window.end = end;
            }
        } else {
            let start = end.checked_sub(span).unwrap_or(Date::MIN).max(floor);
            let first = bars.len().saturating_sub(count);
            self.histories.insert(
                symbol,
                HistoryWindow {
                    start,
                    end,
                    raw_close: bars.last().map(|bar| bar.close),
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
            let range = DateRange::new(start, window.start.previous_day().unwrap());
            let bars = ctx.stock_bars(symbol, range).await;
            for bar in bars.into_iter().rev().take(count - window.bars.len()) {
                if window.bars.is_empty() {
                    window.raw_close = Some(bar.close);
                }
                window.bars.push_front(bar.adjusted());
            }
            window.start = start;
            span = span.saturating_mul(2);
        }
    }

    fn candidate(&self, symbol: StockSymbol, date: Date) -> Option<Candidate> {
        let bars = &self.histories[&symbol].bars;
        let current = bars.back()?;
        if bars.len() < self.config.warmup_period()
            || current.date != date
            || current.st
            || current.delisting
            || bars
                .iter()
                .any(|bar| !bar.turnover.is_finite() || bar.turnover <= 0.0)
        {
            return None;
        }
        let liquidity = bars
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
        let trend = current.close / mean - 1.0;
        let reversal =
            current.close / bars[bars.len() - 1 - self.config.reversal_lookback].close - 1.0;
        assert!(
            liquidity.is_finite() && trend.is_finite() && reversal.is_finite(),
            "轮动指标计算溢出"
        );
        Some(Candidate {
            symbol,
            liquidity,
            reversal,
            trend,
            raw_close: self.histories[&symbol].raw_close.unwrap(),
        })
    }

    async fn signal(&mut self, ctx: &BtContext<'_>) -> Option<(Vec<StockSymbol>, f64)> {
        let date = ctx.date();
        if date < self.config.history_start {
            return None;
        }
        let hist = ctx
            .index_comp(&self.config.symbol, DateRange::new(date, date))
            .await;
        let comp = match hist.composition(date) {
            Ok(comp) => comp,
            Err(err) => {
                log::warn!("{date} 没有已生效的指数成分，跳过自适应动态轮动信号: {err}");
                return None;
            }
        };
        let members: BTreeSet<_> = comp.weights().keys().copied().collect();
        let missing: Vec<_> = members
            .iter()
            .copied()
            .filter(|s| !self.listed_dates.contains_key(s))
            .collect();
        for (symbol, info) in ctx.stocks_info(&missing).await {
            self.listed_dates.insert(symbol, info.listed);
        }
        let members: Vec<_> = members
            .into_iter()
            .filter(|s| {
                (date - self.listed_dates[s]).whole_days()
                    >= i64::from(self.config.minimum_listed_days)
            })
            .collect();
        let mut groups: BTreeMap<Date, Vec<StockSymbol>> = BTreeMap::new();
        for &symbol in &members {
            let start = if let Some(window) = self.histories.get(&symbol) {
                assert!(date >= window.end, "策略日期不能倒退");
                if date == window.end {
                    continue;
                }
                window.end.next_day().unwrap()
            } else {
                date.checked_sub(Duration::days(self.config.warmup_period() as i64 * 2))
                    .unwrap_or(Date::MIN)
                    .max(self.listed_dates[&symbol].max(self.config.history_start))
            };
            groups.entry(start).or_default().push(symbol);
        }
        let mut downloaded = BTreeMap::new();
        for (start, symbols) in groups {
            downloaded.extend(ctx.stocks_bars(&symbols, DateRange::new(start, date)).await);
        }
        let mut ranked = Vec::new();
        for symbol in members {
            self.update_history(ctx, symbol, downloaded.remove(&symbol).unwrap_or_default())
                .await;
            if let Some(row) = self.candidate(symbol, date) {
                ranked.push(row);
            }
        }
        ranked.sort_by(|a, b| {
            a.liquidity
                .total_cmp(&b.liquidity)
                .then(a.symbol.cmp(&b.symbol))
        });
        if ranked.len() < self.config.breadth_count {
            log::warn!(
                "{date} 合格风险观察池仅 {} 只，需要 {} 只，跳过自适应动态轮动信号",
                ranked.len(),
                self.config.breadth_count
            );
            return None;
        }
        let mut trends: Vec<_> = ranked
            .iter()
            .take(self.config.breadth_count)
            .map(|row| row.trend)
            .collect();
        trends.sort_by(f64::total_cmp);
        let middle = trends.len() / 2;
        let trend = if trends.len().is_multiple_of(2) {
            trends[middle - 1] / 2.0 + trends[middle] / 2.0
        } else {
            trends[middle]
        };
        ranked.truncate(self.config.liquidity_pool);
        Some((self.rank_candidates(&ranked), trend))
    }

    fn rank_candidates(&self, candidates: &[Candidate]) -> Vec<StockSymbol> {
        let reversal = Self::average_ranks(candidates, |row| row.reversal);
        let price = Self::average_ranks(candidates, |row| row.raw_close);
        let score = |row: &Candidate| {
            reversal[&row.symbol] * (1.0 - self.config.price_weight)
                + price[&row.symbol] * self.config.price_weight
        };
        let mut ranked: Vec<_> = candidates.iter().collect();
        ranked.sort_by(|a, b| {
            score(a)
                .total_cmp(&score(b))
                .then(a.liquidity.total_cmp(&b.liquidity))
                .then(a.symbol.cmp(&b.symbol))
        });
        ranked.into_iter().map(|row| row.symbol).collect()
    }

    fn average_ranks(
        candidates: &[Candidate],
        key: impl Fn(&Candidate) -> f64,
    ) -> BTreeMap<StockSymbol, f64> {
        let mut rows: Vec<_> = candidates.iter().collect();
        rows.sort_by(|a, b| key(a).total_cmp(&key(b)));
        let mut result = BTreeMap::new();
        let mut start = 0;
        while start < rows.len() {
            let mut end = start + 1;
            while end < rows.len() && key(rows[start]) == key(rows[end]) {
                end += 1;
            }
            let rank = (start as f64 + (end - 1) as f64) / 2.0;
            for row in &rows[start..end] {
                result.insert(row.symbol, rank);
            }
            start = end;
        }
        result
    }

    fn update_regime(&mut self, trend: f64) {
        if self
            .config
            .max_trend
            .is_some_and(|ceiling| trend >= ceiling)
        {
            self.risk_on = false;
        } else if trend > self.config.entry_band {
            self.risk_on = true;
        } else if trend < self.config.exit_band {
            self.risk_on = false;
        }
    }

    fn select(&self, ranked: &[StockSymbol], held: &BTreeSet<StockSymbol>) -> Vec<StockSymbol> {
        let mut selected: Vec<_> = ranked
            .iter()
            .take(self.config.top_k * self.config.retain_buffer)
            .filter(|s| held.contains(s))
            .take(self.config.top_k)
            .copied()
            .collect();
        for &symbol in ranked {
            if selected.len() == self.config.top_k {
                break;
            }
            if !selected.contains(&symbol) {
                selected.push(symbol);
            }
        }
        selected.sort();
        selected
    }

    fn orders(
        &self,
        held: &BTreeSet<StockSymbol>,
        selection: &[StockSymbol],
        trend: f64,
    ) -> Vec<Vec<Order>> {
        let selected: BTreeSet<_> = if self.risk_on {
            selection.iter().copied().collect()
        } else {
            BTreeSet::new()
        };
        let sells: Vec<_> = held
            .difference(&selected)
            .map(|&symbol| Order::SellAll { symbol })
            .collect();
        let added: Vec<_> = selected.difference(held).copied().collect();
        let mut batches = Vec::new();
        if !sells.is_empty() {
            batches.push(sells);
        }
        if !added.is_empty() {
            let alloc = if trend < self.config.strong_trend_band {
                self.config.conservative_allocation
            } else {
                self.config.allocation
            };
            let weight = alloc / added.len() as f64;
            batches.push(vec![Order::BuyWeights {
                weights: added.into_iter().map(|symbol| (symbol, weight)).collect(),
            }]);
        }
        batches
    }
}

#[async_trait::async_trait]
impl Strategy for DynamicRotation {
    fn name(&self) -> &str {
        "dynamic_rotation"
    }

    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        self.day_number += 1;
        let held: BTreeSet<_> = ctx
            .positions
            .iter()
            .filter(|(_, p)| p.purchased_shares > 0)
            .map(|(&s, _)| s)
            .collect();
        let held_bars = ctx
            .stocks_bars(
                &held.iter().copied().collect::<Vec<_>>(),
                DateRange::new(ctx.date(), ctx.date()),
            )
            .await;
        let unsafe_held: Vec<_> = held_bars
            .iter()
            .filter(|(_, bars)| bars.last().is_some_and(|bar| bar.st || bar.delisting))
            .map(|(&symbol, _)| Order::SellAll { symbol })
            .collect();
        let Some((ranked, trend)) = self.signal(ctx).await else {
            return if !self.risk_on {
                self.orders(&held, &[], 0.0)
            } else if unsafe_held.is_empty() {
                Vec::new()
            } else {
                vec![unsafe_held]
            };
        };
        let old_regime = self.risk_on;
        self.update_regime(trend);
        let selection = self.select(&ranked, &held);
        if selection != self.latest_selection || old_regime != self.risk_on {
            log::info!(
                "{} 自适应动态轮动：中位数 {:.2}%，风险开关 {}，名单 {}",
                ctx.date(),
                trend * 100.0,
                self.risk_on,
                selection
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.latest_selection = selection;
        if old_regime != self.risk_on
            || self.retry_orders
            || self.day_number.saturating_sub(self.last_rebalance.unwrap_or(0))
                >= self.config.rebalance_days
        {
            self.last_rebalance = Some(self.day_number);
            self.retry_orders = false;
            self.orders(&held, &self.latest_selection, trend)
        } else if !unsafe_held.is_empty() {
            vec![unsafe_held]
        } else {
            Vec::new()
        }
    }

    async fn on_order_failed(&mut self, _failure: &OrderFailure) {
        self.retry_orders = true;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::data::{Adjustment, DataProvider, IndexComp, IndexHistComp, Stock, StockHistBar};
    use crate::engine::{BacktestConfig, BacktestEngine};
    use std::sync::Arc;
    use time::macros::date;

    #[derive(Clone)]
    struct FixtureProvider {
        bars: BTreeMap<StockSymbol, Vec<StockBar>>,
    }

    #[async_trait::async_trait]
    impl DataProvider for FixtureProvider {
        async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
            (0..=(range.end() - range.start()).whole_days())
                .map(|day| range.start() + Duration::days(day))
                .collect()
        }

        async fn stock_info(&mut self, symbol: StockSymbol) -> Stock {
            Stock {
                symbol,
                name: symbol.to_string(),
                listed: date!(2020 - 01 - 01),
                delisted: None,
                industry: None,
                bars: None,
            }
        }

        async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
            StockHistBar::new(
                range,
                self.bars[&symbol]
                    .iter()
                    .filter(|bar| range.contains(bar.date))
                    .copied()
                    .collect(),
            )
            .unwrap()
        }

        async fn is_tradable(&mut self, _symbol: StockSymbol, _date: Date) -> bool {
            unreachable!("回测缓存负责判断可交易状态")
        }

        async fn index_name(&mut self, _symbol: &str) -> String {
            "测试指数".into()
        }

        async fn index_comp(&mut self, _symbol: &str, range: DateRange) -> IndexHistComp {
            IndexHistComp::new(
                range,
                vec![(
                    date!(2020 - 01 - 01),
                    Arc::new(IndexComp::new([(symbol(1), 0.5), (symbol(2), 0.5)].into()).unwrap()),
                )],
            )
            .unwrap()
        }
    }

    fn fixture(prices: &[f64]) -> FixtureProvider {
        FixtureProvider {
            bars: (1..=2)
                .map(|id| {
                    (
                        symbol(id),
                        prices
                            .iter()
                            .enumerate()
                            .map(|(i, &price)| StockBar {
                                symbol: symbol(id),
                                date: date!(2020 - 01 - 01) + Duration::days(i as i64),
                                open: price,
                                high: price,
                                low: price,
                                close: price,
                                turnover: 1000.0 * id as f64,
                                volume: 100.0,
                                limit_up: None,
                                limit_down: None,
                                float_market_cap: None,
                                adjustment: Some(Adjustment::Raw(1.0)),
                                st: false,
                                delisting: false,
                            })
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    fn symbol(id: u32) -> StockSymbol {
        format!("{id:06}.SZ").as_str().into()
    }

    fn config() -> DynamicRotationConfig {
        DynamicRotationConfig {
            symbol: "399101.XSHE".into(),
            top_k: 1,
            liquidity_pool: 2,
            liquidity_lookback: 2,
            reversal_lookback: 2,
            price_weight: 0.5,
            trend_lookback: 2,
            breadth_count: 2,
            entry_band: 0.005,
            exit_band: 0.0,
            max_trend: Some(0.032),
            strong_trend_band: 0.011,
            conservative_allocation: 0.35,
            allocation: 0.95,
            retain_buffer: 2,
            rebalance_days: 1,
            minimum_listed_days: 0,
            history_start: date!(2020 - 01 - 01),
        }
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let mut cfg = config();
        cfg.conservative_allocation = 0.99;
        cfg.allocation = 0.50;
        assert!(std::panic::catch_unwind(move || cfg.validate()).is_err());

        let mut cfg = config();
        cfg.strong_trend_band = 0.002;
        assert!(std::panic::catch_unwind(move || cfg.validate()).is_err());

        let mut cfg = config();
        cfg.max_trend = Some(0.008);
        assert!(std::panic::catch_unwind(move || cfg.validate()).is_err());
    }

    #[test]
    fn regime_has_hysteresis_and_starts_in_cash() {
        let mut strategy = DynamicRotation::new(config());
        assert!(!strategy.risk_on);
        strategy.update_regime(0.005);
        assert!(!strategy.risk_on);
        strategy.update_regime(0.0051);
        assert!(strategy.risk_on);
        strategy.update_regime(0.002);
        assert!(strategy.risk_on);
        strategy.update_regime(0.0);
        assert!(strategy.risk_on);
        strategy.update_regime(-0.0001);
        assert!(!strategy.risk_on);
    }

    #[test]
    fn overheat_exit_takes_precedence_and_reentry_requires_entry_threshold() {
        let mut strategy = DynamicRotation::new(config());
        strategy.update_regime(0.01);
        assert!(strategy.risk_on);
        strategy.update_regime(0.032);
        assert!(!strategy.risk_on);
        strategy.update_regime(0.005);
        assert!(!strategy.risk_on);
        strategy.update_regime(0.0051);
        assert!(strategy.risk_on);
    }

    #[test]
    fn dynamic_allocation_orders() {
        let strategy = DynamicRotation::new(config());
        let held = BTreeSet::new();
        let selection = vec![symbol(1)];

        // risk_on is false -> no buy orders
        let orders = strategy.orders(&held, &selection, 0.008);
        assert!(orders.is_empty());

        let mut strategy = DynamicRotation::new(config());
        strategy.risk_on = true;

        // Weak trend -> conservative allocation (0.35)
        let orders = strategy.orders(&held, &selection, 0.008);
        assert_eq!(orders.len(), 1);
        if let Order::BuyWeights { weights } = &orders[0][0] {
            assert!((weights[&symbol(1)] - 0.35).abs() < 1e-6);
        } else {
            panic!("expected BuyWeights");
        }

        // Strong trend -> full allocation (0.95)
        let orders = strategy.orders(&held, &selection, 0.015);
        assert_eq!(orders.len(), 1);
        if let Order::BuyWeights { weights } = &orders[0][0] {
            assert!((weights[&symbol(1)] - 0.95).abs() < 1e-6);
        } else {
            panic!("expected BuyWeights");
        }
    }

    #[tokio::test]
    async fn backtest_runs_with_engine() {
        let bt = BacktestConfig {
            start: date!(2020 - 01 - 01),
            end: date!(2020 - 01 - 04),
            initial_cash: 10_000.0,
            commission_rate: 0.0003,
            minimum_commission: 5.0,
            stamp_tax_rate: 0.0005,
            slippage_bps: 0.0,
            lot_size: 100,
            adjust_returns: true,
            reporter: crate::report::ReporterKind::QuantStats,
            report_output: "test_report".into(),
        };
        let engine = BacktestEngine::new(bt).unwrap();
        // Prices: 10, 10, 10.2, 10.5 -> trend turns positive on day 3
        let provider = Box::new(fixture(&[10.0, 10.0, 10.2, 10.5]));
        let strategy = Box::new(DynamicRotation::new(config()));
        let result = engine.run(provider, strategy).await.unwrap();
        assert!(result.performance.final_equity > 0.0);
    }
}
