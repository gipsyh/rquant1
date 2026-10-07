use super::*;
use crate::data::Adjustment;
use crate::strategy::Strategy;

const MAX_SHARES: u64 = 1_u64 << 53;

pub(super) struct Account {
    pub cash: f64,
    pub positions: BTreeMap<StockSymbol, Position>,
    pub units: BTreeMap<StockSymbol, f64>,
    // 当天开盘前的可卖股数、复权收益单位；新买入不计入可卖数量。
    sellable: BTreeMap<StockSymbol, (u64, f64)>,
}

impl Account {
    pub fn new(cash: f64) -> Self {
        Self {
            cash,
            positions: BTreeMap::new(),
            units: BTreeMap::new(),
            sellable: BTreeMap::new(),
        }
    }

    pub fn start_day(&mut self) {
        self.sellable = self
            .positions
            .iter()
            .map(|(&symbol, pos)| (symbol, (pos.purchased_shares, self.units[&symbol])))
            .collect();
    }
}

#[derive(Clone, Copy)]
enum Intent {
    BuyLimit { shares: u64, price: f64 },
    BuyAmount { cash: f64 },
    SellLimit { shares: u64, price: f64 },
    SellAll,
}

impl Intent {
    fn is_buy(self) -> bool {
        matches!(self, Self::BuyLimit { .. } | Self::BuyAmount { .. })
    }
    fn side(self) -> &'static str {
        if self.is_buy() { "buy" } else { "sell" }
    }
}

struct Leg {
    order_index: usize,
    symbol: StockSymbol,
    intent: Intent,
}

struct Fill {
    leg: Leg,
    shares: u64,
    units: f64,
    price: f64,
    notional: f64,
    commission: f64,
    stamp_tax: f64,
}

impl Fill {
    fn cash_delta(&self) -> f64 {
        if self.leg.intent.is_buy() {
            -self.notional - self.commission
        } else {
            self.notional - self.commission - self.stamp_tax
        }
    }
}

pub(super) struct BatchResult {
    pub trades: Vec<Trade>,
    pub skipped: Vec<SkippedOrder>,
}

