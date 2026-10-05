use super::*;
use crate::data::{Adjustment, Stock, StockHistBar};
use crate::utils::DateRange;
use std::sync::{Arc, Mutex};
use time::macros::date;

type Requests = Arc<Mutex<Vec<(StockSymbol, Date, Date)>>>;

#[tokio::test]
async fn index_history_persists_and_reloads_without_downloading() {
    use crate::data::cache::mem::test::{IndexProvider, Requests as IndexRequests, history};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.ron");
    let requests = IndexRequests::default();
    let name_requests = Arc::new(Mutex::new(Vec::new()));
    let start = date!(2024 - 01 - 01);
    let end = date!(2024 - 03 - 31);
    for _ in 0..2 {
        let mut cache = DiskCacheProvider::with_path(
            Box::new(IndexProvider {
                name: "沪深300",
                name_requests: name_requests.clone(),
                requests: requests.clone(),
                hist: history(),
            }),
            start,
            end,
            path.clone(),
        )
        .unwrap();
        let comp = cache.index_comp("000300.SH", start, end).await;
        assert_eq!(comp, history().slice(start, end).unwrap());
        let index = cache.inner.data.index.get_mut("000300.XSHG").unwrap();
        assert_eq!(index.symbol, "000300.XSHG");
        assert_eq!(index.name, "沪深300");
        assert_eq!(cache.index_name("000300.SH").await, "沪深300");
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(*name_requests.lock().unwrap(), vec!["000300.XSHG"]);
}

#[test]
fn invalid_index_cache_is_rejected_and_original_file_is_preserved() {
    use crate::data::{Index, cache::mem::test::history};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.ron");
    for case in 0..5 {
        let mut data = RqData::default();
        let symbol = if case == 0 {
            "000905.XSHG"
        } else {
            "000300.XSHG"
        };
        let key = if case == 1 {
            "000300.SH"
        } else {
            "000300.XSHG"
        };
        data.index.insert(
            key.into(),
            Index {
                symbol: symbol.into(),
                name: match case {
                    3 => "",
                    4 => "  ",
                    _ => "沪深300",
                }
                .into(),
                comp: history(),
            },
        );
        let mut text = ron::to_string(&data).unwrap();
        if case == 2 {
            // 生效日移到覆盖范围以外，加载时必须拒绝。
            assert!(text.contains("2024-03-25"));
            text = text.replace("2024-03-25", "2025-03-25");
        }
        std::fs::write(&path, &text).unwrap();
        let result = DiskCacheProvider::with_path(
            provider(vec![], &Requests::default()),
            date!(2024 - 01 - 01),
            date!(2024 - 03 - 31),
            path.clone(),
        );
        assert!(result.is_err(), "case {case}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}

struct Provider {
    bars: Vec<StockBar>,
    requests: Requests,
}

#[async_trait::async_trait]
impl DataProvider for Provider {
    async fn index_name(&mut self, _symbol: &str) -> String {
        panic!("本测试数据源不提供指数名称")
    }

    async fn index_comp(&mut self, _symbol: &str, _start: Date, _end: Date) -> IndexHistComp {
        panic!("本测试数据源不提供指数成分")
    }

    async fn trading_days(&mut self, start: Date, _end: Date) -> Vec<Date> {
        vec![start]
    }

    async fn stock_bar(&mut self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        self.requests.lock().unwrap().push((symbol, start, end));
        self.bars
            .iter()
            .filter(|bar| bar.symbol == symbol && bar.date >= start && bar.date <= end)
            .copied()
            .collect()
    }
}

fn provider(bars: Vec<StockBar>, requests: &Requests) -> Box<dyn DataProvider> {
    Box::new(Provider {
        bars,
        requests: Arc::clone(requests),
    })
}

#[tokio::test]
async fn drop保存并在重建后命中日线缓存() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let start = date!(2024 - 01 - 02);
    let end = date!(2024 - 01 - 05);
    let symbol = StockSymbol::from("000001.SZ");
    let bars: Vec<_> = [
        None,
        Some(Adjustment::Raw(1.23)),
        Some(Adjustment::Pre),
        Some(Adjustment::Post),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, adjustment)| StockBar {
        symbol,
        date: start + time::Duration::days(i as i64),
        open: 9.41,
        high: 9.52,
        low: 9.30,
        close: 9.45,
        volume: 123456.0,
        turnover: 1234567.89,
        limit_up: Some(10.40),
        limit_down: None,
        float_market_cap: Some(2.2e11),
        adjustment,
        st: i % 2 == 0,
    })
    .collect();
    let requests = Requests::default();
    {
        let mut cache = DiskCacheProvider::with_path(
            provider(bars.clone(), &requests),
            start,
            end,
            path.clone(),
        )
        .unwrap();
        assert!(path.exists(), "构造时创建空缓存文件");
        assert!(
            ron::from_str::<RqData>(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .stock_bars
                .is_empty()
        );
        cache.inner.data.stock.insert(
            symbol,
            Stock {
                symbol,
                name: "测试股票".into(),
                listed: date!(1991 - 04 - 03),
                delisted: None,
                industry: Some("银行".into()),
            },
        );
        // 首次只查询一天，也预取构造时的整个范围。
        assert_eq!(cache.stock_bar(symbol, start, start).await, bars[..1]);
        assert_eq!(*requests.lock().unwrap(), vec![(symbol, start, end)]);
    }
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("2024-01-02"));
    assert!(text.contains("Raw(1.23)"));
    requests.lock().unwrap().clear();
    {
        let mut cache =
            DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path.clone())
                .unwrap();
        assert_eq!(cache.stock_bar(symbol, start, end).await, bars);
        let stock = &cache.inner.data.stock[&symbol];
        assert_eq!(stock.symbol, symbol);
        assert_eq!(stock.name, "测试股票");
        assert_eq!(stock.listed, date!(1991 - 04 - 03));
        assert_eq!(stock.delisted, None);
        assert_eq!(stock.industry.as_deref(), Some("银行"));
        assert_eq!(
            cache.inner.data.stock_bars[&symbol].range(),
            DateRange::new(start, end)
        );
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(cache.trading_days(start, end).await, vec![start]);
        // 即使没有新增数据，Drop 也会重写。
        std::fs::write(&path, format!("{text}\n// rewrite me\n")).unwrap();
    }
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("rewrite me")
    );
}

