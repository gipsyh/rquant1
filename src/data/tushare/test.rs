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

use super::provider::{is_rate_limited, is_retryable};
use crate::data::{DataProvider, StockSymbol};
use crate::engine::{BacktestConfig, BacktestEngine};
use crate::strategy::{BuyAndHold, BuyAndHoldConfig};
use crate::utils::parse_date;
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Column, ParamsExt, RetryPolicy, Table, TushareProvider, params};

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

async fn client_for(server: &MockServer, retry: RetryPolicy) -> TushareProvider {
    TushareProvider::new()
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

    let message = err.to_string();
    assert!(message.contains("code=40001"));
    assert!(message.contains("抱歉，您没有访问该接口的权限"));
    assert!(message.contains("[daily]"));
    // 保留原有限流文案识别规则。
    assert!(is_rate_limited("抱歉，您没有访问该接口的权限"));
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
        err.downcast_ref::<reqwest::Error>().is_some(),
        "期望 Http 错误，实际 {err:?}"
    );
    assert!(is_retryable(&err), "5xx 应当可重试");
}

#[tokio::test]
async fn http_404_不可重试() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = client_for(&server, fast_retry()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();
    assert!(!is_retryable(&err), "4xx 重试没有意义");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn 响应非_json_时给出原文片段() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>502 Bad Gateway</html>"))
        .mount(&server)
        .await;

    let client = client_for(&server, fast_retry()).await;
    let err = client.query("daily", params! {}, "").await.unwrap_err();

    assert!(err.downcast_ref::<serde_json::Error>().is_some());
    assert!(err.to_string().contains("Bad Gateway"));
    assert!(!is_retryable(&err));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
    assert!(
        err.to_string().contains("code=0 但响应缺少 data"),
        "实际 {err:?}"
    );
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

    let message = err.to_string();
    assert!(message.contains("响应结构异常"));
    assert!(message.contains("2 个值"));
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
    let table = client
        .query("daily", params! {}, "")
        .await
        .expect("重试后应成功");

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
    assert!(err.downcast_ref::<reqwest::Error>().is_some());

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
    assert_eq!(
        table.column("close").unwrap().as_f64(),
        vec![Some(10.5), None]
    );
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
        vec![
            vec![json!("3400")],
            vec![json!(1200)],
            vec![json!("不是数字")],
            vec![json!(null)],
        ],
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
        Bar {
            ts_code: "000001.SZ".into(),
            close: Some(10.5),
            vol: 1200.0
        }
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
    assert!(err.to_string().contains("第 0 行"));
    assert!(err.downcast_ref::<serde_json::Error>().is_some());
    assert!(format!("{err:#}").contains("missing"));
}

#[test]
fn 按行列取单元格() {
    let table = sample_table();
    assert_eq!(table.get(0, "close"), Some(&json!(10.5)));
    assert_eq!(table.get(1, "close"), Some(&Value::Null));
    assert_eq!(table.get(9, "close"), None);
    assert_eq!(
        table.row(0).unwrap().get("ts_code"),
        Some(&json!("000001.SZ"))
    );
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
    let client = TushareProvider::new();
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
    let client = TushareProvider::new();
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
    let dates = table
        .column("trade_date")
        .expect("有 trade_date 列")
        .as_str();
    assert_eq!(
        dates.first(),
        Some(&Some("20240131")),
        "tushare 返回日期降序"
    );
    assert!(dates.iter().all(|d| d.is_some()));

    // 收盘价是正数。
    let closes = table.column("close").expect("有 close 列").as_f64();
    assert!(closes.iter().all(|c| c.is_some_and(|v| v > 0.0)));
}

#[tokio::test]
async fn api_限流文案触发重试() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": -2001, "msg": "每分钟访问次数超限", "data": null
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let table = client_for(&server, fast_retry())
        .await
        .query("daily", params! {}, "")
        .await
        .unwrap();
    assert_eq!(table.len(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn 普通_api_错误不重试且保留错误详情() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 2002, "msg": "没有接口权限", "data": null
        })))
        .mount(&server)
        .await;
    let err = client_for(&server, fast_retry())
        .await
        .query("daily", params! {}, "")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "tushare [daily] 返回 code=2002: 没有接口权限"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

async fn response(server: &MockServer, api: &str, fields: &[&str], rows: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/{api}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "data": { "fields": fields, "items": rows }
        })))
        .mount(server)
        .await;
}

fn provider(server: &MockServer) -> TushareProvider {
    TushareProvider::new()
        .with_base_url(server.uri())
        .with_retry(RetryPolicy::none())
}

async fn fixture(server: &MockServer, factors: Value) {
    fixture_with_st(server, factors, json!([])).await;
}

