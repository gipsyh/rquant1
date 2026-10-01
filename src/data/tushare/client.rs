//! tushare HTTP 客户端 —— Python `tushare/pro/client.py` 的 `DataApi` 等价物。

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;

use super::error::{TushareError, snippet};
use super::table::Table;

/// 与 Python `client.py:20` 的 `__http_url` 同址，但改用 HTTPS。
///
/// Python 硬编码 `http://api.waditu.com/dataapi`，token 会明文过网；
/// 实测该主机支持 TLS，因此默认升级。`https://api.tushare.pro` 同样可用，
/// 可用 [`TushareClient::with_base_url`] 切换。
pub const DEFAULT_BASE_URL: &str = "https://api.waditu.com/dataapi";

/// 对齐 Python `DataApi.__init__` 的 `timeout=30`。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 内置 token —— 对齐 Python `rquant/data/tushare/helpers.py` 的 `_DEFAULT_TOKEN`。
///
/// token 直接固化在源码里，运行时不读环境变量。要换 token 改这一行。
///
/// ⚠️ 它随源码提交进 git，也会被打进编译产物（cdylib）。能读到仓库或二进制的人
/// 都能拿到它，并消耗该账号的接口额度。详见文件末尾「关于 token」的说明。
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

/// tushare Pro 数据接口客户端。
///
/// 所有方法都是异步的，runtime 由调用方提供（本 crate 不创建 runtime）。
///
/// ```no_run
/// # async fn demo() -> Result<(), rquant::data::tushare::TushareError> {
/// use rquant::data::tushare::{TushareClient, params};
///
/// let client = TushareClient::new();
/// let table = client
///     .daily(params! { "ts_code" => "000001.SZ", "start_date" => "20240101" },
///            "ts_code,trade_date,close")
///     .await?;
/// println!("{} 行", table.len());
/// # Ok(())
/// # }
/// ```
pub struct TushareClient {
    token: String,
    base_url: String,
    http: reqwest::Client,
    retry: RetryPolicy,
    gate: Arc<Semaphore>,
}

impl std::fmt::Debug for TushareClient {
    /// 手写实现，避免把 token 打进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TushareClient")
            .field("base_url", &self.base_url)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Default for TushareClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TushareClient {
    /// 构造客户端，默认 [`DEFAULT_BASE_URL`] 与 [`DEFAULT_TIMEOUT`]。
    ///
    /// token 固定取 [`DEFAULT_TOKEN`]：不读环境变量，也没有传入 token 的入口。
    /// 要换 token 改 [`DEFAULT_TOKEN`] 一处。
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
    /// 未在 [`super::api`] 里具名封装的接口走这里，这正是 Python `__getattr__`
    /// 提供的逃生通道。
    pub async fn query(
        &self,
        api_name: &str,
        params: Params,
        fields: &str,
    ) -> Result<Table, TushareError> {
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
                .map_err(|_| TushareError::Shape("并发信号量已关闭".to_string()))?;

            let outcome = self.send(&url, &body, api_name).await;
            // 先放行再退避，避免重试的等待时间占用并发额度。
            drop(permit);

            match outcome {
                Ok(table) => return Ok(table),
                Err(err) if attempt < self.retry.max_retries && err.is_retryable() => {
                    let delay = self
                        .retry
                        .base_delay
                        .saturating_mul(2u32.saturating_pow(attempt));
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// 单次请求。重试逻辑在 [`Self::query`]。
    async fn send(&self, url: &str, body: &Value, api_name: &str) -> Result<Table, TushareError> {
        // Python 在 HTTP >= 400 时静默返回空 DataFrame（client.py:51-52 的 `if res:`），
        // 这里改为报错 —— 否则上游 502 会伪装成「当天没有数据」。
        let response = self
            .http
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        let text = response.text().await?;

        let parsed: Response =
            serde_json::from_str(&text).map_err(|source| TushareError::Json {
                source,
                snippet: snippet(&text),
            })?;

        if parsed.code != 0 {
            return Err(TushareError::Api {
                code: parsed.code,
                msg: parsed.msg,
                api_name: api_name.to_string(),
            });
        }

        let data = parsed.data.ok_or_else(|| {
            TushareError::Shape(format!("[{api_name}] code=0 但响应缺少 data 字段"))
        })?;
        Table::new(data.fields, data.items)
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

// =====================================================================
// 关于 token
// =====================================================================
//
// [`DEFAULT_TOKEN`] 是硬编码的，与 Python 端 `helpers.py` 的 `_DEFAULT_TOKEN`
// 保持一致。这意味着：
//
// * token 随源码提交进 git，克隆仓库的人都能看到 —— 含历史提交，即使之后再删。
// * token 会被编进 cdylib，`strings librquant.dylib` 就能捞出来。
// * 因此它只能当「个人内部数据管道的便利默认值」，不能当密钥。
//
// 换 token 改 [`DEFAULT_TOKEN`] 一处即可。若之后要改成运行时注入，给
// [`TushareClient`] 加个接受 token 的构造函数、或读环境变量即可 ——
// 客户端内部只当 token 是个 `String`，不关心它从哪来。
