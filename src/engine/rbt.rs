use super::execution::Account;
use super::*;
use crate::data::{Adjustment, DataProvider, MemCacheProvider};
use crate::strategy::Strategy;
use std::collections::BTreeSet;

#[derive(Debug, Default)]
pub struct BacktestEngine {
    pub config: BacktestConfig,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    /// 每日依次执行昨日订单、收盘估值、调用策略；末日生成的订单不执行。
    pub async fn run(
        &self,
        provider: Box<dyn DataProvider>,
        mut strategy: Box<dyn Strategy>,
    ) -> Result<BacktestResult> {
        self.config.validate()?;
        let range = DateRange::new(self.config.start, self.config.end);
        let mut provider = MemCacheProvider::new(provider, range);
        let calendar = provider.trading_days(range).await;
        let days: BTreeSet<_> = calendar.iter().copied().collect();
        anyhow::ensure!(
            !days.is_empty()
                && days.len() == calendar.len()
                && days.iter().all(|d| range.contains(*d)),
            "回测交易日历为空、重复或超出区间"
        );
        let mut account = Account::new(self.config.initial_cash);
        let mut symbols = BTreeSet::new();
        let mut pending: Option<(Date, Vec<Vec<Order>>)> = None;
        let mut previous_equity = account.cash;
        let mut peak = account.cash;
        let mut trades = Vec::new();
        let mut equity_curve = Vec::new();
        let mut skipped_orders = Vec::new();
        for date in days {
            account.start_day();
            if let Some((signal_date, batches)) = pending.take() {
                for (batch_index, orders) in batches.into_iter().enumerate() {
                    for order in &orders {
                        match order {
                            Order::BuyLimit { symbol, .. }
                            | Order::BuyAmount { symbol, .. }
                            | Order::SellLimit { symbol, .. }
                            | Order::SellAll { symbol } => {
                                symbols.insert(*symbol);
                            }
                            Order::BuyWeights { weights } => {
                                symbols.extend(weights.keys().copied())
                            }
                        }
                    }
                    let result = self
                        .execute_batch(
                            &mut account,
                            &mut provider,
                            &mut *strategy,
                            signal_date,
                            date,
                            batch_index,
                            orders,
                        )
                        .await?;
                    trades.extend(result.trades);
                    skipped_orders.extend(result.skipped);
                }
            }
            for (&symbol, position) in &mut account.positions {
                let day = DateRange::new(date, date);
                if let Some(bar) = load_bars(&mut provider, symbol, day, self.config.adjust_returns)
                    .await
                    .into_iter()
                    .next()
                {
                    if bar.delisting {
                        unimplemented!(
                            "持仓 {symbol} 于 {date} 处于退市整理期，引擎尚未实现该情形的估值与了结"
                        );
                    }
                    position.market_value = account.units[&symbol]
                        * bar.close
                        * factor(&bar, self.config.adjust_returns);
                }
            }
            let market_value: f64 = account.positions.values().map(|p| p.market_value).sum();
            let equity = account.cash + market_value;
            anyhow::ensure!(
                equity.is_finite() && equity > 0.0 && account.cash >= 0.0,
                "资产估值或现金无效"
            );
            peak = peak.max(equity);
            equity_curve.push(EquityPoint {
                date,
                cash: account.cash,
                positions: account
                    .positions
                    .iter()
                    .map(|(s, p)| (s.to_string(), p.clone()))
                    .collect(),
                market_value,
                equity,
                net_value: equity / self.config.initial_cash,
                daily_return: equity / previous_equity - 1.0,
                drawdown: 1.0 - equity / peak,
            });
            previous_equity = equity;
            let ctx = BtContext {
                date,
                init_cash: self.config.initial_cash,
                cash: account.cash,
                equity,
                positions: &account.positions,
                provider: tokio::sync::Mutex::new(&mut provider),
            };
            pending = Some((date, strategy.on_trade_day(&ctx).await));
        }
        let performance = performance(&equity_curve, &trades, self.config.initial_cash);
        Ok(BacktestResult {
            strategy: strategy.name().into(),
            symbols: symbols.iter().map(ToString::to_string).collect(),
            start: range.start(),
            end: range.end(),
            config: self.config.clone(),
            performance,
            trades,
            equity_curve,
            skipped_orders,
        })
    }
}

/// 缓存属于数据源；引擎仅校验本次实际读取的数据及估值口径。
pub(super) async fn load_bars(
    provider: &mut dyn DataProvider,
    symbol: StockSymbol,
    range: DateRange,
    adjusted: bool,
) -> Vec<StockBar> {
    let hist = provider.stock_bar(symbol, range).await;
    for bar in hist.bars() {
        validate_bar(bar, adjusted).unwrap();
    }
    hist.into_bars()
}

fn factor(bar: &StockBar, adjusted: bool) -> f64 {
    if adjusted && let Some(Adjustment::Raw(factor)) = bar.adjustment {
        factor
    } else {
        1.0
    }
}

pub(super) fn validate_bar(bar: &StockBar, adjusted: bool) -> Result<()> {
    assert!(
        bar.volume.is_finite() && bar.volume > 0.0,
        "日线成交量必须为有限正数"
    );
    let prices = [bar.open, bar.high, bar.low, bar.close];
    if prices.iter().any(|v| !v.is_finite() || *v <= 0.0)
        || bar.low > bar.open.min(bar.close)
        || bar.high < bar.open.max(bar.close)
        || !bar.turnover.is_finite()
        || bar.turnover < 0.0
        || [bar.limit_up, bar.limit_down]
            .into_iter()
            .flatten()
            .any(|v| !v.is_finite() || v <= 0.0)
        || matches!((bar.limit_down, bar.limit_up), (Some(low), Some(high)) if low > high)
        || matches!(
            bar.adjustment,
            Some(Adjustment::Pre | Adjustment::Post | Adjustment::FactorAdjusted)
        )
        || matches!(bar.adjustment, Some(Adjustment::Raw(f)) if !f.is_finite() || f <= 0.0)
        || (adjusted && bar.adjustment.is_none())
    {
        return Err(anyhow!(
            "回测数据无效: {} OHLC、成交量、涨跌停价或原始复权因子无效",
            bar.date
        ));
    }
    Ok(())
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn performance(curve: &[EquityPoint], trades: &[Trade], initial: f64) -> Performance {
    let n = curve.len() as f64;
    let final_equity = curve.last().expect("nonempty calendar").equity;
    let mean = curve.iter().map(|p| p.daily_return).sum::<f64>() / n;
    let stdev = if curve.len() > 1 {
        finite(
            (curve
                .iter()
                .map(|p| (p.daily_return - mean).powi(2))
                .sum::<f64>()
                / (n - 1.0))
                .sqrt(),
        )
    } else {
        None
    };
    Performance {
        final_equity,
        total_return: final_equity / initial - 1.0,
        annualized_return: finite((final_equity / initial).powf(252.0 / n) - 1.0),
        max_drawdown: curve.iter().map(|p| p.drawdown).fold(0.0, f64::max),
        annualized_volatility: stdev.and_then(|s| finite(s * 252.0_f64.sqrt())),
        sharpe_ratio: stdev
            .filter(|s| *s > 0.0)
            .and_then(|s| finite(mean / s * 252.0_f64.sqrt())),
        total_commission: trades.iter().map(|t| t.commission).sum(),
        total_stamp_tax: trades.iter().map(|t| t.stamp_tax).sum(),
        total_fees: trades.iter().map(|t| t.commission + t.stamp_tax).sum(),
        trade_count: trades.len(),
    }
}
