//! Read-only offline provider for research. Calendar must come from an existing official-calendar backtest.
use rquant::{
    data::{DataProvider, Index, IndexHistComp, Stock, StockHistBar, StockSymbol},
    utils::DateRange,
};
use serde::Deserialize;
use std::{collections::HashMap, path::Path};
use time::Date;

#[derive(Deserialize)]
pub struct CachedProvider {
    #[serde(skip)]
    pub calendar: Vec<Date>,
    pub stock: HashMap<StockSymbol, Stock>,
    pub index: HashMap<String, Index>,
}
impl CachedProvider {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        let (data, count) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
        anyhow::ensure!(count == bytes.len(), "Trailing cache bytes");
        Ok(data)
    }
}
#[async_trait::async_trait]
impl DataProvider for CachedProvider {
    async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
        self.calendar
            .iter()
            .copied()
            .filter(|&date| range.contains(date))
            .collect()
    }
    async fn stock_info(&mut self, symbol: StockSymbol) -> Stock {
        let mut s = self.stock[&symbol].clone();
        s.bars = None;
        s
    }
    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
        self.stock[&symbol]
            .bars
            .as_ref()
            .expect("Missing cached bars")
            .slice(range)
            .expect("Cache does not cover requested dates")
    }
    async fn is_tradable(&mut self, symbol: StockSymbol, date: Date) -> bool {
        self.stock[&symbol].bars.as_ref().unwrap().is_tradable(date)
    }
    async fn index_name(&mut self, symbol: &str) -> String {
        self.index[symbol].name.clone()
    }
    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        self.index[symbol]
            .comp
            .slice(range)
            .expect("Missing cached index dates")
    }
}
