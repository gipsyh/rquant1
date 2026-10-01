use super::{DataProvider, InstrSymbol, MarketData, StockDailyBar};
use anyhow::{Result, anyhow};
use std::collections::{BTreeMap, BTreeSet};
use time::Date;

/// 离线数据源；未知股票或缺失日期返回空行情，行为与在线空结果一致。
pub struct InMemoryProvider {
    days: BTreeSet<Date>,
    bars: BTreeMap<InstrSymbol, BTreeMap<Date, StockDailyBar>>,
}

impl InMemoryProvider {
    pub fn new(data: &[MarketData]) -> Result<Self> {
        let mut days = BTreeSet::new();
        let mut bars = BTreeMap::new();
        for market in data {
            let calendar: BTreeSet<_> = market.trading_days.iter().copied().collect();
            if calendar.len() != market.trading_days.len() || bars.contains_key(&market.symbol) {
                return Err(anyhow!("行情数据无效: 重复股票或交易日期"));
            }
            let mut by_date = BTreeMap::new();
            for bar in &market.bars {
                if bar.symbol != market.symbol
                    || !calendar.contains(&bar.date)
                    || by_date.insert(bar.date, *bar).is_some()
                {
                    return Err(anyhow!("行情数据无效: 股票不匹配、非交易日或重复日线"));
                }
            }
            days.extend(calendar);
            bars.insert(market.symbol, by_date);
        }
        Ok(Self { days, bars })
    }
}

#[async_trait::async_trait]
impl DataProvider for InMemoryProvider {
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date> {
        assert!(start <= end, "开始日期不能晚于结束日期");
        self.days.range(start..=end).copied().collect()
    }

    async fn daily_bars(
        &mut self,
        symbol: InstrSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        assert!(start <= end, "开始日期不能晚于结束日期");
        self.bars
            .get(&symbol)
            .map(|bars| bars.range(start..=end).map(|(_, bar)| *bar).collect())
            .unwrap_or_default()
    }
}