#[tokio::test]
async fn 空区间跨实例保留且只补拉两端和新股票() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let start = date!(2024 - 01 - 02);
    let end = date!(2024 - 01 - 05);
    let symbol = StockSymbol::from("000001.SZ");
    let other = StockSymbol::from("600000.SH");
    let requests = Requests::default();
    {
        let mut cache =
            DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path.clone())
                .unwrap();
        assert!(cache.stock_bar(symbol, start, end).await.is_empty());
    }
    requests.lock().unwrap().clear();
    {
        let mut cache =
            DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path.clone())
                .unwrap();
        assert!(cache.stock_bar(symbol, start, end).await.is_empty());
        assert!(requests.lock().unwrap().is_empty());
        let earlier = date!(2024 - 01 - 01);
        let later = date!(2024 - 01 - 06);
        assert!(cache.stock_bar(symbol, earlier, later).await.is_empty());
        assert!(cache.stock_bar(symbol, earlier, later).await.is_empty());
        assert!(cache.stock_bar(other, start, end).await.is_empty());
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                (symbol, earlier, earlier),
                (symbol, later, later),
                (other, start, end),
            ]
        );
    }
    requests.lock().unwrap().clear();
    let mut cache =
        DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path).unwrap();
    assert!(
        cache
            .stock_bar(symbol, date!(2024 - 01 - 01), date!(2024 - 01 - 06))
            .await
            .is_empty()
    );
    assert!(cache.stock_bar(other, start, end).await.is_empty());
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn 损坏或旧格式时保留原文件() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let start = date!(2024 - 01 - 02);
    for text in ["not valid ron", "(version: 999, bars: {})"] {
        std::fs::write(&path, text).unwrap();
        let result = DiskCacheProvider::with_path(
            provider(vec![], &Requests::default()),
            start,
            start,
            path.clone(),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}

fn bar(symbol: StockSymbol, date: Date) -> StockBar {
    StockBar {
        symbol,
        date,
        open: 10.0,
        high: 11.0,
        low: 9.0,
        close: 10.5,
        volume: 100.0,
        turnover: 1000.0,
        limit_up: None,
        limit_down: None,
        float_market_cap: None,
        adjustment: None,
        st: false,
    }
}

#[tokio::test]
async fn 两端补拉后所有日线有序且完整保存() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let symbol = StockSymbol::from("000001.SZ");
    let first = date!(2024 - 01 - 01);
    let middle = date!(2024 - 01 - 03);
    let last = date!(2024 - 01 - 05);
    let bars: Vec<_> = (0..5)
        .map(|i| bar(symbol, first + time::Duration::days(i)))
        .collect();
    let requests = Requests::default();
    {
        // 上游即使返回乱序，也会在进入缓存时排好序。
        let mut cache = DiskCacheProvider::with_path(
            provider(bars.iter().rev().copied().collect(), &requests),
            middle,
            middle,
            path.clone(),
        )
        .unwrap();
        assert_eq!(cache.stock_bar(symbol, middle, middle).await, bars[2..3]);
        assert_eq!(cache.stock_bar(symbol, first, last).await, bars);
        assert_eq!(cache.stock_bar(symbol, middle, last).await, bars[2..]);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                (symbol, middle, middle),
                (symbol, first, middle.previous_day().unwrap()),
                (symbol, middle.next_day().unwrap(), last),
            ]
        );
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let data: RqData = ron::from_str(&text).unwrap();
    assert_eq!(
        data.stock_bars[&symbol].range(),
        DateRange::new(first, last)
    );
    assert_eq!(data.stock_bars[&symbol].bars(), bars);
    requests.lock().unwrap().clear();
    let mut cache =
        DiskCacheProvider::with_path(provider(vec![], &requests), first, last, path).unwrap();
    assert_eq!(cache.stock_bar(symbol, first, last).await, bars);
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn 拒绝区间不一致或无序重复日线且保留文件() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let symbol = StockSymbol::from("000001.SZ");
    let first = date!(2024 - 01 - 02);
    let last = date!(2024 - 01 - 03);
    for case in 0..6 {
        let mut data = RqData::default();
        data.stock_bars.insert(
            symbol,
            StockHistBar::new(
                DateRange::new(first, last),
                vec![bar(symbol, first), bar(symbol, last)],
            )
            .unwrap(),
        );
        let hist = data.stock_bars.get_mut(&symbol).unwrap();
        match case {
            0 => hist.range = DateRange::new(first, first),
            1 => hist.bars.reverse(),
            // 无效区间只能在序列化后篡改，公开 API 不允许构造。
            2 => {}
            3 => hist.bars[1].date = first,
            4 => hist.bars[0].symbol = "600000.SH".into(),
            5 => {
                for bar in &mut hist.bars {
                    bar.symbol = "600000.SH".into();
                }
            }
            _ => unreachable!(),
        }
        let mut text = ron::to_string(&data).unwrap();
        if case == 2 {
            let valid = ron::to_string(&DateRange::new(first, last)).unwrap();
            let invalid = format!(
                "(start:{},end:{})",
                ron::to_string(&last).unwrap(),
                ron::to_string(&first).unwrap()
            );
            assert!(text.contains(&valid));
            text = text.replace(&valid, &invalid);
        }
        std::fs::write(&path, &text).unwrap();
        let result = DiskCacheProvider::with_path(
            provider(vec![], &Requests::default()),
            first,
            last,
            path.clone(),
        );
        assert!(result.is_err(), "case {case}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}

#[test]
fn stock_history_validates_and_slices_covered_dates() {
    let symbol = StockSymbol::from("000001.SZ");
    let first = date!(2024 - 01 - 02);
    let middle = date!(2024 - 01 - 03);
    let last = date!(2024 - 01 - 04);
    let range = DateRange::new(first, last);
    let hist = StockHistBar::new(range, vec![bar(symbol, first), bar(symbol, last)]).unwrap();
    let empty = hist.slice(middle, middle).unwrap();
    assert_eq!(empty.range(), DateRange::new(middle, middle));
    assert!(empty.bars().is_empty());
    assert_eq!(hist.slice(first, first).unwrap().bars(), &hist.bars()[..1]);
    assert_eq!(hist.slice(first, last).unwrap(), hist);
    assert!(hist.slice(first.previous_day().unwrap(), last).is_err());
    assert!(hist.slice(first, last.next_day().unwrap()).is_err());
    assert!(hist.slice(last, first).is_err());
    for bars in [
        vec![bar(symbol, last), bar(symbol, first)],
        vec![bar(symbol, first), bar(symbol, first)],
        vec![bar(symbol, first.previous_day().unwrap())],
        vec![bar(symbol, last.next_day().unwrap())],
        vec![bar(symbol, first), bar("600000.SH".into(), last)],
    ] {
        assert!(StockHistBar::new(range, bars).is_err());
    }
    let mut combined = empty;
    combined.extend(hist.slice(first, first).unwrap());
    combined.extend(hist.slice(last, last).unwrap());
    assert_eq!(combined, hist);
}
