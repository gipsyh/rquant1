mod cache;
pub use cache::{DiskCacheProvider, MemCacheProvider};
mod index;
#[cfg(test)]
mod test;
pub mod tushare;

use crate::utils::DateRange;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::{self, Display},
};
use time::Date;

/// 股票代码
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StockSymbol {
    id: u32,
    board: StockBoard,
}

impl From<&str> for StockSymbol {
    /// 解析股票代码，接受两种写法：
    /// - 裸 6 位数字：`"000001"`，由号段推断 [`StockBoard`]
    /// - 带交易所后缀：`"000001.XSHE"` 或 `"000001.SZ"`，后缀须与号段推断结果一致
    fn from(value: &str) -> Self {
        value.parse().unwrap_or_else(|err| panic!("{err}"))
    }
}

impl std::str::FromStr for StockSymbol {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let text = value.trim().to_ascii_uppercase();
        let (code, suffix) = match text.split_once('.') {
            Some((code, suffix)) => (code, Some(suffix)),
            None => (text.as_str(), None),
        };

        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(anyhow::anyhow!("股票代码须为 6 位数字，实际为 {value:?}"));
        }

        // 号段表对应 helpers.py 的 _STOCK_PREFIXES，但只保留本枚举有的四个分类。
        let board = match &code[..3] {
            // 沪市主板 600/601/603/605
            "600" | "601" | "603" | "605" => StockBoard::ShMain,
            // 科创板 688，以及 689 的 CDR
            "688" | "689" => StockBoard::ShStar,
            // 深市主板 000/001/002/003（002 原中小板，2021 年并入主板）
            "000" | "001" | "002" | "003" => StockBoard::SzMain,
            // 创业板 300/301
            "300" | "301" => StockBoard::SzChiNext,
            other => {
                return Err(anyhow::anyhow!(
                    "未知的股票号段 {other:?}（代码 {value:?}）"
                ));
            }
        };

        if let Some(suffix) = suffix {
            // 两位缩写与四位 RQAlpha 后缀都接受，对应 helpers.py 的
            // _EXCHANGE_SUFFIXES：SH ≡ XSHG、SZ ≡ XSHE。
            let accepted: &[&str] = match board {
                StockBoard::ShMain | StockBoard::ShStar => &["SH", "XSHG"],
                StockBoard::SzMain | StockBoard::SzChiNext => &["SZ", "XSHE"],
            };
            if !accepted.contains(&suffix) {
                return Err(anyhow::anyhow!(
                    "后缀与号段矛盾：{value:?} 属于 {}，却写成 {suffix:?}",
                    accepted.join(" 或 ")
                ));
            }
        }

        let id = code
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("6 位数字无法解析成 u32：{value:?}"));

        Ok(Self { id, board })
    }
}

impl Display for StockSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}.{}", self.id, self.board)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StockBoard {
    /// 上交所主板
    ShMain,
    /// 科创板
    ShStar,
    /// 深交所主板
    SzMain,
    /// 创业板
    SzChiNext,
}

impl Display for StockBoard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShMain | Self::ShStar => write!(f, "XSHG"),
            Self::SzMain | Self::SzChiNext => write!(f, "XSHE"),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Stock {
    /// 股票代码
    pub symbol: StockSymbol,
    /// 股票名称
    pub name: String,
    /// 上市日期
    pub listed: Date,
    /// 退市日
    pub delisted: Option<Date>,
    /// 行业
    pub industry: Option<String>,
}

/// OHLC 与涨跌停价的实际复权状态。
///
/// 对应 Python `Bar.adjustment` 的 `float | str | None`。换成枚举后，
/// 「字符串只允许 `"pre"`、`"post"`」这条约束由类型系统保证，不再需要运行时校验；
/// 「请求复权但实际未完成时不得标记为已复权」也由构造方式保证。不影响成交量、成交额和市值 —— 它们恒为未复权口径。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Adjustment {
    /// 未复权，数值是当日原始 `adj_factor`（恒 > 0）。`1.0` 也表示未复权。
    Raw(f64),
    /// 已前复权，只记录类型，不携带因子或基准。
    Pre,
    /// 已后复权，只记录类型，不携带因子或基准。
    Post,
}

/// 个股日线
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StockDailyBar {
    /// 股票代码
    pub symbol: StockSymbol,
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
    /// 成交量，单位「股」（「手」 ×100）
    pub volume: f64,
    /// 成交额，单位「元」，不参与复权
    pub turnover: f64,
    /// 涨停价；接口无有效价格时为 `None`
    /// 读取时与 OHLC 使用相同倍率复权，状态统一见
    /// [`Self::adjustment`]。未复权时是原始价格口径；
    /// 复权后是分析用换算值，不是当日实际交易报价。
    pub limit_up: Option<f64>,
    /// 跌停价；接口无有效价格时为 `None`。口径同 [`Self::limit_up`]。
    pub limit_down: Option<f64>,
    /// 流通市值（元）。来自 Tushare `daily_basic.circ_mv`（上游单位万元，已 ×10000）。
    /// 与 OHLC 不同，它不参与复权，是当日真实口径；数据缺失时为 `None`。
    pub float_market_cap: Option<f64>,
    /// OHLC 与涨跌停价的实际复权状态。
    /// `None` 表示未复权且无可用因子（如指数、因子缺失或无效）；
    /// 这与 `Some(Adjustment::Raw(_))` 不同，前者是因子不可得，后者是因子可得。
    pub adjustment: Option<Adjustment>,
    /// 该交易日是否处于 ST/*ST 状态，随历史日期变化。
    /// Tushare 数据源按 stock_st 当日名单填充；查询失败会报错。
    pub st: bool,
}

/// 全部已有数据；交易日历暂不存储。
#[derive(Default, Serialize, Deserialize)]
pub struct RqData {
    stock: HashMap<StockSymbol, Stock>,
    /// Bar 数据的日期闭区间，如果bars在这个区间的数据不存在则代表非交易日、停牌、退市等
    stock_bar_date: HashMap<StockSymbol, DateRange>,
    /// 与 bar_date 具有相同的股票键；日线按日期严格升序且位于对应闭区间内。
    /// 已查询但没有日线的区间用空 Vec 表示。
    stock_bars: HashMap<StockSymbol, Vec<StockDailyBar>>,
}

/// 交易日历与股票行情独立查询
#[async_trait::async_trait]
pub trait DataProvider: Send + Sync {
    /// 股票交易日查询
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date>;

    /// 股票日线，返回时按时间排序
    async fn daily_bars(
        &mut self,
        symbol: StockSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar>;
}
