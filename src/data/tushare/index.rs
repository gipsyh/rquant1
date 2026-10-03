use super::{TushareProvider, params, provider::api_date};
use crate::data::{IndexComp, IndexHistComp, StockSymbol, index::normalize_index_symbol};
use crate::utils::{DateRange, parse_date};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use time::Date;

#[derive(Deserialize)]
struct IndexWeight {
    index_code: String,
    con_code: String,
    trade_date: String,
    weight: f64,
}

#[derive(Deserialize)]
struct IndexBasic {
    ts_code: String,
    name: String,
}

fn index_code(symbol: &str) -> Result<String> {
    let symbol = normalize_index_symbol(symbol)?;
    let (id, suffix) = symbol.split_once('.').unwrap();
    let suffix = match suffix {
        "XSHG" => "SH",
        "XSHE" => "SZ",
        "INDX" => "CSI",
        _ => unreachable!(),
    };
    Ok(format!("{id}.{suffix}"))
}

impl TushareProvider {
    pub(super) async fn fetch_index_name(&self, symbol: &str) -> Result<String> {
        let code = index_code(symbol)?;
        // market 指发布市场，不能仅由代码后缀判断；先查对应市场，再查其他发布方。
        let preferred = if code.ends_with(".SH") {
            "SSE"
        } else if code.ends_with(".SZ") {
            "SZSE"
        } else {
            "CSI"
        };
        for market in std::iter::once(preferred).chain(
            ["CSI", "SSE", "SZSE", "CICC", "SW", "MSCI", "OTH"]
                .into_iter()
                .filter(|market| *market != preferred),
        ) {
            let rows = self
                .index_basic(
                    params! { "ts_code" => code.clone(), "market" => market },
                    "ts_code,name",
                )
                .await?
                .to_typed::<IndexBasic>()?;
            if rows.is_empty() {
                continue;
            }
            ensure!(
                rows.len() == 1 && rows[0].ts_code == code,
                "指数基础信息返回重复或不匹配的代码: {code}"
            );
            let name = rows.into_iter().next().unwrap().name;
            ensure!(!name.trim().is_empty(), "指数名称不能为空: {code}");
            return Ok(name);
        }
        anyhow::bail!("找不到指数基础信息: {code}")
    }

