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

use super::provider::is_retryable;
use crate::data::{DataProvider, StockSymbol};
use crate::engine::{BacktestConfig, BacktestEngine};
use crate::strategy::{BuyAndHold, BuyAndHoldConfig};
use crate::utils::{DateRange, parse_date};
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Column, RetryPolicy, Table, TushareProvider, params};

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

// =====================================================================
// 错误路径
// =====================================================================

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
fn 空表按列取名与按行列取单元格() {
    let empty = Table::new(vec!["ts_code".into()], vec![]).unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.fields(), ["ts_code"]);

    let table = sample_table();
    let column: Column<'_> = table.column("close").expect("有 close 列");
    assert_eq!(column.name(), "close");
    assert_eq!(column.len(), 2);
    assert!(table.column("不存在").is_none());

    assert_eq!(table.get(0, "close"), Some(&json!(10.5)));
    assert_eq!(table.get(1, "close"), Some(&Value::Null));
    assert_eq!(table.get(9, "close"), None);
    assert_eq!(
        table.row(0).unwrap().get("ts_code"),
        Some(&json!("000001.SZ"))
    );
    assert_eq!(table.row(0).unwrap().at(2), Some(&json!(1200)));
}

#[test]
fn as_f64_把_null_映射成_none_并宽松解析数字字符串() {
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
fn 字符串列访问器_as_str_与_as_string() {
    let table = sample_table();
    assert_eq!(
        table.column("ts_code").unwrap().as_str(),
        vec![Some("000001.SZ"), Some("000002.SZ")]
    );
    // close 是数字，as_str 应为 None（数字不是字符串）。
    assert_eq!(table.column("close").unwrap().as_str(), vec![None, None]);

    // as_string 对标 astype(str)：数字也转成字符串，整数形式不带小数点。
    assert_eq!(
        table.column("close").unwrap().as_string(),
        vec![Some("10.5".to_string()), None]
    );
    assert_eq!(
        table.column("vol").unwrap().as_string(),
        vec![Some("1200".to_string()), Some("3400".to_string())]
    );
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

    // records() 对标 to_dict("records")。
    let records = sample_table().records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["ts_code"], "000001.SZ");
    assert_eq!(records[1]["close"], Value::Null);
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

    // None 序列化成 JSON null，与 Python 传 None 一致。
    let end: Option<&str> = None;
    let p = params! { "ts_code" => "000001.SZ", "end_date" => end };
    assert_eq!(p["end_date"], Value::Null);
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
            DateRange::new(
                parse_date("20240101").unwrap(),
                parse_date("20240104").unwrap(),
            ),
        )
        .await
        .into_bars();
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
                    DateRange::new(
                        parse_date("20240101").unwrap(),
                        parse_date("20240104").unwrap(),
                    ),
                )
                .await
                .into_bars();
        })
        .await
        .expect_err("无效行情必须终止查询");
        assert!(err.is_panic());
        let message = err.to_string();
        assert!(message.contains("重复日期") || message.contains("请求范围外"));
    }
}

