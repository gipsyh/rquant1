use super::{
    Strategy,
    online_model::{Factors, OnlineModel},
};
use crate::{
    data::{StockBar, StockSymbol},
    engine::{BtContext, Order},
    utils::{DateRange, parse_date},
};
use clap::{Args, Parser, ValueEnum};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use time::{Date, Duration};

/// Distinct price/volume hypotheses; none ranks stocks by low turnover.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum Pattern {
    Breakout,
    Pullback,
    RsiReversion,
    Compression,
    Recovery,
    SmoothTrend,
    OnlineAlpha,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum TrendRank {
    RiskAdjusted,
    Momentum,
    Volume,
}

#[derive(Args, Clone, Debug)]
pub struct PatternRotationConfig {
    /// Historical index constituents
    #[arg(long = "symbol", value_delimiter = ',', default_value = "399006.XSHE")]
    pub symbols: Vec<String>,
    #[arg(long, value_enum, default_value = "breakout")]
    pub pattern: Pattern,
    /// Ranking rule for breakout, compression and smooth-trend
    #[arg(long, value_enum, default_value = "volume")]
    pub rank_by: TrendRank,
    #[arg(long, default_value_t = 1)]
    pub top_k: usize,
    /// Medium-term momentum / breakout window in actual bars
    #[arg(long, default_value_t = 120)]
    pub lookback: usize,
    /// Pullback / volatility-contraction window
    #[arg(long, default_value_t = 5)]
    pub short_window: usize,
    /// Exit mean window; 0 disables mean exits for trend patterns
    #[arg(long, default_value_t = 10)]
    pub exit_window: usize,
    /// Maximum holding age in exchange trading days
    #[arg(long, default_value_t = 60)]
    pub max_hold_days: usize,
    /// Exit after this loss from the highest closing price since entry (next open)
    #[arg(long, default_value_t = 0.12)]
    pub trailing_stop: f64,
    /// Required medium-term return (or trend gate for RSI / recovery)
    #[arg(long, default_value_t = 0.2, allow_hyphen_values = true)]
    pub min_momentum: f64,
    /// Required current turnover / prior 20-bar mean for breakout or recovery
    #[arg(long, default_value_t = 2.0)]
    pub volume_ratio: f64,
    /// Minimum (close-low)/(high-low) on a breakout day; 0 disables this filter
    #[arg(long, default_value_t = 0.0)]
    pub close_strength: f64,
    /// Short-window RSI entry ceiling for RSI reversion
    #[arg(long, default_value_t = 15.0)]
    pub rsi_entry: f64,
    /// Minimum fraction above medium-term MA; 0 disables the market breadth gate
    #[arg(long, default_value_t = 0.5)]
    pub min_breadth: f64,
    /// Breadth risk-off threshold; 0 disables it; must not exceed min-breadth
    #[arg(long, default_value = "0.4")]
    pub exit_breadth: Option<f64>,
    /// Prior 20-bar mean turnover floor in yuan, not a ranking factor
    #[arg(long, default_value_t = 30_000_000.0)]
    pub min_turnover: f64,
    #[arg(long, default_value_t = 3.0)]
    pub minimum_price: f64,
    #[arg(long, default_value_t = 0.7)]
    pub allocation: f64,
    #[arg(long, default_value_t = 240)]
    pub minimum_listed_days: u32,
    #[arg(long, default_value = "20000101", value_parser = parse_date)]
    pub history_start: Date,
    /// Matured-label rolling training window (exchange days), for online-alpha
    #[arg(long, default_value_t = 126)]
    pub training_days: usize,
    /// Target open-to-open holding horizon, also actual maximum holding age
    #[arg(long, default_value_t = 5)]
    pub prediction_days: usize,
    /// Ridge penalty after dividing the loss by sample count
    #[arg(long, default_value_t = 0.1)]
    pub ridge: f64,
}
impl Default for PatternRotationConfig {
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            config: PatternRotationConfig,
        }
        Defaults::parse_from(["pattern-rotation"]).config
    }
}
impl PatternRotationConfig {
    pub fn validate(&self) {
        assert!(!self.symbols.is_empty() && self.symbols.iter().all(|s| !s.trim().is_empty()));
        assert!(
            self.training_days > 0
                && self.training_days <= 2000
                && self.prediction_days > 0
                && self.prediction_days <= 252
        );
        assert!(self.ridge.is_finite() && self.ridge > 0.0);
        assert!(self.top_k > 0 && self.max_hold_days > 0);
        assert!(
            self.short_window > 0
                && self.lookback > self.short_window
                && self.lookback <= 10000
                && self.exit_window <= 10000
        );
        assert!(
            self.trailing_stop.is_finite() && self.trailing_stop > 0.0 && self.trailing_stop <= 1.0
        );
        assert!(self.min_momentum.is_finite() && self.min_momentum > -1.0);
        assert!(self.volume_ratio.is_finite() && self.volume_ratio > 0.0);
        assert!((0.0..=100.0).contains(&self.rsi_entry));
        assert!((0.0..=1.0).contains(&self.close_strength));
        assert!((0.0..=1.0).contains(&self.min_breadth));
        assert!(
            self.exit_breadth
                .is_none_or(|x| (0.0..=self.min_breadth).contains(&x))
        );
        assert!(self.min_turnover.is_finite() && self.min_turnover >= 0.0);
        assert!(self.minimum_price.is_finite() && self.minimum_price >= 0.0);
        assert!(self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0);
    }
    fn warmup_period(&self) -> usize {
        (self.lookback + 1).max(21).max(self.exit_window)
    }
}
struct HistoryWindow {
    start: Date,
    end: Date,
    bars: VecDeque<StockBar>,
    raw_close: Option<f64>,
}
#[derive(Debug, Clone, Copy)]
struct Features {
    close: f64,
    raw_close: f64,
    turnover: f64,
    mean: f64,
    exit_mean: f64,
    momentum: f64,
    short_return: f64,
    volatility: f64,
    contraction: f64,
    prior_high: f64,
    volume_ratio: f64,
    rsi: f64,
    lower_wick: f64,
    close_strength: f64,
    intraday: f64,
}
struct Holding {
    entered: usize,
    peak: f64,
}
/// Event entries and independent exits. A sold slot is reused only after the sell is confirmed.
pub struct PatternRotation {
    config: PatternRotationConfig,
    listed_dates: BTreeMap<StockSymbol, Date>,
    histories: BTreeMap<StockSymbol, HistoryWindow>,
    holdings: BTreeMap<StockSymbol, Holding>,
    pending_exits: BTreeSet<StockSymbol>,
    day_number: usize,
    model: OnlineModel,
}
impl PatternRotation {
    pub fn new(config: PatternRotationConfig) -> Self {
        config.validate();
        let model = OnlineModel::new(config.prediction_days, config.training_days, config.ridge);
        Self {
            model,
            config,
            listed_dates: BTreeMap::new(),
            histories: BTreeMap::new(),
            holdings: BTreeMap::new(),
            pending_exits: BTreeSet::new(),
            day_number: 0,
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

    fn features(&self, symbol: StockSymbol, date: Date) -> Option<Features> {
        let history = self.histories.get(&symbol)?;
        let bars = &history.bars;
        let b = bars.back()?;
        if bars.len() < self.config.warmup_period() || b.date != date || b.st || b.delisting {
            return None;
        }
        let n = bars.len();
        let turnover = bars
            .iter()
            .rev()
            .skip(1)
            .take(20)
            .map(|b| b.turnover / 20.0)
            .sum::<f64>();
        if turnover <= 0.0 {
            return None;
        }
        let mean = |count: usize| {
            bars.iter()
                .rev()
                .take(count)
                .map(|b| b.close / count as f64)
                .sum::<f64>()
        };
        let returns: Vec<_> = (n - self.config.lookback..n)
            .map(|i| bars[i].close / bars[i - 1].close - 1.0)
            .collect();
        let rms = |v: &[f64]| (v.iter().map(|r| r * r).sum::<f64>() / v.len() as f64).sqrt();
        let short = &returns[returns.len() - self.config.short_window..];
        let gains: f64 = short.iter().map(|r| r.max(0.0)).sum();
        let losses: f64 = short.iter().map(|r| (-r).max(0.0)).sum();
        let volatility = rms(&returns).max(1e-9);
        Some(Features {
            close: b.close,
            raw_close: history.raw_close?,
            turnover,
            mean: mean(self.config.lookback),
            exit_mean: if self.config.exit_window > 0 {
                mean(self.config.exit_window)
            } else {
                b.close
            },
            momentum: b.close / bars[n - 1 - self.config.lookback].close - 1.0,
            short_return: b.close / bars[n - 1 - self.config.short_window].close - 1.0,
            volatility,
            contraction: rms(short) / volatility,
            prior_high: bars
                .iter()
                .rev()
                .skip(1)
                .take(self.config.lookback)
                .map(|b| b.high)
                .fold(0.0, f64::max),
            volume_ratio: b.turnover / turnover,
            rsi: if gains + losses > 0.0 {
                100.0 * gains / (gains + losses)
            } else {
                50.0
            },
            close_strength: if b.high > b.low {
                (b.close - b.low) / (b.high - b.low)
            } else {
                1.0
            },
            lower_wick: (b.close.min(b.open) - b.low) / (b.high - b.low).max(1e-9),
            intraday: b.close / b.open - 1.0,
        })
    }
    fn trend_strength(&self, f: Features) -> f64 {
        match self.config.rank_by {
            TrendRank::RiskAdjusted => f.momentum / f.volatility,
            TrendRank::Momentum => f.momentum,
            TrendRank::Volume => f.volume_ratio,
        }
    }
    fn entry_score(&self, f: Features) -> Option<f64> {
        if self.config.pattern != Pattern::OnlineAlpha && f.momentum < self.config.min_momentum {
            return None;
        }
        match self.config.pattern {
            Pattern::OnlineAlpha => self.model.predict(Self::factors(f)).filter(|&p| p > 0.0),
            Pattern::Breakout => (f.close > f.prior_high
                && f.volume_ratio >= self.config.volume_ratio
                && f.close_strength >= self.config.close_strength)
                .then_some(self.trend_strength(f)),
            Pattern::Pullback => {
                (f.close > f.mean && f.short_return < 0.0).then_some(-f.short_return / f.volatility)
            }
            Pattern::RsiReversion => (f.rsi < self.config.rsi_entry && f.close > f.mean)
                .then_some(-f.rsi - f.short_return),
            Pattern::Compression => (f.close > f.mean
                && f.close >= f.prior_high * 0.9
                && f.contraction < 0.75
                && f.short_return > 0.0)
                .then_some(self.trend_strength(f)),
            Pattern::SmoothTrend => {
                (f.close > f.mean && f.short_return > 0.0 && f.contraction < 1.0 && f.rsi < 90.0)
                    .then_some(self.trend_strength(f))
            }
            Pattern::Recovery => (f.lower_wick >= 0.4
                && f.intraday > 0.0
                && f.volume_ratio >= self.config.volume_ratio)
                .then_some(f.lower_wick * f.volume_ratio),
        }
    }
    fn factors(f: Features) -> Factors {
        let clip = |x: f64| x.clamp(-3.0, 3.0);
        let momentum = clip(f.momentum / 0.3);
        let short = clip(f.short_return / 0.1);
        let volume = clip(f.volume_ratio.max(1e-9).ln());
        let volatility = clip(f.volatility / 0.05);
        [
            1.0,
            momentum,
            short,
            clip(f.intraday / 0.05),
            volume,
            clip(f.contraction - 1.0),
            (f.rsi - 50.0) / 50.0,
            clip((f.close / f.mean - 1.0) / 0.1),
            clip((f.close / f.prior_high - 1.0) / 0.2),
            volatility,
            f.lower_wick,
            clip((f.turnover / 1e8).ln()),
            clip((f.raw_close / 10.0).ln()),
            short * momentum,
            short * volume,
            short * volatility,
        ]
    }
    fn pattern_exit(&self, f: Features) -> bool {
        match self.config.pattern {
            Pattern::OnlineAlpha => false,
            Pattern::RsiReversion | Pattern::Recovery | Pattern::Pullback => {
                f.close > f.exit_mean && self.config.exit_window > 0
            }
            Pattern::Breakout | Pattern::Compression | Pattern::SmoothTrend => {
                f.close < f.exit_mean && self.config.exit_window > 0
            }
        }
    }
    async fn refresh(&mut self, ctx: &BtContext<'_>, members: &BTreeSet<StockSymbol>) {
        let date = ctx.date();
        let missing: Vec<_> = members
            .iter()
            .copied()
            .filter(|s| !self.listed_dates.contains_key(s))
            .collect();
        for (symbol, info) in ctx.stocks_info(&missing).await {
            self.listed_dates.insert(symbol, info.listed);
        }
        let mut groups: BTreeMap<Date, Vec<StockSymbol>> = BTreeMap::new();
        for &symbol in members {
            let start = if let Some(window) = self.histories.get(&symbol) {
                assert!(date >= window.end);
                if date == window.end {
                    continue;
                }
                window.end.next_day().unwrap()
            } else {
                date.checked_sub(Duration::days(self.config.warmup_period() as i64 * 2))
                    .unwrap_or(Date::MIN)
                    .max(self.listed_dates[&symbol].max(self.config.history_start))
            };
            if start <= date {
                groups.entry(start).or_default().push(symbol);
            }
        }
        for (start, symbols) in groups {
            let mut bars = ctx.stocks_bars(&symbols, DateRange::new(start, date)).await;
            for symbol in symbols {
                self.update_history(ctx, symbol, bars.remove(&symbol).unwrap_or_default())
                    .await;
            }
        }
    }
}
#[async_trait::async_trait]
impl Strategy for PatternRotation {
    fn name(&self) -> &str {
        match self.config.pattern {
            Pattern::Breakout => "volume_breakout",
            Pattern::Pullback => "trend_pullback",
            Pattern::RsiReversion => "rsi_reversion",
            Pattern::Compression => "volatility_compression",
            Pattern::Recovery => "wick_recovery",
            Pattern::SmoothTrend => "smooth_trend",
            Pattern::OnlineAlpha => "online_alpha",
        }
    }
    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        self.day_number += 1;
        let date = ctx.date();
        if date < self.config.history_start {
            return Vec::new();
        }
        let mut members = Some(BTreeSet::new());
        for symbol in &self.config.symbols {
            let hist = ctx.index_comp(symbol, DateRange::new(date, date)).await;
            match hist.composition(date) {
                Ok(comp) => members
                    .as_mut()
                    .unwrap()
                    .extend(comp.weights().keys().copied()),
                Err(_) => {
                    members = None;
                    break;
                }
            }
        }
        let held: BTreeSet<_> = ctx.positions.keys().copied().collect();
        let all: BTreeSet<_> = members
            .iter()
            .flatten()
            .copied()
            .chain(held.iter().copied())
            .chain(self.model.symbols())
            .collect();
        self.refresh(ctx, &all).await;
        let features: BTreeMap<_, _> = all
            .iter()
            .filter_map(|&s| self.features(s, date).map(|f| (s, f)))
            .collect();
        if self.config.pattern == Pattern::OnlineAlpha {
            let opens = all
                .iter()
                .filter_map(|&s| {
                    self.histories
                        .get(&s)
                        .and_then(|h| h.bars.back())
                        .filter(|b| b.date == date)
                        .map(|b| (s, b.open))
                })
                .collect();
            self.model.observe(self.day_number, &opens);
        }
        self.holdings.retain(|s, _| held.contains(s));
        self.pending_exits.retain(|s| held.contains(s));
        for &s in &held {
            let bar = self
                .histories
                .get(&s)
                .and_then(|h| h.bars.back())
                .filter(|b| b.date == date);
            let state = self.holdings.entry(s).or_insert(Holding {
                entered: self.day_number,
                peak: bar.map_or(0.0, |b| b.open),
            });
            if let Some(bar) = bar {
                state.peak = state.peak.max(bar.close);
                if bar.st
                    || bar.delisting
                    || bar.close <= state.peak * (1.0 - self.config.trailing_stop)
                {
                    self.pending_exits.insert(s);
                }
            }
            if self.day_number - state.entered + 1
                >= if self.config.pattern == Pattern::OnlineAlpha {
                    self.config.prediction_days
                } else {
                    self.config.max_hold_days
                }
                || members.as_ref().is_some_and(|m| !m.contains(&s))
                || features.get(&s).is_some_and(|&f| self.pattern_exit(f))
            {
                self.pending_exits.insert(s);
            }
        }
        let mut batches = Vec::new();
        if !self.pending_exits.is_empty() {
            batches.push(
                self.pending_exits
                    .iter()
                    .map(|&symbol| Order::SellAll { symbol })
                    .collect(),
            );
        }
        let slots = self.config.top_k.saturating_sub(held.len());
        let Some(members) = members else {
            return batches;
        };
        let eligible: Vec<_> = features
            .iter()
            .filter(|(s, _)| {
                members.contains(s)
                    && features[s].turnover >= self.config.min_turnover
                    && features[s].raw_close >= self.config.minimum_price
                    && (date - self.listed_dates[s]).whole_days()
                        >= i64::from(self.config.minimum_listed_days)
            })
            .collect();
        if self.config.pattern == Pattern::OnlineAlpha {
            self.model.enqueue(
                self.day_number,
                eligible
                    .iter()
                    .map(|&(&s, &f)| (s, Self::factors(f)))
                    .collect(),
            );
        }
        let breadth = eligible.iter().filter(|(_, f)| f.close > f.mean).count() as f64
            / eligible.len().max(1) as f64;
        if self
            .config
            .exit_breadth
            .is_some_and(|limit| breadth < limit)
        {
            self.pending_exits.extend(held.iter().copied());
            return if self.pending_exits.is_empty() {
                Vec::new()
            } else {
                vec![
                    self.pending_exits
                        .iter()
                        .map(|&symbol| Order::SellAll { symbol })
                        .collect(),
                ]
            };
        }
        if slots == 0 || breadth < self.config.min_breadth || ctx.cash <= 0.0 {
            return batches;
        }
        let mut ranked: Vec<_> = eligible
            .into_iter()
            .filter(|(s, _)| !held.contains(s))
            .filter_map(|(&s, &f)| self.entry_score(f).map(|score| (s, score)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(slots);
        if !ranked.is_empty() {
            // The buy batch executes after sells. Using pre-sale cash here could
            // unintentionally concentrate almost the whole account in one new stock.
            let expected_cash = ctx.cash
                + self
                    .pending_exits
                    .iter()
                    .map(|symbol| ctx.positions[symbol].market_value)
                    .sum::<f64>();
            let weight =
                (self.config.allocation * ctx.equity / self.config.top_k as f64 / expected_cash)
                    .min(self.config.allocation / ranked.len() as f64);
            batches.push(vec![Order::BuyWeights {
                weights: ranked.into_iter().map(|(s, _)| (s, weight)).collect(),
            }]);
        }
        batches
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
                    Arc::new(
                        IndexComp::new(
                            self.bars
                                .keys()
                                .map(|&symbol| (symbol, 1.0 / self.bars.len() as f32))
                                .collect(),
                        )
                        .unwrap(),
                    ),
                )],
            )
            .unwrap()
        }
    }

    /// 两只股票价格路径成比例；同分时按代码选第一只。
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

    fn symbol(id: usize) -> StockSymbol {
        format!("{id:06}").as_str().into()
    }

    fn small_config() -> PatternRotationConfig {
        PatternRotationConfig {
            pattern: Pattern::Breakout,
            top_k: 1,
            lookback: 3,
            short_window: 1,
            exit_window: 2,
            max_hold_days: 100,
            min_momentum: 0.01,
            volume_ratio: 0.1,
            min_breadth: 0.0,
            exit_breadth: None,
            allocation: 0.95,
            min_turnover: 0.0,
            minimum_price: 0.0,
            minimum_listed_days: 0,
            ..Default::default()
        }
    }

    fn bar(symbol: StockSymbol, day: i64, close: f64, turnover: f64) -> StockBar {
        StockBar {
            symbol,
            date: date!(2024 - 01 - 01) + Duration::days(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 10000.0,
            turnover,
            limit_up: None,
            limit_down: None,
            float_market_cap: None,
            adjustment: Some(Adjustment::Raw(1.0)),
            st: false,
            delisting: false,
        }
    }

    fn window(bars: Vec<StockBar>) -> HistoryWindow {
        let raw_close = bars.last().map(|bar| bar.close);
        HistoryWindow {
            start: bars[0].date,
            end: bars.last().unwrap().date,
            raw_close,
            bars: bars.into_iter().map(StockBar::adjusted).collect(),
        }
    }

    type Signals = Arc<Mutex<Vec<(Date, String)>>>;

    struct RecordingStrategy {
        inner: PatternRotation,
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
    }

    async fn backtest(provider: FixtureProvider) -> (BacktestResult, Vec<(Date, String)>) {
        backtest_with_config(provider, small_config()).await
    }
    async fn backtest_with_config(
        provider: FixtureProvider,
        config: PatternRotationConfig,
    ) -> (BacktestResult, Vec<(Date, String)>) {
        let signals = Signals::default();
        let strategy = RecordingStrategy {
            inner: PatternRotation::new(config),
            signals: signals.clone(),
        };
        let result = BacktestEngine::new(BacktestConfig {
            start: date!(2024 - 01 - 21),
            end: date!(2024 - 01 - 26),
            ..Default::default()
        })
        .unwrap()
        .run(Box::new(provider), Box::new(strategy))
        .await
        .unwrap();
        let recorded = signals.lock().unwrap().clone();
        (result, recorded)
    }

    fn path() -> Vec<f64> {
        [vec![10.0; 20], vec![11.0, 12.0, 13.0, 8.0, 8.0, 8.0]].concat()
    }
    fn feature() -> Features {
        Features {
            close: 11.0,
            raw_close: 11.0,
            turnover: 1e8,
            mean: 10.0,
            exit_mean: 10.5,
            momentum: 0.2,
            short_return: -0.05,
            volatility: 0.03,
            contraction: 0.5,
            prior_high: 10.9,
            volume_ratio: 2.0,
            rsi: 5.0,
            lower_wick: 0.5,
            close_strength: 0.9,
            intraday: 0.01,
        }
    }
    #[test]
    fn modes_have_distinct_entry_and_exit_conditions() {
        let mut strategy = PatternRotation::new(small_config());
        assert!(strategy.entry_score(feature()).is_some());
        strategy.config.close_strength = 0.8;
        assert!(
            strategy
                .entry_score(Features {
                    close_strength: 0.4,
                    ..feature()
                })
                .is_none()
        );
        assert!(
            strategy
                .entry_score(Features {
                    close: 10.8,
                    ..feature()
                })
                .is_none()
        );
        strategy.config.pattern = Pattern::Pullback;
        assert!(strategy.entry_score(feature()).is_some());
        assert!(
            strategy
                .entry_score(Features {
                    short_return: 0.01,
                    ..feature()
                })
                .is_none()
        );
        assert!(strategy.pattern_exit(feature()));
        strategy.config.pattern = Pattern::RsiReversion;
        assert!(strategy.entry_score(feature()).is_some());
        assert!(
            strategy
                .entry_score(Features {
                    rsi: 70.0,
                    ..feature()
                })
                .is_none()
        );
        strategy.config.pattern = Pattern::Compression;
        assert!(strategy.entry_score(feature()).is_none());
        assert!(
            strategy
                .entry_score(Features {
                    short_return: 0.01,
                    ..feature()
                })
                .is_some()
        );
        assert!(!strategy.pattern_exit(feature()));
        strategy.config.pattern = Pattern::Recovery;
        assert!(strategy.entry_score(feature()).is_some());
        assert!(
            strategy
                .entry_score(Features {
                    lower_wick: 0.1,
                    ..feature()
                })
                .is_none()
        );
    }
    #[test]
    fn invalid_parameters_fail_before_backtesting() {
        let bad = [
            PatternRotationConfig {
                lookback: usize::MAX,
                ..Default::default()
            },
            PatternRotationConfig {
                short_window: 0,
                ..Default::default()
            },
            PatternRotationConfig {
                top_k: 0,
                ..Default::default()
            },
            PatternRotationConfig {
                max_hold_days: 0,
                ..Default::default()
            },
            PatternRotationConfig {
                min_momentum: f64::NAN,
                ..Default::default()
            },
            PatternRotationConfig {
                trailing_stop: 0.0,
                ..Default::default()
            },
            PatternRotationConfig {
                min_breadth: 1.1,
                ..Default::default()
            },
            PatternRotationConfig {
                volume_ratio: -1.0,
                ..Default::default()
            },
            PatternRotationConfig {
                allocation: 1.1,
                ..Default::default()
            },
        ];
        for c in bad {
            assert!(std::panic::catch_unwind(|| PatternRotation::new(c)).is_err());
        }
    }
    #[test]
    fn breakout_uses_prior_high_and_adjusted_prices() {
        let mut bars: Vec<_> = (0..21).map(|i| bar(symbol(1), i, 10.0, 100.0)).collect();
        bars[20].close = 5.5;
        bars[20].open = 5.5;
        bars[20].high = 7.0;
        bars[20].low = 5.0;
        bars[20].adjustment = Some(Adjustment::Raw(2.0));
        let mut strategy = PatternRotation::new(small_config());
        strategy.histories.insert(symbol(1), window(bars));
        let f = strategy.features(symbol(1), date!(2024 - 01 - 21)).unwrap();
        assert!((f.prior_high - 10.0).abs() < 1e-12);
        assert!((f.momentum - 0.1).abs() < 1e-12);
        assert!(strategy.entry_score(f).is_some());
        assert!(
            strategy
                .features(symbol(1), date!(2024 - 01 - 22))
                .is_none()
        );
    }
    #[tokio::test]
    async fn breakout_executes_next_open_and_independent_exit_closes_it() {
        let (result, _) = backtest(fixture(&path())).await;
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.trades[0].signal_date, date!(2024 - 01 - 21));
        assert_eq!(result.trades[0].date, date!(2024 - 01 - 22));
        assert_eq!(result.trades[0].side, "buy");
        assert_eq!(result.trades[1].signal_date, date!(2024 - 01 - 24));
        assert_eq!(result.trades[1].date, date!(2024 - 01 - 25));
        assert_eq!(result.trades[1].side, "sell");
    }
    #[tokio::test]
    async fn failed_sell_is_latched_and_cannot_free_a_buy_slot() {
        let mut provider = fixture(&path());
        provider.bars.get_mut(&symbol(1)).unwrap()[24].limit_down = Some(8.0);
        // Another stock breaks out while the first is still blocked from selling.
        let other = provider.bars.get_mut(&symbol(2)).unwrap();
        for b in &mut other[24..] {
            b.open = 40.0;
            b.high = 40.0;
            b.low = 40.0;
            b.close = 40.0;
            b.limit_up = Some(44.0);
            b.limit_down = Some(36.0);
        }
        let (result, signals) = backtest(provider).await;
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.trades[1].date, date!(2024 - 01 - 26));
        assert_eq!(result.skipped_orders.len(), 1);
        assert!(
            !signals
                .iter()
                .find(|(date, _)| *date == date!(2024 - 01 - 25))
                .unwrap()
                .1
                .contains("BuyWeights")
        );
    }
    #[tokio::test]
    async fn future_prices_factors_and_status_do_not_change_earlier_decisions() {
        let original = fixture(&path());
        let mut modified = original.clone();
        for bars in modified.bars.values_mut() {
            for b in bars.iter_mut().filter(|b| b.date > date!(2024 - 01 - 24)) {
                b.open *= 3.0;
                b.high *= 3.0;
                b.low *= 3.0;
                b.close *= 3.0;
                b.limit_up = b.limit_up.map(|p| p * 3.0);
                b.limit_down = b.limit_down.map(|p| p * 3.0);
                b.adjustment = Some(Adjustment::Raw(2.0));
                b.st = true;
            }
        }
        let (a, sa) = backtest(original).await;
        let (b, sb) = backtest(modified).await;
        assert!(!a.trades.is_empty());
        let before = |s: Vec<(Date, String)>| {
            s.into_iter()
                .filter(|(d, _)| *d <= date!(2024 - 01 - 24))
                .collect::<Vec<_>>()
        };
        assert_eq!(before(sa), before(sb));
        assert_eq!(
            serde_json::to_value(&a.equity_curve[..4]).unwrap(),
            serde_json::to_value(&b.equity_curve[..4]).unwrap()
        );
    }

    #[tokio::test]
    async fn same_day_sells_do_not_inflate_the_new_position_budget() {
        let prices = [vec![10.0; 20], vec![11.0, 9.0, 9.0, 9.0, 9.0, 9.0]].concat();
        let mut provider = fixture(&prices);
        for (i, bar) in provider
            .bars
            .get_mut(&symbol(2))
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let price = if i <= 20 { 20.0 } else { 22.0 };
            bar.open = price;
            bar.close = price;
            bar.high = price;
            bar.low = price;
            bar.limit_up = Some(price * 1.1);
            bar.limit_down = Some(price * 0.9);
        }
        let (result, _) = backtest_with_config(
            provider,
            PatternRotationConfig {
                top_k: 2,
                ..small_config()
            },
        )
        .await;
        let equity = result
            .equity_curve
            .iter()
            .find(|p| p.date == date!(2024 - 01 - 22))
            .unwrap()
            .equity;
        let buy = result
            .trades
            .iter()
            .find(|t| t.symbol == symbol(2).to_string() && t.side == "buy")
            .unwrap();
        assert_eq!(buy.date, date!(2024 - 01 - 23));
        assert!(
            result
                .trades
                .iter()
                .any(|t| t.date == buy.date && t.side == "sell")
        );
        assert!(buy.notional + buy.commission <= equity * 0.475 + 1e-8);
        assert!(buy.notional > equity * 0.4);
    }

    #[tokio::test]
    async fn online_predictions_ignore_future_prices_and_status() {
        let prices = [vec![10.0; 20], vec![11.0, 12.0, 13.0, 14.0, 15.0, 16.0]].concat();
        let base = fixture(&prices).bars.remove(&symbol(1)).unwrap();
        let original = FixtureProvider {
            bars: (1..=600)
                .map(|id| {
                    (
                        symbol(id),
                        base.iter()
                            .map(|bar| StockBar {
                                symbol: symbol(id),
                                ..*bar
                            })
                            .collect(),
                    )
                })
                .collect(),
        };
        let mut changed = original.clone();
        for bars in changed.bars.values_mut() {
            for bar in &mut bars[24..] {
                bar.open *= 2.0;
                bar.close *= 2.0;
                bar.high *= 2.0;
                bar.low *= 2.0;
                bar.limit_up = bar.limit_up.map(|p| p * 2.0);
                bar.limit_down = bar.limit_down.map(|p| p * 2.0);
                bar.adjustment = Some(Adjustment::Raw(3.0));
                bar.st = true;
            }
        }
        let config = PatternRotationConfig {
            pattern: Pattern::OnlineAlpha,
            prediction_days: 2,
            ..small_config()
        };
        let (a, sa) = backtest_with_config(original, config.clone()).await;
        let (b, sb) = backtest_with_config(changed, config).await;
        let before = |s: Vec<(Date, String)>| {
            s.into_iter()
                .filter(|(d, _)| *d <= date!(2024 - 01 - 24))
                .collect::<Vec<_>>()
        };
        let sa = before(sa);
        assert!(sa.iter().any(|(_, s)| s.contains("BuyWeights")));
        assert_eq!(sa, before(sb));
        assert_eq!(
            serde_json::to_value(&a.equity_curve[..4]).unwrap(),
            serde_json::to_value(&b.equity_curve[..4]).unwrap()
        );
    }

    #[tokio::test]
    async fn breadth_risk_exit_can_close_a_stock_that_is_still_rising() {
        let mut provider = fixture(&path());
        let other = &mut provider.bars.get_mut(&symbol(2)).unwrap()[21];
        other.open = 15.0;
        other.close = 15.0;
        other.high = 15.0;
        other.low = 15.0;
        other.limit_up = Some(16.5);
        other.limit_down = Some(13.5);
        let config = PatternRotationConfig {
            min_breadth: 0.8,
            exit_breadth: Some(0.75),
            ..small_config()
        };
        let (result, _) = backtest_with_config(provider, config).await;
        let sell = result.trades.iter().find(|t| t.side == "sell").unwrap();
        assert_eq!(sell.signal_date, date!(2024 - 01 - 22));
        assert_eq!(sell.date, date!(2024 - 01 - 23));
    }
}