async fn fixture_with_st(server: &MockServer, factors: Value, st_rows: Value) {
    response(server, "stock_st", &["ts_code", "trade_date"], st_rows).await;
    response(
        server,
        "trade_cal",
        &["cal_date", "is_open"],
        json!([["20240104", 1], ["20240103", 1], ["20240102", 1]]),
    )
    .await;
    // 非固定列序、倒序行情、其中一个交易日停牌。
    response(
        server,
        "daily",
        &[
            "close",
            "trade_date",
            "ts_code",
            "open",
            "low",
            "high",
            "amount",
            "vol",
        ],
        json!([
            [11, "20240104", "000001.SZ", 10, 9, 12, 150, 1000],
            [10, "20240102", "000001.SZ", 10, 9, 11, 100, 1000]
        ]),
    )
    .await;
    response(
        server,
        "adj_factor",
        &["ts_code", "trade_date", "adj_factor"],
        factors,
    )
    .await;
    response(
        server,
        "stk_limit",
        &["ts_code", "trade_date", "up_limit", "down_limit"],
        json!([
            ["000001.SZ", "20240102", 11, 9],
            ["000001.SZ", "20240104", 12, 9]
        ]),
    )
    .await;
}

#[tokio::test]
#[should_panic(expected = "缺少复权因子")]
async fn missing_factor_fails_instead_of_silently_switching_to_raw_returns() {
    let server = MockServer::start().await;
    fixture(&server, json!([["000001.SZ", "20240102", 2]])).await;
    provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20240101").unwrap(),
            parse_date("20240104").unwrap(),
        )
        .await;
}

#[tokio::test]
async fn duplicate_dates_and_foreign_symbols_are_rejected() {
    for factors in [
        json!([["000001.SZ", "20240102", 2], ["000001.SZ", "20240102", 2]]),
        json!([["600000.SH", "20240102", 2]]),
        json!([["000001.SZ", "20231231", 2]]),
    ] {
        let server = MockServer::start().await;
        fixture(&server, factors).await;
        let err = tokio::spawn(async move {
            provider(&server)
                .stock_bar(
                    StockSymbol::from("000001"),
                    parse_date("20240101").unwrap(),
                    parse_date("20240104").unwrap(),
                )
                .await;
        })
        .await
        .expect_err("无效行情必须终止查询");
        assert!(err.is_panic());
        let message = err.to_string();
        assert!(message.contains("重复日期") || message.contains("请求范围外"));
    }
}

#[tokio::test]
async fn requests_non_overlapping_year_chunks_without_truncating_history() {
    let server = MockServer::start().await;
    for (start, end, day) in [
        ("20231229", "20231231", "20231229"),
        ("20240101", "20240102", "20240102"),
    ] {
        for (api, fields, items) in [
            (
                "trade_cal",
                json!(["cal_date", "is_open"]),
                json!([[day, 1]]),
            ),
            (
                "daily",
                json!([
                    "ts_code",
                    "trade_date",
                    "open",
                    "high",
                    "low",
                    "close",
                    "vol",
                    "amount"
                ]),
                json!([["000001.SZ", day, 10, 11, 9, 10, 100, 100]]),
            ),
            (
                "adj_factor",
                json!(["ts_code", "trade_date", "adj_factor"]),
                json!([["000001.SZ", day, 2]]),
            ),
            (
                "stk_limit",
                json!(["ts_code", "trade_date", "up_limit", "down_limit"]),
                json!([["000001.SZ", day, 11, 9]]),
            ),
            (
                "stock_st",
                json!(["ts_code", "trade_date"]),
                if day == "20231229" {
                    json!([["000001.SZ", day]])
                } else {
                    json!([])
                },
            ),
        ] {
            Mock::given(path(format!("/{api}")))
                .and(body_partial_json(
                    json!({"params": {"start_date": start, "end_date": end}}),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "code": 0, "data": {"fields": fields, "items": items}
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
    }
    let mut provider = provider(&server);
    let trading_days = provider
        .trading_days(
            parse_date("20231229").unwrap(),
            parse_date("20240102").unwrap(),
        )
        .await;
    let bars = provider
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20231229").unwrap(),
            parse_date("20240102").unwrap(),
        )
        .await;
    assert_eq!(bars.len(), 2);
    assert_eq!(trading_days.len(), 2);
    assert!(bars[0].st);
    assert!(!bars[1].st);
    assert_eq!(server.received_requests().await.unwrap().len(), 10);
}

#[tokio::test]
async fn empty_daily_query_returns_no_bars() {
    let server = MockServer::start().await;
    response(&server, "daily", &[], json!([])).await;
    let bars = provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20240101").unwrap(),
            parse_date("20240104").unwrap(),
        )
        .await;
    assert!(bars.is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
#[should_panic(expected = "2002")]
async fn upstream_permission_error_propagates_to_caller() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 2002, "msg": "没有接口权限", "data": null})),
        )
        .mount(&server)
        .await;
    provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20240101").unwrap(),
            parse_date("20240104").unwrap(),
        )
        .await;
}

