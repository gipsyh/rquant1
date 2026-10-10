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
pub struct MomentumLeaderConfig {
    /// 动态股票池的指数代码
    #[arg(long, default_value = "399101.XSHE")]
    pub symbol: String,
    /// 目标持仓股票数
    #[arg(long, default_value_t = 2)]
    pub top_k: usize,
    /// 调仓间隔（交易日）
    #[arg(long, default_value_t = 15)]
    pub rebalance_days: usize,
    /// 动量收益窗口（日线根数）
    #[arg(long, default_value_t = 20)]
    pub mom_lookback: usize,
    /// 均线趋势过滤窗口（日线根数）
    #[arg(long, default_value_t = 20)]
    pub trend_lookback: usize,
    /// 平均成交额过滤窗口（日线根数）
    #[arg(long, default_value_t = 20)]
    pub turnover_lookback: usize,
    /// 最低平均日成交额（元），过滤流动性陷阱
    #[arg(long, default_value_t = 30_000_000.0)]
    pub min_turnover: f64,
    /// 现有持仓排名不超过 top_k × 此倍数时优先保留，避免频繁摩擦
    #[arg(long, default_value_t = 2)]
    pub retain_buffer: usize,
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

impl Default for MomentumLeaderConfig {
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            config: MomentumLeaderConfig,
        }
        Defaults::parse_from(["momentum-leader"]).config
    }
}

impl MomentumLeaderConfig {
    pub fn validate(&self) {
        assert!(!self.symbol.trim().is_empty(), "指数代码不能为空");
        assert!(self.top_k > 0, "持仓股票数必须为正整数");
        assert!(self.rebalance_days > 0, "调仓间隔必须为正整数");
        assert!(self.mom_lookback > 0, "动量窗口必须为正整数");
        assert!(self.trend_lookback > 0, "均线窗口必须为正整数");
        assert!(self.turnover_lookback > 0, "成交额窗口必须为正整数");
        assert!(self.min_turnover >= 0.0, "最小成交额必须为非负数");
        assert!(
            self.retain_buffer > 0 && self.top_k.checked_mul(self.retain_buffer).is_some(),
            "持仓缓冲倍数必须为正整数且不能溢出"
        );
        assert!(
            self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0,
            "现金比例须在 (0, 1] 内"
        );
    }

    pub fn warmup_period(&self) -> usize {
        (self.mom_lookback + 1)
            .max(self.trend_lookback)
            .max(self.turnover_lookback)
    }
}

struct HistoryWindow {
    start: Date,
    end: Date,
    bars: VecDeque<StockBar>,
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    symbol: StockSymbol,
    momentum: f64,
    avg_turnover: f64,
}

/// 动量龙头轮动策略：聚焦指数内高成交额、站在 MA20 之上且过去 60 日动量领涨的核心龙头个股。
/// 结合宽容缓冲池（retain_buffer）与双周调仓节奏，降低摩擦损耗，充分捕捉主升浪。
pub struct MomentumLeader {
    config: MomentumLeaderConfig,
    listed_dates: BTreeMap<StockSymbol, Date>,
    histories: BTreeMap<StockSymbol, HistoryWindow>,
    latest_selection: Vec<StockSymbol>,
    day_number: usize,
    last_rebalance: Option<usize>,
    retry_orders: bool,
}

