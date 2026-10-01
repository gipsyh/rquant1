//! tushare 客户端测试。
//!
//! 除标了 `#[ignore]` 的在线冒烟测试外，全部走 wiremock，不碰网络。
//! 在线测试用内置的 [`DEFAULT_TOKEN`](super::DEFAULT_TOKEN)，不需要任何环境变量，
//! 但会真实联网并消耗接口额度，所以默认不跑：
//!
//! ```text
//! cargo test data::tushare                # 离线部分
//! cargo test data::tushare -- --ignored   # 加上在线冒烟
//! ```

use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    Column, DEFAULT_TOKEN, ParamsExt, RetryPolicy, Table, TushareClient, TushareError, params,
};

/// 快速失败的重试策略：错误路径测试不该真的等退避。
fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        max_concurrency: 4,
    }
}

/// 一个正常的成功响应。
fn ok_body() -> Value {
    json!({
        "code": 0,
        "msg": "",
        "data": {
            "fields": ["ts_code", "trade_date", "close"],
            "items": [
                ["000001.SZ", "20240102", 10.5],
                ["000002.SZ", "20240102", null]
            ]
        }
    })
}

async fn client_for(server: &MockServer, retry: RetryPolicy) -> TushareClient {
    TushareClient::new()
        .with_base_url(server.uri())
        .with_retry(retry)
}

// =====================================================================
// 请求构造
// =====================================================================

#[tokio::test]
async fn query_发送正确的请求体() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/daily"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let table = client
        .daily(
            params! { "ts_code" => "000001.SZ", "start_date" => "20240101" },
            "ts_code,trade_date,close",
        )
        .await
        .expect("应当成功");

    assert_eq!(table.len(), 2);

    let requests = server.received_requests().await.expect("能取到请求记录");
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("请求体是 JSON");

    assert_eq!(body["api_name"], "daily");
    assert_eq!(body["token"], DEFAULT_TOKEN);
    assert_eq!(body["fields"], "ts_code,trade_date,close");
    assert_eq!(body["params"]["ts_code"], "000001.SZ");
    // 对齐 client.py:34 —— ts_type_name 是 Python 每次都带的参数。
    assert_eq!(body["params"]["ts_type_name"], server.uri());
}

#[tokio::test]
async fn 调用方传入的_ts_type_name_不被覆盖() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    client
        .query("daily", params! { "ts_type_name" => "custom" }, "")
        .await
        .expect("应当成功");

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["params"]["ts_type_name"], "custom");
}

// =====================================================================
// 错误路径
// =====================================================================

#[tokio::test]
async fn code_非零时保留_code_与_msg() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 40001,
            "msg": "抱歉，您没有访问该接口的权限",
            "data": null
        })))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();

    match err {
        TushareError::Api { code, ref msg, ref api_name } => {
            assert_eq!(code, 40001, "code 必须保留（Python 把它丢了）");
            assert_eq!(msg, "抱歉，您没有访问该接口的权限");
            assert_eq!(api_name, "daily");
        }
        other => panic!("期望 Api 错误，实际 {other:?}"),
    }

    // 「抱歉」是限流关键词，应被识别为可重试。
    assert!(err.is_rate_limited());
}

#[tokio::test]
async fn http_500_返回错误而非空表() {
    // Python 在 HTTP >= 400 时静默返回空 DataFrame（client.py:51-52）。
    // 这里刻意不复刻：上游故障不能伪装成「当天没有数据」。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();

    assert!(
        matches!(err, TushareError::Http(_)),
        "期望 Http 错误，实际 {err:?}"
    );
    assert!(err.is_retryable(), "5xx 应当可重试");
}

#[tokio::test]
async fn http_404_不可重试() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();
    assert!(!err.is_retryable(), "4xx 重试没有意义");
}

#[tokio::test]
async fn 响应非_json_时给出原文片段() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>502 Bad Gateway</html>"))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();

    match err {
        TushareError::Json { ref snippet, .. } => {
            assert!(snippet.contains("Bad Gateway"), "片段应包含原文：{snippet}");
        }
        other => panic!("期望 Json 错误，实际 {other:?}"),
    }
}

#[tokio::test]
async fn code_为零但缺少_data_时报结构异常() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": 0, "msg": "" })))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();
    assert!(matches!(err, TushareError::Shape(_)), "实际 {err:?}");
}

