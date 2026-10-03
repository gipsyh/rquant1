use crate::data::index::normalize_index_symbol;
use crate::data::{DataProvider, Index, IndexHistComp, RqData, StockBar, StockSymbol};
use crate::utils::DateRange;
use time::Date;

#[cfg(test)]
pub(crate) mod test;

/// 每次回测独立创建；首次访问股票或指数时加载整个配置区间，销毁时释放缓存。
/// 缓存日线和指数成分，交易日历直接转发给底层数据源。
pub struct MemCacheProvider {
    provider: Box<dyn DataProvider>,
    start: Date,
    end: Date,
    pub(super) data: RqData,
}

impl MemCacheProvider {
    async fn download_index(&mut self, symbol: &str, start: Date, end: Date) -> IndexHistComp {
        let comp = self.provider.index_comp(symbol, start, end).await;
        assert_eq!(
            comp.range(),
            DateRange::new(start, end),
            "指数成分覆盖区间不匹配"
        );
        comp.validate()
            .unwrap_or_else(|err| panic!("指数成分无效: {symbol}: {err:#}"));
        comp
    }

    pub fn new(provider: Box<dyn DataProvider>, start: Date, end: Date) -> Self {
        assert!(start <= end, "缓存开始日期不能晚于结束日期");
        Self {
            provider,
            start,
            end,
            data: RqData::default(),
        }
    }

    async fn download(&mut self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        let mut bars = self.provider.stock_bar(symbol, start, end).await;
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

    async fn stock_bar(&mut self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        assert!(start <= end, "查询开始日期不能晚于结束日期");
        if let Some(&range) = self.data.stock_bar_date.get(&symbol) {
            let (cached_start, cached_end) = (range.start(), range.end());
            // 历史预热或更宽的查询仅补拉已下载区间外的部分。
            if start < cached_start {
                let mut bars = self
                    .download(symbol, start, cached_start.previous_day().unwrap())
                    .await;
                let cached = self.data.stock_bars.get_mut(&symbol).unwrap();
                // 新数据全部早于已有区间，直接前接即可保持严格升序。
                bars.append(cached);
                *cached = bars;
                self.data
                    .stock_bar_date
                    .get_mut(&symbol)
                    .unwrap()
                    .set_start(start);
            }
            if end > cached_end {
                let bars = self
                    .download(symbol, cached_end.next_day().unwrap(), end)
                    .await;
                self.data.stock_bars.get_mut(&symbol).unwrap().extend(bars);
                self.data
                    .stock_bar_date
                    .get_mut(&symbol)
                    .unwrap()
                    .set_end(end);
            }
        } else {
            let (start, end) = (self.start.min(start), self.end.max(end));
            let bars = self.download(symbol, start, end).await;
            self.data.stock_bars.insert(symbol, bars);
            self.data
                .stock_bar_date
                .insert(symbol, DateRange::new(start, end));
        }
        let bars = &self.data.stock_bars[&symbol];
        let first = bars.partition_point(|bar| bar.date < start);
        let last = bars.partition_point(|bar| bar.date <= end);
        bars[first..last].to_vec()
    }

    async fn index_name(&mut self, symbol: &str) -> String {
        let symbol = normalize_index_symbol(symbol).unwrap_or_else(|err| panic!("{err:#}"));
        if let Some(index) = self.data.index.get(&symbol) {
            return index.name.clone();
        }
        let name = self.provider.index_name(&symbol).await;
        assert!(!name.trim().is_empty(), "指数名称不能为空: {symbol}");
        name
    }

    async fn index_comp(&mut self, symbol: &str, start: Date, end: Date) -> IndexHistComp {
        assert!(start <= end, "查询开始日期不能晚于结束日期");
        let symbol = normalize_index_symbol(symbol).unwrap_or_else(|err| panic!("{err:#}"));
        if let Some(range) = self.data.index.get(&symbol).map(|index| index.comp.range()) {
            if start < range.start() {
                let comp = self
                    .download_index(&symbol, start, range.start().previous_day().unwrap())
                    .await;
                self.data.index.get_mut(&symbol).unwrap().comp.extend(comp);
            }
            if end > range.end() {
                let comp = self
                    .download_index(&symbol, range.end().next_day().unwrap(), end)
                    .await;
                self.data.index.get_mut(&symbol).unwrap().comp.extend(comp);
            }
        } else {
            let name = self.index_name(&symbol).await;
            let comp = self
                .download_index(&symbol, self.start.min(start), self.end.max(end))
                .await;
            self.data.index.insert(
                symbol.clone(),
                Index {
                    symbol: symbol.clone(),
                    name,
                    comp,
                },
            );
        }
        self.data.index[&symbol].comp.slice(start, end).unwrap()
    }
}
