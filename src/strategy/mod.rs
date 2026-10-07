mod adaptive_rotation;
mod buy_and_hold;
mod low_turnover_trend;
mod ma_cross;
use crate::engine::{BtContext, Order, OrderFailure};
pub use adaptive_rotation::{AdaptiveRotation, AdaptiveRotationConfig};
pub use buy_and_hold::{BuyAndHold, BuyAndHoldConfig};
use clap::Subcommand;
pub use low_turnover_trend::{LowTurnoverTrend, LowTurnoverTrendConfig};
pub use ma_cross::{MaCross, MaCrossConfig};

#[derive(Subcommand, Clone, Debug)]
pub enum StrategyConfig {
    /// 首次可成交时买入并持有至回测结束
    #[command(alias = "buy_and_hold")]
    BuyAndHold(BuyAndHoldConfig),
    /// 短期均线上穿长期均线买入，下穿时清仓
    #[command(alias = "ma_cross")]
    MaCross(MaCrossConfig),
    /// 按低成交额选股，趋势状态控制新增股票的现金买入比例
    #[command(alias = "low_turnover_trend")]
    LowTurnoverTrend(LowTurnoverTrendConfig),
    /// 低成交额股票池反转轮动，趋势恶化时清仓
    #[command(alias = "adaptive_rotation")]
    AdaptiveRotation(AdaptiveRotationConfig),
}

impl StrategyConfig {
    pub fn build(self) -> Box<dyn Strategy> {
        match self {
            Self::BuyAndHold(config) => Box::new(BuyAndHold::new(config)),
            Self::MaCross(config) => Box::new(MaCross::new(config)),
            Self::LowTurnoverTrend(config) => Box::new(LowTurnoverTrend::new(config)),
            Self::AdaptiveRotation(config) => Box::new(AdaptiveRotation::new(config)),
        }
    }
}

#[async_trait::async_trait]
pub trait Strategy: Send {
    fn name(&self) -> &str;

    /// 每个交易日收盘后调用。外层批次在下一交易日开盘依次结算，内层订单并行撮合。
    /// 同批共享批次开始时的现金与可卖股数，不使用同批卖出所得；末日订单不执行。
    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>>;

    /// 失败即结束，不自动重试。默认不处理；回调不能向当前批次追加订单。
    async fn on_order_failed(&mut self, _failure: &OrderFailure) {}
}