#[tokio::test]
async fn requests_full_stock_history_without_year_chunks() {
    let server = MockServer::start().await;
    for (api, fields, items) in [
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
            json!([
                ["000001.SZ", "20260928", 10, 11, 9, 10, 100, 100],
                ["000001.SZ", "20230103", 10, 11, 9, 10, 100, 100]
            ]),
        ),
        (
            "adj_factor",
            json!(["ts_code", "trade_date", "adj_factor"]),
            json!([["000001.SZ", "20260928", 2], ["000001.SZ", "20230103", 1]]),
        ),
        (
            "stk_limit",
            json!(["ts_code", "trade_date", "up_limit", "down_limit"]),
            json!([
                ["000001.SZ", "20260928", 11, 9],
                ["000001.SZ", "20230103", 11, 9]
            ]),
        ),
        (
            "stock_st",
            json!(["ts_code", "trade_date"]),
            json!([["000001.SZ", "20230103"]]),
        ),
    ] {
        Mock::given(path(format!("/{api}")))
            .and(body_partial_json(json!({"params": {
                "ts_code": "000001.SZ", "start_date": "20230101", "end_date": "20260928"
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "data": {"fields": fields, "items": items}
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let bars = provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            DateRange::new(
                parse_date("20230101").unwrap(),
                parse_date("20260928").unwrap(),
            ),
        )
        .await
        .into_bars();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].date, parse_date("20230103").unwrap());
    assert_eq!(bars[1].date, parse_date("20260928").unwrap());
    assert!(bars[0].st);
    assert!(!bars[1].st);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn row_limits_panic_at_boundary_without_retry_or_pagination() {
    for (api, max_rows) in [
        ("daily", 6000),
        ("daily_basic", 6000),
        ("stk_limit", 5800),
        ("stock_basic", 6000),
        ("suspend_d", 5000),
        ("index_basic", 8000),
        ("index_weight", 6000),
    ] {
        for row_count in [max_rows - 1, max_rows, max_rows + 1] {
            let server = MockServer::start().await;
            response(
                &server,
                api,
                &["ts_code"],
                Value::Array(vec![json!(["000001.SZ"]); row_count]),
            )
            .await;
            // 保持重试开启，确认行数上限直接 panic 而不进入重试。
            let provider = TushareProvider::new().with_base_url(server.uri());
            let result = tokio::spawn(async move {
                provider.query(api, params! {
                    "ts_code" => "000001.SZ", "start_date" => "20230101", "end_date" => "20260928",
                }, "ts_code").await.unwrap()
            })
            .await;
            if row_count < max_rows {
                assert_eq!(result.unwrap().len(), row_count);
            } else {
                let err = result.unwrap_err();
                assert!(err.is_panic());
                let message = err.to_string();
                for expected in [
                    api,
                    "000001.SZ",
                    "20230101",
                    "20260928",
                    &format!("{max_rows} 行上限"),
                ] {
                    assert!(message.contains(expected), "{message}");
                }
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn stock_st_splits_only_at_limit_and_merges_complete_history() {
    use std::collections::BTreeSet;
    use time::{Duration as Days, macros::date};

    for count in [0, 999, 1000, 2001] {
        let server = MockServer::start().await;
        let start = date!(2019 - 01 - 01);
        let end = start + Days::days((count.max(1) - 1) as i64);
        let api_date = |d: time::Date| d.to_string().replace('-', "");
        Mock::given(path("/stock_st"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["fields"], "ts_code,trade_date");
                assert_eq!(body["params"]["ts_code"], "002005.SZ");
                assert_eq!(body["params"]["custom"], "preserved");
                let first = parse_date(body["params"]["start_date"].as_str().unwrap()).unwrap();
                let last = parse_date(body["params"]["end_date"].as_str().unwrap()).unwrap();
                let rows: Vec<_> = (0..count)
                    .rev()
                    .map(|i| start + Days::days(i as i64))
                    .filter(|date| *date >= first && *date <= last)
                    .take(1000)
                    .map(|date| json!(["002005.SZ", api_date(date)]))
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({
                    "code": 0, "data": {"fields": ["ts_code", "trade_date"], "items": rows}
                }))
            })
            .mount(&server)
            .await;
        let provider = client_for(&server, fast_retry()).await;
        let params = params! {
            "ts_code" => "002005.SZ", "start_date" => api_date(start),
            "end_date" => api_date(end), "custom" => "preserved",
        };
        for direct in [false, true] {
            let table = if direct {
                provider
                    .query("stock_st", params.clone(), "ts_code,trade_date")
                    .await
            } else {
                provider
                    .stock_st(params.clone(), "ts_code,trade_date")
                    .await
            }
            .unwrap();
            assert_eq!(table.len(), count);
            let actual: BTreeSet<_> = table
                .column("trade_date")
                .unwrap()
                .as_str()
                .into_iter()
                .map(|v| v.unwrap().to_owned())
                .collect();
            let expected: BTreeSet<_> = (0..count)
                .map(|i| api_date(start + Days::days(i as i64)))
                .collect();
            assert_eq!(actual, expected, "拆分边界不得遗漏或重复");
        }
        let expected_calls = match count {
            0 | 999 => 1,
            1000 => 3,
            2001 => 7,
            _ => unreachable!(),
        };
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            expected_calls * 2
        );
    }
}

#[tokio::test]
async fn stock_st_panics_when_date_range_cannot_be_split() {
    for (extra, expected) in [
        (json!({}), "缺少日期参数"),
        (
            json!({"start_date": "bad", "end_date": "20240102"}),
            "日期无效",
        ),
        (
            json!({"start_date": "20240101", "end_date": "20240101"}),
            "不能继续拆分",
        ),
        (
            json!({"start_date": "20240102", "end_date": "20240101"}),
            "不能继续拆分",
        ),
        (json!({"trade_date": "20240101"}), "含 trade_date"),
        (json!({"limit": 1000}), "含 limit"),
        (json!({"offset": 0}), "含 offset"),
    ] {
        let server = MockServer::start().await;
        response(
            &server,
            "stock_st",
            &["ts_code"],
            json!(vec![vec!["002005.SZ"]; 1000]),
        )
        .await;
        let provider = client_for(&server, fast_retry()).await;
        let error = tokio::spawn(async move {
            provider
                .stock_st(extra.as_object().unwrap().clone(), "ts_code")
                .await
        })
        .await
        .unwrap_err();
        assert!(error.is_panic());
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn stock_st_does_not_return_partial_history_on_child_failure() {
    for inconsistent_fields in [false, true] {
        let server = MockServer::start().await;
        Mock::given(path("/stock_st"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let first = body["params"]["start_date"].as_str().unwrap();
                let last = body["params"]["end_date"].as_str().unwrap();
                let body = if first != last {
                    json!({"code": 0, "data": {"fields": ["trade_date"], "items": vec![vec!["20240102"]; 1000]}})
                } else if first == "20240101" {
                    json!({"code": 0, "data": {"fields": ["trade_date"], "items": [["20240101"]]}})
                } else if inconsistent_fields {
                    json!({"code": 0, "data": {"fields": ["wrong_field"], "items": [["20240102"]]}})
                } else {
                    json!({"code": 2002, "msg": "没有接口权限"})
                };
                ResponseTemplate::new(200).set_body_json(body)
            }).mount(&server).await;
        let error = client_for(&server, fast_retry())
            .await
            .stock_st(
                params! {"start_date" => "20240101", "end_date" => "20240102"},
                "trade_date",
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(if inconsistent_fields {
            "字段不一致"
        } else {
            "没有接口权限"
        }));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}

#[test]
fn split_tables_allow_fieldless_empty_responses() {
    let data = Table::new(vec!["date".into()], vec![vec![json!("20240101")]]).unwrap();
    let mut merged = Table::new(vec![], vec![]).unwrap();
    merged.append(data.clone()).unwrap();
    merged.append(Table::new(vec![], vec![]).unwrap()).unwrap();
    assert_eq!(merged, data);
}

#[tokio::test]
#[ignore = "会真实联网并消耗接口额度"]
async fn online_stock_st_long_range_matches_yearly_queries() {
    use std::collections::BTreeSet;
    let provider = TushareProvider::new();
    let table = provider
        .stock_st(
            params! {
                "ts_code" => "002005.SZ", "start_date" => "20190101", "end_date" => "20260928",
            },
            "ts_code,trade_date",
        )
        .await
        .unwrap();
    let dates = |table: &Table| -> BTreeSet<String> {
        assert!(
            table
                .column("ts_code")
                .unwrap()
                .as_str()
                .iter()
                .all(|s| *s == Some("002005.SZ"))
        );
        table
            .column("trade_date")
            .unwrap()
            .as_str()
            .into_iter()
            .map(|v| v.unwrap().to_owned())
            .collect()
    };
    let actual = dates(&table);
    assert_eq!(table.len(), actual.len(), "存在重复的 ST 日期");
    assert!(
        table.len() >= 1000,
        "此次在线样本未触及上限，无法验证拆分恢复"
    );
    let mut expected = BTreeSet::new();
    for year in 2019..=2026 {
        let rows = provider.stock_st(params! {
            "ts_code" => "002005.SZ", "start_date" => format!("{year}0101"),
            "end_date" => if year == 2026 { "20260928".into() } else { format!("{year}1231") },
        }, "ts_code,trade_date").await.unwrap();
        assert!(rows.len() < 1000);
        expected.extend(dates(&rows));
    }
    assert_eq!(actual, expected);
    eprintln!(
        "002005.SZ / 20190101..=20260928: {} 条 ST 记录，与逐年独立查询一致",
        table.len()
    );
}

#[tokio::test]
async fn empty_daily_query_returns_no_bars() {
    let server = MockServer::start().await;
    response(&server, "daily", &[], json!([])).await;
    let range = DateRange::new(
        parse_date("20240101").unwrap(),
        parse_date("20240104").unwrap(),
    );
    let hist = provider(&server)
        .stock_bar(StockSymbol::from("000001"), range)
        .await;
    assert_eq!(hist.range(), range);
    assert!(hist.bars().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn lazy_engine_downloads_full_backtest_range_once() {
    let server = MockServer::start().await;
    response(
        &server,
        "stock_basic",
        &["ts_code", "name", "list_date", "delist_date", "industry"],
        json!([["000001.SZ", "平安银行", "19910403", null, "银行"]]),
    )
    .await;
    response(&server, "stock_st", &["ts_code", "trade_date"], json!([])).await;
    response(
        &server,
        "trade_cal",
        &["cal_date", "is_open"],
        json!([["20240102", 1], ["20240103", 1]]),
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
        json!([
            ["000001.SZ", "20240102", 10, 11, 9, 10, 1000, 100],
            ["000001.SZ", "20240103", 10, 11, 9, 10, 1000, 100]
        ]),
    )
    .await;
    response(
        &server,
        "adj_factor",
        &["ts_code", "trade_date", "adj_factor"],
        json!([["000001.SZ", "20240102", 2], ["000001.SZ", "20240103", 2]]),
    )
    .await;
    response(
        &server,
        "stk_limit",
        &["ts_code", "trade_date", "up_limit", "down_limit"],
        json!([
            ["000001.SZ", "20240102", 11, 9],
            ["000001.SZ", "20240103", 11, 9]
        ]),
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
    assert_eq!(result.trades[0].date, parse_date("20240103").unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        6,
        "calendar and stock info once, daily/factor/limit/st once each"
    );
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["api_name"] == "stock_basic" {
            assert_eq!(body["params"]["ts_code"], "000001.SZ");
            continue;
        }
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
            DateRange::new(
                parse_date("20240101").unwrap(),
                parse_date("20240104").unwrap(),
            ),
        )
        .await
        .into_bars();
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
                    DateRange::new(
                        parse_date("20240101").unwrap(),
                        parse_date("20240104").unwrap(),
                    ),
                )
                .await
                .into_bars();
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
            DateRange::new(
                parse_date("20240101").unwrap(),
                parse_date("20240104").unwrap(),
            ),
        )
        .await
        .into_bars();
}

#[tokio::test]
#[should_panic(expected = "stock_st 仅提供 20000101 起的历史状态")]
async fn dates_before_st_coverage_are_rejected() {
    let server = MockServer::start().await;
    provider(&server)
        .stock_bar(
            StockSymbol::from("000001"),
            DateRange::new(
                parse_date("19991231").unwrap(),
                parse_date("20000104").unwrap(),
            ),
        )
        .await
        .into_bars();
}

#[tokio::test]
async fn stock_info_rejects_missing_duplicate_and_invalid_metadata() {
    let valid = json!(["000001.SZ", "测试股票", "19910403", null, null]);
    for rows in [
        json!([]),
        json!([valid.clone(), valid]),
        json!([["600000.SH", "其他股票", "19910403", null, null]]),
        json!([["000001.SZ", " ", "19910403", null, null]]),
        json!([["000001.SZ", "测试股票", "invalid", null, null]]),
        json!([["000001.SZ", "测试股票", "19910403", "19900101", null]]),
    ] {
        let server = MockServer::start().await;
        response(
            &server,
            "stock_basic",
            &["ts_code", "name", "list_date", "delist_date", "industry"],
            rows,
        )
        .await;
        let mut source = provider(&server);
        assert!(
            tokio::spawn(async move { source.stock_info("000001".into()).await })
                .await
                .unwrap_err()
                .is_panic()
        );
    }
}

#[tokio::test]
async fn stocks_bars_runs_four_downloads_and_respects_shared_http_limit() {
    use crate::data::MemCacheProvider;
    // HTTP 上限高于 4 时仍只启动 4 只；低于 4 时继续服从共享信号量。
    for http_limit in [8, 2] {
        let server = MockServer::start().await;
        Mock::given(path("/stock_basic"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = request.body_json().unwrap();
                ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {
                    "fields": ["ts_code", "name", "list_date", "delist_date", "industry"],
                    "items": body["params"]["ts_code"].as_str().unwrap().split(',')
                        .map(|code| json!([code, "测试", "20000101", null, null])).collect::<Vec<_>>()
                }}))
            })
            .mount(&server)
            .await;
        Mock::given(path("/daily"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(10))
                    .set_body_json(json!({"code": 0, "data": {"fields": [], "items": []}})),
            )
            .mount(&server)
            .await;
        let start = parse_date("20250101").unwrap();
        let end = parse_date("20260928").unwrap();
        let provider = TushareProvider::new()
            .with_base_url(server.uri())
            .with_retry(RetryPolicy {
                max_concurrency: http_limit,
                ..RetryPolicy::none()
            });
        // 穿过两层缓存，验证批量调用未退化成逐只加锁/下载。
        let mut cache = MemCacheProvider::new(
            Box::new(MemCacheProvider::new(
                Box::new(provider),
                DateRange::new(start, end),
            )),
            DateRange::new(start, end),
        );
        let requests: Vec<_> = (1..=6)
            .map(|i| {
                (
                    StockSymbol::from(format!("{i:06}.SZ").as_str()),
                    DateRange::new(start, start),
                )
            })
            .collect();
        let batch = cache.stocks_bar(&requests);
        tokio::pin!(batch);
        let expected = 4.min(http_limit);
        tokio::select! {
            _ = &mut batch => panic!("延迟响应不应提前完成"),
            _ = async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let received = server.received_requests().await.unwrap();
                        if received.iter().filter(|r| r.url.path() == "/daily").count() >= expected { break; }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.expect("多个股票的下载没有同时启动");
                tokio::time::sleep(Duration::from_millis(30)).await;
                let received = server.received_requests().await.unwrap();
                let daily: Vec<_> = received.iter().filter(|r| r.url.path() == "/daily").collect();
                assert_eq!(daily.len(), expected);
                for request in daily {
                    let body: Value = request.body_json().unwrap();
                    assert_eq!(body["params"]["start_date"], "20250101");
                    assert_eq!(body["params"]["end_date"], "20260928");
                }
            } => {}
        }
        // 丢弃整批 future 会取消未完成下载，无后台任务继续写缓存。
    }
}

#[tokio::test]
async fn stocks_bars_preserves_request_order_and_empty_results() {
    let server = MockServer::start().await;
    for api in ["daily", "adj_factor", "stk_limit", "stock_st"] {
        Mock::given(path(format!("/{api}")))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = request.body_json().unwrap();
                let symbol = &body["params"]["ts_code"];
                let date = &body["params"]["start_date"];
                let (fields, items) = match api {
                    "daily" if symbol == "000002.SZ" => (json!([]), json!([])),
                    "daily" => (
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
                        json!([[symbol, date, 10, 11, 9, 10, 100, 100]]),
                    ),
                    "adj_factor" => (
                        json!(["ts_code", "trade_date", "adj_factor"]),
                        json!([[symbol, date, 2]]),
                    ),
                    "stk_limit" => (
                        json!(["ts_code", "trade_date", "up_limit", "down_limit"]),
                        json!([[symbol, date, 11, 9]]),
                    ),
                    _ => (json!(["ts_code", "trade_date"]), json!([])),
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "data": {"fields": fields, "items": items}}))
            })
            .mount(&server)
            .await;
    }
    let day = parse_date("20250102").unwrap();
    let symbols: Vec<_> = ["000003", "000002", "000001"].map(StockSymbol::from).into();
    let requests: Vec<_> = symbols
        .iter()
        .map(|&s| (s, DateRange::new(day, day)))
        .collect();
    let mut provider = provider(&server);
    let results = provider.stocks_bar(&requests).await;
    assert_eq!(results.len(), 3);
    assert!(results[1].bars().is_empty());
    assert!(
        results
            .iter()
            .all(|hist| hist.range() == DateRange::new(day, day))
    );
    for (i, &symbol) in symbols.iter().enumerate() {
        assert_eq!(
            results[i].bars(),
            provider
                .stock_bar(symbol, DateRange::new(day, day))
                .await
                .into_bars()
        );
    }
    assert!(provider.stocks_bar(&[]).await.is_empty());
}