#[tokio::test]
async fn 行长度与_fields_不等时报结构异常() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0,
            "msg": "",
            "data": { "fields": ["a", "b", "c"], "items": [["1", "2"]] }
        })))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();

    match err {
        TushareError::Shape(ref detail) => {
            assert!(detail.contains("2 个值"), "应说明实际列数：{detail}");
        }
        other => panic!("期望 Shape 错误，实际 {other:?}"),
    }
}

// =====================================================================
// 重试
// =====================================================================

#[tokio::test]
async fn 限流后重试直到成功() {
    let server = MockServer::start().await;
    // 前两次 500，第三次成功。
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let client = client_for(&server, fast_retry()).await;
    let table = client.query("daily", params! {}, "").await.expect("重试后应成功");

    assert_eq!(table.len(), 2);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "应当是 2 次失败 + 1 次成功"
    );
}

#[tokio::test]
async fn 重试次数耗尽后向上抛出() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = client_for(&server, fast_retry()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();
    assert!(matches!(err, TushareError::Http(_)));

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        4,
        "max_retries=3 表示最多 4 次请求"
    );
}

#[tokio::test]
async fn retry_policy_none_不重试() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = client_for(&server, RetryPolicy::none()).await;
    client.query("daily", params! {}, "").await.unwrap_err();

    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// =====================================================================
// Table —— pd.DataFrame 的等价物
// =====================================================================

fn sample_table() -> Table {
    Table::new(
        vec!["ts_code".into(), "close".into(), "vol".into()],
        vec![
            vec![json!("000001.SZ"), json!(10.5), json!(1200)],
            vec![json!("000002.SZ"), json!(null), json!(3400)],
        ],
    )
    .expect("构造成功")
}

#[test]
fn 空结果得到空表() {
    let table = Table::new(vec!["ts_code".into()], vec![]).unwrap();
    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
    assert_eq!(table.fields(), ["ts_code"]);
}

#[test]
fn 按列取名() {
    let table = sample_table();
    let column: Column<'_> = table.column("close").expect("有 close 列");
    assert_eq!(column.name(), "close");
    assert_eq!(column.len(), 2);
    assert!(table.column("不存在").is_none());
}

#[test]
fn as_f64_把_null_映射成_none() {
    let table = sample_table();
    // close 第二行是 JSON null。
    assert_eq!(table.column("close").unwrap().as_f64(), vec![Some(10.5), None]);
    assert_eq!(
        table.column("vol").unwrap().as_f64(),
        vec![Some(1200.0), Some(3400.0)]
    );
}

#[test]
fn as_f64_宽松解析数字字符串() {
    // 对标 pd.to_numeric(errors="coerce")：数字字符串也解析。
    let table = Table::new(
        vec!["vol".into()],
        vec![vec![json!("3400")], vec![json!(1200)], vec![json!("不是数字")], vec![json!(null)]],
    )
    .unwrap();

    assert_eq!(
        table.column("vol").unwrap().as_f64(),
        vec![Some(3400.0), Some(1200.0), None, None]
    );
}

#[test]
fn as_str_只接受字符串() {
    let table = sample_table();
    assert_eq!(
        table.column("ts_code").unwrap().as_str(),
        vec![Some("000001.SZ"), Some("000002.SZ")]
    );
    // close 是数字，as_str 应为 None（数字不是字符串）。
    assert_eq!(table.column("close").unwrap().as_str(), vec![None, None]);
}

#[test]
fn as_string_等价于_astype_str() {
    let table = sample_table();
    assert_eq!(
        table.column("close").unwrap().as_string(),
        vec![Some("10.5".to_string()), None]
    );
    // 整数形式的 JSON 数字不带小数点。
    assert_eq!(
        table.column("vol").unwrap().as_string(),
        vec![Some("1200".to_string()), Some("3400".to_string())]
    );
}

#[test]
fn records_对应_to_dict_records() {
    let table = sample_table();
    let records = table.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["ts_code"], "000001.SZ");
    assert_eq!(records[1]["close"], Value::Null);
}

#[test]
fn to_typed_反序列化成结构体() {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Bar {
        ts_code: String,
        close: Option<f64>,
        vol: f64,
    }

    let typed: Vec<Bar> = sample_table().to_typed().expect("应当反序列化成功");
    assert_eq!(typed.len(), 2);
    assert_eq!(
        typed[0],
        Bar { ts_code: "000001.SZ".into(), close: Some(10.5), vol: 1200.0 }
    );
    assert_eq!(typed[1].close, None, "JSON null 应变成 None");
}

