mod buy_and_hold;

use crate::engine::BtContext;

pub trait Strategy {
    fn name(&self) -> &str;

    fn on_trade_day(&mut self, ctx: BtContext);
}
