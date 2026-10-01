use rquant::utils::parse_date;
use rquant::{
    data::{Adjustment, DataProvider, InMemoryProvider, InstrSymbol, MarketData, StockDailyBar},
    engine::{BacktestConfig, BacktestEngine, BtContext, Order},
    strategy::{BuyAndHold, BuyAndHoldConfig, Strategy},
};
use std::sync::{Arc, Mutex};
use time::Date;

fn date(day: u8) -> Date {
    parse_date(&format!("202401{day:02}")).unwrap()
}
fn a() -> InstrSymbol {
    InstrSymbol::from("000001")
}
fn b() -> InstrSymbol {
    InstrSymbol::from("600000")
}
fn bar(symbol: InstrSymbol, day: u8, price: f64, factor: f64) -> StockDailyBar {
    StockDailyBar {
        symbol,
        date: date(day),
        open: price,
        high: price + 1.0,
        low: price - 1.0,
        close: price,
        volume: 10000.0,
        turnover: 100000.0,
        limit_up: Some(100.0),
        limit_down: Some(1.0),
        float_market_cap: None,
        adjustment: Some(Adjustment::Raw(factor)),
        st: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Query(InstrSymbol, Date, Date);
struct CountingProvider {
    inner: InMemoryProvider,
    calls: Arc<Mutex<Vec<Query>>>,
}
impl CountingProvider {
    fn new(markets: &[MarketData]) -> Self {
        Self {
            inner: InMemoryProvider::new(markets).unwrap(),
            calls: Arc::new(Mutex::new(vec![])),
        }
    }
}
#[async_trait::async_trait]
impl DataProvider for CountingProvider {
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date> {
        self.inner.trading_days(start, end).await
    }
    async fn daily_bars(
        &mut self,
        symbol: InstrSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        self.calls.lock().unwrap().push(Query(symbol, start, end));

        // 查询必须让运行时实际推进，避免仅用立即就绪的内存数据掩盖死锁。
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        self.inner.daily_bars(symbol, start, end).await
    }
}
fn market(symbol: InstrSymbol, bars: Vec<StockDailyBar>) -> MarketData {
    MarketData {
        symbol,
        trading_days: vec![date(2), date(3), date(4)],
        bars,
    }
}
fn engine(cash: f64, start: Date, end: Date) -> BacktestEngine {
    BacktestEngine::new(BacktestConfig {
        start,
        end,
        initial_cash: cash,
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn dynamically_selects_new_symbols_and_caches_history_execution_and_empty_results() {
    let provider = CountingProvider::new(&[
        market(
            a(),
            vec![
                bar(a(), 2, 11.0, 2.0),
                bar(a(), 3, 10.0, 2.0),
                bar(a(), 4, 12.0, 2.0),
            ],
        ),
        market(b(), vec![bar(b(), 3, 20.0, 5.0), bar(b(), 4, 10.0, 10.0)]),
    ]);
    let calls = provider.calls.clone();
    struct Dynamic(Arc<Mutex<Vec<Query>>>);
    #[async_trait::async_trait]
    impl Strategy for Dynamic {
        fn name(&self) -> &str {
            "dynamic"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            match ctx.date() {
                d if d == date(2) => {
                    assert!(self.0.lock().unwrap().is_empty(), "must not preload stocks");
                    vec![]
                }
                d if d == date(3) => {
                    assert!(self.0.lock().unwrap().is_empty());
                    let history = ctx.history(a(), date(1), date(2)).await;
                    assert_eq!(history.len(), 1);
                    assert_eq!(ctx.history(a(), date(2), date(2)).await, history);
                    // 策略根据此时查到的历史决定关注另一只股票，无需预注册。
                    let selected = if history[0].close > 10.0 { b() } else { a() };
                    assert!(
                        ctx.bar(selected, date(2))
                            .await
                            .expect("历史行情查询失败")
                            .is_none()
                    );
                    assert!(
                        ctx.bar(selected, date(2))
                            .await
                            .expect("历史行情查询失败")
                            .is_none()
                    );
                    let count = self.0.lock().unwrap().len();
                    assert_eq!(
                        self.0.lock().unwrap().len(),
                        count,
                        "future queries must fail before I/O"
                    );
                    vec![
                        Order::Buy {
                            symbol: selected,
                            cash_amount: 5005.0,
                        },
                        Order::Buy {
                            symbol: a(),
                            cash_amount: 5005.0,
                        },
                    ]
                }
                _ => {
                    assert_eq!(ctx.position(a()).unwrap().purchased_shares, 500);
                    assert_eq!(ctx.position(b()).unwrap().purchased_shares, 200);
                    assert_eq!(ctx.equity, 10000.0);
                    let count = self.0.lock().unwrap().len();
                    assert_eq!(
                        ctx.bar(b(), date(3))
                            .await
                            .expect("历史行情查询失败")
                            .unwrap()
                            .close,
                        20.0
                    );
                    assert_eq!(
                        self.0.lock().unwrap().len(),
                        count,
                        "execution cache must serve later history"
                    );
                    vec![]
                }
            }
        }
    }
    let result = engine(10010.0, date(2), date(4))
        .run(Box::new(provider), Box::new(Dynamic(calls.clone())))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 2);
    assert_eq!(result.performance.final_equity, 11000.0);
    let last = result.equity_curve.last().unwrap();
    assert_eq!(last.cash, 1000.0);
    assert_eq!(last.positions[&a().to_string()].market_value, 6000.0);
    assert_eq!(last.positions[&b().to_string()].market_value, 4000.0);
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Query(a(), date(1), date(4)), Query(b(), date(2), date(4)),]
    );
}

#[tokio::test]
async fn idle_strategy_never_requests_stock_data() {
    struct Idle;
    #[async_trait::async_trait]
    impl Strategy for Idle {
        fn name(&self) -> &str {
            "idle"
        }
        async fn on_trade_day(&mut self, _: &BtContext<'_>) -> Vec<Order> {
            vec![]
        }
    }
    let provider = CountingProvider::new(&[market(a(), vec![])]);
    let calls = provider.calls.clone();
    let result = engine(10000.0, date(2), date(4))
        .run(Box::new(provider), Box::new(Idle))
        .await
        .unwrap();
    assert!(calls.lock().unwrap().clone().is_empty());
    assert!(result.symbols.is_empty());
    assert_eq!(result.performance.final_equity, 10000.0);
}

#[tokio::test]
async fn insufficient_shared_cash_rejects_later_order_without_loading_its_stock() {
    struct Spend;
    #[async_trait::async_trait]
    impl Strategy for Spend {
        fn name(&self) -> &str {
            "spend"
        }
        async fn on_trade_day(&mut self, _: &BtContext<'_>) -> Vec<Order> {
            vec![
                Order::Buy {
                    symbol: a(),
                    cash_amount: 10005.0,
                },
                Order::Buy {
                    symbol: b(),
                    cash_amount: 10005.0,
                },
            ]
        }
    }
    let provider = CountingProvider::new(&[market(a(), vec![bar(a(), 2, 10.0, 1.0)])]);
    let calls = provider.calls.clone();
    let result = engine(10005.0, date(2), date(2))
        .run(Box::new(provider), Box::new(Spend))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.skipped_orders[0].symbol, b().to_string());
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Query(a(), date(2), date(2))]
    );
    assert_eq!(result.equity_curve[0].cash, 0.0);
}

