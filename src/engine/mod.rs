mod rbt;

use std::collections::HashMap;

use crate::data::{DataProvider, InstrSymbol};
use time::Date;

pub trait Engine {}

/// Backtest Context
pub struct BtContext {
    date: Date,
    provider: Box<dyn DataProvider>,
}

impl BtContext {
    /// 股票占比调仓
    pub fn rebalance(instr: HashMap<InstrSymbol, f64>) {
        todo!()
    }
}
