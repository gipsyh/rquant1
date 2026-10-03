//! Tushare Pro 客户端与回测数据源适配。
//!
//! 对标 Python `tushare` 包的 Pro 部分 —— `tushare/pro/client.py` 里的 `DataApi`。
//! 那一层本质上只是个 HTTP 客户端：POST `{base}/{api_name}`，body 是
//! `{api_name, token, params, fields}`，返回 `{code, msg, data:{fields, items}}`。
//!
//! 全部方法都是异步的，runtime 由调用方提供（本 crate 不创建 runtime）。
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use rquant::data::tushare::{TushareProvider, params};
//!
//! let client = TushareProvider::new();
//! let cal = client
//!     .trade_cal(
//!         params! { "exchange" => "SSE", "start_date" => "20240101", "end_date" => "20240131" },
//!         "cal_date",
//!     )
//!     .await?;
//!
//! for date in cal.column("cal_date").expect("有 cal_date 列").as_str() {
//!     println!("{:?}", date);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! 未具名封装的接口走 [`TushareProvider::query`]，例如
//! `client.query("moneyflow", params! { "trade_date" => "20240102" }, "").await?`。
//!
//! # 两个实测确认的坑
//!
//! **日期降序。** tushare 对带日期的接口（`daily`、`trade_cal` 等）返回的行是
//! **日期降序**排列的。Python 侧每处 `sort_values("trade_date")` / `sorted(...)`
//! 都是必需的，不是防御性代码。客户端作为薄封装**不做排序**，调用方需自行处理。
//!
//! **数字不区分整数与浮点。** JSON 只有一种数字类型，`trade_cal.is_open` 返回的是
//! 整数 `1`，而 `daily_basic.circ_mv` 返回的是浮点。取值时按字段口径选
//! [`Column::as_i64`] 或 [`Column::as_f64`]；[`Column::as_bool`] 已兼容 `1`/`0`。

mod index;
mod provider;
mod table;

#[cfg(test)]
mod test;

pub use provider::{Params, ParamsExt, RetryPolicy, TushareProvider};
pub use table::{Column, Row, Table};

// `params!` 定义在 provider.rs 且带 #[macro_export]，落在 crate 根；
// 这里重导出，让 `rquant::data::tushare::params!` 这个更收敛的路径也可用。
pub use crate::params;

use super::{StockBoard, StockSymbol};

/// 将通用股票代码转换为 Tushare 格式，例如 `000001.SZ`。
impl StockSymbol {
    pub fn tushare_code(self) -> String {
        let suffix = match self.board {
            StockBoard::ShMain | StockBoard::ShStar => "SH",
            StockBoard::SzMain | StockBoard::SzChiNext => "SZ",
        };
        format!("{:06}.{suffix}", self.id)
    }
}
