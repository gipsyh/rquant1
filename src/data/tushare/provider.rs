//! Tushare 数据源：HTTP 接口调用、重试与日线数据组装。

use super::table::Table;
use crate::data::{Adjustment, DataProvider, StockDailyBar, StockSymbol};
use crate::utils::parse_date;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use time::{Date, Month, macros::format_description};
use tokio::sync::Semaphore;

/// 与 Python `client.py:20` 的 `__http_url` 同址，但改用 HTTPS。
///
/// Python 硬编码 `http://api.waditu.com/dataapi`，token 会明文过网；
/// 实测该主机支持 TLS，因此默认升级。`https://api.tushare.pro` 同样可用，
/// 可用 [`TushareProvider::with_base_url`] 切换。
pub const DEFAULT_BASE_URL: &str = "https://api.waditu.com/dataapi";

/// 对齐 Python `DataApi.__init__` 的 `timeout=30`。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub const DEFAULT_TOKEN: &str = "5154467945318d04b93f4e49e19fa24efcccedae10c8802936aadf47";

/// 请求参数，对应 Python `DataApi.query(**kwargs)` 里的 kwargs。
pub type Params = Map<String, Value>;

/// 链式构造 [`Params`]。
pub trait ParamsExt {
    /// 插入一个参数。值支持 `&str` / `String` / 整数 / 浮点 / 布尔，
    /// 以及 `Option<T>`（`None` 会被序列化成 JSON null）。
    fn with(self, key: &str, value: impl Into<Value>) -> Self;
}

impl ParamsExt for Params {
    fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.insert(key.to_string(), value.into());
        self
    }
}

/// 构造 [`Params`]，对标 Python 的关键字参数。
///
/// ```
/// use rquant::data::tushare::params;
/// let p = params! { "ts_code" => "000001.SZ", "start_date" => "20240101" };
/// assert_eq!(p["ts_code"], "000001.SZ");
/// ```
///
/// 值支持 `&str` / `String` / 整数 / 浮点 / 布尔 / `Option<T>`。
///
/// `#[macro_export]` 会把宏放在 crate 根（`rquant::params!`）；
/// [`super`] 里额外重导出，因此 `rquant::data::tushare::params!` 也可用。
#[macro_export]
macro_rules! params {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut params = $crate::data::tushare::Params::new();
        $(
            params.insert(($key).to_string(), ::serde_json::Value::from($value));
        )*
        params
    }};
}

/// 重试与并发策略。
///
/// Python SDK 完全没有这两样东西。tushare 有按分钟的频率限制，裸客户端在批量拉取时
/// 容易连环撞墙，因此这里默认开启；[`RetryPolicy::none`] 可退回 Python 的行为。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 首次失败后的最大重试次数（总请求数 = max_retries + 1）。
    pub max_retries: u32,
    /// 指数退避的基准延时，第 n 次重试等待 `base_delay * 2^n`。
    pub base_delay: Duration,
    /// 同时在途的请求数上限。
    pub max_concurrency: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_concurrency: 8,
        }
    }
}

impl RetryPolicy {
    /// 不重试、不限并发 —— 与 Python SDK 的行为一致。
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::ZERO,
            max_concurrency: Semaphore::MAX_PERMITS,
        }
    }
}

/// Tushare 数据源，提供具名 API、通用 query，并实现 DataProvider。
///
/// 数据请求为异步调用，runtime 由调用方提供（本 crate 不创建 runtime）。
///
/// ```no_run
/// # async fn demo() -> anyhow::Result<()> {
/// use rquant::data::tushare::{TushareProvider, params};
///
/// let client = TushareProvider::new();
/// let table = client
///     .daily(params! { "ts_code" => "000001.SZ", "start_date" => "20240101" },
///            "ts_code,trade_date,close")
///     .await?;
/// println!("{} 行", table.len());
/// # Ok(())
/// # }
/// ```
pub struct TushareProvider {
    token: String,
    base_url: String,
    http: reqwest::Client,
    retry: RetryPolicy,
    gate: Arc<Semaphore>,
}