#[tokio::test]
async fn buy_and_hold_keeps_separate_budgets_and_marks_suspended_stock_independently() {
    let provider = CountingProvider::new(&[
        market(a(), vec![bar(a(), 2, 10.0, 2.0), bar(a(), 3, 12.0, 2.0)]),
        market(b(), vec![bar(b(), 3, 20.0, 5.0), bar(b(), 4, 25.0, 5.0)]),
    ]);
    let strategy = BuyAndHold::new(BuyAndHoldConfig {
        symbols: vec![a(), b()],
        allocation: 1.0,
    });
    let result = engine(10010.0, date(2), date(4))
        .run(Box::new(provider), Box::new(strategy.clone()))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 2);
    assert_eq!(result.trades[0].date, date(2));
    assert_eq!(result.trades[1].date, date(3));
    assert_eq!(
        result.equity_curve[2].positions[&a().to_string()].market_value,
        6000.0
    );
    assert_eq!(
        result.equity_curve[2].positions[&b().to_string()].market_value,
        5000.0
    );
    assert_eq!(result.performance.final_equity, 12000.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_history_requests_fetch_only_missing_dates_including_warmup() {
    struct History;
    #[async_trait::async_trait]
    impl Strategy for History {
        fn name(&self) -> &str {
            "history"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            ctx.history(a(), date(2), date(3)).await;
            ctx.history(a(), date(1), date(3)).await;
            ctx.history(a(), date(1), date(3)).await;
            vec![]
        }
    }
    let provider = CountingProvider::new(&[market(a(), vec![bar(a(), 2, 10.0, 1.0)])]);
    let calls = provider.calls.clone();
    engine(10000.0, date(4), date(4))
        .run(Box::new(provider), Box::new(History))
        .await
        .unwrap();
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Query(a(), date(2), date(4)), Query(a(), date(1), date(1))]
    );
}