impl MomentumLeader {
    pub fn new(config: MomentumLeaderConfig) -> Self {
        config.validate();
        log::debug!("动量龙头策略参数: {config:?}");
        Self {
            config,
            listed_dates: BTreeMap::new(),
            histories: BTreeMap::new(),
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
        let mut span = Duration::days(count as i64 * 3);
        assert!(bars.iter().all(|bar| bar.date <= end), "日线包含未来数据");
        if let Some(window) = self.histories.get_mut(&symbol) {
            assert!(end >= window.end, "策略日期不能倒退");
            if end > window.end {
                for bar in bars {
                    assert!(bar.date > window.end, "日线增量区间重叠");
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
        let avg_turnover = bars
            .iter()
            .rev()
            .take(self.config.turnover_lookback)
            .map(|bar| bar.turnover / self.config.turnover_lookback as f64)
            .sum::<f64>();
        if avg_turnover < self.config.min_turnover {
            return None;
        }
        let ma_trend = bars
            .iter()
            .rev()
            .take(self.config.trend_lookback)
            .map(|bar| bar.close / self.config.trend_lookback as f64)
            .sum::<f64>();
        if current.close <= ma_trend {
            return None;
        }
        let past = &bars[bars.len() - 1 - self.config.mom_lookback];
        let momentum = current.close / past.close - 1.0;
        assert!(momentum.is_finite(), "动量指标计算溢出");
        Some(Candidate {
            symbol,
            momentum,
            avg_turnover,
        })
    }

    async fn signal(&mut self, ctx: &BtContext<'_>) -> Option<Vec<Candidate>> {
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
                log::warn!("{date} 没有已生效的指数成分，跳过动量龙头信号: {err}");
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
                date.checked_sub(Duration::days(self.config.warmup_period() as i64 * 3))
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
            b.momentum
                .total_cmp(&a.momentum)
                .then(b.avg_turnover.total_cmp(&a.avg_turnover))
                .then(a.symbol.cmp(&b.symbol))
        });
        Some(ranked)
    }

    fn select(&self, ranked: &[Candidate], held: &BTreeSet<StockSymbol>) -> Vec<StockSymbol> {
        let mut selected: Vec<StockSymbol> = Vec::new();
        let buffer_limit = self.config.top_k * self.config.retain_buffer;
        for (rank, cand) in ranked.iter().enumerate() {
            if rank < buffer_limit && held.contains(&cand.symbol) {
                selected.push(cand.symbol);
                if selected.len() == self.config.top_k {
                    break;
                }
            }
        }
        for cand in ranked {
            if selected.len() == self.config.top_k {
                break;
            }
            if !selected.contains(&cand.symbol) {
                selected.push(cand.symbol);
            }
        }
        selected.sort();
        selected
    }

    fn orders(&self, held: &BTreeSet<StockSymbol>, selection: &[StockSymbol]) -> Vec<Vec<Order>> {
        let selected: BTreeSet<_> = selection.iter().copied().collect();
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
impl Strategy for MomentumLeader {
    fn name(&self) -> &str {
        "momentum_leader"
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

        let Some(ranked) = self.signal(ctx).await else {
            return if unsafe_held.is_empty() {
                Vec::new()
            } else {
                vec![unsafe_held]
            };
        };

        let selection = self.select(&ranked, &held);
        if selection != self.latest_selection {
            log::info!(
                "{} 动量龙头：选股 {}",
                ctx.date(),
                selection
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        self.latest_selection = selection.clone();

        let is_scheduled = self
            .last_rebalance
            .is_none_or(|day| self.day_number - day >= self.config.rebalance_days);

        if is_scheduled {
            self.last_rebalance = Some(self.day_number);
            self.retry_orders = false;
            self.orders(&held, &selection)
        } else if self.retry_orders && held.len() < self.config.top_k {
            self.retry_orders = false;
            let needed: Vec<_> = selection
                .iter()
                .filter(|s| !held.contains(s))
                .copied()
                .collect();
            if !needed.is_empty() {
                let slots = (self.config.top_k - held.len()).min(needed.len());
                let weight = self.config.allocation / slots as f64;
                let weights = needed
                    .into_iter()
                    .take(slots)
                    .map(|s| (s, weight))
                    .collect();
                vec![vec![Order::BuyWeights { weights }]]
            } else if !unsafe_held.is_empty() {
                vec![unsafe_held]
            } else {
                Vec::new()
            }
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

    fn sym(code: &str) -> StockSymbol {
        code.parse().unwrap()
    }

    #[test]
    fn select_keeps_held_stocks_within_retain_buffer() {
        let strategy = MomentumLeader::new(MomentumLeaderConfig {
            top_k: 3,
            retain_buffer: 2,
            ..Default::default()
        });
        let cands = vec![
            Candidate {
                symbol: sym("000001.SZ"),
                momentum: 0.50,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000002.SZ"),
                momentum: 0.40,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000003.SZ"),
                momentum: 0.30,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000004.SZ"),
                momentum: 0.25,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000005.SZ"),
                momentum: 0.20,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000006.SZ"),
                momentum: 0.15,
                avg_turnover: 1e8,
            },
            Candidate {
                symbol: sym("000007.SZ"),
                momentum: 0.10,
                avg_turnover: 1e8,
            },
        ];
        // Suppose held contains 000004.SZ (rank 3 < top_k * 2 = 6) and 000007.SZ (rank 6, not < 6)
        let mut held = BTreeSet::new();
        held.insert(sym("000004.SZ"));
        held.insert(sym("000007.SZ"));

        let selected = strategy.select(&cands, &held);
        assert_eq!(selected.len(), 3);
        // 000004.SZ is retained because its rank is within buffer limit
        assert!(selected.contains(&sym("000004.SZ")));
        // 000007.SZ is dropped because its rank >= buffer limit (6)
        assert!(!selected.contains(&sym("000007.SZ")));
        // Top new candidates fill remaining 2 slots: 000001.SZ and 000002.SZ
        assert!(selected.contains(&sym("000001.SZ")));
        assert!(selected.contains(&sym("000002.SZ")));
    }

    #[test]
    fn orders_splits_sells_and_buys_correctly() {
        let strategy = MomentumLeader::new(MomentumLeaderConfig {
            top_k: 2,
            allocation: 0.90,
            ..Default::default()
        });
        let mut held = BTreeSet::new();
        held.insert(sym("000001.SZ"));
        held.insert(sym("000002.SZ"));

        let selection = vec![sym("000001.SZ"), sym("000003.SZ")];
        let batches = strategy.orders(&held, &selection);
        assert_eq!(batches.len(), 2);
        // Batch 0: SellAll for 000002.SZ
        assert_eq!(batches[0].len(), 1);
        match &batches[0][0] {
            Order::SellAll { symbol } => assert_eq!(*symbol, sym("000002.SZ")),
            _ => panic!("Expected SellAll"),
        }
        // Batch 1: BuyWeights for 000003.SZ with weight 0.90
        assert_eq!(batches[1].len(), 1);
        match &batches[1][0] {
            Order::BuyWeights { weights } => {
                assert_eq!(weights.len(), 1);
                assert!((weights[&sym("000003.SZ")] - 0.90).abs() < 1e-6);
            }
            _ => panic!("Expected BuyWeights"),
        }
    }
}
