use rquant::utils::parse_date;
use rquant::{
    data::{Adjustment, InstrSymbol, MarketData, StockDailyBar},
    engine::{BacktestConfig, BacktestEngine, BtContext, Order},
    strategy::{BuyAndHold, BuyAndHoldConfig, Strategy},
};
use std::sync::{Arc, Mutex};
use time::Date;

fn date(day: u8) -> Date {
    parse_date(&format!("202401{day:02}")).unwrap()
}
fn bar(day: u8, open: f64, close: f64) -> StockDailyBar {
    StockDailyBar {
        symbol: InstrSymbol::from("000001"),
        date: date(day),
        open,
        high: open.max(close) + 1.0,
        low: open.min(close) - 1.0,
        close,
        volume: 100_000.0,
        turnover: 1_000_000.0,
        limit_up: Some(100.0),
        limit_down: Some(1.0),
        float_market_cap: None,
        adjustment: Some(Adjustment::Raw(2.0)),
        st: false,
    }
}
fn market(bars: Vec<StockDailyBar>) -> MarketData {
    MarketData {
        symbol: InstrSymbol::from("000001"),
        trading_days: vec![date(2), date(3), date(4)],
        bars,
    }
}
fn engine() -> BacktestEngine {
    BacktestEngine::new(BacktestConfig {
        start: date(2),
        end: date(4),
        initial_cash: 10_005.0,
        ..Default::default()
    })
    .unwrap()
}
fn near(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-8, "{a} != {b}");
}

#[tokio::test]
async fn buys_once_at_open_and_marks_every_close_with_fees_and_drawdown() {
    let data = market(vec![
        bar(4, 9.0, 8.0),
        bar(2, 10.0, 10.0),
        bar(3, 10.0, 11.0),
    ]);
    let result = engine()
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(trade.date, date(2));
    assert_eq!(trade.shares, 1000);
    near(trade.price, 10.0);
    near(trade.commission, 5.0);
    near(trade.cash_after, 0.0);
    near(result.performance.final_equity, 8000.0);
    near(result.performance.total_return, 8000.0 / 10005.0 - 1.0);
    near(result.performance.max_drawdown, 3.0 / 11.0);
    near(result.equity_curve[0].daily_return, -5.0 / 10005.0);
    near(result.equity_curve[0].drawdown, 5.0 / 10005.0);
    near(
        result.performance.annualized_return.unwrap(),
        (8000.0_f64 / 10005.0).powf(84.0) - 1.0,
    );
    assert_eq!(result.equity_curve.len(), 3);
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["trades"][0]["date"], "2024-01-02");
}

#[tokio::test]
async fn factor_change_preserves_split_adjusted_value_without_adjusting_fill_price() {
    let mut after_split = bar(3, 5.0, 5.0);
    after_split.adjustment = Some(Adjustment::Raw(4.0));
    let data = market(vec![bar(2, 10.0, 10.0), after_split]);
    let result = engine()
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    near(result.performance.final_equity, 10_000.0);
    near(result.trades[0].price, 10.0);
    near(result.equity_curve[1].daily_return, 0.0);
    // 最后一天无日线时延续前一个估值。
    near(result.equity_curve[2].equity, 10_000.0);
    let mut raw = engine();
    raw.config.adjust_returns = false;
    let result = raw
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    near(result.performance.final_equity, 5_000.0);
}

#[tokio::test]
async fn missing_bar_and_limit_up_retry_until_first_fill() {
    let mut locked = bar(3, 10.0, 10.0);
    locked.limit_up = Some(10.0);
    let data = market(vec![locked, bar(4, 10.0, 11.0)]);
    let result = engine()
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    assert_eq!(result.skipped_orders.len(), 2);
    assert_eq!(result.trades[0].date, date(4));
    near(result.equity_curve[0].equity, 10005.0);
    near(result.equity_curve[1].equity, 10005.0);
}

#[tokio::test]
async fn insufficient_cash_leaves_cash_intact_and_no_commission() {
    let mut engine = engine();
    engine.config.initial_cash = 1000.0;
    let data = market(vec![bar(2, 10.0, 10.0)]);
    let result = engine
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    assert!(result.trades.is_empty());
    near(result.performance.final_equity, 1000.0);
    near(result.performance.total_commission, 0.0);
    assert_eq!(result.performance.sharpe_ratio, None);
    assert_eq!(result.performance.annualized_volatility, Some(0.0));
}

#[tokio::test]
async fn allocation_slippage_and_proportional_commission_respect_cash_budget() {
    let mut engine = engine();
    engine.config.initial_cash = 100_000.0;
    engine.config.slippage_bps = 100.0;
    let data = market(vec![bar(2, 10.0, 10.0)]);
    let result = engine
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 0.5,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    let trade = &result.trades[0];
    assert_eq!(trade.shares, 4900);
    near(trade.price, 10.1);
    near(trade.commission, 14.847);
    near(trade.cash_after, 50_495.153);
    near(result.performance.final_equity, 99_495.153);
}

