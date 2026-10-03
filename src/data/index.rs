use crate::{data::StockSymbol, utils::DateRange};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use time::Date;

/// 指数与股票的 000 号段含义不同，独立规范化为完整交易所后缀。
pub(crate) fn normalize_index_symbol(symbol: &str) -> anyhow::Result<String> {
    let text = symbol.trim().to_ascii_uppercase();
    let (code, suffix) = text.split_once('.').unwrap_or((&text, ""));
    anyhow::ensure!(
        code.len() == 6 && code.bytes().all(|c| c.is_ascii_digit()),
        "无效指数代码 {symbol:?}"
    );
    let suffix = match suffix {
        "SH" | "XSHG" => "XSHG",
        "SZ" | "XSHE" => "XSHE",
        "CSI" | "INDX" => "INDX",
        "" if code.starts_with("000") => "XSHG",
        "" if code.starts_with("399") => "XSHE",
        "" if code.starts_with("93") => "INDX",
        _ => anyhow::bail!("无法识别指数代码后缀 {symbol:?}"),
    };
    Ok(format!("{code}.{suffix}"))
}

#[derive(Serialize, Deserialize)]
pub struct Index {
    /// 指数代码
    pub symbol: String,
    /// 指数名称，创建指数时必须已获取，不允许为空。
    pub name: String,
    /// 指数历史成分
    pub comp: IndexHistComp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexComp {
    /// 权重比例，0.005 表示 0.5%；保留零权重，不重新归一化。
    weight: HashMap<StockSymbol, f32>,
}

impl IndexComp {
    pub fn new(weight: HashMap<StockSymbol, f32>) -> anyhow::Result<Self> {
        let comp = Self { weight };
        comp.validate()?;
        Ok(comp)
    }

    pub fn weights(&self) -> &HashMap<StockSymbol, f32> {
        &self.weight
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.weight
                .values()
                .all(|weight| (0.0..=1.0).contains(weight)),
            "指数权重必须为 [0, 1] 比例"
        );
        Ok(())
    }
}
/// Index History Composition
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexHistComp {
    /// 指数成分历史覆盖的日期闭区间
    range: DateRange,
    /// 指数成分，生效日严格升序；可包含起点以前的快照供向前查询。
    hist: Vec<(Date, Arc<IndexComp>)>,
}

impl IndexHistComp {
    pub fn new(range: DateRange, hist: Vec<(Date, Arc<IndexComp>)>) -> anyhow::Result<Self> {
        let comp = Self { range, hist };
        comp.validate()?;
        Ok(comp)
    }

    pub fn range(&self) -> DateRange {
        self.range
    }

    pub fn snapshots(&self) -> &[(Date, Arc<IndexComp>)] {
        &self.hist
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.hist.iter().all(|(date, _)| *date <= self.range.end())
                && self.hist.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "指数成分生效日必须严格升序且不晚于区间终点"
        );
        for (_, comp) in &self.hist {
            comp.validate()?;
        }
        Ok(())
    }

    /// 截取已覆盖区间，同时保留起点已生效的最近一期快照。
    pub fn slice(&self, start: Date, end: Date) -> anyhow::Result<Self> {
        anyhow::ensure!(
            start <= end && self.range.contains(start) && self.range.contains(end),
            "指数成分查询区间超出已覆盖范围"
        );
        let first = self
            .hist
            .partition_point(|(date, _)| *date <= start)
            .saturating_sub(1);
        let last = self.hist.partition_point(|(date, _)| *date <= end);
        Self::new(DateRange::new(start, end), self.hist[first..last].to_vec())
    }

    /// 合并相邻区间；补拉附带的历史基准快照不覆盖已有区间内的数据。
    pub(crate) fn extend(&mut self, other: Self) {
        let left = other.range.end().next_day() == Some(self.range.start());
        let right = self.range.end().next_day() == Some(other.range.start());
        assert!(left || right, "只能合并相邻的指数成分区间");
        if left {
            let mut hist = other.hist;
            hist.extend(
                self.hist
                    .iter()
                    .filter(|(date, _)| *date >= self.range.start())
                    .cloned(),
            );
            self.hist = hist;
            self.range.set_start(other.range.start());
        } else {
            self.hist.extend(
                other
                    .hist
                    .into_iter()
                    .filter(|(date, _)| *date > self.range.end()),
            );
            self.range.set_end(other.range.end());
        }
    }

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

#[cfg(test)]
mod test {
    use super::*;
    use time::macros::date;

    #[test]
    fn composition_never_uses_future_snapshots() {
        let start = date!(2024 - 01 - 01);
        let effective = date!(2024 - 01 - 10);
        let end = date!(2024 - 01 - 31);
        let comp = Arc::new(IndexComp::new([(StockSymbol::from("600000"), 0.0)].into()).unwrap());
        let hist = IndexHistComp::new(DateRange::new(start, end), vec![(effective, comp.clone())])
            .unwrap();
        assert!(hist.composition(start).is_err());
        assert!(hist.slice(start, start).unwrap().snapshots().is_empty());
        assert!(hist.composition(start.previous_day().unwrap()).is_err());
        assert!(hist.composition(end.next_day().unwrap()).is_err());
        assert!(Arc::ptr_eq(&comp, &hist.composition(effective).unwrap()));
        assert!(Arc::ptr_eq(&comp, &hist.composition(end).unwrap()));
    }

    #[test]
    fn rejects_invalid_weights_and_snapshot_dates() {
        let stock = StockSymbol::from("600000");
        for weight in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert!(IndexComp::new([(stock, weight)].into()).is_err());
        }
        let start = date!(2024 - 01 - 01);
        let end = date!(2024 - 01 - 31);
        let comp = Arc::new(IndexComp::new([(stock, 1.0)].into()).unwrap());
        for dates in [
            [start, start],
            [end, start],
            [start, end.next_day().unwrap()],
        ] {
            assert!(
                IndexHistComp::new(
                    DateRange::new(start, end),
                    dates.into_iter().map(|date| (date, comp.clone())).collect()
                )
                .is_err()
            );
        }
    }
}
