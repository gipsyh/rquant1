use super::execution::Account;
use super::*;
use crate::data::{Adjustment, IndexHistComp};
use crate::strategy::Strategy;
use std::sync::{Arc, Mutex};
use time::macros::date;

const FIRST: Date = date!(2024 - 01 - 05);
const NEXT: Date = date!(2024 - 01 - 08);
const LAST: Date = date!(2024 - 01 - 09);
fn a() -> StockSymbol {
    "000001".into()
}
fn b() -> StockSymbol {
    "600000".into()
}
fn c() -> StockSymbol {
    "300001".into()
}

fn bar(symbol: StockSymbol, date: Date) -> StockBar {
    StockBar {
        symbol,
        date,
        open: 10.0,
        high: 20.0,
        low: 1.0,
        close: 10.0,
        volume: 1000.0,
        turnover: 10000.0,
        limit_up: Some(100.0),
        limit_down: Some(0.1),
        float_market_cap: None,
        adjustment: Some(Adjustment::Raw(1.0)),
        st: false,
    }
}

struct Provider {
    bars: Vec<StockBar>,
}
impl Default for Provider {
    fn default() -> Self {
        Self {
            bars: [FIRST, NEXT, LAST]
                .into_iter()
                .flat_map(|d| [a(), b(), c()].map(|s| bar(s, d)))
                .collect(),
        }
    }
}
#[async_trait::async_trait]
impl DataProvider for Provider {
    async fn trading_days(&mut self, _start: Date, _end: Date) -> Vec<Date> {
        vec![FIRST, NEXT, LAST]
    }
    async fn stock_bar(&mut self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        self.bars
            .iter()
            .filter(|bar| bar.symbol == symbol && bar.date >= start && bar.date <= end)
            .copied()
            .collect()
    }
    async fn index_name(&mut self, _: &str) -> String {
        unreachable!()
    }
    async fn index_comp(&mut self, _: &str, _: Date, _: Date) -> IndexHistComp {
        unreachable!()
    }
}

#[derive(Default)]
struct Probe {
    orders: BTreeMap<Date, Vec<Vec<Order>>>,
    failures: Arc<Mutex<Vec<OrderFailure>>>,
    observations: Arc<Mutex<Vec<(Date, f64, f64)>>>,
    read_history: bool,
}
#[async_trait::async_trait]
impl Strategy for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        if self.read_history {
            let bars = ctx.history(a(), ctx.date(), ctx.date()).await;
            assert_eq!(bars[0].date, ctx.date());
        }
        self.observations
            .lock()
            .unwrap()
            .push((ctx.date(), ctx.cash, ctx.equity));
        self.orders.remove(&ctx.date()).unwrap_or_default()
    }
    async fn on_order_failed(&mut self, failure: &OrderFailure) {
        self.failures.lock().unwrap().push(failure.clone());
    }
}

