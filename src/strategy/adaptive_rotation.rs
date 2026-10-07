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
pub struct AdaptiveRotationConfig {
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
    /// 趋势超过此阈值时开仓；两阈值之间维持原状态
    #[arg(long, default_value_t = 0.005)]
    pub entry_band: f64,
    /// 趋势低于此阈值时清仓
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub exit_band: f64,
    /// 趋势达到此上限时退出，避免过热追涨；默认不启用
    #[arg(long)]
    pub max_trend: Option<f64>,
    /// 现有持仓排名不超过 top_k × 此倍数时优先保留
    #[arg(long, default_value_t = 2)]
    pub retain_buffer: usize,
    /// 正常调仓间隔（交易日）；风险关闭或重新开启立即处理
    #[arg(long, default_value_t = 1)]
    pub rebalance_days: usize,
    /// 新增股票合计使用买入批次可用现金的比例
    #[arg(long, default_value_t = 0.95)]
    pub allocation: f64,
    /// 最少上市自然日数
    #[arg(long, default_value_t = 120)]
    pub minimum_listed_days: u32,
    /// 历史预热最早查询日期
    #[arg(long, default_value = "20000101", value_parser = parse_date)]
    pub history_start: Date,
}

impl Default for AdaptiveRotationConfig {
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            config: AdaptiveRotationConfig,
        }
        Defaults::parse_from(["adaptive-rotation"]).config
    }
}

