use super::*;
use crate::data::{IndexComp, IndexHistComp, Stock, StockHistBar};
use crate::strategy::{LowTurnoverTrend, LowTurnoverTrendConfig, StrategyConfig};
use crate::utils::DateRange;
use clap::Parser;
use std::collections::BTreeSet;
use time::Duration;

const DAY: Date = date!(2024 - 01 - 15);
fn d() -> StockSymbol {
    "002001".into()
}
fn e() -> StockSymbol {
    "002002".into()
}
type Queries = Arc<Mutex<Vec<(StockSymbol, Date, Date)>>>;

struct Source {
    bars: Vec<StockBar>,
    listed: BTreeMap<StockSymbol, Date>,
    comp: IndexHistComp,
    days: Vec<Date>,
    queries: Queries,
    batch_sizes: Vec<usize>,
}

impl Source {
    fn new(symbols: &[StockSymbol]) -> Self {
        Self {
            bars: symbols
                .iter()
                .enumerate()
                .flat_map(|(i, &symbol)| {
                    (-3..=3).map(move |offset| {
                        let mut bar = bar(symbol, DAY + Duration::days(offset));
                        bar.turnover = (i + 1) as f64 * 100.0;
                        bar.close = 10.0 + offset as f64;
                        bar
                    })
                })
                .collect(),
            listed: symbols
                .iter()
                .map(|&symbol| (symbol, date!(2000 - 01 - 01)))
                .collect(),
            comp: IndexHistComp::new(
                DateRange::new(DAY - Duration::days(30), DAY + Duration::days(30)),
                vec![(
                    DAY - Duration::days(30),
                    Arc::new(IndexComp::new(symbols.iter().map(|&s| (s, 0.0)).collect()).unwrap()),
                )],
            )
            .unwrap(),
            days: vec![DAY, DAY.next_day().unwrap(), DAY + Duration::days(2)],
            queries: Arc::default(),
            batch_sizes: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl DataProvider for Source {
    async fn is_tradable(&mut self, symbol: StockSymbol, date: Date) -> bool {
        self.bars
            .iter()
            .any(|bar| bar.symbol == symbol && bar.date == date && !bar.st)
    }

    async fn stock_info(&mut self, symbol: StockSymbol) -> Stock {
        Stock {
            symbol,
            name: symbol.to_string(),
            listed: self.listed[&symbol],
            delisted: None,
            industry: None,
            bars: None,
        }
    }
    async fn stocks_bar(&mut self, requests: &[(StockSymbol, DateRange)]) -> Vec<StockHistBar> {
        self.batch_sizes.push(requests.len());
        let mut results = Vec::new();
        for &(symbol, range) in requests {
            results.push(self.stock_bar(symbol, range).await);
        }
        results
    }

    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
        let (start, end) = (range.start(), range.end());
        self.queries.lock().unwrap().push((symbol, start, end));
        let bars = self
            .bars
            .iter()
            .filter(|bar| bar.symbol == symbol && start <= bar.date && bar.date <= end)
            .copied()
            .collect();
        StockHistBar::new(range, bars).unwrap()
    }
    async fn index_name(&mut self, _: &str) -> String {
        "测试指数".into()
    }
    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        assert_eq!(symbol, "399101.XSHE");
        self.comp.slice(range).unwrap()
    }
    async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
        self.days
            .iter()
            .filter(|&&day| range.contains(day))
            .copied()
            .collect()
    }
}

fn config() -> LowTurnoverTrendConfig {
    LowTurnoverTrendConfig {
        top_k: 2,
        breadth_count: 3,
        liquidity_lookback: 3,
        trend_lookback: 3,
        history_start: DAY - Duration::days(30),
        ..Default::default()
    }
}

async fn signal(
    strategy: &mut LowTurnoverTrend,
    source: &mut dyn DataProvider,
    date: Date,
    positions: &BTreeMap<StockSymbol, Position>,
) -> Vec<Vec<Order>> {
    let ctx = BtContext {
        date,
        init_cash: 10000.0,
        cash: 0.0,
        equity: 10000.0,
        positions,
        provider: tokio::sync::Mutex::new(source),
    };
    strategy.on_trade_day(&ctx).await
}

fn weights(orders: &[Vec<Order>]) -> BTreeMap<StockSymbol, f64> {
    match &orders.last().unwrap()[0] {
        Order::BuyWeights { weights } => weights.clone(),
        other => panic!("unexpected order: {other:?}"),
    }
}