impl BacktestEngine {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_batch(
        &self,
        account: &mut Account,
        provider: &mut dyn DataProvider,
        strategy: &mut dyn Strategy,
        signal_date: Date,
        date: Date,
        batch_index: usize,
        orders: Vec<Order>,
    ) -> Result<BatchResult> {
        let batch_cash = account.cash;
        let mut legs = Vec::new();
        let mut skipped = Vec::new();
        let failure = |order_index, symbol: String, side: &str, reason: String| SkippedOrder {
            signal_date,
            date,
            batch_index,
            order_index,
            symbol,
            side: side.into(),
            reason,
        };
        for (order_index, order) in orders.iter().enumerate() {
            match order {
                Order::BuyLimit {
                    symbol,
                    shares,
                    price,
                } => legs.push(Leg {
                    order_index,
                    symbol: *symbol,
                    intent: Intent::BuyLimit {
                        shares: *shares,
                        price: *price,
                    },
                }),
                Order::BuyAmount {
                    symbol,
                    cash_amount,
                } => legs.push(Leg {
                    order_index,
                    symbol: *symbol,
                    intent: Intent::BuyAmount { cash: *cash_amount },
                }),
                Order::SellLimit {
                    symbol,
                    shares,
                    price,
                } => legs.push(Leg {
                    order_index,
                    symbol: *symbol,
                    intent: Intent::SellLimit {
                        shares: *shares,
                        price: *price,
                    },
                }),
                Order::SellAll { symbol } => legs.push(Leg {
                    order_index,
                    symbol: *symbol,
                    intent: Intent::SellAll,
                }),
                Order::BuyWeights { weights } => {
                    let total: f64 = weights.values().sum();
                    if weights.is_empty()
                        || weights
                            .values()
                            .any(|w| !w.is_finite() || !(0.0..=1.0).contains(w))
                        || !total.is_finite()
                        || total > 1.0 + 1e-12
                    {
                        skipped.push(failure(
                            order_index,
                            String::new(),
                            "buy",
                            "权重必须有限、非负且合计不超过 1，组合不能为空".into(),
                        ));
                        continue;
                    }
                    for (&symbol, &weight) in weights {
                        if weight > 0.0 {
                            legs.push(Leg {
                                order_index,
                                symbol,
                                intent: Intent::BuyAmount {
                                    cash: batch_cash * weight,
                                },
                            });
                        }
                    }
                }
            }
        }
        // 同批所有委托基于同一个账户快照形成候选成交；读取顺序不赋予资金优先级。
        let mut bars = BTreeMap::new();
        let mut signal_bars = BTreeMap::new();
        let mut fills = Vec::new();
        for leg in legs {
            if let std::collections::btree_map::Entry::Vacant(entry) = bars.entry(leg.symbol) {
                let bar = provider
                    .stock_bar(leg.symbol, DateRange::new(date, date))
                    .await
                    .into_bars()
                    .into_iter()
                    .next();
                entry.insert(bar);
            }
            let factor_check = if matches!(
                leg.intent,
                Intent::BuyLimit { .. } | Intent::SellLimit { .. }
            ) {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    signal_bars.entry(leg.symbol)
                {
                    entry.insert(
                        provider
                            .stock_bar(leg.symbol, DateRange::new(signal_date, signal_date))
                            .await
                            .into_bars()
                            .into_iter()
                            .next(),
                    );
                }
                check_limit_factors(
                    signal_bars[&leg.symbol].as_ref(),
                    bars[&leg.symbol].as_ref(),
                )
            } else {
                Ok(())
            };
            match factor_check.and_then(|()| self.plan(&leg, bars[&leg.symbol].as_ref(), account)) {
                Ok((shares, units, price, notional, commission, stamp_tax)) => {
                    fills.push(Fill {
                        leg,
                        shares,
                        units,
                        price,
                        notional,
                        commission,
                        stamp_tax,
                    });
                }
                Err(err) => skipped.push(failure(
                    leg.order_index,
                    leg.symbol.to_string(),
                    leg.intent.side(),
                    err.to_string(),
                )),
            }
        }
        // 同股票卖单争用股数时一起失败。Sell All 也按批次开始时可卖量展开。
        let mut sell_totals: BTreeMap<StockSymbol, u128> = BTreeMap::new();
        let mut buy_totals: BTreeMap<StockSymbol, u128> = BTreeMap::new();
        for fill in &fills {
            let totals = if fill.leg.intent.is_buy() {
                &mut buy_totals
            } else {
                &mut sell_totals
            };
            *totals.entry(fill.leg.symbol).or_default() += u128::from(fill.shares);
        }
        fills.retain(|fill| {
            let symbol = fill.leg.symbol;
            let reason = match fill.leg.intent {
                Intent::SellLimit { .. } | Intent::SellAll
                    if sell_totals[&symbol]
                        > u128::from(account.sellable.get(&symbol).map_or(0, |p| p.0)) =>
                {
                    Some("同批卖单合计超过可卖股数，冲突卖单全部失败")
                }
                Intent::BuyLimit { .. } | Intent::BuyAmount { .. }
                    if buy_totals[&symbol]
                        + u128::from(
                            account
                                .positions
                                .get(&symbol)
                                .map_or(0, |p| p.purchased_shares),
                        )
                        > u128::from(MAX_SHARES) =>
                {
                    Some("同批买入后持股数量超出支持范围")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                skipped.push(failure(
                    fill.leg.order_index,
                    symbol.to_string(),
                    fill.leg.intent.side(),
                    reason.into(),
                ));
                false
            } else {
                true
            }
        });
        let required: f64 = fills.iter().map(|fill| (-fill.cash_delta()).max(0.0)).sum();
        if !required.is_finite() || required > batch_cash {
            fills.retain(|fill| {
                if fill.cash_delta() < 0.0 {
                    skipped.push(failure(
                        fill.leg.order_index,
                        fill.leg.symbol.to_string(),
                        fill.leg.intent.side(),
                        "同批订单合计超过批次开始时现金，冲突订单全部失败".into(),
                    ));
                    false
                } else {
                    true
                }
            });
        }
        // 先减持再增持只是结算实现；所有撮合与资源检查均已完成。
        for fill in fills.iter().filter(|f| !f.leg.intent.is_buy()) {
            let symbol = fill.leg.symbol;
            let position = account.positions.get_mut(&symbol).unwrap();
            let before = position.purchased_shares;
            position.purchased_shares -= fill.shares;
            position.market_value *= position.purchased_shares as f64 / before as f64;
            *account.units.get_mut(&symbol).unwrap() -= fill.units;
            let sellable = account.sellable.get_mut(&symbol).unwrap();
            sellable.0 -= fill.shares;
            sellable.1 = (sellable.1 - fill.units).max(0.0);
            if position.purchased_shares == 0 {
                account.positions.remove(&symbol);
                account.units.remove(&symbol);
            }
        }
        for fill in fills.iter().filter(|f| f.leg.intent.is_buy()) {
            let position = account.positions.entry(fill.leg.symbol).or_default();
            position.purchased_shares += fill.shares;
            position.market_value += fill.notional;
            *account.units.entry(fill.leg.symbol).or_default() += fill.units;
        }
        let spent: f64 = fills.iter().map(|f| (-f.cash_delta()).max(0.0)).sum();
        let received: f64 = fills.iter().map(|f| f.cash_delta().max(0.0)).sum();
        account.cash = batch_cash - spent + received;
        anyhow::ensure!(
            account.cash.is_finite() && account.cash >= 0.0,
            "批次结算现金无效"
        );
        let trades = fills
            .into_iter()
            .map(|fill| Trade {
                signal_date,
                date,
                batch_index,
                order_index: fill.leg.order_index,
                symbol: fill.leg.symbol.to_string(),
                side: fill.leg.intent.side().into(),
                shares: fill.shares,
                price: fill.price,
                notional: fill.notional,
                commission: fill.commission,
                stamp_tax: fill.stamp_tax,
                cash_after: account.cash,
            })
            .collect();
        skipped.sort_by(|a, b| (a.order_index, &a.symbol).cmp(&(b.order_index, &b.symbol)));
        for detail in &skipped {
            log::warn!(
                "订单被拒绝: signal_date={}, date={}, batch_index={}, order_index={}, symbol={}, side={}, reason={}",
                detail.signal_date,
                detail.date,
                detail.batch_index,
                detail.order_index,
                detail.symbol,
                detail.side,
                detail.reason,
            );
            strategy
                .on_order_failed(&OrderFailure {
                    order: orders[detail.order_index].clone(),
                    detail: detail.clone(),
                })
                .await;
        }
        Ok(BatchResult { trades, skipped })
    }

    fn commission(&self, notional: f64) -> f64 {
        (notional * self.config.commission_rate).max(self.config.minimum_commission)
    }

    fn plan(
        &self,
        leg: &Leg,
        bar: Option<&StockBar>,
        account: &Account,
    ) -> Result<(u64, f64, f64, f64, f64, f64)> {
        let bar = bar.ok_or_else(|| anyhow!("无当日开盘行情"))?;
        // 成交判定不读取当日 high/low/close/volume；只有开盘价、涨跌停价和复权因子。
        let price = bar.open;
        anyhow::ensure!(price.is_finite() && price > 0.0, "开盘价无效");
        let factor = match (self.config.adjust_returns, bar.adjustment) {
            (true, Some(Adjustment::Raw(f))) if f.is_finite() && f > 0.0 => f,
            (false, Some(Adjustment::Raw(f))) if f.is_finite() && f > 0.0 => 1.0,
            (false, None) => 1.0,
            _ => return Err(anyhow!("开盘行情缺少有效原始复权因子")),
        };
        let buying = leg.intent.is_buy();
        let limit = if buying { bar.limit_up } else { bar.limit_down };
        let limit = limit
            .filter(|p| p.is_finite() && *p > 0.0)
            .ok_or_else(|| anyhow!("缺少有效涨跌停价"))?;
        anyhow::ensure!(
            if buying { price < limit } else { price > limit },
            "开盘价触及涨跌停，无法成交"
        );
        let shares = match leg.intent {
            Intent::BuyLimit { shares, price: bid } => {
                anyhow::ensure!(
                    bid.is_finite()
                        && bid > 0.0
                        && shares > 0
                        && shares <= MAX_SHARES
                        && shares.is_multiple_of(u64::from(self.config.lot_size)),
                    "买入股数或限价无效，股数须为整手"
                );
                anyhow::ensure!(bid >= price, "买入限价低于开盘价");
                shares
            }
            Intent::BuyAmount { cash } => {
                anyhow::ensure!(cash.is_finite() && cash > 0.0, "买入金额必须为有限正数");
                let available = (cash - self.config.minimum_commission)
                    .max(0.0)
                    .min(cash / (1.0 + self.config.commission_rate));
                let lot = u64::from(self.config.lot_size);
                let lots = (available / price / lot as f64).floor();
                anyhow::ensure!(
                    lots.is_finite() && lots * lot as f64 <= MAX_SHARES as f64,
                    "买入数量超出支持范围"
                );
                let mut shares = lots as u64 * lot;
                while shares > 0
                    && shares as f64 * price + self.commission(shares as f64 * price) > cash
                {
                    shares -= lot;
                }
                anyhow::ensure!(shares > 0, "金额不足以支付一手及佣金");
                shares
            }
            Intent::SellLimit { shares, price: ask } => {
                anyhow::ensure!(
                    shares > 0 && shares <= MAX_SHARES && ask.is_finite() && ask > 0.0,
                    "卖出股数或限价无效"
                );
                anyhow::ensure!(ask <= price, "卖出限价高于开盘价");
                shares
            }
            Intent::SellAll => account.sellable.get(&leg.symbol).map_or(0, |p| p.0),
        };
        let units = if buying {
            shares as f64 / factor
        } else {
            let (available, available_units) = account
                .sellable
                .get(&leg.symbol)
                .copied()
                .unwrap_or_default();
            anyhow::ensure!(
                shares > 0 && available > 0,
                "没有可卖持仓（当天买入不可卖）"
            );
            // 超卖保留为候选，统一与同股票其他卖单检查，避免先来先得。
            available_units * (shares as f64 / available as f64)
        };
        let notional = if buying {
            shares as f64 * price
        } else {
            units * factor * price
        };
        let commission = self.commission(notional);
        let stamp_tax = if buying {
            0.0
        } else {
            notional * self.config.stamp_tax_rate
        };
        anyhow::ensure!(
            notional.is_finite()
                && notional > 0.0
                && units.is_finite()
                && units > 0.0
                && (notional + commission + stamp_tax).is_finite(),
            "成交金额或收益单位无效"
        );
        Ok((shares, units, price, notional, commission, stamp_tax))
    }
}

fn check_limit_factors(signal: Option<&StockBar>, execution: Option<&StockBar>) -> Result<()> {
    let factor = |bar: Option<&StockBar>| -> Result<f64> {
        match bar.map(|bar| bar.adjustment) {
            Some(Some(Adjustment::Raw(f))) if f.is_finite() && f > 0.0 => Ok(f),
            _ => Err(anyhow!("限价单无法确认信号日与执行日的原始复权因子")),
        }
    };
    let (before, after) = (factor(signal)?, factor(execution)?);
    anyhow::ensure!(
        before == after,
        "复权因子发生变化（{before} -> {after}），限价单失效"
    );
    Ok(())
}