    pub(super) async fn fetch_index_comp(
        &self,
        symbol: &str,
        start: Date,
        end: Date,
    ) -> Result<IndexHistComp> {
        ensure!(start <= end, "查询开始日期不能晚于结束日期");
        let code = index_code(symbol)?;
        // 多取前一个自然月，供区间起点查询最近已生效的快照。
        let first = start.replace_day(1)?;
        let mut cursor = first
            .previous_day()
            .map_or(first, |date| date.replace_day(1).unwrap());
        let mut snapshots: BTreeMap<Date, HashMap<StockSymbol, f32>> = BTreeMap::new();
        loop {
            let month_end = cursor.replace_day(cursor.month().length(cursor.year()))?;
            let table = self
                .index_weight(
                    params! {
                        "index_code" => code.clone(),
                        "start_date" => api_date(cursor),
                        "end_date" => api_date(month_end),
                        "limit" => 6000,
                    },
                    "index_code,con_code,trade_date,weight",
                )
                .await?;
            // 与 Python 版本一致：达到上限时拒绝可能截断的月份，不缓存部分成分。
            ensure!(
                table.len() < 6000,
                "{code} {cursor} 返回达到 6000 行上限，可能被截断"
            );
            for row in table.to_typed::<IndexWeight>()? {
                let date = parse_date(&row.trade_date)?;
                ensure!(
                    row.index_code == code && date >= cursor && date <= month_end,
                    "指数接口返回请求范围外数据 {} {date}",
                    row.index_code
                );
                // 请求整月但只保留截止日及以前的数据，避免使用未来成分。
                if date > end {
                    continue;
                }
                ensure!(
                    (0.0..=100.0).contains(&row.weight),
                    "{code} {date} 无效指数权重 {}",
                    row.weight
                );
                let stock = row.con_code.parse::<StockSymbol>()?;
                let weights = snapshots.entry(date).or_default();
                ensure!(
                    weights.insert(stock, (row.weight / 100.0) as f32).is_none(),
                    "{code} {date} 存在重复成分 {stock}"
                );
            }
            if month_end >= end {
                break;
            }
            cursor = month_end.next_day().expect("日期溢出");
        }
        let hist = snapshots
            .into_iter()
            .map(|(date, weight)| Ok((date, Arc::new(IndexComp::new(weight)?))))
            .collect::<Result<Vec<_>>>()?;
        IndexHistComp::new(DateRange::new(start, end), hist)?.slice(start, end)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::data::{DataProvider, tushare::RetryPolicy};
    use serde_json::{Value, json};
    use time::macros::date;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, path},
    };

    async fn month(server: &MockServer, start: &str, end: &str, rows: Value) {
        Mock::given(path("/index_weight"))
            .and(body_partial_json(json!({"params": {
                "index_code": "000300.SH", "start_date": start, "end_date": end, "limit": 6000,
            }, "fields": "index_code,con_code,trade_date,weight"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {
                    "fields": ["index_code", "con_code", "trade_date", "weight"], "items": rows,
                }})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    fn provider(server: &MockServer) -> TushareProvider {
        TushareProvider::new()
            .with_base_url(server.uri())
            .with_retry(RetryPolicy::none())
    }

    #[tokio::test]
    async fn index_name_uses_requested_code_and_falls_back_to_publisher_market() {
        let server = MockServer::start().await;
        for (market, rows) in [
            ("SSE", json!([])),
            ("CSI", json!([["000300.SH", "沪深300"]])),
        ] {
            Mock::given(path("/index_basic"))
                .and(body_partial_json(json!({"params": {"ts_code": "000300.SH", "market": market}, "fields": "ts_code,name"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {
                    "fields": ["ts_code", "name"], "items": rows,
                }})))
                .expect(1).mount(&server).await;
        }
        assert_eq!(provider(&server).index_name("000300.XSHG").await, "沪深300");
    }

    #[tokio::test]
    async fn index_name_rejects_missing_empty_mismatched_and_duplicate_metadata() {
        for rows in [
            json!([]),
            json!([["000300.SH", ""]]),
            json!([["000300.SH", "  "]]),
            json!([["000905.SH", "中证500"]]),
            json!([["000300.SH", "沪深300"], ["000300.SH", "沪深300"]]),
            json!([["000300.SH", null]]),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/index_basic"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {
                        "fields": ["ts_code", "name"], "items": rows,
                    }})),
                )
                .mount(&server)
                .await;
            assert!(provider(&server).fetch_index_name("000300").await.is_err());
        }
        let server = MockServer::start().await;
        Mock::given(path("/index_basic"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 2002, "msg": "没有接口权限"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            provider(&server)
                .fetch_index_name("000300")
                .await
                .unwrap_err()
                .to_string()
                .contains("没有接口权限")
        );
    }

    #[tokio::test]
    async fn monthly_history_keeps_baseline_all_snapshots_and_proportional_weights() {
        let server = MockServer::start().await;
        month(
            &server,
            "20231201",
            "20231231",
            json!([["000300.SH", "600000.SH", "20231229", 0.5],]),
        )
        .await;
        month(
            &server,
            "20240101",
            "20240131",
            json!([
                ["000300.SH", "600000.SH", "20240131", 80.0],
                ["000300.SH", "000001.SZ", "20240110", 0.0],
                ["000300.SH", "600000.SH", "20240110", 100.0],
            ]),
        )
        .await;
        month(
            &server,
            "20240201",
            "20240229",
            json!([["000300.SH", "600000.SH", "20240229", 10.0],]),
        )
        .await;
        let hist = provider(&server)
            .index_comp("000300.XSHG", date!(2024 - 01 - 05), date!(2024 - 02 - 15))
            .await;
        assert_eq!(
            hist.range(),
            DateRange::new(date!(2024 - 01 - 05), date!(2024 - 02 - 15))
        );
        assert_eq!(
            hist.snapshots()
                .iter()
                .map(|(date, _)| *date)
                .collect::<Vec<_>>(),
            vec![
                date!(2023 - 12 - 29),
                date!(2024 - 01 - 10),
                date!(2024 - 01 - 31)
            ]
        );
        let stock = StockSymbol::from("600000.SH");
        assert_eq!(
            hist.composition(date!(2024 - 01 - 05)).unwrap().weights()[&stock],
            0.005
        );
        let middle = hist.composition(date!(2024 - 01 - 10)).unwrap();
        assert_eq!(middle.weights().len(), 2);
        assert_eq!(middle.weights()[&StockSymbol::from("000001.SZ")], 0.0);
        assert_eq!(
            hist.composition(date!(2024 - 02 - 15)).unwrap().weights()[&stock],
            0.8
        );
    }

    #[tokio::test]
    async fn rejects_invalid_rows_duplicate_members_and_truncated_months() {
        let row = json!(["000300.SH", "600000.SH", "20231229", 50.0]);
        let cases = [
            (json!([row.clone(), row.clone()]), "重复成分"),
            (
                json!([["000905.SH", "600000.SH", "20231229", 50.0]]),
                "请求范围外",
            ),
            (
                json!([["000300.SH", "600000.SH", "20231130", 50.0]]),
                "请求范围外",
            ),
            (
                json!([["000300.SH", "600000.SH", "20231229", -1.0]]),
                "无效指数权重",
            ),
            (
                json!([["000300.SH", "600000.SH", "20231229", 100.1]]),
                "无效指数权重",
            ),
            (
                json!([["000300.SH", "600000.SH", "20231229", null]]),
                "反序列化",
            ),
            (Value::Array(vec![row; 6000]), "6000 行上限"),
        ];
        for (rows, expected) in cases {
            let server = MockServer::start().await;
            month(&server, "20231201", "20231231", rows).await;
            let err = provider(&server)
                .fetch_index_comp("000300", date!(2024 - 01 - 01), date!(2024 - 01 - 31))
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{err:#}");
        }
    }

    #[tokio::test]
    async fn missing_history_stays_empty_and_api_errors_propagate() {
        let server = MockServer::start().await;
        month(&server, "20231201", "20231231", json!([])).await;
        month(&server, "20240101", "20240131", json!([])).await;
        let hist = provider(&server)
            .fetch_index_comp("000300", date!(2024 - 01 - 01), date!(2024 - 01 - 31))
            .await
            .unwrap();
        assert!(hist.snapshots().is_empty());
        assert!(hist.composition(date!(2024 - 01 - 01)).is_err());

        let server = MockServer::start().await;
        Mock::given(path("/index_weight"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 2002, "msg": "没有接口权限"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = provider(&server)
            .fetch_index_comp("000300", date!(2024 - 01 - 01), date!(2024 - 01 - 31))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("没有接口权限"));
    }
}