impl AdaptiveRotationConfig {
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
            (0.0..=1.0).contains(&self.price_weight),
            "低价权重须在 [0, 1] 内"
        );
        assert!(
            self.max_trend.is_none_or(|ceiling| {
                ceiling.is_finite() && ceiling > self.entry_band && ceiling < 1.0
            }),
            "趋势上限须为有限值且满足 entry_band < max_trend < 1"
        );
        assert!(
            self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0,
            "现金比例须在 (0, 1] 内"
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

/// 历史指数成分内的低成交额反转轮动，股票池趋势关闭时实际清仓。
/// 信号只使用当日及以前的数据，卖出和买入分批在下一交易日开盘执行。
pub struct AdaptiveRotation {
    config: AdaptiveRotationConfig,
    listed_dates: BTreeMap<StockSymbol, Date>,
    histories: BTreeMap<StockSymbol, HistoryWindow>,
    risk_on: bool,
    latest_selection: Vec<StockSymbol>,
    day_number: usize,
    last_rebalance: Option<usize>,
    retry_orders: bool,
}

impl AdaptiveRotation {
    pub fn new(config: AdaptiveRotationConfig) -> Self {
        config.validate();
        log::debug!("自适应轮动参数: {config:?}");
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
        // 当日必须有实际行情；历史停牌前收盘价不能冒充当日信号。
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
                log::warn!("{date} 没有已生效的指数成分，跳过自适应轮动信号: {err}");
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
                "{date} 合格风险观察池仅 {} 只，需要 {} 只，跳过自适应轮动信号",
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

    /// 相同因子值使用平均名次；综合分相同优先低成交额，再按股票代码排序。
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

    fn orders(&self, held: &BTreeSet<StockSymbol>, selection: &[StockSymbol]) -> Vec<Vec<Order>> {
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
            let weight = self.config.allocation / added.len() as f64;
            batches.push(vec![Order::BuyWeights {
                weights: added.into_iter().map(|symbol| (symbol, weight)).collect(),
            }]);
        }
        batches
    }
}

#[async_trait::async_trait]
impl Strategy for AdaptiveRotation {
    fn name(&self) -> &str {
        "adaptive_rotation"
    }

    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        self.day_number += 1;
        let held: BTreeSet<_> = ctx
            .positions
            .iter()
            .filter(|(_, p)| p.purchased_shares > 0)
            .map(|(&s, _)| s)
            .collect();
        // 风险退出不等待定期调仓，也不能因观察池不足而停止重试清仓。
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
                self.orders(&held, &[])
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
                "{} 自适应轮动：中位数 {:.2}%，风险开关 {}，名单 {}",
                ctx.date(),
                trend * 100.0,
                self.risk_on,
                selection
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        self.latest_selection = selection.clone();
        let rebalance = !self.risk_on
            || !old_regime
            || self.retry_orders
            || self
                .last_rebalance
                .is_none_or(|day| self.day_number - day >= self.config.rebalance_days);
        if rebalance {
            self.last_rebalance = Some(self.day_number);
            self.retry_orders = false;
            self.orders(&held, &selection)
        } else if !unsafe_held.is_empty() {
            vec![unsafe_held]
        } else {
            Vec::new()
        }
    }

    async fn on_order_failed(&mut self, _failure: &OrderFailure) {
        // 次日收盘重新根据最新信号生成订单；不会复用过期信号或追加到当前批次。
        self.retry_orders = true;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::data::{Adjustment, DataProvider, IndexComp, IndexHistComp, Stock, StockHistBar};
    use crate::engine::{BacktestConfig, BacktestEngine, BacktestResult};
    use std::sync::{Arc, Mutex};
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
                            .map(|(day, price)| {
                                let price = *price * id as f64;
                                StockBar {
                                    symbol: symbol(id),
                                    date: date!(2024 - 01 - 01) + Duration::days(day as i64),
                                    open: price,
                                    high: price,
                                    low: price,
                                    close: price,
                                    volume: 10000.0,
                                    turnover: 100000.0 * id as f64,
                                    limit_up: Some(price * 1.1),
                                    limit_down: Some(price * 0.9),
                                    float_market_cap: None,
                                    adjustment: Some(Adjustment::Raw(1.0)),
                                    st: false,
                                    delisting: false,
                                }
                            })
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    fn small_config() -> AdaptiveRotationConfig {
        AdaptiveRotationConfig {
            top_k: 1,
            liquidity_pool: 2,
            breadth_count: 2,
            liquidity_lookback: 2,
            reversal_lookback: 2,
            trend_lookback: 2,
            minimum_listed_days: 0,
            rebalance_days: 100,
            ..Default::default()
        }
    }

    type Signals = Arc<Mutex<Vec<(Date, String)>>>;

    struct RecordingStrategy {
        inner: AdaptiveRotation,
        signals: Signals,
    }

    #[async_trait::async_trait]
    impl Strategy for RecordingStrategy {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
            let orders = self.inner.on_trade_day(ctx).await;
            self.signals
                .lock()
                .unwrap()
                .push((ctx.date(), format!("{orders:?}")));
            orders
        }
        async fn on_order_failed(&mut self, failure: &OrderFailure) {
            self.inner.on_order_failed(failure).await;
        }
    }

    async fn backtest(provider: FixtureProvider) -> (BacktestResult, Vec<(Date, String)>) {
        let signals = Signals::default();
        let strategy = RecordingStrategy {
            inner: AdaptiveRotation::new(small_config()),
            signals: signals.clone(),
        };
        let result = BacktestEngine::new(BacktestConfig {
            start: date!(2024 - 01 - 03),
            end: date!(2024 - 01 - 08),
            ..Default::default()
        })
        .unwrap()
        .run(Box::new(provider), Box::new(strategy))
        .await
        .unwrap();
        let recorded = signals.lock().unwrap().clone();
        (result, recorded)
    }

    fn symbol(id: usize) -> StockSymbol {
        format!("{id:06}").as_str().into()
    }

    #[test]
    fn risk_off_liquidates_even_when_selection_is_unchanged() {
        let mut strategy = AdaptiveRotation::new(AdaptiveRotationConfig::default());
        let held = BTreeSet::from([symbol(1), symbol(2)]);
        let selection = vec![symbol(1), symbol(2)];
        strategy.update_regime(0.02);
        assert!(strategy.orders(&held, &selection).is_empty());
        strategy.update_regime(-0.02);
        let orders = strategy.orders(&held, &selection);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].len(), 2);
        assert!(orders[0].iter().all(|o| matches!(o, Order::SellAll { .. })));
    }

    #[test]
    fn retention_buffer_reduces_churn_but_ejects_low_ranked_holding() {
        let strategy = AdaptiveRotation::new(AdaptiveRotationConfig {
            top_k: 2,
            retain_buffer: 2,
            ..Default::default()
        });
        let ranked: Vec<_> = (1..=6).map(symbol).collect();
        assert_eq!(
            strategy.select(&ranked, &BTreeSet::from([symbol(3), symbol(4)])),
            vec![symbol(3), symbol(4)]
        );
        assert_eq!(
            strategy.select(&ranked, &BTreeSet::from([symbol(3), symbol(5)])),
            vec![symbol(1), symbol(3)]
        );
    }

    #[test]
    fn regime_has_hysteresis_and_starts_in_cash() {
        let mut strategy = AdaptiveRotation::new(AdaptiveRotationConfig::default());
        strategy.update_regime(0.005);
        assert!(!strategy.risk_on);
        strategy.update_regime(0.006);
        assert!(strategy.risk_on);
        strategy.update_regime(0.0);
        assert!(strategy.risk_on);
        strategy.update_regime(-0.001);
        assert!(!strategy.risk_on);
    }

    #[test]
    fn overheat_exit_takes_precedence_and_reentry_requires_entry_threshold() {
        let mut strategy = AdaptiveRotation::new(AdaptiveRotationConfig {
            max_trend: Some(0.035),
            ..Default::default()
        });
        strategy.update_regime(0.02);
        assert!(strategy.risk_on);
        strategy.update_regime(0.035);
        assert!(!strategy.risk_on, "到达上限即关闭，即使仍高于入场阈值");
        strategy.update_regime(0.04);
        assert!(!strategy.risk_on);
        strategy.update_regime(0.005);
        assert!(!strategy.risk_on, "入场阈值边界不重新开仓");
        strategy.update_regime(0.006);
        assert!(strategy.risk_on);
        strategy.update_regime(0.034);
        assert!(strategy.risk_on);
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let configs = [
            AdaptiveRotationConfig {
                top_k: 0,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                liquidity_pool: 101,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                reversal_lookback: usize::MAX,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                retain_buffer: 0,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                rebalance_days: 0,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                entry_band: f64::NAN,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                exit_band: 0.01,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                price_weight: -0.1,
                ..Default::default()
            },
            AdaptiveRotationConfig {
                max_trend: Some(0.005),
                ..Default::default()
            },
            AdaptiveRotationConfig {
                max_trend: Some(1.0),
                ..Default::default()
            },
            AdaptiveRotationConfig {
                max_trend: Some(f64::NAN),
                ..Default::default()
            },
            AdaptiveRotationConfig {
                allocation: 1.1,
                ..Default::default()
            },
        ];
        for config in configs {
            assert!(std::panic::catch_unwind(|| config.validate()).is_err());
        }
    }

    #[test]
    fn ranking_combines_raw_price_and_reversal_ranks() {
        let candidates = [
            Candidate {
                symbol: symbol(1),
                liquidity: 1.0,
                trend: 0.0,
                reversal: -0.3,
                raw_close: 30.0,
            },
            Candidate {
                symbol: symbol(2),
                liquidity: 1.0,
                trend: 0.0,
                reversal: -0.2,
                raw_close: 10.0,
            },
            Candidate {
                symbol: symbol(3),
                liquidity: 1.0,
                trend: 0.0,
                reversal: -0.1,
                raw_close: 20.0,
            },
        ];
        let mut strategy = AdaptiveRotation::new(AdaptiveRotationConfig::default());
        assert_eq!(
            strategy.rank_candidates(&candidates),
            vec![symbol(2), symbol(1), symbol(3)]
        );
        strategy.config.price_weight = 0.0;
        assert_eq!(
            strategy.rank_candidates(&candidates),
            vec![symbol(1), symbol(2), symbol(3)]
        );
        strategy.config.price_weight = 1.0;
        assert_eq!(
            strategy.rank_candidates(&candidates),
            vec![symbol(2), symbol(3), symbol(1)]
        );
        let tied_prices = [
            Candidate {
                raw_close: 10.0,
                ..candidates[0]
            },
            candidates[1],
            candidates[2],
        ];
        assert_eq!(
            AdaptiveRotation::average_ranks(&tied_prices, |row| row.raw_close),
            BTreeMap::from([(symbol(1), 0.5), (symbol(2), 0.5), (symbol(3), 2.0)])
        );
        let tied_scores = [
            Candidate {
                symbol: symbol(1),
                liquidity: 3.0,
                ..candidates[0]
            },
            Candidate {
                symbol: symbol(2),
                liquidity: 1.0,
                ..candidates[0]
            },
            Candidate {
                symbol: symbol(3),
                liquidity: 2.0,
                ..candidates[0]
            },
        ];
        assert_eq!(
            strategy.rank_candidates(&tied_scores),
            vec![symbol(2), symbol(3), symbol(1)]
        );
    }

    #[tokio::test]
    async fn backtest_risk_exit_sells_retained_stock_without_waiting_for_rebalance() {
        let (result, _) = backtest(fixture(&[10.0, 10.0, 11.0, 12.0, 9.0, 9.0, 8.0, 8.0])).await;
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.trades[0].date, date!(2024 - 01 - 04));
        assert_eq!(result.trades[1].date, date!(2024 - 01 - 06));
        assert_eq!(result.trades[1].side, "sell");
        assert!(result.equity_curve.last().unwrap().positions.is_empty());
    }

    #[tokio::test]
    async fn backtest_retries_risk_exit_when_limit_down_and_observation_pool_is_incomplete() {
        let mut provider = fixture(&[10.0, 10.0, 11.0, 12.0, 9.0, 9.0, 8.0, 8.0]);
        provider.bars.get_mut(&symbol(1)).unwrap()[5].limit_down = Some(9.0);
        provider
            .bars
            .get_mut(&symbol(2))
            .unwrap()
            .retain(|bar| bar.date != date!(2024 - 01 - 06));
        let (result, _) = backtest(provider).await;
        assert_eq!(result.skipped_orders.len(), 1);
        assert_eq!(result.skipped_orders[0].date, date!(2024 - 01 - 06));
        assert_eq!(result.trades[1].date, date!(2024 - 01 - 07));
        assert_eq!(result.trades[1].signal_date, date!(2024 - 01 - 06));
        assert_eq!(result.trades[1].side, "sell");
    }

    #[tokio::test]
    async fn backtest_retries_failed_buy_before_next_regular_rebalance() {
        let mut provider = fixture(&[10.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0]);
        provider.bars.get_mut(&symbol(1)).unwrap()[3].limit_up = Some(12.0);
        let (result, _) = backtest(provider).await;
        assert_eq!(result.skipped_orders.len(), 1);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].date, date!(2024 - 01 - 05));
        assert_eq!(result.trades[0].signal_date, date!(2024 - 01 - 04));
    }

    #[tokio::test]
    async fn backtest_st_exit_does_not_wait_for_rebalance_or_full_breadth() {
        let mut provider = fixture(&[10.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0]);
        for bar in provider
            .bars
            .get_mut(&symbol(1))
            .unwrap()
            .iter_mut()
            .skip(3)
        {
            bar.st = true;
        }
        let (result, _) = backtest(provider).await;
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.trades[1].signal_date, date!(2024 - 01 - 04));
        assert_eq!(result.trades[1].date, date!(2024 - 01 - 05));
        assert_eq!(result.trades[1].side, "sell");
    }

    #[tokio::test]
    async fn future_prices_factors_and_st_status_do_not_change_prior_orders() {
        let original = fixture(&[10.0, 10.0, 11.0, 12.0, 9.0, 9.0, 8.0, 8.0]);
        let mut changed = original.clone();
        for bars in changed.bars.values_mut() {
            for bar in bars
                .iter_mut()
                .filter(|bar| bar.date > date!(2024 - 01 - 05))
            {
                bar.open *= 3.0;
                bar.high *= 3.0;
                bar.low *= 3.0;
                bar.close *= 3.0;
                bar.limit_up = bar.limit_up.map(|price| price * 3.0);
                bar.limit_down = bar.limit_down.map(|price| price * 3.0);
                bar.adjustment = Some(Adjustment::Raw(2.0));
                bar.st = true;
            }
        }
        let (a, signals_a) = backtest(original).await;
        let (b, signals_b) = backtest(changed).await;
        let before = |signals: Vec<(Date, String)>| {
            signals
                .into_iter()
                .filter(|(day, _)| *day <= date!(2024 - 01 - 05))
                .collect::<Vec<_>>()
        };
        assert_eq!(before(signals_a), before(signals_b));
        assert_eq!(
            serde_json::to_value(&a.equity_curve[..3]).unwrap(),
            serde_json::to_value(&b.equity_curve[..3]).unwrap()
        );
        assert!(!a.trades.is_empty(), "避免空交易结果造成无效的独立性测试");
    }
}
