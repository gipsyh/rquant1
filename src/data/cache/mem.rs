use crate::data::index::normalize_index_symbol;
use crate::data::{
    DataProvider, Index, IndexHistComp, RqData, Stock, StockBar, StockHistBar, StockSymbol,
};
use crate::utils::DateRange;
use std::collections::{BTreeMap, BTreeSet};
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
        let comp = self
            .provider
            .index_comp(symbol, DateRange::new(start, end))
            .await;
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

    async fn download(&mut self, symbol: StockSymbol, start: Date, end: Date) -> StockHistBar {
        let mut bars = self
            .provider
            .stock_bar(symbol, DateRange::new(start, end))
            .await;
        bars.sort_unstable_by_key(|bar| bar.date);
        assert!(
            bars.iter().all(|bar| bar.symbol == symbol),
            "行情股票不匹配: {symbol}"
        );
        StockHistBar::new(DateRange::new(start, end), bars)
            .unwrap_or_else(|err| panic!("行情数据无效: {symbol}: {err:#}"))
    }
}

#[async_trait::async_trait]
impl DataProvider for MemCacheProvider {
    async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
        self.provider.trading_days(range).await
    }

    async fn stock_info(&mut self, symbol: StockSymbol) -> Stock {
        if let Some(stock) = self.data.stock.get(&symbol) {
            return stock.info();
        }
        let info = self.provider.stock_info(symbol).await;
        assert_eq!(info.symbol, symbol, "股票基础信息代码不匹配");
        info.validate()
            .unwrap_or_else(|err| panic!("股票基础信息无效: {symbol}: {err:#}"));
        assert!(info.bars.is_none(), "股票基础信息查询不应返回日线");
        self.data.stock.insert(symbol, info);
        self.data.stock[&symbol].info()
    }

    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> Vec<StockBar> {
        let (start, end) = (range.start(), range.end());
        if let Some(range) = self
            .data
            .stock
            .get(&symbol)
            .and_then(|stock| stock.bars.as_ref().map(StockHistBar::range))
        {
            // 历史预热或更宽的查询仅补拉已下载区间外的部分。
            if start < range.start() {
                let bars = self
                    .download(symbol, start, range.start().previous_day().unwrap())
                    .await;
                self.data
                    .stock
                    .get_mut(&symbol)
                    .unwrap()
                    .bars
                    .as_mut()
                    .unwrap()
                    .extend(bars);
            }
            if end > range.end() {
                let bars = self
                    .download(symbol, range.end().next_day().unwrap(), end)
                    .await;
                self.data
                    .stock
                    .get_mut(&symbol)
                    .unwrap()
                    .bars
                    .as_mut()
                    .unwrap()
                    .extend(bars);
            }
        } else {
            self.stock_info(symbol).await;
            let bars = self
                .download(symbol, self.start.min(start), self.end.max(end))
                .await;
            self.data.stock.get_mut(&symbol).unwrap().bars = Some(bars);
        }
        self.data.stock[&symbol]
            .bars
            .as_ref()
            .unwrap()
            .slice(start, end)
            .unwrap()
            .bars
    }

    async fn stocks_info(&mut self, symbols: &[StockSymbol]) -> Vec<Stock> {
        let missing: Vec<_> = symbols
            .iter()
            .copied()
            .filter(|symbol| !self.data.stock.contains_key(symbol))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !missing.is_empty() {
            let infos = self.provider.stocks_info(&missing).await;
            assert_eq!(infos.len(), missing.len(), "基础信息批量结果数量不匹配");
            for (symbol, info) in missing.into_iter().zip(infos) {
                assert_eq!(info.symbol, symbol, "股票基础信息代码不匹配");
                info.validate().unwrap();
                assert!(info.bars.is_none(), "股票基础信息查询不应返回日线");
                self.data.stock.insert(symbol, info);
            }
        }
        symbols.iter().map(|s| self.data.stock[s].info()).collect()
    }

    async fn stocks_bars(&mut self, requests: &[(StockSymbol, DateRange)]) -> Vec<Vec<StockBar>> {
        // 同一股票的重复/重叠请求先合并，避免并发下载相同区间。
        let mut ranges: BTreeMap<StockSymbol, DateRange> = BTreeMap::new();
        for &(symbol, range) in requests {
            ranges
                .entry(symbol)
                .and_modify(|existing| {
                    existing.set(
                        existing.start().min(range.start()),
                        existing.end().max(range.end()),
                    );
                })
                .or_insert(range);
        }
        self.stocks_info(&ranges.keys().copied().collect::<Vec<_>>())
            .await;
        let mut missing = Vec::new();
        for (&symbol, range) in &ranges {
            let (start, end) = (range.start(), range.end());
            if let Some(hist) = &self.data.stock[&symbol].bars {
                let range = hist.range();
                if start < range.start() {
                    missing.push((
                        symbol,
                        DateRange::new(start, range.start().previous_day().unwrap()),
                    ));
                }
                if end > range.end() {
                    missing.push((symbol, DateRange::new(range.end().next_day().unwrap(), end)));
                }
            } else {
                missing.push((
                    symbol,
                    DateRange::new(self.start.min(start), self.end.max(end)),
                ));
            }
        }
        if !missing.is_empty() {
            let results = self.provider.stocks_bars(&missing).await;
            assert_eq!(results.len(), missing.len(), "日线批量结果数量不匹配");
            for ((symbol, range), mut bars) in missing.into_iter().zip(results) {
                assert!(
                    bars.iter().all(|b| b.symbol == symbol),
                    "行情股票不匹配: {symbol}"
                );
                bars.sort_unstable_by_key(|bar| bar.date);
                let hist = StockHistBar::new(range, bars).unwrap();
                let cached = &mut self.data.stock.get_mut(&symbol).unwrap().bars;
                if let Some(cached) = cached {
                    cached.extend(hist);
                } else {
                    *cached = Some(hist);
                }
            }
        }
        requests
            .iter()
            .map(|&(symbol, range)| {
                let (start, end) = (range.start(), range.end());
                self.data.stock[&symbol]
                    .bars
                    .as_ref()
                    .unwrap()
                    .slice(start, end)
                    .unwrap()
                    .bars
            })
            .collect()
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

    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        let (start, end) = (range.start(), range.end());
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
