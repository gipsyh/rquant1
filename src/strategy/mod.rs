mod buy_and_hold;
use crate::engine::{BtContext, Order};
pub use buy_and_hold::{BuyAndHold, BuyAndHoldConfig};
use clap::Subcommand;

#[derive(Subcommand, Clone, Debug)]
pub enum StrategyConfig {
    /// 首次可成交时买入并持有至回测结束
    #[command(alias = "buy_and_hold")]
    BuyAndHold(BuyAndHoldConfig),
}

impl StrategyConfig {
    pub fn build(self) -> Box<dyn Strategy> {
        match self {
            Self::BuyAndHold(config) => Box::new(BuyAndHold::new(config)),
        }
    }
}

#[async_trait::async_trait]
pub trait Strategy: Send {
    fn name(&self) -> &str;

    /// 每个交易日调用一次，可按需查询历史行情，并为任意股票返回零笔或多笔订单。
    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Order>;
}
