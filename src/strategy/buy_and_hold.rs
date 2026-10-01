use crate::{engine::BtContext, strategy::Strategy};

pub struct BuyAndHold {}

impl Strategy for BuyAndHold {
    fn name(&self) -> &str {
        todo!()
    }

    fn on_trade_day(&mut self, ctx: BtContext) {
        todo!()
    }
}
