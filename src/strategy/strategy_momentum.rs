use super::Strategy;
use crate::{
    data::StockSymbol,
    engine::{BacktestResult, BtContext, EquityPoint, Order},
};
use clap::Args;
use std::{collections::BTreeMap, path::PathBuf};
use time::Date;

/// Research allocator: causal shadow-account snapshots become next-open orders.
#[derive(Args, Clone, Debug)]
pub struct StrategyMomentumConfig {
    /// Independently simulated adaptive-rotation account, covering every requested date
    #[arg(long)]
    pub core_report: PathBuf,
    /// Independently simulated volume-breakout account, on the identical calendar
    #[arg(long)]
    pub satellite_report: PathBuf,
    /// Trailing exchange days used to compare strategy returns
    #[arg(long, default_value_t = 40)]
    pub window: usize,
    /// Breakout must exceed this return AND the core return to receive capital
    #[arg(long, default_value_t = 0.05)]
    pub threshold: f64,
    #[arg(long, default_value_t = 1.0)]
    pub satellite_weight: f64,
    /// Scales the chosen shadow account's stock weights, leaving the rest in cash
    #[arg(long, default_value_t = 0.8)]
    pub allocation: f64,
    /// Ignore target deviations below this fraction of equity or 2000 yuan
    #[arg(long, default_value_t = 0.02)]
    pub rebalance_band: f64,
}
pub struct StrategyMomentum {
    core: BTreeMap<Date, EquityPoint>,
    satellite: BTreeMap<Date, EquityPoint>,
    weights: BTreeMap<Date, f64>,
    allocation: f64,
    band: f64,
}
impl StrategyMomentum {
    pub fn new(config: StrategyMomentumConfig) -> Self {
        let a = serde_json::from_slice(
            &std::fs::read(&config.core_report).expect("Cannot read core report"),
        )
        .expect("Invalid core report");
        let b = serde_json::from_slice(
            &std::fs::read(&config.satellite_report).expect("Cannot read satellite report"),
        )
        .expect("Invalid satellite report");
        Self::from_reports(config, a, b)
            .expect("Invalid strategy momentum configuration or reports")
    }
    pub fn from_reports(
        args: StrategyMomentumConfig,
        a: BacktestResult,
        b: BacktestResult,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(args.window > 0 && (0.0..=1.0).contains(&args.satellite_weight));
        anyhow::ensure!(args.allocation > 0.0 && args.allocation <= 1.0);
        anyhow::ensure!(args.threshold.is_finite() && args.threshold >= 0.0);
        anyhow::ensure!(args.rebalance_band.is_finite() && args.rebalance_band >= 0.0);
        anyhow::ensure!(
            !a.equity_curve.is_empty(),
            "Shadow reports must not be empty"
        );
        anyhow::ensure!(a.equity_curve.len() == b.equity_curve.len());
        let mut weights = BTreeMap::new();
        for (i, (ap, bp)) in a.equity_curve.iter().zip(&b.equity_curve).enumerate() {
            anyhow::ensure!(ap.date == bp.date, "Shadow calendars differ");
            for point in [ap, bp] {
                anyhow::ensure!(
                    point.equity.is_finite()
                        && point.equity > 0.0
                        && point.cash.is_finite()
                        && point.cash >= 0.0,
                    "Invalid shadow equity or cash"
                );
                for (symbol, position) in &point.positions {
                    symbol.parse::<StockSymbol>()?;
                    anyhow::ensure!(
                        position.market_value.is_finite() && position.market_value >= 0.0,
                        "Invalid shadow holding value"
                    );
                }
                let assets = point.cash
                    + point
                        .positions
                        .values()
                        .map(|p| p.market_value)
                        .sum::<f64>();
                anyhow::ensure!(
                    (assets - point.equity).abs() <= point.equity * 1e-8,
                    "Shadow assets do not reconcile"
                );
            }
            if i > 0 {
                anyhow::ensure!(a.equity_curve[i - 1].date < ap.date);
            }
            // Only trailing observations ending at this close, never subsequent rows.
            let w = if i >= args.window {
                let ar = ap.equity / a.equity_curve[i - args.window].equity - 1.0;
                let br = bp.equity / b.equity_curve[i - args.window].equity - 1.0;
                if br > ar.max(args.threshold) {
                    args.satellite_weight
                } else {
                    0.0
                }
            } else {
                0.0
            };
            weights.insert(ap.date, w);
        }
        Ok(Self {
            core: a.equity_curve.into_iter().map(|p| (p.date, p)).collect(),
            satellite: b.equity_curve.into_iter().map(|p| (p.date, p)).collect(),
            weights,
            allocation: args.allocation,
            band: args.rebalance_band,
        })
    }
    fn target_weights(&self, date: Date) -> BTreeMap<StockSymbol, f64> {
        let w = self.weights[&date];
        let mut targets = BTreeMap::<StockSymbol, f64>::new();
        for (point, scale) in [(&self.core[&date], 1.0 - w), (&self.satellite[&date], w)] {
            for (s, p) in &point.positions {
                *targets.entry(s.parse().unwrap()).or_default() +=
                    self.allocation * scale * p.market_value / point.equity;
            }
        }
        targets.retain(|_, w| *w > 1e-10);
        targets
    }
}
#[async_trait::async_trait]
impl Strategy for StrategyMomentum {
    fn name(&self) -> &str {
        "strategy_momentum"
    }
    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        let date = ctx.date();
        let targets = self.target_weights(date);
        let mut sells = Vec::new();
        let mut released = 0.0;
        for (&s, p) in ctx.positions {
            let target = targets.get(&s).copied().unwrap_or(0.0) * ctx.equity;
            if target == 0.0 {
                sells.push(Order::SellAll { symbol: s });
                released += p.market_value;
            } else if p.market_value - target > (ctx.equity * self.band).max(2000.0) {
                let shares = ((p.purchased_shares as f64 * (1.0 - target / p.market_value) / 100.0)
                    .floor() as u64)
                    * 100;
                if shares > 0 {
                    sells.push(Order::Sell {
                        symbol: s,
                        shares,
                        price: None,
                    });
                    released += p.market_value * shares as f64 / p.purchased_shares as f64;
                }
            }
        }
        let mut deficits = BTreeMap::new();
        for (s, w) in targets {
            let held = ctx.positions.get(&s).map_or(0.0, |p| p.market_value);
            let deficit = w * ctx.equity - held;
            if deficit > (ctx.equity * self.band).max(2000.0) {
                deficits.insert(s, deficit);
            }
        }
        let mut batches = Vec::new();
        if !sells.is_empty() {
            batches.push(sells);
        }
        let cash = ctx.cash + released;
        let needed: f64 = deficits.values().sum();
        if cash > 0.0 && needed > 0.0 {
            let denominator = needed.max(cash / 0.99);
            batches.push(vec![Order::BuyWeights {
                weights: deficits
                    .into_iter()
                    .map(|(s, v)| (s, v / denominator))
                    .collect(),
            }]);
        }
        batches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{BacktestConfig, Performance, Position};
    use time::{Duration, macros::date};

