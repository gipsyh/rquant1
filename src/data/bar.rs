use super::{Adjustment, StockBar};

impl StockBar {
    /// 原始价格乘当日因子，返回策略指标用的 `FactorAdjusted` 日线。
    /// OHLC 与涨跌停价使用相同倍率，成交量、成交额、市值等字段保持不变。
    /// 仅依赖本根日线；缺失有效因子、价格无效或已复权时 panic。
    #[must_use]
    pub fn adjusted(self) -> Self {
        let factor = match self.adjustment {
            Some(Adjustment::Raw(f)) if f.is_finite() && f > 0.0 => f,
            _ => panic!(
                "指标计算需要有效的原始复权因子: {} {}",
                self.symbol, self.date
            ),
        };
        let adjust = |price: f64| {
            assert!(price.is_finite() && price > 0.0, "价格无效");
            let price = price * factor;
            assert!(price.is_finite() && price > 0.0, "复权价格计算溢出或下溢");
            price
        };
        Self {
            open: adjust(self.open),
            high: adjust(self.high),
            low: adjust(self.low),
            close: adjust(self.close),
            limit_up: self.limit_up.map(adjust),
            limit_down: self.limit_down.map(adjust),
            adjustment: Some(Adjustment::FactorAdjusted),
            ..self
        }
    }
}
