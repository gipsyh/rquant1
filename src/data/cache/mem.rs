use crate::data::index::normalize_index_symbol;
use crate::data::{DataProvider, Index, IndexHistComp, RqData, Stock, StockHistBar, StockSymbol};
use crate::utils::DateRange;
use std::collections::{BTreeMap, BTreeSet};
use time::Date;

#[cfg(test)]
pub(crate) mod test;

/// 每次回测独立创建；首次访问股票或指数时加载整个配置区间，销毁时释放缓存。
/// 缓存日线和指数成分，交易日历直接转发给底层数据源。
pub struct MemCacheProvider {
    provider: Box<dyn DataProvider>,
    /// 首次访问股票或指数时预取的日期闭区间。
    range: DateRange,
    pub(super) data: RqData,
}

impl MemCacheProvider {
    async fn download_index(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        let comp = self.provider.index_comp(symbol, range).await;
        assert_eq!(comp.range(), range, "指数成分覆盖区间不匹配");
        comp.validate()
            .unwrap_or_else(|err| panic!("指数成分无效: {symbol}: {err:#}"));
        comp
    }

    pub fn new(provider: Box<dyn DataProvider>, range: DateRange) -> Self {
        Self {
            provider,
            range,
            data: RqData::default(),
        }
    }

    async fn download(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
        let hist = self.provider.stock_bar(symbol, range).await;
        assert_eq!(hist.range(), range, "日线历史覆盖区间不匹配");
        hist.validate()
            .unwrap_or_else(|err| panic!("行情数据无效: {symbol}: {err:#}"));
        assert!(
            hist.bars().iter().all(|bar| bar.symbol == symbol),
            "行情股票不匹配: {symbol}"
        );
        hist
    }
}

#[async_trait::async_trait]
impl DataProvider for MemCacheProvider {
    async fn is_tradable(&mut self, symbol: StockSymbol, date: Date) -> bool {
        let covered = self
            .data
            .stock
            .get(&symbol)
            .and_then(|stock| stock.bars.as_ref())
            .is_some_and(|hist| hist.range().contains(date));
        if !covered {
            // 复用首次预取和两端补拉；已覆盖但没有 bar 的日期不重复下载。
            self.stock_bar(symbol, DateRange::new(date, date)).await;
        }
        self.data.stock[&symbol]
            .bars
            .as_ref()
            .unwrap()
            .is_tradable(date)
    }

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

    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
        if let Some(cached) = self
            .data
            .stock
            .get(&symbol)
            .and_then(|stock| stock.bars.as_ref().map(StockHistBar::range))
        {
            // 历史预热或更宽的查询仅补拉已下载区间外的部分。
            if range.start() < cached.start() {
                let gap = DateRange::new(range.start(), cached.start().previous_day().unwrap());
                let bars = self.download(symbol, gap).await;
                self.data
                    .stock
                    .get_mut(&symbol)
                    .unwrap()
                    .bars
                    .as_mut()
                    .unwrap()
                    .extend(bars);
            }
            if range.end() > cached.end() {
                let gap = DateRange::new(cached.end().next_day().unwrap(), range.end());
                let bars = self.download(symbol, gap).await;
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
            let bars = self.download(symbol, self.range.union(range)).await;
            self.data.stock.get_mut(&symbol).unwrap().bars = Some(bars);
        }
        self.data.stock[&symbol]
            .bars
            .as_ref()
            .unwrap()
            .slice(range)
            .unwrap()
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

    async fn stocks_bar(&mut self, requests: &[(StockSymbol, DateRange)]) -> Vec<StockHistBar> {
        // 同一股票的重复/重叠请求先合并，避免并发下载相同区间。
        let mut ranges: BTreeMap<StockSymbol, DateRange> = BTreeMap::new();
        for &(symbol, range) in requests {
            ranges
                .entry(symbol)
                .and_modify(|existing| *existing = existing.union(range))
                .or_insert(range);
        }
        self.stocks_info(&ranges.keys().copied().collect::<Vec<_>>())
            .await;
        let mut missing = Vec::new();
        for (&symbol, range) in &ranges {
            if let Some(cached) = &self.data.stock[&symbol].bars {
                let cached = cached.range();
                if range.start() < cached.start() {
                    missing.push((
                        symbol,
                        DateRange::new(range.start(), cached.start().previous_day().unwrap()),
                    ));
                }
                if range.end() > cached.end() {
                    missing.push((
                        symbol,
                        DateRange::new(cached.end().next_day().unwrap(), range.end()),
                    ));
                }
            } else {
                missing.push((symbol, self.range.union(*range)));
            }
        }
        if !missing.is_empty() {
            let results = self.provider.stocks_bar(&missing).await;
            assert_eq!(results.len(), missing.len(), "日线批量结果数量不匹配");
            for ((symbol, range), hist) in missing.into_iter().zip(results) {
                assert_eq!(hist.range(), range, "日线历史覆盖区间不匹配");
                hist.validate().unwrap();
                assert!(
                    hist.bars().iter().all(|bar| bar.symbol == symbol),
                    "行情股票不匹配: {symbol}"
                );
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
                self.data.stock[&symbol]
                    .bars
                    .as_ref()
                    .unwrap()
                    .slice(range)
                    .unwrap()
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
        let symbol = normalize_index_symbol(symbol).unwrap_or_else(|err| panic!("{err:#}"));
        if let Some(cached) = self.data.index.get(&symbol).map(|index| index.comp.range()) {
            if range.start() < cached.start() {
                let gap = DateRange::new(range.start(), cached.start().previous_day().unwrap());
                let comp = self.download_index(&symbol, gap).await;
                self.data.index.get_mut(&symbol).unwrap().comp.extend(comp);
            }
            if range.end() > cached.end() {
                let gap = DateRange::new(cached.end().next_day().unwrap(), range.end());
                let comp = self.download_index(&symbol, gap).await;
                self.data.index.get_mut(&symbol).unwrap().comp.extend(comp);
            }
        } else {
            let name = self.index_name(&symbol).await;
            let comp = self.download_index(&symbol, self.range.union(range)).await;
            self.data.index.insert(
                symbol.clone(),
                Index {
                    symbol: symbol.clone(),
                    name,
                    comp,
                },
            );
        }
        self.data.index[&symbol].comp.slice(range).unwrap()
    }
}
