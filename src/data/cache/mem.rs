use crate::data::{DataProvider, RqData, StockDailyBar, StockSymbol};
use time::Date;

/// 每次回测独立创建；首次访问股票时加载整个配置区间，销毁时释放缓存。
/// 仅缓存日线，交易日历直接转发给底层数据源。
pub struct MemCacheProvider {
    provider: Box<dyn DataProvider>,
    start: Date,
    end: Date,
    pub(super) data: RqData,
}

impl MemCacheProvider {
    pub fn new(provider: Box<dyn DataProvider>, start: Date, end: Date) -> Self {
        assert!(start <= end, "缓存开始日期不能晚于结束日期");
        Self {
            provider,
            start,
            end,
            data: RqData::default(),
        }
    }

    async fn download(
        &mut self,
        symbol: StockSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        let mut bars = self.provider.daily_bars(symbol, start, end).await;
        bars.sort_unstable_by_key(|bar| bar.date);
        assert!(
            bars.iter()
                .all(|bar| bar.symbol == symbol && bar.date >= start && bar.date <= end)
                && bars.windows(2).all(|pair| pair[0].date < pair[1].date),
            "行情数据无效: {symbol} 返回股票不匹配、越界或重复日线"
        );
        bars
    }
}

#[async_trait::async_trait]
impl DataProvider for MemCacheProvider {
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date> {
        self.provider.trading_days(start, end).await
    }

    async fn daily_bars(
        &mut self,
        symbol: StockSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        assert!(start <= end, "查询开始日期不能晚于结束日期");
        if let Some(&(cached_start, cached_end)) = self.data.bar_date.get(&symbol) {
            // 历史预热或更宽的查询仅补拉已下载区间外的部分。
            if start < cached_start {
                let mut bars = self
                    .download(symbol, start, cached_start.previous_day().unwrap())
                    .await;
                let cached = self.data.bars.get_mut(&symbol).unwrap();
                // 新数据全部早于已有区间，直接前接即可保持严格升序。
                bars.append(cached);
                *cached = bars;
                self.data.bar_date.get_mut(&symbol).unwrap().0 = start;
            }
            if end > cached_end {
                let bars = self
                    .download(symbol, cached_end.next_day().unwrap(), end)
                    .await;
                self.data.bars.get_mut(&symbol).unwrap().extend(bars);
                self.data.bar_date.get_mut(&symbol).unwrap().1 = end;
            }
        } else {
            let (start, end) = (self.start.min(start), self.end.max(end));
            let bars = self.download(symbol, start, end).await;
            self.data.bars.insert(symbol, bars);
            self.data.bar_date.insert(symbol, (start, end));
        }
        let bars = &self.data.bars[&symbol];
        let first = bars.partition_point(|bar| bar.date < start);
        let last = bars.partition_point(|bar| bar.date <= end);
        bars[first..last].to_vec()
    }
}