#[tokio::test]
async fn rejects_bad_config_data_and_already_adjusted_prices() {
    for cash in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            BacktestEngine::new(BacktestConfig {
                initial_cash: cash,
                ..Default::default()
            })
            .is_err()
        );
    }
    for allocation in [0.0, -1.0, 1.1, f64::NAN] {
        assert!(
            std::panic::catch_unwind(|| BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation
            }))
            .is_err()
        );
    }
    let invalid = [
        StockDailyBar {
            close: f64::NAN,
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            open: 0.0,
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            adjustment: None,
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            adjustment: Some(Adjustment::Pre),
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            adjustment: Some(Adjustment::Raw(0.0)),
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            volume: -1.0,
            ..bar(2, 10.0, 10.0)
        },
    ];
    for bar in invalid {
        assert!(
            engine()
                .run_with_data(
                    Box::new(BuyAndHold::new(BuyAndHoldConfig {
                        symbols: vec![InstrSymbol::from("000001")],
                        allocation: 1.0
                    })),
                    std::slice::from_ref(&market(vec![bar]))
                )
                .await
                .is_err()
        );
    }
    let duplicate = market(vec![bar(2, 10.0, 10.0); 2]);
    assert!(
        engine()
            .run_with_data(
                Box::new(BuyAndHold::new(BuyAndHoldConfig {
                    symbols: vec![InstrSymbol::from("000001")],
                    allocation: 1.0
                })),
                std::slice::from_ref(&duplicate)
            )
            .await
            .is_err()
    );
    let empty = engine()
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            &[market(vec![])],
        )
        .await
        .unwrap();
    assert!(empty.trades.is_empty());
    near(empty.performance.final_equity, 10005.0);
}

#[tokio::test]
async fn cannot_fill_zero_volume_unknown_limit_or_slippage_above_high() {
    for bar in [
        StockDailyBar {
            volume: 0.0,
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            limit_up: None,
            ..bar(2, 10.0, 10.0)
        },
        StockDailyBar {
            high: 10.0,
            ..bar(2, 10.0, 10.0)
        },
    ] {
        let mut engine = engine();
        engine.config.slippage_bps = 10.0;
        let result = engine
            .run_with_data(
                Box::new(BuyAndHold::new(BuyAndHoldConfig {
                    symbols: vec![InstrSymbol::from("000001")],
                    allocation: 1.0,
                })),
                std::slice::from_ref(&market(vec![bar])),
            )
            .await
            .unwrap();
        assert!(result.trades.is_empty());
    }
}

#[tokio::test]
async fn strategy_observes_only_previous_bar_and_boxed_runs_are_independent() {
    struct Observe(Arc<Mutex<Vec<Option<Date>>>>);
    #[async_trait::async_trait]
    impl Strategy for Observe {
        fn name(&self) -> &str {
            "observe"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            let bars = ctx
                .history(
                    InstrSymbol::from("000001"),
                    date(1),
                    ctx.date().previous_day().unwrap(),
                )
                .await;
            self.0.lock().unwrap().push(bars.last().map(|b| b.date));
            vec![]
        }
    }
    let data = market(vec![bar(2, 10.0, 10.0), bar(4, 10.0, 10.0)]);
    let observations = Arc::new(Mutex::new(vec![]));
    let observe = Observe(observations.clone());
    engine()
        .run_with_data(Box::new(observe), std::slice::from_ref(&data))
        .await
        .unwrap();
    assert_eq!(
        *observations.lock().unwrap(),
        vec![None, Some(date(2)), Some(date(2))]
    );
    let strategy = BuyAndHold::new(BuyAndHoldConfig {
        symbols: vec![InstrSymbol::from("000001")],
        allocation: 1.0,
    });
    for _ in 0..2 {
        let result = engine()
            .run_with_data(Box::new(strategy.clone()), std::slice::from_ref(&data))
            .await
            .unwrap();
        assert_eq!(result.trades.len(), 1);
    }
}

#[tokio::test]
async fn single_day_has_no_sample_volatility_and_valid_metrics() {
    let data = MarketData {
        symbol: InstrSymbol::from("000001"),
        trading_days: vec![date(2)],
        bars: vec![bar(2, 10.0, 10.0)],
    };
    let mut engine = engine();
    engine.config.end = date(2);
    let result = engine
        .run_with_data(
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![InstrSymbol::from("000001")],
                allocation: 1.0,
            })),
            std::slice::from_ref(&data),
        )
        .await
        .unwrap();
    assert_eq!(result.performance.annualized_volatility, None);
    assert_eq!(result.performance.sharpe_ratio, None);
    assert!(serde_json::to_string(&result).is_ok());
}
