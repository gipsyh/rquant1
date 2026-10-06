mod cache;
pub use cache::{DiskCacheProvider, MemCacheProvider};
mod index;
pub use index::{Index, IndexComp, IndexHistComp};
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// None 表示未查询日线；Some 即已覆盖对应区间，日线数组允许为空。
    pub bars: Option<StockHistBar>,
}

impl Stock {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty(), "股票名称不能为空");
        anyhow::ensure!(
            self.delisted.is_none_or(|date| date >= self.listed),
            "退市日期不能早于上市日期"
        );
        if let Some(bars) = &self.bars {
            bars.validate()?;
            anyhow::ensure!(
                bars.bars().iter().all(|bar| bar.symbol == self.symbol),
                "股票与日线代码不匹配"
            );
        }
        Ok(())
    }

    /// 仅复制基础信息，日线历史不随基础信息查询返回。
    pub(crate) fn info(&self) -> Self {
        Self {
            symbol: self.symbol,
            name: self.name.clone(),
            listed: self.listed,
            delisted: self.delisted,
            industry: self.industry.clone(),
            bars: None,
        }
    }
}

/// OHLC 与涨跌停价的实际复权状态。
///
/// 仅描述价格口径；成交量、成交额和市值始终保持原始口径。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Adjustment {
    /// 未复权，数值是当日原始 `adj_factor`（恒 > 0）。`1.0` 也表示未复权。
    Raw(f64),
    /// 已前复权，只记录类型，不携带因子或基准。
    Pre,
    /// 已后复权，只记录类型，不携带因子或基准。
    Post,
    /// 原始价格直接乘当日 `adj_factor`，不按窗口首尾因子归一化。
    /// 策略指标统一使用此口径；不可作为实际交易报价。
    FactorAdjusted,
}

/// 个股日线
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StockBar {
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
    /// 成交量，单位「股」（「手」 ×100）；有效日线必须为有限正数。
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

impl StockBar {
    /// 原始价格乘当日因子，返回策略指标用的 `FactorAdjusted` 日线。
    /// OHLC 与涨跌停价使用相同倍率，成交量、成交额、市值等字段保持不变。
    /// 仅依赖本根日线；缺失有效因子、价格无效或已复权时 panic。
    #[must_use]
    pub fn adjusted(self) -> Self {
        let factor = match self.adjustment {
            Some(Adjustment::Raw(f)) if f.is_finite() && f > 0.0 => f,
            _ => panic!(
                "指标计算需要有效的原始复权因子: {} {}",
                self.symbol, self.date
            ),
        };
        let adjust = |price: f64| {
            assert!(price.is_finite() && price > 0.0, "价格无效");
            let price = price * factor;
            assert!(price.is_finite() && price > 0.0, "复权价格计算溢出或下溢");
            price
        };
        Self {
            open: adjust(self.open),
            high: adjust(self.high),
            low: adjust(self.low),
            close: adjust(self.close),
            limit_up: self.limit_up.map(adjust),
            limit_down: self.limit_down.map(adjust),
            adjustment: Some(Adjustment::FactorAdjusted),
            ..self
        }
    }
}

/// 单只股票的日线历史，包含覆盖闭区间内全部可用日线。
/// 区间内没有日线的日期表示已查询但无行情，如非交易日、停牌或退市。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StockHistBar {
    range: DateRange,
    /// 同一股票，日期严格升序且位于覆盖区间内；允许为空。
    bars: Vec<StockBar>,
}

impl StockHistBar {
    pub fn new(range: DateRange, bars: Vec<StockBar>) -> anyhow::Result<Self> {
        let hist = Self { range, bars };
        hist.validate()?;
        Ok(hist)
    }

    pub fn range(&self) -> DateRange {
        self.range
    }

    pub fn bars(&self) -> &[StockBar] {
        &self.bars
    }