#[test]
fn to_typed_缺列时报错并给出行号() {
    #[derive(Debug, serde::Deserialize)]
    struct Need {
        #[allow(dead_code)]
        ts_code: String,
        #[allow(dead_code)]
        missing: f64,
    }

    let err = sample_table().to_typed::<Need>().unwrap_err();
    match err {
        TushareError::Deserialize { row, .. } => assert_eq!(row, 0),
        other => panic!("期望 Deserialize 错误，实际 {other:?}"),
    }
}

#[test]
fn 按行列取单元格() {
    let table = sample_table();
    assert_eq!(table.get(0, "close"), Some(&json!(10.5)));
    assert_eq!(table.get(1, "close"), Some(&Value::Null));
    assert_eq!(table.get(9, "close"), None);
    assert_eq!(table.row(0).unwrap().get("ts_code"), Some(&json!("000001.SZ")));
    assert_eq!(table.row(0).unwrap().at(2), Some(&json!(1200)));
}

// =====================================================================
// Params
// =====================================================================

#[test]
fn params_宏构造参数表() {
    let p = params! {
        "ts_code" => "000001.SZ",
        "start_date" => "20240101",
        "limit" => 5000,
    };
    assert_eq!(p["ts_code"], "000001.SZ");
    assert_eq!(p["limit"], 5000);
    assert_eq!(p.len(), 3);
}

#[test]
fn params_支持可选值() {
    let end: Option<&str> = None;
    let p = params! { "ts_code" => "000001.SZ", "end_date" => end };
    // None 序列化成 JSON null，与 Python 传 None 一致。
    assert_eq!(p["end_date"], Value::Null);
}

#[test]
fn params_ext_链式构造() {
    let p = params! { "ts_code" => "000001.SZ" }
        .with("start_date", "20240101")
        .with("offset", 5000);
    assert_eq!(p["start_date"], "20240101");
    assert_eq!(p["offset"], 5000);
}

// =====================================================================
// 在线冒烟测试（用内置 token，会真实联网）
// =====================================================================

#[tokio::test]
#[ignore = "会真实联网并消耗接口额度"]
async fn 在线_交易日历() {
    let client = TushareClient::new();
    let table = client
        .trade_cal(
            params! { "exchange" => "SSE", "start_date" => "20240101", "end_date" => "20240131", "is_open" => "1" },
            "cal_date,is_open",
        )
        .await
        .expect("应当成功");

    assert_eq!(table.len(), 22, "2024 年 1 月有 22 个交易日");

    // tushare 返回日期**降序**：Python 侧 `get_trading_dates` 里的
    // `sorted(...)` 不是多余的。客户端不做排序，调用方按需自己处理。
    let dates = table.column("cal_date").expect("有 cal_date 列").as_str();
    assert_eq!(dates.first(), Some(&Some("20240131")));
    assert_eq!(dates.last(), Some(&Some("20240102")));
    assert!(dates.iter().all(|d| d.is_some()), "cal_date 不应有缺失");

    // trade_cal.is_open 以 JSON 整数 1/0 返回，既不是字符串也不是布尔。
    let open = table.column("is_open").expect("有 is_open 列").as_bool();
    assert!(open.iter().all(|v| *v == Some(true)), "1 应解析成 true");
}

#[tokio::test]
#[ignore = "会真实联网并消耗接口额度"]
async fn 在线_日线行情() {
    let client = TushareClient::new();
    let table = client
        .daily(
            params! { "ts_code" => "000001.SZ", "start_date" => "20240101", "end_date" => "20240131" },
            "ts_code,trade_date,open,high,low,close,vol,amount",
        )
        .await
        .expect("应当成功");

    assert_eq!(table.len(), 22, "2024 年 1 月有 22 个交易日");
    assert_eq!(table.fields().len(), 8);

    // 与 trade_cal 一样是降序；Python 的 fetch_bars_raw 会显式 sort_values。
    let dates = table.column("trade_date").expect("有 trade_date 列").as_str();
    assert_eq!(dates.first(), Some(&Some("20240131")), "tushare 返回日期降序");
    assert!(dates.iter().all(|d| d.is_some()));

    // 收盘价是正数。
    let closes = table.column("close").expect("有 close 列").as_f64();
    assert!(closes.iter().all(|c| c.is_some_and(|v| v > 0.0)));
}