fn engine(cash: f64) -> BacktestEngine {
    BacktestEngine::new(BacktestConfig {
        start: FIRST,
        end: LAST,
        initial_cash: cash,
        commission_rate: 0.0,
        minimum_commission: 0.0,
        stamp_tax_rate: 0.0,
        lot_size: 1,
        adjust_returns: false,
        ..Default::default()
    })
    .unwrap()
}
fn buy(symbol: StockSymbol, shares: u64, price: f64) -> Order {
    Order::BuyLimit {
        symbol,
        shares,
        price,
    }
}
fn sell(symbol: StockSymbol, shares: u64, price: f64) -> Order {
    Order::SellLimit {
        symbol,
        shares,
        price,
    }
}
fn hold(account: &mut Account, symbol: StockSymbol, shares: u64) {
    account.positions.insert(
        symbol,
        Position {
            purchased_shares: shares,
            market_value: shares as f64 * 10.0,
        },
    );
    account.units.insert(symbol, shares as f64);
    account.start_day();
}
async fn batch(account: &mut Account, orders: Vec<Order>) -> super::execution::BatchResult {
    engine(10000.0)
        .execute_batch(
            account,
            &mut Provider::default(),
            &mut Probe::default(),
            FIRST,
            NEXT,
            0,
            orders,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn close_signals_execute_next_trading_open_and_final_orders_expire() {
    let mut strategy = Probe {
        read_history: true,
        ..Default::default()
    };
    strategy
        .orders
        .insert(FIRST, vec![vec![buy(a(), 100, 12.0)]]);
    strategy
        .orders
        .insert(LAST, vec![vec![Order::SellAll { symbol: a() }]]);
    let observations = strategy.observations.clone();
    let mut provider = Provider::default();
    provider
        .bars
        .iter_mut()
        .filter(|bar| bar.date == NEXT)
        .for_each(|bar| bar.close = 12.0);
    let result = engine(2000.0)
        .run(Box::new(provider), Box::new(strategy))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    assert_eq!(
        (trade.signal_date, trade.date, trade.price),
        (FIRST, NEXT, 10.0)
    );
    assert_eq!(observations.lock().unwrap()[1], (NEXT, 1000.0, 2200.0));
    assert!(result.equity_curve[0].positions.is_empty());
    assert_eq!(
        result.equity_curve[2].positions[&a().to_string()].purchased_shares,
        100
    );
}

#[tokio::test]
async fn limit_orders_compare_only_open_and_fill_at_open_including_equality() {
    let mut account = Account::new(10000.0);
    hold(&mut account, a(), 300);
    let mut provider = Provider::default();
    // 此测试直接验证开盘撮合，不经过收盘行情校验。
    for bar in &mut provider.bars {
        bar.high = f64::NAN;
        bar.low = f64::NAN;
        bar.close = f64::NAN;
        bar.volume = 0.0;
    }
    let mut probe = Probe::default();
    let result = engine(10000.0)
        .execute_batch(
            &mut account,
            &mut provider,
            &mut probe,
            FIRST,
            NEXT,
            0,
            vec![
                buy(b(), 10, 9.0),
                buy(b(), 10, 10.0),
                buy(b(), 10, 11.0),
                sell(a(), 100, 11.0),
                sell(a(), 100, 10.0),
                sell(a(), 100, 9.0),
            ],
        )
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 4);
    assert!(result.trades.iter().all(|trade| trade.price == 10.0));
    assert_eq!(result.skipped.len(), 2);
    assert_eq!(probe.failures.lock().unwrap().len(), 2);
    assert_eq!(account.positions[&a()].purchased_shares, 100);
    assert_eq!(account.positions[&b()].purchased_shares, 20);
}

#[tokio::test]
async fn cash_conflicts_fail_together_independent_of_input_order() {
    for orders in [
        vec![buy(a(), 60, 10.0), buy(b(), 60, 10.0)],
        vec![buy(b(), 60, 10.0), buy(a(), 60, 10.0)],
    ] {
        let mut account = Account::new(1000.0);
        let result = batch(&mut account, orders).await;
        assert!(result.trades.is_empty());
        assert_eq!(result.skipped.len(), 2);
        assert_eq!(account.cash, 1000.0);
        assert!(account.positions.is_empty());
    }
}

#[tokio::test]
async fn same_batch_sales_cannot_fund_buys_but_next_batch_can() {
    let mut account = Account::new(0.0);
    hold(&mut account, a(), 100);
    let result = batch(
        &mut account,
        vec![Order::SellAll { symbol: a() }, buy(b(), 100, 10.0)],
    )
    .await;
    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.skipped.len(), 1);
    assert_eq!(account.cash, 1000.0);
    let result = batch(
        &mut account,
        vec![Order::BuyWeights {
            weights: [(b(), 1.0)].into(),
        }],
    )
    .await;
    assert_eq!(result.trades.len(), 1);
    assert_eq!(account.cash, 0.0);
    assert_eq!(account.positions[&b()].purchased_shares, 100);
}

