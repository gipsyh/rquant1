use super::*;
use crate::data::IndexComp;
use std::sync::{Arc, Mutex};
use time::macros::date;

pub(crate) type Requests = Arc<Mutex<Vec<(String, Date, Date)>>>;

pub(crate) struct IndexProvider {
    pub requests: Requests,
    pub hist: IndexHistComp,
    pub name: &'static str,
    pub name_requests: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl DataProvider for IndexProvider {
    async fn is_tradable(&mut self, _symbol: StockSymbol, _date: Date) -> bool {
        unreachable!("可交易判断应由缓存层完成")
    }

    async fn stock_info(&mut self, _: StockSymbol) -> crate::data::Stock {
        unreachable!()
    }

    async fn index_name(&mut self, symbol: &str) -> String {
        self.name_requests.lock().unwrap().push(symbol.into());
        self.name.into()
    }

    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        let (start, end) = (range.start(), range.end());
        self.requests
            .lock()
            .unwrap()
            .push((symbol.into(), start, end));
        self.hist.slice(range).unwrap()
    }

    async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
        let (_start, _end) = (range.start(), range.end());
        unreachable!()
    }

    async fn stock_bar(&mut self, _symbol: StockSymbol, range: DateRange) -> StockHistBar {
        let (_start, _end) = (range.start(), range.end());
        unreachable!()
    }
}

pub(crate) fn history() -> IndexHistComp {
    IndexHistComp::new(
        DateRange::new(date!(2023 - 12 - 01), date!(2024 - 04 - 30)),
        [
            (date!(2023 - 12 - 29), 0.1),
            (date!(2024 - 01 - 10), 0.2),
            (date!(2024 - 02 - 20), 0.3),
            (date!(2024 - 03 - 25), 0.4),
        ]
        .into_iter()
        .map(|(date, weight)| {
            (
                date,
                Arc::new(
                    IndexComp::new([(StockSymbol::from("600000.SH"), weight)].into()).unwrap(),
                ),
            )
        })
        .collect(),
    )
    .unwrap()
}

#[tokio::test]
async fn index_prefetches_once_and_extends_only_missing_ranges() {
    let requests = Requests::default();
    let name_requests = Arc::new(Mutex::new(Vec::new()));
    let full = history();
    let mut cache = MemCacheProvider::new(
        Box::new(IndexProvider {
            name: "沪深300",
            name_requests: name_requests.clone(),
            requests: requests.clone(),
            hist: full.clone(),
        }),
        DateRange::new(date!(2024 - 02 - 01), date!(2024 - 02 - 29)),
    );
    let day = date!(2024 - 02 - 05);
    let first = cache
        .index_comp("000300.SH", DateRange::new(day, day))
        .await;
    assert_eq!(cache.data.index["000300.XSHG"].symbol, "000300.XSHG");
    assert_eq!(cache.data.index["000300.XSHG"].name, "沪深300");
    assert_eq!(first.range(), DateRange::new(day, day));
    assert_eq!(
        first.composition(day).unwrap(),
        full.composition(day).unwrap()
    );
    let again = cache
        .index_comp("000300.XSHG", DateRange::new(day, day))
        .await;
    assert!(Arc::ptr_eq(
        &first.composition(day).unwrap(),
        &again.composition(day).unwrap()
    ));
    let wider = cache
        .index_comp(
            "000300",
            DateRange::new(date!(2024 - 01 - 01), date!(2024 - 03 - 31)),
        )
        .await;
    assert_eq!(
        wider,
        full.slice(DateRange::new(date!(2024 - 01 - 01), date!(2024 - 03 - 31)))
            .unwrap()
    );
    assert_eq!(cache.data.index["000300.XSHG"].name, "沪深300");
    assert_eq!(cache.data.index["000300.XSHG"].comp, wider);
    assert_eq!(cache.index_name("000300.SH").await, "沪深300");
    assert_eq!(*name_requests.lock().unwrap(), vec!["000300.XSHG"]);
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            (
                "000300.XSHG".into(),
                date!(2024 - 02 - 01),
                date!(2024 - 02 - 29)
            ),
            (
                "000300.XSHG".into(),
                date!(2024 - 01 - 01),
                date!(2024 - 01 - 31)
            ),
            (
                "000300.XSHG".into(),
                date!(2024 - 03 - 01),
                date!(2024 - 03 - 31)
            ),
        ]
    );
    cache
        .index_comp("000905.SH", DateRange::new(day, day))
        .await;
    assert_eq!(requests.lock().unwrap().len(), 4, "不同指数独立缓存");
    assert_eq!(
        *name_requests.lock().unwrap(),
        vec!["000300.XSHG", "000905.XSHG"]
    );
}

#[tokio::test]
async fn empty_index_history_is_cached_without_fabricating_composition() {
    let requests = Requests::default();
    let start = date!(2024 - 01 - 01);
    let end = date!(2024 - 01 - 31);
    let mut cache = MemCacheProvider::new(
        Box::new(IndexProvider {
            name: "沪深300",
            name_requests: Arc::default(),
            requests: requests.clone(),
            hist: IndexHistComp::new(DateRange::new(start, end), vec![]).unwrap(),
        }),
        DateRange::new(start, end),
    );
    for _ in 0..2 {
        assert!(
            cache
                .index_comp("000300", DateRange::new(start, start))
                .await
                .composition(start)
                .is_err()
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn empty_name_never_creates_an_index_or_downloads_composition() {
    let requests = Requests::default();
    let start = date!(2024 - 01 - 01);
    let end = date!(2024 - 01 - 31);
    let cache = Arc::new(tokio::sync::Mutex::new(MemCacheProvider::new(
        Box::new(IndexProvider {
            requests: requests.clone(),
            hist: history(),
            name: " ",
            name_requests: Arc::default(),
        }),
        DateRange::new(start, end),
    )));
    let task_cache = cache.clone();
    let failure = tokio::spawn(async move {
        task_cache
            .lock()
            .await
            .index_comp("000300", DateRange::new(start, end))
            .await;
    })
    .await
    .unwrap_err();
    assert!(failure.is_panic());
    assert!(cache.lock().await.data.index.is_empty());
    assert!(requests.lock().unwrap().is_empty());
}
