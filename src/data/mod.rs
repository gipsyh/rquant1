pub mod tushare;

use std::fmt::{self, Display};
use time::Date;

/// Instrument Symbol
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct InstrSymbol {
    pub id: u32,
    pub tp: InstrType,
}

impl Display for InstrSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}.{}", self.id, self.tp)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum InstrType {
    /// 上交所主板
    ShMain,
    /// 科创板
    ShStar,
    /// 深交所主板
    SzMain,
    /// 创业板
    SzChiNext,
}

impl Display for InstrType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShMain | Self::ShStar => write!(f, "XSHG"),
            Self::SzMain | Self::SzChiNext => write!(f, "XSHE"),
        }
    }
}

pub struct Stock {
    symbol: InstrSymbol,
    /// 股票名称
    name: String,
    /// 上市日期
    listed: Date,
    /// 退市日
    delisted: Option<Date>,
    /// 行业
    industry: Option<String>,
}

pub trait DataProvider {}