#[tokio::test]
async fn ranks_turnover_uses_adjusted_trend_and_ignores_future_prices() {
    let mut source = Source::new(&[b(), a(), c()]);
    // 成交额相同时按代码选股；因子跳变不应产生虚假的下跌趋势。
    for bar in &mut source.bars {
        if bar.symbol == a() || bar.symbol == b() {
            bar.turnover = 100.0;
        }
        if bar.date >= DAY {
            bar.close /= 2.0;
            bar.low = 1.0;
            bar.adjustment = Some(Adjustment::Raw(2.0));
        }
    }
    let mut cfg = config();
    cfg.top_k = 1;
    let before = weights(
        &signal(
            &mut LowTurnoverTrend::new(cfg.clone()),
            &mut source,
            DAY,
            &BTreeMap::new(),
        )
        .await,
    );
    assert_eq!(before, [(a(), 0.95)].into());
    for bar in &mut source.bars {
        if bar.date > DAY {
            bar.close = 1e9;
            bar.turnover = 1e15;
        }
    }
    let after = weights(
        &signal(
            &mut LowTurnoverTrend::new(cfg),
            &mut source,
            DAY,
            &BTreeMap::new(),
        )
        .await,
    );
    assert_eq!(before, after);
    assert!(
        source
            .queries
            .lock()
            .unwrap()
            .iter()
            .all(|&(_, _, end)| end <= DAY)
    );
}

