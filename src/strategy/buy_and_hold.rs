use super::Strategy;
use crate::{
    data::StockSymbol,
    engine::{BtContext, Order},
};
use clap::Args;
use std::collections::BTreeSet;

#[derive(Args, Debug, Clone)]
pub struct BuyAndHoldConfig {
    /// 目标股票，可重复指定或用逗号分隔
    #[arg(long = "symbol", required = true, value_delimiter = ',')]
    pub symbols: Vec<StockSymbol>,
    /// 初始资金用于买入的比例，范围 (0, 1]
    #[arg(long, default_value_t = 1.0)]
    pub allocation: f64,
}

impl BuyAndHoldConfig {
    /// 配置无效时直接 panic。
    pub fn validate(&self) {
        assert!(
            !self.symbols.is_empty()
                && self.symbols.iter().collect::<BTreeSet<_>>().len() == self.symbols.len(),
            "目标股票不能为空或包含重复股票"
        );
        assert!(
            self.allocation.is_finite() && self.allocation > 0.0 && self.allocation <= 1.0,
            "仓位比例须在 (0, 1] 内"
        );
    }
}

/// 将初始资金的 allocation 部分等额分给各股票，各自首次买入后一直持有。
/// 某股票未成交时保留其预算并逐日重试，不把预算转给其他股票。
#[derive(Debug, Clone)]
pub struct BuyAndHold {
    config: BuyAndHoldConfig,
}

impl BuyAndHold {
    /// 配置无效时直接 panic。
    pub fn new(config: BuyAndHoldConfig) -> Self {
        config.validate();
        Self { config }
    }
}

#[async_trait::async_trait]
impl Strategy for BuyAndHold {
    fn name(&self) -> &str {
        "buy_and_hold"
    }

    async fn on_trade_day(&mut self, ctx: &BtContext<'_>) -> Vec<Vec<Order>> {
        let cash_amount = ctx.init_cash * self.config.allocation / self.config.symbols.len() as f64;
        let orders = self
            .config
            .symbols
            .iter()
            .copied()
            .filter(|symbol| {
                ctx.position(*symbol)
                    .is_none_or(|p| p.purchased_shares == 0)
            })
            .map(|symbol| Order::BuyAmount {
                symbol,
                cash_amount,
            })
            .collect();
        vec![orders]
    }
}
