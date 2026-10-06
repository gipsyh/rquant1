use super::Strategy;
use crate::{
    data::{StockBar, StockSymbol},
    engine::{BtContext, Order},
    utils::DateRange,
};
use clap::Args;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use time::Date;

#[derive(Args, Debug, Clone)]
pub struct MaCrossConfig {
    /// 目标股票，可重复指定或用逗号分隔
    #[arg(long = "symbol", required = true, value_delimiter = ',')]
    pub symbols: Vec<StockSymbol>,
    /// 短期 SMA 周期，按有日线的交易日计数
    #[arg(long, default_value_t = 5)]
    pub short: usize,
    /// 长期 SMA 周期，必须大于短期周期
    #[arg(long, default_value_t = 20)]
    pub long: usize,
    /// 目标总仓位比例；按信号日收盘权益等额分配各股票的买入预算
    #[arg(long, default_value_t = 1.0)]
    pub allocation: f64,
}

impl MaCrossConfig {
    pub fn validate(&self) {
        assert!(
            !self.symbols.is_empty()
                && self.symbols.iter().collect::<BTreeSet<_>>().len() == self.symbols.len(),
            "目标股票不能为空或包含重复股票"
        );
        assert!(
            self.short > 0 && self.short < self.long && self.long < usize::MAX,
            "均线周期须满足 0 < short < long < usize::MAX"
        );
        assert!(
            self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0,
            "仓位比例须在 (0, 1] 内"
        );
    }
}

#[derive(Default)]
struct SignalState {
    // 仅缓存 FactorAdjusted 日线，每根新增日线只转换一次。
    bars: VecDeque<StockBar>,
    // None 表示尚未发生交叉；不能仅因初始短均线较高就买入。
    target_long: Option<bool>,
}

/// 回测内积累 long + 1 根已完成日线，按相邻两根日线的 SMA 判断交叉。
/// 信号在下一次开盘交易；未成交时维持目标，直到出现反向交叉。
pub struct MaCross {
    config: MaCrossConfig,
    states: BTreeMap<StockSymbol, SignalState>,
    next_history_date: Option<Date>,
}

impl MaCross {
    pub fn new(config: MaCrossConfig) -> Self {
        config.validate();
        Self {
            config,
            states: BTreeMap::new(),
            next_history_date: None,
        }
    }
}

fn crossover(bars: &VecDeque<StockBar>, short: usize, long: usize) -> Option<bool> {
    if bars.len() < long + 1 {
        return None;
    }
    let mean = |offset: usize, period: usize| {
        bars.iter()
            .skip(offset)
            .take(period)
            .map(|bar| bar.close / period as f64)
            .sum::<f64>()
    };
    let prev_short = mean(long - short, short);
    let prev_long = mean(0, long);
    let curr_short = mean(long + 1 - short, short);
    let curr_long = mean(1, long);
    assert!(
        [prev_short, prev_long, curr_short, curr_long]
            .iter()
            .all(|v| v.is_finite()),
        "均线计算溢出"
    );
    if prev_short <= prev_long && curr_short > curr_long {
        Some(true)
    } else if prev_short >= prev_long && curr_short < curr_long {
        Some(false)
    } else {
        None
    }
}

#[async_trait::async_trait]
impl Strategy for MaCross {
    fn name(&self) -> &str {
        "ma_cross"
    }

    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        let start = self.next_history_date.unwrap_or(ctx.date());
        let end = ctx.date();
        self.next_history_date = end.next_day();
        let mut sells = Vec::new();
        let mut buys = Vec::new();
        let budget = ctx.equity * self.config.allocation / self.config.symbols.len() as f64;
        let mut available = ctx.cash;
        for &symbol in &self.config.symbols {
            let history = ctx.stock_bars(symbol, DateRange::new(start, end)).await;
            let state = self.states.entry(symbol).or_default();
            for bar in history {
                let bar = bar.adjusted();
                state.bars.push_back(bar);
                if state.bars.len() > self.config.long + 1 {
                    state.bars.pop_front();
                }
                if let Some(target) = crossover(&state.bars, self.config.short, self.config.long) {
                    state.target_long = Some(target);
                }
            }
            let held = ctx.position(symbol).is_some_and(|p| p.purchased_shares > 0);
            match (state.target_long, held) {
                (Some(false), true) => sells.push(Order::SellAll { symbol }),
                (Some(true), false) if available > 0.0 => {
                    let cash_amount = budget.min(available);
                    buys.push(Order::BuyAmount {
                        symbol,
                        cash_amount,
                    });
                    available -= cash_amount;
                }
                _ => {}
            }
        }
        // 卖出批次先结算；买入预算仍保守使用信号日已有现金。
        vec![sells, buys]
    }
}
