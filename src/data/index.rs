use crate::{data::StockSymbol, utils::DateRange};
use std::{collections::HashMap, sync::Arc};
use time::Date;

pub struct Index {
    /// 指数名称
    pub name: String,
    /// 上市日期
    pub listed: Date,
    /// 退市日
    pub delisted: Option<Date>,
}

pub struct IndexComp {
    weight: HashMap<StockSymbol, f32>,
}
/// Index History Composition
pub struct IndexHistComp {
    /// 指数成分历史覆盖的日期闭区间
    range: DateRange,
    /// 指数成分，按时间排序，离散
    hist: Vec<(Date, Arc<IndexComp>)>,
}

impl IndexHistComp {
    /// 二分查找查询日已生效的最近一期成分，查询范围为 `range.start..=range.end`。
    pub fn composition(&self, date: Date) -> anyhow::Result<Arc<IndexComp>> {
        anyhow::ensure!(
            self.range.contains(date),
            "查询日期 {date} 超出指数成分历史范围 {}..={}",
            self.range.start(),
            self.range.end()
        );

        let pos = self
            .hist
            .partition_point(|(effective_date, _)| *effective_date <= date);
        let index = pos
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("查询日期 {date} 没有已生效的指数成分"))?;
        Ok(Arc::clone(&self.hist[index].1))
    }
}
