//! tushare 客户端的错误类型。

/// tushare 客户端错误。
///
/// 对标 Python `DataApi.query` 的 `raise Exception(result['msg'])`，但保留 `code`
/// （Python 把它丢掉了），限流判断才有依据。
#[derive(Debug, thiserror::Error)]
pub enum TushareError {
    /// 服务端返回 `code != 0`。tushare 没有稳定的错误码常量，码值全由服务端定义，
    /// 因此这里原样保留 `code` 与 `msg`，不做解释。
    #[error("tushare [{api_name}] 返回 code={code}: {msg}")]
    Api {
        code: i64,
        msg: String,
        api_name: String,
    },

    /// 传输层失败，或 HTTP 状态码异常。
    ///
    /// 对齐 Python 的 `requests.post` 异常；但 HTTP >= 400 时 Python 会静默返回
    /// 空 DataFrame（`client.py:51-52` 的 `if res:` 判断），这里改为报错。
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),

    /// 响应体不是合法 JSON。
    #[error("解析响应 JSON 失败: {source}（原文前 200 字符: {snippet}）")]
    Json {
        #[source]
        source: serde_json::Error,
        snippet: String,
    },

    /// 响应结构符合 JSON 语法，但形状不符合预期（如缺 `data`、行长度与 `fields` 不等）。
    #[error("响应结构异常: {0}")]
    Shape(String),

    /// 某一行无法反序列化成调用方指定的类型。
    #[error("第 {row} 行无法反序列化为目标类型: {source}")]
    Deserialize {
        row: usize,
        #[source]
        source: serde_json::Error,
    },
}

impl TushareError {
    /// 是否是可重试的频率限制错误。
    ///
    /// tushare 对限流没有专用错误码，只能按服务端文案判断；文案可能随服务端调整，
    /// 因此这里匹配一组常见关键词，属于启发式判断。
    pub fn is_rate_limited(&self) -> bool {
        const NEEDLES: [&str; 6] = ["每分钟", "频率", "超限", "抱歉", "访问过快", "too many"];
        match self {
            Self::Api { msg, .. } => NEEDLES.iter().any(|needle| msg.contains(needle)),
            _ => false,
        }
    }

    /// 是否值得重试：限流，或传输层错误 / 5xx。
    ///
    /// 4xx 不重试（token 无效、参数错误重试多少次都一样）。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { .. } => self.is_rate_limited(),
            Self::Http(err) => match err.status() {
                Some(status) => status.is_server_error(),
                None => err.is_timeout() || err.is_connect(),
            },
            _ => false,
        }
    }
}

/// 截取前若干字符用于错误信息，按字符边界切分以免切坏 UTF-8。
pub(crate) fn snippet(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_CHARS).collect();
    format!("{head}…")
}
