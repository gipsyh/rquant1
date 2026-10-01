use time::Date;

pub trait Strategy {
    fn name(&self) -> &str;

    fn handle_trade_day(&mut self, day: Date);
}
