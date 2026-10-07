//! Tushare 数据源：HTTP 接口调用、重试与日线数据组装。

use super::table::Table;
use crate::data::{
    Adjustment, DataProvider, IndexHistComp, Stock, StockBar, StockHistBar, StockSymbol,
};
use crate::utils::{DateRange, parse_date};
use anyhow::{Context, Result, anyhow};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use time::{Date, Month, macros::format_description};

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

/// 请求重试策略。
///
/// Python SDK 没有自动重试。tushare 有按分钟的频率限制，裸客户端在批量拉取时
/// 容易连环撞墙，因此这里默认开启；[`RetryPolicy::none`] 可退回 Python 的行为。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 普通瞬时错误的最大重试次数；0 只禁用普通瞬时错误的重试，
    /// 不影响分钟限频重试（见 [`Self::rate_limit_max_retries`]）。
    pub max_retries: u32,
    /// 普通瞬时错误的指数退避基准延时，第 n 次重试等待 `base_delay * 2^n`。
    pub base_delay: Duration,
    /// 命中分钟限频后，每次重试前的固定冷却时长。
    pub rate_limit_cooldown: Duration,
    /// 分钟限频的最大重试次数。
    pub rate_limit_max_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            rate_limit_cooldown: Duration::from_secs(10),
            rate_limit_max_retries: 20,
        }
    }
}

impl RetryPolicy {
    /// 不自动重试 —— 与 Python SDK 的行为一致。
    /// 两类重试上限都归零，普通错误与分钟限频都不重试。
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::ZERO,
            rate_limit_cooldown: Duration::ZERO,
            rate_limit_max_retries: 0,
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
#[derive(Clone)]
pub struct TushareProvider {
    token: String,
    base_url: String,
    http: reqwest::Client,
    retry: RetryPolicy,
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
        Self {
            token: DEFAULT_TOKEN.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: http_client(DEFAULT_TIMEOUT),
            retry,
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
        self.retry = retry;
        self
    }

    /// 服务地址（不含 api_name 路径段）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 先请求完整区间，触顶后仅重查两个子请求；不保留可能截断的父响应。
    async fn query_api<A: TushareApi>(&self, params: Params, fields: &str) -> Result<Table> {
        let table = self.query_once(A::NAME, params.clone(), fields).await?;
        if let Some(limit) = A::MAX_ROWS
            && table.len() >= limit
        {
            log::debug!(
                "tushare [{}] 返回 {} 行，触及 {limit} 行上限，尝试拆分",
                A::NAME,
                table.len()
            );
            let (left, right) = A::split_on_limit(&params);
            drop(table);
            // 子请求顺序执行；Box::pin 为递归 future 提供间接层。
            let mut merged = Box::pin(self.query_api::<A>(left, fields)).await?;
            merged.append(Box::pin(self.query_api::<A>(right, fields)).await?)?;
            return Ok(merged);
        }
        Ok(table)
    }