impl std::fmt::Debug for TushareProvider {
    /// 手写实现，避免把 token 打进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TushareProvider")
            .field("base_url", &self.base_url)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Default for TushareProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TushareProvider {
    pub fn new() -> Self {
        Self::build(RetryPolicy::default())
    }

    fn build(retry: RetryPolicy) -> Self {
        let permits = retry.max_concurrency.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            token: DEFAULT_TOKEN.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: http_client(DEFAULT_TIMEOUT),
            retry,
            gate: Arc::new(Semaphore::new(permits)),
        }
    }

    /// 切换服务地址。传 `https://api.tushare.pro` 可走官方域名。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = http_client(timeout);
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.gate = Arc::new(Semaphore::new(
            retry.max_concurrency.clamp(1, Semaphore::MAX_PERMITS),
        ));
        self.retry = retry;
        self
    }

    /// 服务地址（不含 api_name 路径段）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 调用任意 tushare 接口 —— 对应 Python `DataApi.query`。
    ///
    /// `fields` 传空串表示用服务端默认的全字段，对齐 Python 的 `fields=''`。
    ///
    /// 未提供具名方法的接口走这里，这正是 Python `__getattr__`
    /// 提供的逃生通道。
    pub async fn query(&self, api_name: &str, params: Params, fields: &str) -> Result<Table> {
        let mut params = params;
        // 对齐 client.py:34 —— Python 每次请求都会把 base_url 作为 ts_type_name 塞进 params。
        params
            .entry("ts_type_name".to_string())
            .or_insert_with(|| Value::String(self.base_url.clone()));

        let body = json!({
            "api_name": api_name,
            "token": self.token,
            "params": Value::Object(params),
            "fields": fields,
        });
        let url = format!("{}/{}", self.base_url, api_name);

        let mut attempt: u32 = 0;
        loop {
            let permit = self
                .gate
                .clone()
                .acquire_owned()
                .await
                .context("并发信号量已关闭")?;

            let outcome = self.send(&url, &body, api_name).await;
            // 先放行再退避，避免重试的等待时间占用并发额度。
            drop(permit);

            let (err, retryable) = match outcome {
                Ok(parsed) if parsed.code == 0 => {
                    let data = parsed.data.with_context(|| {
                        format!("响应结构异常: [{api_name}] code=0 但响应缺少 data 字段")
                    })?;
                    return Table::new(data.fields, data.items);
                }
                Ok(parsed) => {
                    let retryable = is_rate_limited(&parsed.msg);
                    let err = anyhow!(
                        "tushare [{api_name}] 返回 code={}: {}",
                        parsed.code,
                        parsed.msg
                    );
                    (err, retryable)
                }
                Err(err) => {
                    let retryable = is_retryable(&err);
                    (err, retryable)
                }
            };
            if attempt >= self.retry.max_retries || !retryable {
                return Err(err);
            }
            let delay = self
                .retry
                .base_delay
                .saturating_mul(2u32.saturating_pow(attempt));
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// 单次 HTTP 请求及响应解析；API 状态和重试由 query 处理。
    async fn send(&self, url: &str, body: &Value, api_name: &str) -> Result<Response> {
        let response = self
            .http
            .post(url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("HTTP 请求失败: [{api_name}]"))?
            .error_for_status()
            .with_context(|| format!("HTTP 请求失败: [{api_name}]"))?;
        let text = response
            .text()
            .await
            .with_context(|| format!("读取 HTTP 响应失败: [{api_name}]"))?;
        serde_json::from_str(&text).with_context(|| {
            format!(
                "解析响应 JSON 失败: [{api_name}]（原文前 200 字符: {}）",
                snippet(&text)
            )
        })
    }
}

// 生成具名接口方法，等价于 Python 客户端 __getattr__ 的动态接口调用。
macro_rules! tushare_apis {
    ($($(#[$attr:meta])* $name:ident),* $(,)?) => {
        impl TushareProvider {
            $(
                $(#[$attr])*
                ///
                /// 与 [`TushareProvider::query`] 等价，只是固定了 `api_name`。
                pub async fn $name(
                    &self,
                    params: Params,
                    fields: &str,
                ) -> Result<Table> {
                    self.query(stringify!($name), params, fields).await
                }
            )*
        }
    };
}

tushare_apis! {
    /// 日线行情（股票）。
    daily,
    /// 每日指标：流通市值、换手率、估值等。
    daily_basic,
    /// 复权因子。
    adj_factor,
    /// 每日涨跌停价格。
    stk_limit,
    /// 股票基础信息。
    stock_basic,
    /// 历史每日 ST/*ST 股票列表。
    stock_st,
    /// 股票曾用名。
    namechange,
    /// 每日停复牌信息。
    suspend_d,
    /// 交易日历。
    trade_cal,
    /// 指数基础信息。
    index_basic,
    /// 指数日线行情。
    index_daily,
    /// 指数成分和权重。
    index_weight,
}

/// 服务端没有稳定的限流错误码，保留按文案识别的启发式规则。
pub(super) fn is_rate_limited(msg: &str) -> bool {
    const NEEDLES: [&str; 6] = ["每分钟", "频率", "超限", "抱歉", "访问过快", "too many"];
    NEEDLES.iter().any(|needle| msg.contains(needle))
}

/// anyhow 保留底层 reqwest 错误，可按状态码和连接状态决定是否重试。
pub(super) fn is_retryable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<reqwest::Error>()
        .is_some_and(|err| match err.status() {
            Some(status) => status.is_server_error(),
            None => err.is_timeout() || err.is_connect(),
        })
}

/// 按字符截断，保留非 JSON 响应片段，避免切断 UTF-8。
fn snippet(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    if text.chars().count() <= MAX_CHARS {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX_CHARS).collect::<String>())
    }
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("构造 reqwest::Client 失败")
}

/// 线格式响应。`has_more` / `count` 与 Python SDK 一样忽略 —— 翻页由调用方
/// 通过 `limit` / `offset` 参数控制。
#[derive(Debug, Deserialize)]
struct Response {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<ResponseData>,
}

#[derive(Debug, Deserialize)]
struct ResponseData {
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    items: Vec<Vec<Value>>,
}

fn api_date(date: Date) -> String {
    date.format(format_description!("[year][month][day]"))
        .expect("date format")
}

#[derive(Deserialize)]
struct Daily {
    ts_code: String,
    trade_date: String,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    vol: f64,
    amount: f64,
}

#[derive(Deserialize)]
struct Factor {
    ts_code: String,
    trade_date: String,
    adj_factor: f64,
}

#[derive(Deserialize)]
struct Limit {
    ts_code: String,
    trade_date: String,
    up_limit: Option<f64>,
    down_limit: Option<f64>,
}

#[derive(Deserialize)]
struct Calendar {
    cal_date: String,
    is_open: i64,
}

#[derive(Deserialize)]
struct StStatus {
    ts_code: String,
    trade_date: String,
}

fn insert_unique<T>(map: &mut BTreeMap<Date, T>, date: Date, value: T) {
    assert!(map.insert(date, value).is_none(), "重复日期 {date}");
}

fn check_row(code: &str, expected: &str, date: Date, start: Date, end: Date) {
    assert!(
        code == expected && date >= start && date <= end,
        "接口返回了请求范围外的数据 {code} {date}"
    );
}

#[async_trait::async_trait]
impl DataProvider for TushareProvider {
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date> {
        assert!(start <= end, "开始日期不能晚于结束日期");
        let mut calendar = BTreeMap::new();
        let mut cursor = start;
        loop {
            let chunk_end = end.min(
                Date::from_calendar_date(cursor.year(), Month::December, 31)
                    .expect("无法构造年末日期"),
            );
            let days = self
                .trade_cal(
                    params! {
                        "exchange" => "SSE", "start_date" => api_date(cursor),
                        "end_date" => api_date(chunk_end), "is_open" => "1",
                    },
                    "cal_date,is_open",
                )
                .await
                .unwrap_or_else(|err| panic!("{err:#}"))
                .to_typed::<Calendar>()
                .unwrap_or_else(|err| panic!("{err:#}"));
            for day in days {
                let date = parse_date(&day.cal_date).unwrap_or_else(|err| panic!("{err:#}"));
                assert!(
                    date >= cursor && date <= chunk_end,
                    "交易日历返回请求范围外日期 {date}"
                );
                match day.is_open {
                    1 => insert_unique(&mut calendar, date, ()),
                    0 => (),
                    _ => panic!("{date} 无效 is_open"),
                }
            }
            if chunk_end == end {
                break;
            }
            cursor = chunk_end.next_day().expect("日期溢出");
        }
        calendar.into_keys().collect()
    }

    async fn daily_bars(
        &mut self,
        symbol: StockSymbol,
        start: Date,
        end: Date,
    ) -> Vec<StockDailyBar> {
        assert!(start <= end, "开始日期不能晚于结束日期");
        assert!(
            start >= time::macros::date!(2000 - 01 - 01),
            "stock_st 仅提供 20000101 起的历史状态，无法确定更早日线的 st"
        );
        let code = symbol.tushare_code();
        let mut bars = BTreeMap::new();
        let mut cursor = start;
        // 每次至多一个自然年的单只股票，低于这些接口的单次行数限制。
        // 使用不重叠的闭区间，避免长回测被服务端悄悄截断。
        loop {
            let chunk_end = end.min(
                Date::from_calendar_date(cursor.year(), Month::December, 31)
                    .expect("无法构造年末日期"),
            );
            let params = params! {
                "ts_code" => code.clone(),
                "start_date" => api_date(cursor),
                "end_date" => api_date(chunk_end),
            };
            log::debug!(
                "下载日线 bar: symbol={code}, start={}, end={}",
                api_date(cursor),
                api_date(chunk_end),
            );
            let daily = self
                .daily(
                    params.clone(),
                    "ts_code,trade_date,open,high,low,close,vol,amount",
                )
                .await
                .unwrap_or_else(|err| panic!("{err:#}"))
                .to_typed::<Daily>()
                .unwrap_or_else(|err| panic!("{err:#}"));
            if !daily.is_empty() {
                let mut factors = BTreeMap::new();
                for row in self
                    .adj_factor(params.clone(), "ts_code,trade_date,adj_factor")
                    .await
                    .unwrap_or_else(|err| panic!("{err:#}"))
                    .to_typed::<Factor>()
                    .unwrap_or_else(|err| panic!("{err:#}"))
                {
                    let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                    check_row(&row.ts_code, &code, date, cursor, chunk_end);
                    assert!(
                        row.adj_factor.is_finite() && row.adj_factor > 0.0,
                        "{date} 复权因子必须为正数"
                    );
                    insert_unique(&mut factors, date, row.adj_factor);
                }
                let mut limits = BTreeMap::new();
                for row in self
                    .stk_limit(params.clone(), "ts_code,trade_date,up_limit,down_limit")
                    .await
                    .unwrap_or_else(|err| panic!("{err:#}"))
                    .to_typed::<Limit>()
                    .unwrap_or_else(|err| panic!("{err:#}"))
                {
                    let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                    check_row(&row.ts_code, &code, date, cursor, chunk_end);
                    insert_unique(&mut limits, date, row);
                }
                // 单只股票按年查询，最多 366 个日期，低于 stock_st 的 1000 行上限。
                // 名单只包含当日 ST/*ST 股票；成功查询后未出现的日期即非 ST。
                let mut st_dates = BTreeMap::new();
                for row in self
                    .stock_st(params, "ts_code,trade_date")
                    .await
                    .unwrap_or_else(|err| panic!("stock_st 查询失败: {err:#}"))
                    .to_typed::<StStatus>()
                    .unwrap_or_else(|err| panic!("stock_st 数据无效: {err:#}"))
                {
                    let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                    check_row(&row.ts_code, &code, date, cursor, chunk_end);
                    insert_unique(&mut st_dates, date, ());
                }
                for row in daily {
                    let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                    check_row(&row.ts_code, &code, date, cursor, chunk_end);
                    let factor = factors
                        .get(&date)
                        .copied()
                        .unwrap_or_else(|| panic!("{code} {date} 缺少复权因子"));
                    let limit = limits
                        .get(&date)
                        .unwrap_or_else(|| panic!("{code} {date} 缺少涨跌停数据"));
                    insert_unique(
                        &mut bars,
                        date,
                        StockDailyBar {
                            symbol,
                            date,
                            open: row.open,
                            high: row.high,
                            low: row.low,
                            close: row.close,
                            volume: row.vol * 100.0,
                            turnover: row.amount * 1000.0,
                            limit_up: limit.up_limit,
                            limit_down: limit.down_limit,
                            float_market_cap: None,
                            adjustment: Some(Adjustment::Raw(factor)),
                            st: st_dates.contains_key(&date),
                        },
                    );
                }
            }
            if chunk_end == end {
                break;
            }
            cursor = chunk_end.next_day().expect("日期溢出");
        }
        bars.into_values().collect()
    }
}