#[tokio::test]
#[should_panic(expected = "upstream failure")]
async fn lazy_query_failure_propagates_and_does_not_become_a_suspension() {
    struct Failing;
    #[async_trait::async_trait]
    impl DataProvider for Failing {
        async fn trading_days(&mut self, _: Date, _: Date) -> Vec<Date> {
            vec![date(2)]
        }
        async fn daily_bars(&mut self, _: InstrSymbol, _: Date, _: Date) -> Vec<StockDailyBar> {
            panic!("upstream failure")
        }
    }
    let _ = engine(10000.0, date(2), date(2))
        .run(
            Box::new(Failing),
            Box::new(BuyAndHold::new(BuyAndHoldConfig {
                symbols: vec![a()],
                allocation: 1.0,
            })),
        )
        .await;
}

#[tokio::test]
#[should_panic(expected = "history failure")]
async fn async_history_failure_terminates_the_strategy() {
    struct Failing;
    #[async_trait::async_trait]
    impl DataProvider for Failing {
        async fn trading_days(&mut self, _: Date, _: Date) -> Vec<Date> {
            vec![date(3)]
        }
        async fn daily_bars(&mut self, _: InstrSymbol, _: Date, _: Date) -> Vec<StockDailyBar> {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            panic!("history failure")
        }
    }
    struct History;
    #[async_trait::async_trait]
    impl Strategy for History {
        fn name(&self) -> &str {
            "history"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            ctx.bar(a(), date(2)).await.expect("历史行情查询失败");
            vec![]
        }
    }
    let _ = engine(10000.0, date(3), date(3))
        .run(Box::new(Failing), Box::new(History))
        .await;
}

#[tokio::test]
#[should_panic(expected = "callback panic")]
async fn strategy_panic_terminates_the_backtest() {
    struct Panicking;
    #[async_trait::async_trait]
    impl Strategy for Panicking {
        fn name(&self) -> &str {
            "panicking"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            ctx.bar(a(), date(2)).await.expect("历史行情查询失败");
            panic!("callback panic");
        }
    }
    let provider = CountingProvider::new(&[market(a(), vec![bar(a(), 2, 10.0, 1.0)])]);
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine(10000.0, date(3), date(3)).run(Box::new(provider), Box::new(Panicking)),
    )
    .await
    .expect("callback panic must not deadlock the engine");
}

#[tokio::test]
async fn st_status_survives_memory_provider_and_history_cache() {
    struct ObserveSt;
    #[async_trait::async_trait]
    impl Strategy for ObserveSt {
        fn name(&self) -> &str {
            "observe_st"
        }
        async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order> {
            let history = ctx.history(a(), date(2), date(3)).await;
            assert_eq!(
                history.iter().map(|bar| bar.st).collect::<Vec<_>>(),
                vec![true, false]
            );
            assert!(ctx.bar(a(), date(2)).await.unwrap().unwrap().st);
            assert!(!ctx.bar(a(), date(3)).await.unwrap().unwrap().st);
            vec![]
        }
    }
    let mut st_bar = bar(a(), 2, 10.0, 1.0);
    st_bar.st = true;
    let provider = CountingProvider::new(&[market(a(), vec![st_bar, bar(a(), 3, 10.0, 1.0)])]);
    let calls = provider.calls.clone();
    engine(10000.0, date(4), date(4))
        .run(Box::new(provider), Box::new(ObserveSt))
        .await
        .unwrap();
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Query(a(), date(2), date(4))]
    );
}