#[tokio::test]
async fn weights_use_batch_start_cash_and_do_not_redistribute_failed_allocations() {
    let mut account = Account::new(10000.0);
    let result = batch(
        &mut account,
        vec![
            Order::BuyAmount {
                symbol: a(),
                cash_amount: 2000.0,
            },
            Order::BuyWeights {
                weights: [(b(), 0.25), (c(), 0.25)].into(),
            },
        ],
    )
    .await;
    assert_eq!(account.positions[&b()].purchased_shares, 250);
    assert_eq!(account.positions[&c()].purchased_shares, 250);
    assert_eq!(account.cash, 3000.0);
    assert!(result.trades.iter().all(|trade| trade.cash_after == 3000.0));
    let mut account = Account::new(10000.0);
    let mut provider = Provider::default();
    provider.bars.retain(|bar| bar.symbol != b());
    let result = engine(10000.0)
        .execute_batch(
            &mut account,
            &mut provider,
            &mut Probe::default(),
            FIRST,
            NEXT,
            0,
            vec![Order::BuyWeights {
                weights: [(a(), 0.5), (b(), 0.5)].into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(result.skipped.len(), 1);
    assert_eq!(account.positions[&a()].purchased_shares, 500);
    assert_eq!(account.cash, 5000.0);
}

#[tokio::test]
async fn share_conflicts_fail_for_that_symbol_only() {
    let mut account = Account::new(1000.0);
    hold(&mut account, a(), 100);
    hold(&mut account, b(), 100);
    let result = batch(
        &mut account,
        vec![
            sell(a(), 60, 10.0),
            sell(a(), 60, 10.0),
            Order::SellAll { symbol: b() },
        ],
    )
    .await;
    assert_eq!(result.skipped.len(), 2);
    assert_eq!(result.trades.len(), 1);
    assert_eq!(account.positions[&a()].purchased_shares, 100);
    assert!(!account.positions.contains_key(&b()));
    let result = batch(
        &mut account,
        vec![Order::SellAll { symbol: a() }, sell(a(), 1, 10.0)],
    )
    .await;
    assert_eq!(result.skipped.len(), 2);
    assert!(result.trades.is_empty());
}

#[tokio::test]
async fn sell_all_sells_old_shares_and_preserves_todays_buys() {
    let mut account = Account::new(2000.0);
    hold(&mut account, a(), 100);
    batch(&mut account, vec![buy(a(), 100, 10.0)]).await;
    let result = batch(&mut account, vec![Order::SellAll { symbol: a() }]).await;
    assert_eq!(result.trades[0].shares, 100);
    assert_eq!(account.positions[&a()].purchased_shares, 100);
    assert_eq!(account.units[&a()], 100.0);
    assert_eq!(
        batch(&mut account, vec![Order::SellAll { symbol: a() }])
            .await
            .skipped
            .len(),
        1
    );
    account.start_day();
    assert_eq!(
        batch(&mut account, vec![Order::SellAll { symbol: a() }])
            .await
            .trades
            .len(),
        1
    );
    assert!(account.positions.is_empty());
}

#[tokio::test]
async fn amount_buys_include_fees_and_exact_orders_are_not_resized() {
    let mut e = engine(1005.0);
    e.config.minimum_commission = 5.0;
    e.config.lot_size = 100;
    let mut account = Account::new(1005.0);
    let result = e
        .execute_batch(
            &mut account,
            &mut Provider::default(),
            &mut Probe::default(),
            FIRST,
            NEXT,
            0,
            vec![Order::BuyAmount {
                symbol: a(),
                cash_amount: 1005.0,
            }],
        )
        .await
        .unwrap();
    assert_eq!(result.trades[0].shares, 100);
    assert_eq!(result.trades[0].commission, 5.0);
    assert_eq!(account.cash, 0.0);
    let mut account = Account::new(1000.0);
    let result = e
        .execute_batch(
            &mut account,
            &mut Provider::default(),
            &mut Probe::default(),
            FIRST,
            NEXT,
            0,
            vec![buy(a(), 100, 10.0)],
        )
        .await
        .unwrap();
    assert!(result.trades.is_empty());
    assert_eq!(account.cash, 1000.0);
}

#[tokio::test]
async fn partial_sales_preserve_adjusted_return_units() {
    let mut e = engine(1000.0);
    e.config.adjust_returns = true;
    let mut account = Account::new(0.0);
    hold(&mut account, a(), 100);
    let mut provider = Provider::default();
    provider
        .bars
        .iter_mut()
        .for_each(|bar| bar.adjustment = Some(Adjustment::Raw(2.0)));
    let result = e
        .execute_batch(
            &mut account,
            &mut provider,
            &mut Probe::default(),
            FIRST,
            NEXT,
            0,
            vec![sell(a(), 40, 10.0)],
        )
        .await
        .unwrap();
    assert_eq!(result.trades[0].notional, 800.0);
    assert_eq!(account.positions[&a()].purchased_shares, 60);
    assert_eq!(account.units[&a()], 60.0);
}

#[tokio::test]
async fn invalid_weights_and_limits_fail_without_mutating_account() {
    let mut account = Account::new(10000.0);
    let result = batch(
        &mut account,
        vec![
            Order::BuyWeights {
                weights: [(a(), 0.8), (b(), 0.8)].into(),
            },
            Order::BuyWeights {
                weights: [(a(), f64::NAN)].into(),
            },
            buy(a(), 0, 10.0),
            buy(a(), 100, f64::INFINITY),
            Order::BuyAmount {
                symbol: b(),
                cash_amount: -1.0,
            },
        ],
    )
    .await;
    assert_eq!(result.skipped.len(), 5);
    assert!(account.positions.is_empty());
    assert_eq!(account.cash, 10000.0);
}

#[tokio::test]
async fn failed_orders_are_reported_once_and_not_carried_to_later_days() {
    let mut strategy = Probe::default();
    strategy
        .orders
        .insert(FIRST, vec![vec![buy(a(), 100, 9.0)]]);
    let failures = strategy.failures.clone();
    let result = engine(2000.0)
        .run(Box::new(Provider::default()), Box::new(strategy))
        .await
        .unwrap();
    assert!(result.trades.is_empty());
    assert_eq!(result.skipped_orders.len(), 1);
    let failures = failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].detail.signal_date, FIRST);
    assert_eq!(failures[0].detail.date, NEXT);
}

#[tokio::test]
async fn engine_runs_batches_in_order_using_cash_after_previous_batch() {
    let mut strategy = Probe::default();
    strategy.orders.insert(
        FIRST,
        vec![vec![Order::BuyAmount {
            symbol: a(),
            cash_amount: 2000.0,
        }]],
    );
    strategy.orders.insert(
        NEXT,
        vec![
            vec![Order::SellAll { symbol: a() }],
            vec![Order::BuyWeights {
                weights: [(b(), 0.5), (c(), 0.5)].into(),
            }],
        ],
    );
    let result = engine(2000.0)
        .run(Box::new(Provider::default()), Box::new(strategy))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 4);
    assert_eq!(result.trades[1].batch_index, 0);
    assert_eq!(result.trades[1].side, "sell");
    assert_eq!(result.trades[1].cash_after, 2000.0);
    assert_eq!(result.trades[2].batch_index, 1);
    assert_eq!(result.trades[3].batch_index, 1);
    let final_positions = &result.equity_curve.last().unwrap().positions;
    assert!(!final_positions.contains_key(&a().to_string()));
    assert_eq!(final_positions[&b().to_string()].purchased_shares, 100);
    assert_eq!(final_positions[&c().to_string()].purchased_shares, 100);
}

#[tokio::test]
#[should_panic(expected = "未来数据")]
async fn close_context_still_rejects_future_history() {
    let mut provider = Provider::default();
    let positions = BTreeMap::new();
    let ctx = BtContext {
        date: FIRST,
        init_cash: 1000.0,
        cash: 1000.0,
        equity: 1000.0,
        positions: &positions,
        provider: tokio::sync::Mutex::new(&mut provider),
        adjust_returns: false,
    };
    ctx.history(a(), FIRST, NEXT).await;
}
