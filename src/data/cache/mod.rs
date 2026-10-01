use super::{DataProvider, InstrSymbol, StockDailyBar};
use std::collections::BTreeMap;
use time::Date;

/// 每次回测独立创建；首次访问股票时加载整个配置区间，销毁时释放缓存。
/// 仅缓存日线，交易日历直接转发给底层数据源。
pub struct MemCacheProvider {
    provider: Box<dyn DataProvider>,
    start: Date,
    end: Date,
    bars: BTreeMap<InstrSymbol, CachedBars>,
}

struct CachedBars {
    // 下载覆盖范围包含无日线的日期，避免停牌或空区间重复请求。
    start: Date,
    end: Date,
    bars: BTreeMap<Date, StockDailyBar>,
}

impl MemCacheProvider {
    pub fn new(provider: Box<dyn DataProvider>, start: Date, end: Date) -> Self {
        assert!(start <= end, "缓存开始日期不能晚于结束日期");
        Self {
            provider,
            start,
            end,
            bars: BTreeMap::new(),
        }
    }

    async fn download(
        &mut self,
        symbol: InstrSymbol,
        start: Date,
        end: Date,
    ) -> BTreeMap<Date, StockDailyBar> {
        let loaded = self.provider.daily_bars(symbol, start, end).await;
        let mut bars = BTreeMap::new();
        for bar in loaded {
            assert!(
                bar.symbol == symbol
                    && bar.date >= start
                    && bar.date <= end
                    && bars.insert(bar.date, bar).is_none(),
                "行情数据无效: {symbol} 返回股票不匹配、越界或重复日线"
            );
        }
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
        symbol: InstrSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        assert!(start <= end, "查询开始日期不能晚于结束日期");
        if let Some(cached) = self.bars.get(&symbol) {
            let (cached_start, cached_end) = (cached.start, cached.end);
            // 历史预热或更宽的查询仅补拉已下载区间外的部分。
            if start < cached_start {
                let bars = self
                    .download(symbol, start, cached_start.previous_day().unwrap())
                    .await;
                let cached = self.bars.get_mut(&symbol).unwrap();
                cached.bars.extend(bars);
                cached.start = start;
            }
            if end > cached_end {
                let bars = self
                    .download(symbol, cached_end.next_day().unwrap(), end)
                    .await;
                let cached = self.bars.get_mut(&symbol).unwrap();
                cached.bars.extend(bars);
                cached.end = end;
            }
        } else {
            let (start, end) = (self.start.min(start), self.end.max(end));
            let bars = self.download(symbol, start, end).await;
            self.bars.insert(symbol, CachedBars { start, end, bars });
        }
        self.bars[&symbol]
            .bars
            .range(start..=end)
            .map(|(_, bar)| *bar)
            .collect()
    }
}