async fn stock_info_batch_response(server: &MockServer, codes: &str, status: &str, rows: Value) {
    Mock::given(path("/stock_basic"))
        .and(body_partial_json(
            json!({"params": {"ts_code": codes, "list_status": status}}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {
                "fields": ["ts_code", "name", "list_date", "delist_date", "industry"],
                "items": rows
            }})),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn stocks_info_uses_one_request_and_restores_input_order_and_duplicates() {
    let server = MockServer::start().await;
    stock_info_batch_response(
        &server,
        "000001.SZ,600000.SH",
        "L",
        json!([
            ["600000.SH", "浦发银行", "19991110", null, "银行"],
            ["000001.SZ", "平安银行", "19910403", null, "银行"]
        ]),
    )
    .await;
    let a = StockSymbol::from("000001.SZ");
    let b = StockSymbol::from("600000.SH");
    let mut source = provider(&server);
    assert!(source.stocks_info(&[]).await.is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
    let results = source.stocks_info(&[b, a, b]).await;
    assert_eq!(
        results.iter().map(|s| s.symbol).collect::<Vec<_>>(),
        vec![b, a, b]
    );
    assert_eq!(results[0].name, "浦发银行");
    assert_eq!(results[1].name, "平安银行");
    assert_eq!(results[0], results[2]);
    assert!(results.iter().all(|s| s.bars.is_none()));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn stocks_info_queries_only_unresolved_codes_in_each_status() {
    let server = MockServer::start().await;
    stock_info_batch_response(
        &server,
        "000001.SZ,000002.SZ,600000.SH",
        "L",
        json!([["000001.SZ", "上市股票", "19910403", null, null]]),
    )
    .await;
    stock_info_batch_response(
        &server,
        "000002.SZ,600000.SH",
        "D",
        json!([["600000.SH", "退市股票", "19991110", "20240101", null]]),
    )
    .await;
    stock_info_batch_response(
        &server,
        "000002.SZ",
        "P",
        json!([["000002.SZ", "暂停上市股票", "19910129", null, null]]),
    )
    .await;
    let symbols = ["000002.SZ", "600000.SH", "000001.SZ"].map(StockSymbol::from);
    let results = provider(&server).stocks_info(&symbols).await;
    assert_eq!(
        results.iter().map(|s| s.symbol).collect::<Vec<_>>(),
        symbols
    );
    assert_eq!(results[0].name, "暂停上市股票");
    assert_eq!(results[1].delisted, Some(parse_date("20240101").unwrap()));
    assert_eq!(results[2].name, "上市股票");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
#[should_panic(expected = "找不到已上市股票的基础信息: 000002.SZ")]
async fn stocks_info_rejects_partial_results_instead_of_returning_incomplete_batch() {
    let server = MockServer::start().await;
    stock_info_batch_response(
        &server,
        "000001.SZ,000002.SZ",
        "L",
        json!([["000001.SZ", "上市股票", "19910403", null, null]]),
    )
    .await;
    for status in ["D", "P"] {
        stock_info_batch_response(&server, "000002.SZ", status, json!([])).await;
    }
    provider(&server)
        .stocks_info(&["000001.SZ".into(), "000002.SZ".into()])
        .await;
}