    fn config() -> StrategyMomentumConfig {
        StrategyMomentumConfig {
            core_report: PathBuf::new(),
            satellite_report: PathBuf::new(),
            window: 2,
            threshold: 0.05,
            satellite_weight: 1.0,
            allocation: 0.8,
            rebalance_band: 0.02,
        }
    }
    fn report(values: &[f64], symbol: &str) -> BacktestResult {
        BacktestResult {
            strategy: "fixture".into(),
            symbols: vec![symbol.into()],
            start: date!(2024 - 01 - 01),
            end: date!(2024 - 01 - 01) + Duration::days(values.len() as i64 - 1),
            config: BacktestConfig::default(),
            trades: vec![],
            skipped_orders: vec![],
            performance: Performance {
                final_equity: 0.0,
                total_return: 0.0,
                annualized_return: None,
                max_drawdown: 0.0,
                annualized_volatility: None,
                sharpe_ratio: None,
                total_commission: 0.0,
                total_stamp_tax: 0.0,
                total_fees: 0.0,
                trade_count: 0,
            },
            equity_curve: values
                .iter()
                .enumerate()
                .map(|(i, &v)| EquityPoint {
                    date: date!(2024 - 01 - 01) + Duration::days(i as i64),
                    cash: v * 0.1,
                    positions: BTreeMap::from([(
                        symbol.into(),
                        Position {
                            purchased_shares: 100,
                            market_value: v * 0.9,
                        },
                    )]),
                    market_value: v * 0.9,
                    equity: v,
                    net_value: v / values[0],
                    daily_return: 0.0,
                    drawdown: 0.0,
                })
                .collect(),
        }
    }
    #[test]
    fn trailing_performance_selects_strategy_only_after_warmup() {
        let a = report(&[100.0, 101.0, 102.0, 103.0], "000001.XSHE");
        let b = report(&[100.0, 104.0, 110.0, 100.0], "300001.XSHE");
        let s = StrategyMomentum::from_reports(config(), a, b).unwrap();
        for (d, expected) in [
            (1, "000001.XSHE"),
            (2, "000001.XSHE"),
            (3, "300001.XSHE"),
            (4, "000001.XSHE"),
        ] {
            let w = s.target_weights(date!(2024 - 01 - 01) + Duration::days(d - 1));
            assert_eq!(w.len(), 1);
            assert!((w[&expected.parse().unwrap()] - 0.72).abs() < 1e-12);
        }
    }
    #[test]
    fn future_rows_and_shortened_reports_cannot_change_prior_targets() {
        let a = report(&[100.0, 101.0, 102.0, 103.0], "000001.XSHE");
        let b = report(&[100.0, 104.0, 110.0, 100.0], "300001.XSHE");
        let full = StrategyMomentum::from_reports(config(), a.clone(), b.clone()).unwrap();
        let altered = StrategyMomentum::from_reports(
            config(),
            report(&[100.0, 101.0, 102.0, 900.0], "000001.XSHE"),
            report(&[100.0, 104.0, 110.0, 10.0], "300001.XSHE"),
        )
        .unwrap();
        let mut a = a;
        let mut b = b;
        a.equity_curve.truncate(3);
        b.equity_curve.truncate(3);
        let prefix = StrategyMomentum::from_reports(config(), a, b).unwrap();
        for i in 0..3 {
            let d = date!(2024 - 01 - 01) + Duration::days(i);
            assert_eq!(full.target_weights(d), altered.target_weights(d));
            assert_eq!(full.target_weights(d), prefix.target_weights(d));
        }
    }
    #[test]
    fn mismatched_calendar_or_nonfinite_equity_is_rejected() {
        let a = report(&[100.0, 101.0, 102.0], "000001.XSHE");
        let mut b = report(&[100.0, 104.0, 110.0], "300001.XSHE");
        b.equity_curve[1].date += Duration::days(1);
        assert!(StrategyMomentum::from_reports(config(), a.clone(), b).is_err());
        let b = report(&[100.0, f64::INFINITY, 110.0], "300001.XSHE");
        assert!(StrategyMomentum::from_reports(config(), a, b).is_err());
    }
}