    /// 已覆盖日期有日线且非 ST 才可交易；无日线返回 false，区间外查询 panic。
    /// 仅用于策略筛选，不判断成交量、涨跌停或订单能否成交。
    pub fn is_tradable(&self, date: Date) -> bool {
        assert!(
            self.range.contains(date),
            "可交易状态查询日期超出已覆盖范围"
        );
        self.bars
            .binary_search_by_key(&date, |bar| bar.date)
            .is_ok_and(|index| !self.bars[index].st)
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.bars.iter().all(|bar| self.range.contains(bar.date))
                && self.bars.windows(2).all(|pair| {
                    pair[0].date < pair[1].date && pair[0].symbol == pair[1].symbol
                }),
            "股票日线必须属于同一股票、日期严格升序且位于覆盖区间内"
        );
        assert!(
            self.bars
                .iter()
                .all(|bar| bar.volume.is_finite() && bar.volume > 0.0),
            "日线成交量必须为有限正数"
        );
        Ok(())
    }

    /// 截取已覆盖的闭区间；无行情时仍保留查询区间。
    pub fn slice(&self, start: Date, end: Date) -> anyhow::Result<Self> {
        anyhow::ensure!(
            start <= end && self.range.contains(start) && self.range.contains(end),
            "股票日线查询区间超出已覆盖范围"
        );
        let first = self.bars.partition_point(|bar| bar.date < start);
        let last = self.bars.partition_point(|bar| bar.date <= end);
        Self::new(DateRange::new(start, end), self.bars[first..last].to_vec())
    }

    /// 合并相邻的已查询区间，空行情区间也会扩大覆盖范围。
    pub(crate) fn extend(&mut self, other: Self) {
        let left = other.range.end().next_day() == Some(self.range.start());
        let right = self.range.end().next_day() == Some(other.range.start());
        assert!(left || right, "只能合并相邻的股票日线区间");
        if let (Some(a), Some(b)) = (self.bars.first(), other.bars.first()) {
            assert_eq!(a.symbol, b.symbol, "不能合并不同股票的日线");
        }
        if left {
            let mut bars = other.bars;
            bars.append(&mut self.bars);
            self.bars = bars;
            self.range.set_start(other.range.start());
        } else {
            self.bars.extend(other.bars);
            self.range.set_end(other.range.end());
        }
    }
}

/// 全部已有数据；交易日历暂不存储。
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RqData {
    /// 股票基础信息及其日线历史。
    stock: HashMap<StockSymbol, Stock>,
    /// 指数
    index: HashMap<String, Index>,
}

/// 交易日历与股票行情独立查询
#[async_trait::async_trait]
pub trait DataProvider: Send + Sync {
    /// 股票交易日查询
    async fn trading_days(&mut self, range: DateRange) -> Vec<Date>;

    /// 查询股票基础信息，返回 Stock 的 bars 为 None；数据无效或查询失败时 panic。
    async fn stock_info(&mut self, symbol: StockSymbol) -> Stock;

    /// 批量基础信息，返回顺序与输入一致；每个 Stock 的 bars 为 None。
    async fn stocks_info(&mut self, symbols: &[StockSymbol]) -> Vec<Stock> {
        let mut results = Vec::with_capacity(symbols.len());
        for &symbol in symbols {
            results.push(self.stock_info(symbol).await);
        }
        results
    }

    /// 股票日线，返回时按时间排序
    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> Vec<StockBar>;

    /// 批量日线请求，每项为（股票、日期闭区间）。
    /// 返回数组与请求逐项对应，空行情保留空 Vec；默认串行，数据源可覆盖为并发。
    async fn stocks_bars(&mut self, requests: &[(StockSymbol, DateRange)]) -> Vec<Vec<StockBar>> {
        let mut results = Vec::with_capacity(requests.len());
        for &(symbol, range) in requests {
            results.push(self.stock_bar(symbol, range).await);
        }
        results
    }

    /// 查询指定日期是否有日线且非 ST；通过日线查询复用缓存及区间补拉。
    /// 查询失败或返回数据范围、股票代码无效时 panic，不将失败视为无行情。
    async fn is_tradable(&mut self, symbol: StockSymbol, date: Date) -> bool {
        let bars = self.stock_bar(symbol, DateRange::new(date, date)).await;
        assert!(
            bars.iter().all(|bar| bar.symbol == symbol),
            "行情股票不匹配: {symbol}"
        );
        StockHistBar::new(DateRange::new(date, date), bars)
            .unwrap()
            .is_tradable(date)
    }

    /// 查询指数名称，必须非空；查询失败或指数不存在时 panic。
    async fn index_name(&mut self, symbol: &str) -> String;

    /// 查询指数在闭区间内的成分历史；可包含起点前已生效的基准快照。
    /// 返回范围为 `range`，通过 `composition(date)` 取当天成分。
    /// 没有快照时返回空历史，当天查询会报错；数据源失败沿用日线接口的 panic 约定。
    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp;
}
