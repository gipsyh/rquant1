use super::*;
use crate::data::{Adjustment, DataProvider, InstrSymbol, MemCacheProvider};
use crate::strategy::Strategy;
use anyhow::{Result, anyhow};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct BacktestEngine {
    pub config: BacktestConfig,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    /// 接管数据源和策略；本次回测的日线缓存随调用结束释放。
    /// 股票首次被访问时下载整个回测区间，策略仅能读取当前交易日之前的数据。
    pub async fn run(
        &self,
        provider: Box<dyn DataProvider>,
        mut strategy: Box<dyn Strategy>,
    ) -> Result<BacktestResult> {
        self.config.validate()?;
        let (start, end) = (self.config.start, self.config.end);
        let mut provider = MemCacheProvider::new(provider, start, end);
        let calendar = provider.trading_days(start, end).await;
        let days: BTreeSet<_> = calendar.iter().copied().collect();
        if days.is_empty()
            || days.len() != calendar.len()
            || days.iter().any(|d| *d < start || *d > end)
        {
            return Err(anyhow!("回测数据无效: 交易日历为空、重复或超出区间"));
        }
        let mut symbols = BTreeSet::new();
        let mut cash = self.config.initial_cash;
        let mut positions: BTreeMap<InstrSymbol, Position> = BTreeMap::new();
        // 每只股票独立维护复权收益单位，不能混用不同股票的因子。
        let mut return_units: BTreeMap<InstrSymbol, f64> = BTreeMap::new();
        let mut previous_equity = cash;
        let mut peak = cash;
        let mut trades = Vec::new();
        let mut equity_curve = Vec::new();
        let mut skipped_orders = Vec::new();
        for date in days {
            let orders = {
                let ctx = BtContext {
                    date,
                    init_cash: self.config.initial_cash,
                    cash,
                    equity: previous_equity,
                    positions: &positions,
                    provider: tokio::sync::Mutex::new(&mut provider),
                    adjust_returns: self.config.adjust_returns,
                };
                strategy.on_trade_day(&ctx).await
            };
            // 先校验整批订单金额；股票无需事先注册。

            for &Order::Buy { cash_amount, .. } in &orders {
                if !cash_amount.is_finite() || cash_amount <= 0.0 {
                    return Err(anyhow!("回测参数无效: 订单金额必须是有限正数"));
                }
            }
            for Order::Buy {
                symbol,
                cash_amount,
            } in orders
            {
                symbols.insert(symbol);
                let mut bar = None;
                let outcome = if cash_amount > cash {
                    Err(anyhow!("订单预算超过账户剩余现金"))
                } else {
                    bar = load_bars(
                        &mut provider,
                        symbol,
                        date,
                        date,
                        self.config.adjust_returns,
                    )
                    .await
                    .into_iter()
                    .next();
                    self.buy(bar.as_ref(), cash_amount)
                };
                match outcome {
                    Ok((shares, price, commission)) => {
                        let factor = factor(
                            bar.as_ref().expect("buy requires bar"),
                            self.config.adjust_returns,
                        );
                        cash -= shares as f64 * price + commission;
                        let position = positions.entry(symbol).or_default();
                        position.purchased_shares =
                            position
                                .purchased_shares
                                .checked_add(shares)
                                .ok_or_else(|| anyhow!("回测参数无效: 持股数量溢出"))?;
                        *return_units.entry(symbol).or_default() += shares as f64 / factor;
                        trades.push(Trade {
                            date,
                            symbol: symbol.to_string(),
                            side: "buy",
                            shares,
                            price,
                            commission,
                            cash_after: cash,
                        });
                    }
                    Err(reason) => skipped_orders.push(SkippedOrder {
                        date,
                        symbol: symbol.to_string(),
                        reason: reason.to_string(),
                    }),
                }
            }
            for (&symbol, position) in &mut positions {
                if let Some(bar) = load_bars(
                    &mut provider,
                    symbol,
                    date,
                    date,
                    self.config.adjust_returns,
                )
                .await
                .into_iter()
                .next()
                {
                    position.market_value = return_units[&symbol]
                        * bar.close
                        * factor(&bar, self.config.adjust_returns);
                }
            }
            let market_value: f64 = positions.values().map(|p| p.market_value).sum();
            let equity = cash + market_value;
            if !equity.is_finite() || equity <= 0.0 || cash < 0.0 {
                return Err(anyhow!("回测数据无效: 资产估值溢出或余额无效"));
            }
            peak = peak.max(equity);
            equity_curve.push(EquityPoint {
                date,
                cash,
                positions: positions
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
        }
        let performance = performance(&equity_curve, &trades, self.config.initial_cash);
        Ok(BacktestResult {
            strategy: strategy.name().into(),
            symbols: symbols.iter().map(ToString::to_string).collect(),
            start,
            end,
            config: self.config.clone(),
            performance,
            trades,
            equity_curve,
            skipped_orders,
        })
    }

    fn buy(&self, bar: Option<&StockDailyBar>, budget: f64) -> Result<(u64, f64, f64)> {
        let bar = bar.ok_or_else(|| anyhow!("无当日日线（可能停牌或尚未上市）"))?;
        if bar.volume <= 0.0 {
            return Err(anyhow!("成交量为零"));
        }
        let limit = bar
            .limit_up
            .ok_or_else(|| anyhow!("缺少有效涨停价，跳过买入"))?;
        let price = bar.open * (1.0 + self.config.slippage_bps / 10_000.0);
        if price >= limit - 1e-8 {
            return Err(anyhow!("开盘或滑点后价格触及涨停"));
        }
        if price > bar.high + 1e-8 {
            return Err(anyhow!("滑点后价格超出当日最高价"));
        }
        let available = (budget - self.config.minimum_commission)
            .max(0.0)
            .min(budget / (1.0 + self.config.commission_rate));
        let lots = (available / price / f64::from(self.config.lot_size)).floor();
        // f64 只能精确表示 2^53 以内的整数股数。
        if !lots.is_finite() || lots * f64::from(self.config.lot_size) > (1_u64 << 53) as f64 {
            return Err(anyhow!("买入数量超出支持范围"));
        }
        let mut shares = lots as u64 * u64::from(self.config.lot_size);
        while shares > 0 {
            let notional = shares as f64 * price;
            let commission =
                (notional * self.config.commission_rate).max(self.config.minimum_commission);
            if notional + commission <= budget {
                return Ok((shares, price, commission));
            }
            shares -= u64::from(self.config.lot_size);
        }
        Err(anyhow!("资金不足以支付一个交易单位及佣金"))
    }
}

/// 缓存属于数据源；引擎仅校验本次实际读取的数据及估值口径。
pub(super) async fn load_bars(
    provider: &mut dyn DataProvider,
    symbol: InstrSymbol,
    start: time::Date,
    end: time::Date,
    adjusted: bool,
) -> Vec<StockDailyBar> {
    let bars = provider.daily_bars(symbol, start, end).await;
    for bar in &bars {
        validate_bar(bar, adjusted).unwrap();
    }
    bars
}

fn factor(bar: &StockDailyBar, adjusted: bool) -> f64 {
    if adjusted && let Some(Adjustment::Raw(factor)) = bar.adjustment {
        factor
    } else {
        1.0
    }
}

pub(super) fn validate_bar(bar: &StockDailyBar, adjusted: bool) -> Result<()> {
    let prices = [bar.open, bar.high, bar.low, bar.close];
    if prices.iter().any(|v| !v.is_finite() || *v <= 0.0)
        || bar.low > bar.open.min(bar.close)
        || bar.high < bar.open.max(bar.close)
        || !bar.volume.is_finite()
        || bar.volume < 0.0
        || !bar.turnover.is_finite()
        || bar.turnover < 0.0
        || [bar.limit_up, bar.limit_down]
            .into_iter()
            .flatten()
            .any(|v| !v.is_finite() || v <= 0.0)
        || matches!((bar.limit_down, bar.limit_up), (Some(low), Some(high)) if low > high)
        || matches!(bar.adjustment, Some(Adjustment::Pre | Adjustment::Post))
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
        trade_count: trades.len(),
    }
}