#[tokio::test]
async fn lazy_engine_downloads_full_backtest_range_once() {
    let server = MockServer::start().await;
    response(&server, "stock_st", &["ts_code", "trade_date"], json!([])).await;
    response(
        &server,
        "trade_cal",
        &["cal_date", "is_open"],
        json!([["20240102", 1]]),
    )
    .await;
    response(
        &server,
        "daily",
        &[
            "ts_code",
            "trade_date",
            "open",
            "high",
            "low",
            "close",
            "vol",
            "amount",
        ],
        json!([["000001.SZ", "20240102", 10, 11, 9, 10, 1000, 100]]),
    )
    .await;
    response(
        &server,
        "adj_factor",
        &["ts_code", "trade_date", "adj_factor"],
        json!([["000001.SZ", "20240102", 2]]),
    )
    .await;
    response(
        &server,
        "stk_limit",
        &["ts_code", "trade_date", "up_limit", "down_limit"],
        json!([["000001.SZ", "20240102", 11, 9]]),
    )
    .await;
    let result = BacktestEngine::new(BacktestConfig {
        start: parse_date("20240101").unwrap(),
        end: parse_date("20240104").unwrap(),
        ..Default::default()
    })
    .unwrap()
    .run(
        Box::new(provider(&server)),
        Box::new(BuyAndHold::new(BuyAndHoldConfig {
            symbols: vec![StockSymbol::from("000001")],
            allocation: 1.0,
        })),
    )
    .await
    .unwrap();
    assert_eq!(result.trades.len(), 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        5,
        "calendar once, daily/factor/limit/st once each"
    );
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["params"]["start_date"], "20240101");
        assert_eq!(body["params"]["end_date"], "20240104");
    }
}

#[tokio::test]
async fn st_status_is_joined_by_date_without_carrying_it_forward() {
    let server = MockServer::start().await;
    fixture_with_st(
        &server,
        json!([["000001.SZ", "20240102", 2], ["000001.SZ", "20240104", 2]]),
        // 3 日停牌仍在 ST 名单中；4 日摘帽，不能沿用之前的状态。
        json!([["000001.SZ", "20240103"], ["000001.SZ", "20240102"]]),
    )
    .await;
    let bars = provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20240101").unwrap(),
            parse_date("20240104").unwrap(),
        )
        .await;
    assert_eq!(
        bars.iter()
            .map(|bar| (bar.date, bar.st))
            .collect::<Vec<_>>(),
        vec![
            (parse_date("20240102").unwrap(), true),
            (parse_date("20240104").unwrap(), false),
        ]
    );
    let requests = server.received_requests().await.unwrap();
    let st_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/stock_st")
        .collect();
    assert_eq!(st_requests.len(), 1);
    let body: Value = serde_json::from_slice(&st_requests[0].body).unwrap();
    assert_eq!(body["params"]["ts_code"], "000001.SZ");
    assert_eq!(body["params"]["start_date"], "20240101");
    assert_eq!(body["params"]["end_date"], "20240104");
}

#[tokio::test]
async fn invalid_st_rows_are_rejected() {
    for (rows, expected) in [
        (
            json!([["000001.SZ", "20240102"], ["000001.SZ", "20240102"]]),
            "重复日期",
        ),
        (json!([["600000.SH", "20240102"]]), "请求范围外"),
        (json!([["000001.SZ", "20240105"]]), "请求范围外"),
        (json!([["000001.SZ", "invalid-date"]]), "日期"),
        (json!([["000001.SZ", null]]), "stock_st 数据无效"),
    ] {
        let server = MockServer::start().await;
        fixture_with_st(
            &server,
            json!([["000001.SZ", "20240102", 2], ["000001.SZ", "20240104", 2]]),
            rows,
        )
        .await;
        let err = tokio::spawn(async move {
            provider(&server)
                .stock_bar(
                    StockSymbol::from("000001"),
                    parse_date("20240101").unwrap(),
                    parse_date("20240104").unwrap(),
                )
                .await;
        })
        .await
        .expect_err("不能把非法 ST 数据当作非 ST");
        assert!(err.is_panic());
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[tokio::test]
#[should_panic(expected = "stock_st 查询失败")]
async fn st_permission_failure_is_not_treated_as_non_st() {
    let server = MockServer::start().await;
    fixture(
        &server,
        json!([["000001.SZ", "20240102", 2], ["000001.SZ", "20240104", 2]]),
    )
    .await;
    Mock::given(path("/stock_st"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 2002, "msg": "没有 stock_st 接口权限", "data": null
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("20240101").unwrap(),
            parse_date("20240104").unwrap(),
        )
        .await;
}

#[tokio::test]
#[should_panic(expected = "stock_st 仅提供 20000101 起的历史状态")]
async fn dates_before_st_coverage_are_rejected() {
    let server = MockServer::start().await;
    provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            parse_date("19991231").unwrap(),
            parse_date("20000104").unwrap(),
        )
        .await;
}
