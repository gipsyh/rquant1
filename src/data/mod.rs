#[cfg(test)]
mod test;
pub mod tushare;

use std::{
    collections::HashMap,
    fmt::{self, Display},
};
use time::Date;

/// Instrument Symbol
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct InstrSymbol {
    id: u32,
    tp: InstrType,
}

impl From<&str> for InstrSymbol {
    /// 解析股票代码，接受两种写法：
    /// - 裸 6 位数字：`"000001"`，由号段推断 [`InstrType`]
    /// - 带交易所后缀：`"000001.XSHE"` 或 `"000001.SZ"`，后缀须与号段推断结果一致
    fn from(value: &str) -> Self {
        let text = value.trim().to_ascii_uppercase();
        let (code, suffix) = match text.split_once('.') {
            Some((code, suffix)) => (code, Some(suffix)),
            None => (text.as_str(), None),
        };

        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            panic!("股票代码须为 6 位数字，实际为 {value:?}");
        }

        // 号段表对应 helpers.py 的 _STOCK_PREFIXES，但只保留本枚举有的四个分类。
        let tp = match &code[..3] {
            // 沪市主板 600/601/603/605
            "600" | "601" | "603" | "605" => InstrType::ShMain,
            // 科创板 688，以及 689 的 CDR
            "688" | "689" => InstrType::ShStar,
            // 深市主板 000/001/002/003（002 原中小板，2021 年并入主板）
            "000" | "001" | "002" | "003" => InstrType::SzMain,
            // 创业板 300/301
            "300" | "301" => InstrType::SzChiNext,
            other => panic!("未知的股票号段 {other:?}（代码 {value:?}）"),
        };

        if let Some(suffix) = suffix {
            // 两位缩写与四位 RQAlpha 后缀都接受，对应 helpers.py 的
            // _EXCHANGE_SUFFIXES：SH ≡ XSHG、SZ ≡ XSHE。
            let accepted: &[&str] = match tp {
                InstrType::ShMain | InstrType::ShStar => &["SH", "XSHG"],
                InstrType::SzMain | InstrType::SzChiNext => &["SZ", "XSHE"],
            };
            if !accepted.contains(&suffix) {
                panic!(
                    "后缀与号段矛盾：{value:?} 属于 {}，却写成 {suffix:?}",
                    accepted.join(" 或 ")
                );
            }
        }

        let id = code
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("6 位数字无法解析成 u32：{value:?}"));

        Self { id, tp }
    }
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
    /// 股票代码
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

/// OHLC 与涨跌停价的实际复权状态。
///
/// 对应 Python `Bar.adjustment` 的 `float | str | None`。换成枚举后，
/// 「字符串只允许 `"pre"`、`"post"`」这条约束由类型系统保证，不再需要运行时校验；
/// 「请求复权但实际未完成时不得标记为已复权」也由构造方式保证。不影响成交量、成交额和市值 —— 它们恒为未复权口径。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Adjustment {
    /// 未复权，数值是当日原始 `adj_factor`（恒 > 0）。`1.0` 也表示未复权。
    Raw(f64),
    /// 已前复权，只记录类型，不携带因子或基准。
    Pre,
    /// 已后复权，只记录类型，不携带因子或基准。
    Post,
}

/// 个股日线。
///
/// 除 `float_market_cap` 外，各价格字段一律用 `f64`：流通市值量级到千万
/// （实测 `circ_mv` 可达 `2.2e7` 元），`f32` 的约 7 位有效数字会被截断。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StockDailyBar {
    /// 股票代码
    pub symbol: InstrSymbol,
    /// 交易日
    pub date: Date,
    /// 开盘价，复权状态见 [`Self::adjustment`]
    pub open: f64,
    /// 最高价，复权状态见 [`Self::adjustment`]
    pub high: f64,
    /// 最低价，复权状态见 [`Self::adjustment`]
    pub low: f64,
    /// 收盘价，复权状态见 [`Self::adjustment`]
    pub close: f64,
    /// 成交量，单位「股」（上游「手」已 ×100）；未复权
    pub volume: f64,
    /// 成交额，单位「元」（上游「千元」已 ×1000）；金额不参与复权
    pub turnover: f64,
    /// 涨停价；接口无有效价格时为 `None`
    /// 来自 Tushare `stk_limit` 的 `up_limit` / `down_limit`，不按比例估算。
    /// 读取时与 OHLC 使用相同倍率复权（保留四位小数），状态统一见
    /// [`Self::adjustment`]。未复权时是原始价格口径；复权后是分析用换算值，
    /// 不是当日实际交易报价。
    pub limit_up: Option<f64>,
    /// 跌停价；接口无有效价格时为 `None`。口径同 [`Self::limit_up`]。
    pub limit_down: Option<f64>,
    /// 流通市值（元）。来自 Tushare `daily_basic.circ_mv`（上游单位万元，已 ×10000）。
    ///
    /// 与 OHLC 不同，它不参与复权，是当日真实口径；数据缺失时为 `None`。
    pub float_market_cap: Option<f64>,
    /// OHLC 与涨跌停价的实际复权状态。
    ///
    /// `None` 表示未复权且无可用因子（如指数、因子缺失或无效）；
    /// 这与 `Some(Adjustment::Raw(_))` 不同，前者是因子不可得，后者是因子可得。
    pub adjustment: Option<Adjustment>,
}

pub trait DataProvider {
    fn stock_basic(stock: InstrSymbol, start: Date, end: Date) -> Stock;

    fn stock_basics(stocks: &[InstrSymbol], start: Date, end: Date) -> HashMap<InstrSymbol, Stock>;
}
