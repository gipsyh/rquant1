use super::*;
use crate::data::{Adjustment, Stock};
use std::sync::{Arc, Mutex};
use time::macros::date;

type Requests = Arc<Mutex<Vec<(InstrSymbol, Date, Date)>>>;

struct Provider {
    bars: Vec<StockDailyBar>,
    requests: Requests,
}

#[async_trait::async_trait]
impl DataProvider for Provider {
    async fn trading_days(&mut self, start: Date, _end: Date) -> Vec<Date> {
        vec![start]
    }

    async fn daily_bars(
        &mut self,
        symbol: InstrSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        self.requests.lock().unwrap().push((symbol, start, end));
        self.bars
            .iter()
            .filter(|bar| bar.symbol == symbol && bar.date >= start && bar.date <= end)
            .copied()
            .collect()
    }
}

fn provider(bars: Vec<StockDailyBar>, requests: &Requests) -> Box<dyn DataProvider> {
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
    let symbol = InstrSymbol::from("000001.SZ");
    let bars: Vec<_> = [
        None,
        Some(Adjustment::Raw(1.23)),
        Some(Adjustment::Pre),
        Some(Adjustment::Post),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, adjustment)| StockDailyBar {
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
                .bars
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
        assert_eq!(cache.daily_bars(symbol, start, start).await, bars[..1]);
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
        assert_eq!(cache.daily_bars(symbol, start, end).await, bars);
        let stock = &cache.inner.data.stock[&symbol];
        assert_eq!(stock.symbol, symbol);
        assert_eq!(stock.name, "测试股票");
        assert_eq!(stock.listed, date!(1991 - 04 - 03));
        assert_eq!(stock.delisted, None);
        assert_eq!(stock.industry.as_deref(), Some("银行"));
        assert_eq!(cache.inner.data.bar_date[&symbol], (start, end));
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
    let symbol = InstrSymbol::from("000001.SZ");
    let other = InstrSymbol::from("600000.SH");
    let requests = Requests::default();
    {
        let mut cache =
            DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path.clone())
                .unwrap();
        assert!(cache.daily_bars(symbol, start, end).await.is_empty());
    }
    requests.lock().unwrap().clear();
    {
        let mut cache =
            DiskCacheProvider::with_path(provider(vec![], &requests), start, end, path.clone())
                .unwrap();
        assert!(cache.daily_bars(symbol, start, end).await.is_empty());
        assert!(requests.lock().unwrap().is_empty());
        let earlier = date!(2024 - 01 - 01);
        let later = date!(2024 - 01 - 06);
        assert!(cache.daily_bars(symbol, earlier, later).await.is_empty());
        assert!(cache.daily_bars(symbol, earlier, later).await.is_empty());
        assert!(cache.daily_bars(other, start, end).await.is_empty());
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
            .daily_bars(symbol, date!(2024 - 01 - 01), date!(2024 - 01 - 06))
            .await
            .is_empty()
    );
    assert!(cache.daily_bars(other, start, end).await.is_empty());
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

fn bar(symbol: InstrSymbol, date: Date) -> StockDailyBar {
    StockDailyBar {
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
    let symbol = InstrSymbol::from("000001.SZ");
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
        assert_eq!(cache.daily_bars(symbol, middle, middle).await, bars[2..3]);
        assert_eq!(cache.daily_bars(symbol, first, last).await, bars);
        assert_eq!(cache.daily_bars(symbol, middle, last).await, bars[2..]);
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
    assert_eq!(data.bar_date[&symbol], (first, last));
    assert_eq!(data.bars[&symbol], bars);
    requests.lock().unwrap().clear();
    let mut cache =
        DiskCacheProvider::with_path(provider(vec![], &requests), first, last, path).unwrap();
    assert_eq!(cache.daily_bars(symbol, first, last).await, bars);
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn 拒绝区间不一致或无序重复日线且保留文件() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DiskCacheProvider::FILE_NAME);
    let symbol = InstrSymbol::from("000001.SZ");
    let first = date!(2024 - 01 - 02);
    let last = date!(2024 - 01 - 03);
    for case in 0..7 {
        let mut data = RqData::default();
        data.bar_date.insert(symbol, (first, last));
        data.bars
            .insert(symbol, vec![bar(symbol, first), bar(symbol, last)]);
        match case {
            0 => {
                data.bar_date.clear();
            }
            1 => {
                data.bars.clear();
            }
            2 => {
                data.bar_date.insert(symbol, (last, first));
            }
            3 => {
                data.bar_date.insert(symbol, (first, first));
            }
            4 => {
                data.bars.get_mut(&symbol).unwrap().reverse();
            }
            5 => {
                data.bars.get_mut(&symbol).unwrap()[1].date = first;
            }
            6 => {
                data.bars.get_mut(&symbol).unwrap()[0].symbol = "600000.SH".into();
            }
            _ => unreachable!(),
        }
        let text = ron::to_string(&data).unwrap();
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
