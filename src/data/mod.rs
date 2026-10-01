use std::fmt::{self, Display};

/// Instrument Symbol
struct InstrSymbol {
    id: u32,
    tp: InstrType,
}

impl Display for InstrSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}.{}", self.id, self.tp)
    }
}

enum InstrType {
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