    /// 单个参数集合的请求，含 HTTP/限频重试，不处理行数上限。
    async fn query_once(&self, api_name: &str, params: Params, fields: &str) -> Result<Table> {
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
        let mut rate_limit_attempt: u32 = 0;
        loop {
            let outcome = self.send(&url, &body, api_name).await;

            let (err, rate_limited, retryable) = match outcome {
                Ok(parsed) if parsed.code == 0 => {
                    let data = parsed.data.with_context(|| {
                        format!("响应结构异常: [{api_name}] code=0 但响应缺少 data 字段")
                    })?;
                    return Table::new(data.fields, data.items);
                }
                Ok(parsed) => {
                    let rate_limited = is_rate_limited(&parsed.msg);
                    let err = anyhow!(
                        "tushare [{api_name}] 返回 code={}: {}",
                        parsed.code,
                        parsed.msg
                    );
                    (err, rate_limited, false)
                }
                Err(err) => {
                    let retryable = is_retryable(&err);
                    (err, false, retryable)
                }
            };
            if rate_limited {
                if rate_limit_attempt >= self.retry.rate_limit_max_retries {
                    return Err(err).with_context(|| {
                        format!(
                            "分钟限频重试 {} 次后仍失败",
                            self.retry.rate_limit_max_retries
                        )
                    });
                }
                rate_limit_attempt += 1;
                log::warn!(
                    "tushare [{api_name}] 分钟限频，当前请求冷却 {} 秒后重试 ({}/{})",
                    self.retry.rate_limit_cooldown.as_secs(),
                    rate_limit_attempt,
                    self.retry.rate_limit_max_retries,
                );
                // 仅等待当前 worker，其他任务仍可继续。
                tokio::time::sleep(self.retry.rate_limit_cooldown).await;
                continue;
            }
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

    /// 单次 HTTP 请求及响应解析；API 状态和重试由 query_once 处理。
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

/// 接口的返回上限及超限拆分规则；None 表示上限未知。
trait TushareApi {
    const NAME: &'static str;
    const MAX_ROWS: Option<usize>;

    fn split_on_limit(params: &Params) -> (Params, Params) {
        panic!(
            "tushare [{}] symbol={}, start={}, end={} 达到或超过 {} 行上限，可能被截断；不支持拆分",
            Self::NAME,
            params
                .get("ts_code")
                .or_else(|| params.get("index_code"))
                .unwrap_or(&Value::Null),
            params.get("start_date").unwrap_or(&Value::Null),
            params.get("end_date").unwrap_or(&Value::Null),
            Self::MAX_ROWS.expect("仅已知上限的接口触发拆分"),
        );
    }
}

fn split_date_range(api_name: &str, params: &Params) -> (Params, Params) {
    for key in ["trade_date", "limit", "offset"] {
        assert!(
            !params.contains_key(key),
            "tushare [{api_name}] 含 {key} 参数，不能按日期区间拆分"
        );
    }
    let date = |key: &str| {
        let value = params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("tushare [{api_name}] 拆分缺少日期参数 {key}"));
        parse_date(value).unwrap_or_else(|err| panic!("tushare [{api_name}] 拆分日期无效: {err:#}"))
    };
    let (start, end) = (date("start_date"), date("end_date"));
    assert!(
        start < end,
        "tushare [{api_name}] 日期区间不能继续拆分: {start}..={end}，拒绝返回可能截断的数据"
    );
    let mid = start + time::Duration::days((end - start).whole_days() / 2);
    let mut left = params.clone();
    let mut right = params.clone();
    left.insert("end_date".into(), api_date(mid).into());
    right.insert(
        "start_date".into(),
        api_date(mid.next_day().unwrap()).into(),
    );
    (left, right)
}

// 生成具名接口方法，等价于 Python 客户端 __getattr__ 的动态接口调用。
macro_rules! tushare_apis {
    ($($(#[$attr:meta])* $name:ident => $api:ident($max_rows:expr $(, $split:path)?)),* $(,)?) => {
        $(
            struct $api;
            impl TushareApi for $api {
                const NAME: &'static str = stringify!($name);
                const MAX_ROWS: Option<usize> = $max_rows;
                $(fn split_on_limit(params: &Params) -> (Params, Params) {
                    $split(Self::NAME, params)
                })?
            }
        )*
        impl TushareProvider {
            // 具名接口和直接 query 共用同一份配置；None 表示未配置，非无限制。
            pub(super) fn api_max_rows(api_name: &str) -> Option<usize> {
                match api_name {
                    $(stringify!($name) => $api::MAX_ROWS,)*
                    _ => None,
                }
            }

            /// 调用任意接口；已注册接口共用 trait 的上限和拆分规则。
            /// `fields` 为空时由服务端选择字段。未知接口不假定其返回上限。
            pub async fn query(&self, api_name: &str, params: Params, fields: &str) -> Result<Table> {
                match api_name {
                    $(stringify!($name) => self.query_api::<$api>(params, fields).await,)*
                    _ => self.query_once(api_name, params, fields).await,
                }
            }

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

// 上限于 2026-10-06 核对官方文档；未注明数字的接口显式标为 None。
tushare_apis! {
    /// [日线行情（股票）](https://tushare.pro/document/2?doc_id=27)，单次 6000 行。
    daily => DailyApi(Some(6000)),
    /// [每日指标](https://tushare.pro/document/2?doc_id=32)，单次 6000 行。
    daily_basic => DailyBasicApi(Some(6000)),
    /// [复权因子](https://tushare.pro/document/2?doc_id=28)，文档未注明行数上限。
    adj_factor => AdjFactorApi(None),
    /// [每日涨跌停价格](https://tushare.pro/document/2?doc_id=183)，单次 5800 行。
    stk_limit => StkLimitApi(Some(5800)),
    /// [股票基础信息](https://tushare.pro/document/2?doc_id=25)，单次 6000 行。
    stock_basic => StockBasicApi(Some(6000)),
    /// [历史每日 ST/*ST 股票列表](https://tushare.pro/document/2?doc_id=397)，单次 1000 行。
    /// 触顶后按日期闭区间二分，直到每次响应都小于上限。
    stock_st => StockStApi(Some(1000), split_date_range),
    /// [股票曾用名](https://tushare.pro/document/2?doc_id=100)，文档未注明行数上限。
    namechange => NameChangeApi(None),
    /// [每日停复牌信息](https://tushare.pro/document/2?doc_id=214)，单次 5000 行。
    suspend_d => SuspendDailyApi(Some(5000)),
    /// [交易日历](https://tushare.pro/document/2?doc_id=26)，文档未注明行数上限。
    trade_cal => TradeCalApi(None),
    /// [指数基础信息](https://tushare.pro/document/2?doc_id=94)，单次 8000 行。
    index_basic => IndexBasicApi(Some(8000)),
    /// [指数日线行情](https://tushare.pro/document/2?doc_id=95)，文档未注明行数上限。
    index_daily => IndexDailyApi(None),
    /// [指数成分和权重](https://tushare.pro/document/2?doc_id=96)，文档未注明行数上限。
    /// 保留项目现有的 6000 行请求限制和截断保护，并非已核实的官方上限。
    index_weight => IndexWeightApi(Some(6000)),
}

/// 仅识别明确的分钟频次限制，避免把权限不足、每日总额度当作短暂限频。
pub(super) fn is_rate_limited(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    if ["每天", "每日", "当日", "当天", "per day", "daily limit"]
        .iter()
        .any(|word| msg.contains(word))
    {
        return false;
    }
    ["每分钟", "/分钟", "per minute"]
        .iter()
        .any(|word| msg.contains(word))
        && [
            "超限", "超过", "上限", "最多", "too many", "limit", "exceed",
        ]
        .iter()
        .any(|word| msg.contains(word))
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

pub(super) fn api_date(date: Date) -> String {
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

#[derive(Deserialize)]
struct NameChange {
    ts_code: String,
    name: String,
    start_date: String,
    end_date: Option<String>,
    ann_date: Option<String>,
    change_reason: Option<String>,
}

struct NamePeriod {
    start: Date,
    end: Option<Date>,
    announced: Option<Date>,
    delisting: bool,
}

fn name_periods(rows: Vec<NameChange>, code: &str) -> Result<Vec<NamePeriod>> {
    let optional_date = |value: Option<String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_date(&value))
            .transpose()
    };
    let mut periods = Vec::with_capacity(rows.len());
    for row in rows {
        anyhow::ensure!(row.ts_code == code, "namechange 股票代码不匹配: {code}");
        let name = row.name.trim();
        anyhow::ensure!(!name.is_empty(), "namechange 名称为空: {code}");
        let start = parse_date(&row.start_date)?;
        let end = optional_date(row.end_date)?;
        anyhow::ensure!(
            end.is_none_or(|end| start <= end),
            "namechange 生效区间无效: {code}"
        );
        periods.push(NamePeriod {
            start,
            end,
            announced: optional_date(row.ann_date)?,
            delisting: row
                .change_reason
                .as_deref()
                .is_some_and(|reason| reason.trim() == "退市整理期")
                || name.starts_with("退市")
                || name.ends_with('退'),
        });
    }
    periods.sort_by_key(|period| period.start);
    anyhow::ensure!(
        periods
            .windows(2)
            .all(|pair| { pair[0].end.is_some_and(|end| end < pair[1].start) }),
        "namechange 生效区间重叠: {code}"
    );
    Ok(periods)
}

fn delisting_on(periods: &[NamePeriod], code: &str, date: Date) -> bool {
    let index = periods.partition_point(|period| period.start <= date);
    let period = index
        .checked_sub(1)
        .map(|index| &periods[index])
        .filter(|period| period.end.is_none_or(|end| date <= end))
        .unwrap_or_else(|| panic!("{code} {date} 缺少有效历史名称"));
    assert!(
        period.announced.is_none_or(|announced| announced <= date),
        "{code} {date} 历史名称尚未公告"
    );
    period.delisting
}

fn insert_unique<T>(map: &mut BTreeMap<Date, T>, date: Date, value: T) {
    assert!(map.insert(date, value).is_none(), "重复日期 {date}");
}

fn check_row(code: &str, expected: &str, date: Date, range: DateRange) {
    assert!(
        code == expected && range.contains(date),
        "接口返回了请求范围外的数据 {code} {date}"
    );
}

#[derive(Deserialize)]
struct StockBasic {
    ts_code: String,
    name: String,
    list_date: String,
    delist_date: Option<String>,
    industry: Option<String>,
}

impl TushareProvider {
    async fn fetch_stocks_info(&self, symbols: &[StockSymbol]) -> Result<Vec<Stock>> {
        // 去重后按状态批量查询；已经找到的股票不再请求下一个状态。
        let mut pending: BTreeMap<_, _> = symbols
            .iter()
            .map(|&symbol| (symbol.tushare_code(), symbol))
            .collect();
        let mut infos = BTreeMap::new();
        for status in ["L", "D", "P"] {
            if pending.is_empty() {
                break;
            }
            let codes = pending.keys().cloned().collect::<Vec<_>>().join(",");
            let rows = self
                .stock_basic(
                    params! { "ts_code" => codes, "list_status" => status },
                    "ts_code,name,list_date,delist_date,industry",
                )
                .await?
                .to_typed::<StockBasic>()?;
            for row in rows {
                let symbol = pending.remove(&row.ts_code).with_context(|| {
                    format!("股票基础信息返回重复或不匹配的代码: {}", row.ts_code)
                })?;
                let info = Stock {
                    symbol,
                    bars: None,
                    name: row.name,
                    listed: parse_date(&row.list_date)?,
                    delisted: row
                        .delist_date
                        .filter(|value| !value.trim().is_empty())
                        .map(|date| parse_date(&date))
                        .transpose()?,
                    industry: row.industry.filter(|value| !value.trim().is_empty()),
                };
                info.validate()?;
                anyhow::ensure!(
                    status != "D" || info.delisted.is_some(),
                    "退市股票缺少退市日期: {}",
                    row.ts_code
                );
                infos.insert(symbol, info);
            }
        }
        anyhow::ensure!(
            pending.is_empty(),
            "找不到已上市股票的基础信息: {}",
            pending.keys().cloned().collect::<Vec<_>>().join(",")
        );
        // 上游返回顺序不固定；恢复输入顺序和重复项。
        Ok(symbols.iter().map(|symbol| infos[symbol].clone()).collect())
    }
}

#[async_trait::async_trait]
impl DataProvider for TushareProvider {
    async fn is_tradable(&mut self, _symbol: StockSymbol, _date: Date) -> bool {
        unimplemented!("可交易判断请通过 MemCacheProvider 或 DiskCacheProvider 查询")
    }

    async fn stocks_info(&mut self, symbols: &[StockSymbol]) -> Vec<Stock> {
        self.fetch_stocks_info(symbols)
            .await
            .unwrap_or_else(|err| panic!("股票基础信息查询失败: {err:#}"))
    }

    async fn stock_info(&mut self, symbol: StockSymbol) -> Stock {
        self.stocks_info(&[symbol])
            .await
            .into_iter()
            .next()
            .unwrap()
    }

    async fn trading_days(&mut self, range: DateRange) -> Vec<Date> {
        let (start, end) = (range.start(), range.end());
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

    async fn stock_bar(&mut self, symbol: StockSymbol, range: DateRange) -> StockHistBar {
        let (start, end) = (range.start(), range.end());
        assert!(
            start >= time::macros::date!(2000 - 01 - 01),
            "stock_st 仅提供 20000101 起的历史状态，无法确定更早日线的 st"
        );
        let code = symbol.tushare_code();
        let mut bars = BTreeMap::new();
        // 单只股票按完整请求区间查询；已知上限由 query 检查。
        let params = params! {
            "ts_code" => code.clone(),
            "start_date" => api_date(start),
            "end_date" => api_date(end),
        };
        log::debug!(
            "下载日线 bar: symbol={code}, start={}, end={}",
            api_date(start),
            api_date(end),
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
            // adj_factor 文档未注明行数上限；下方逐根日线检查因子完整性。
            let mut factors = BTreeMap::new();
            for row in self
                .adj_factor(params.clone(), "ts_code,trade_date,adj_factor")
                .await
                .unwrap_or_else(|err| panic!("{err:#}"))
                .to_typed::<Factor>()
                .unwrap_or_else(|err| panic!("{err:#}"))
            {
                let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                check_row(&row.ts_code, &code, date, range);
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
                check_row(&row.ts_code, &code, date, range);
                insert_unique(&mut limits, date, row);
            }
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
                check_row(&row.ts_code, &code, date, range);
                insert_unique(&mut st_dates, date, ());
            }
            // 输入 start_date/end_date 过滤公告日期，不是名称生效区间。
            // 按股票取完整历史，才能覆盖首根 bar 之前已生效的名称。
            let names = self
                .namechange(
                    params! { "ts_code" => code.clone() },
                    "ts_code,name,start_date,end_date,ann_date,change_reason",
                )
                .await
                .unwrap_or_else(|err| panic!("namechange 查询失败: {err:#}"))
                .to_typed::<NameChange>()
                .unwrap_or_else(|err| panic!("namechange 数据无效: {err:#}"));
            let names = name_periods(names, &code)
                .unwrap_or_else(|err| panic!("namechange 数据无效: {err:#}"));
            for row in daily {
                let date = parse_date(&row.trade_date).unwrap_or_else(|err| panic!("{err:#}"));
                check_row(&row.ts_code, &code, date, range);
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
                    StockBar {
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
                        delisting: delisting_on(&names, &code, date),
                    },
                );
            }
        }
        StockHistBar::new(range, bars.into_values().collect()).unwrap()
    }

    async fn stocks_bar(&mut self, requests: &[(StockSymbol, DateRange)]) -> Vec<StockHistBar> {
        // 每项复用完整的单股票处理流程；共享 HTTP 连接池。
        // buffered 保持输入顺序，最多同时处理 4 项，失败沿用 stock_bar 的 panic。
        stream::iter(requests.iter().copied().map(|(symbol, range)| {
            let mut provider = self.clone();
            async move { provider.stock_bar(symbol, range).await }
        }))
        .buffered(4)
        .collect()
        .await
    }

    async fn index_name(&mut self, symbol: &str) -> String {
        self.fetch_index_name(symbol)
            .await
            .unwrap_or_else(|err| panic!("指数名称查询失败: {err:#}"))
    }

    async fn index_comp(&mut self, symbol: &str, range: DateRange) -> IndexHistComp {
        self.fetch_index_comp(symbol, range)
            .await
            .unwrap_or_else(|err| panic!("指数成分查询失败: {err:#}"))
    }
}