#[tokio::test]
async fn excludes_st_missing_day_young_stocks_and_nonpositive_turnover() {
    let mut source = Source::new(&[a(), b(), c(), d(), e()]);
    source.listed.insert(b(), DAY - Duration::days(119));
    for bar in &mut source.bars {
        if bar.symbol == a() && bar.date == DAY {
            bar.st = true;
        }
    }
    source
        .bars
        .retain(|bar| !(bar.symbol == c() && bar.date == DAY));
    let mut cfg = config();
    cfg.breadth_count = 2;
    let orders = signal(
        &mut LowTurnoverTrend::new(cfg.clone()),
        &mut source,
        DAY,
        &BTreeMap::new(),
    )
    .await;
    assert_eq!(weights(&orders), [(d(), 0.475), (e(), 0.475)].into());
    source
        .bars
        .iter_mut()
        .filter(|bar| bar.symbol == d() && bar.date == DAY)
        .for_each(|bar| bar.turnover = 0.0);
    // 候选不足时不生成清仓单，即使实际持有不在目标池的股票。
    let held = [(
        a(),
        Position {
            purchased_shares: 100,
            market_value: 1000.0,
        },
    )]
    .into();
    assert!(
        signal(&mut LowTurnoverTrend::new(cfg), &mut source, DAY, &held)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn warmup_backfills_sparse_history_and_then_only_appends_new_dates() {
    let mut source = Source::new(&[a()]);
    source.bars = [(-20, 8.0), (-10, 9.0), (0, 10.0), (1, 11.0)]
        .into_iter()
        .map(|(offset, close)| StockBar {
            close,
            ..bar(a(), DAY + Duration::days(offset))
        })
        .collect();
    let cfg = LowTurnoverTrendConfig {
        top_k: 1,
        breadth_count: 1,
        ..config()
    };
    let mut strategy = LowTurnoverTrend::new(cfg);
    assert_eq!(
        weights(&signal(&mut strategy, &mut source, DAY, &BTreeMap::new()).await),
        [(a(), 0.95)].into()
    );
    let queries = source.queries.lock().unwrap().clone();
    assert!(
        queries
            .iter()
            .any(|&(_, start, _)| start <= DAY - Duration::days(20))
    );
    source.queries.lock().unwrap().clear();
    signal(
        &mut strategy,
        &mut source,
        DAY.next_day().unwrap(),
        &BTreeMap::new(),
    )
    .await;
    assert!(
        source
            .queries
            .lock()
            .unwrap()
            .iter()
            .all(|&(_, start, end)| start == DAY.next_day().unwrap() && end == start)
    );
}

#[tokio::test]
async fn keeps_intersection_and_buys_only_new_symbols_even_when_signal_cash_is_zero() {
    let mut source = Source::new(&[a(), d(), e()]);
    let cfg = LowTurnoverTrendConfig {
        top_k: 3,
        ..config()
    };
    let held = [a(), b(), c()]
        .map(|s| {
            (
                s,
                Position {
                    purchased_shares: 100,
                    market_value: 1000.0,
                },
            )
        })
        .into();
    let mut strategy = LowTurnoverTrend::new(cfg);
    let orders = signal(&mut strategy, &mut source, DAY, &held).await;
    assert_eq!(orders.len(), 2);
    let sold: BTreeSet<_> = orders[0]
        .iter()
        .map(|order| match order {
            Order::SellAll { symbol } => *symbol,
            _ => panic!(),
        })
        .collect();
    assert_eq!(sold, [b(), c()].into());
    assert_eq!(weights(&orders), [(d(), 0.475), (e(), 0.475)].into());
    // 依据实际持仓重试失败订单；已有少量仓位也不会补仓。
    let partial = [
        (
            a(),
            Position {
                purchased_shares: 1,
                market_value: 10.0,
            },
        ),
        (
            d(),
            Position {
                purchased_shares: 1,
                market_value: 10.0,
            },
        ),
    ]
    .into();
    assert_eq!(
        weights(
            &signal(
                &mut strategy,
                &mut source,
                DAY.next_day().unwrap(),
                &partial
            )
            .await
        ),
        [(e(), 0.95)].into()
    );
    let complete = [a(), d(), e()]
        .map(|s| {
            (
                s,
                Position {
                    purchased_shares: 1,
                    market_value: 10.0,
                },
            )
        })
        .into();
    assert!(
        signal(
            &mut strategy,
            &mut source,
            DAY + Duration::days(2),
            &complete
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn rotation_uses_next_open_sale_proceeds_and_keeps_existing_shares() {
    let mut source = Source::new(&[a(), b(), c(), d(), e()]);
    source.comp = IndexHistComp::new(
        source.comp.range(),
        vec![
            (
                DAY,
                Arc::new(IndexComp::new([a(), b(), c()].map(|s| (s, 0.0)).into()).unwrap()),
            ),
            (
                DAY.next_day().unwrap(),
                Arc::new(IndexComp::new([a(), d(), e()].map(|s| (s, 0.0)).into()).unwrap()),
            ),
        ],
    )
    .unwrap();
    let strategy = LowTurnoverTrend::new(LowTurnoverTrendConfig {
        top_k: 3,
        ..config()
    });
    let mut engine = engine(10000.0);
    engine.config.start = DAY;
    engine.config.end = DAY + Duration::days(2);
    let result = engine
        .run(Box::new(source), Box::new(strategy))
        .await
        .unwrap();
    assert_eq!(result.trades.len(), 7);
    let first_shares = result.equity_curve[1].positions[&a().to_string()].purchased_shares;
    assert_eq!(
        result.equity_curve[2].positions[&a().to_string()].purchased_shares,
        first_shares
    );
    let rotation = &result.trades[3..];
    assert!(
        rotation
            .iter()
            .all(|t| t.signal_date == DAY.next_day().unwrap() && t.date == DAY + Duration::days(2))
    );
    assert!(
        rotation[..2]
            .iter()
            .all(|t| t.side == "sell" && t.batch_index == 0)
    );
    assert!(
        rotation[2..]
            .iter()
            .all(|t| t.side == "buy" && t.batch_index == 1)
    );
    let budget = rotation[0].cash_after * 0.475;
    for trade in &rotation[2..] {
        assert_eq!(trade.shares, (budget / trade.price).floor() as u64);
    }
    assert_eq!(result.equity_curve[2].positions.len(), 3);
}

#[test]
fn cli_accepts_both_strategy_spellings_and_validates_parameters() {
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        strategy: StrategyConfig,
    }
    for name in ["low-turnover-trend", "low_turnover_trend"] {
        let cli = Cli::try_parse_from(["rquant", name]).unwrap();
        assert_eq!(cli.strategy.build().name(), "low_turnover_trend");
    }
    let cfg = LowTurnoverTrendConfig::default();
    assert_eq!(cfg.liquidity_lookback, 120);
    assert_eq!(cfg.minimum_listed_days, 120);
    for bad in [
        LowTurnoverTrendConfig {
            top_k: 0,
            ..cfg.clone()
        },
        LowTurnoverTrendConfig {
            breadth_count: 5,
            ..cfg.clone()
        },
        LowTurnoverTrendConfig {
            trend_lookback: 0,
            ..cfg.clone()
        },
        LowTurnoverTrendConfig {
            defensive_fraction: f64::NAN,
            ..cfg.clone()
        },
        LowTurnoverTrendConfig {
            trend_band: 1.0,
            ..cfg
        },
    ] {
        assert!(std::panic::catch_unwind(|| LowTurnoverTrend::new(bad)).is_err());
    }
}

#[tokio::test]
async fn strategy_batches_warmup_and_incremental_history() {
    let mut source = Source::new(&[a(), b(), c()]);
    let mut strategy = LowTurnoverTrend::new(config());
    signal(&mut strategy, &mut source, DAY, &BTreeMap::new()).await;
    assert_eq!(source.batch_sizes, vec![3]);
    signal(
        &mut strategy,
        &mut source,
        DAY.next_day().unwrap(),
        &BTreeMap::new(),
    )
    .await;
    assert_eq!(source.batch_sizes, vec![3, 3]);
}
