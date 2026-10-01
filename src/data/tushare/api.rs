//! 具名接口方法。
//!
//! Python 用 `__getattr__` 把任意 api_name 变成方法（`client.py:54-55` 的
//! `partial(self.query, name)`）。Rust 没有动态属性，改用宏生成具名方法。
//! 未在此列出的接口仍可走 [`TushareClient::query`] —— 那正是 `__getattr__` 的等价物。

use super::client::{Params, TushareClient};
use super::error::TushareError;
use super::table::Table;

macro_rules! tushare_apis {
    ($($(#[$attr:meta])* $name:ident),* $(,)?) => {
        impl TushareClient {
            $(
                $(#[$attr])*
                ///
                /// 与 [`TushareClient::query`] 等价，只是固定了 `api_name`。
                pub async fn $name(
                    &self,
                    params: Params,
                    fields: &str,
                ) -> Result<Table, TushareError> {
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
