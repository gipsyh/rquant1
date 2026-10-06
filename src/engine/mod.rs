mod execution;
mod rbt;
#[cfg(test)]
mod test;

use crate::data::{DataProvider, IndexHistComp, Stock, StockBar, StockSymbol};
use crate::utils::{DateRange, latest_rqdate, parse_date};
use anyhow::{Result, anyhow};
use clap::{ArgAction, Parser};
pub use rbt::BacktestEngine;
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf};
use time::Date;

#[derive(Parser, Clone, Debug, Serialize)]
pub struct BacktestConfig {
    /// 开始日期（含），支持 YYYYMMDD 或 YYYY-MM-DD
    #[arg(long, default_value = "20200101", value_parser = parse_date)]
    pub start: Date,
    /// 截止日期（含），支持 YYYYMMDD 或 YYYY-MM-DD；默认北京时间 19:00 前取昨天，否则取今天
    #[arg(long, default_value_t = latest_rqdate(), value_parser = parse_date)]
    pub end: Date,
    /// 初始资金
    #[arg(long = "cash", default_value_t = 100_000.0)]
    pub initial_cash: f64,
    /// 买卖双向佣金率，默认万分之三。
    #[arg(long = "commission", default_value_t = 0.0003)]
    pub commission_rate: f64,
    /// 最低佣金（元）
    #[arg(long = "min-commission", default_value_t = 5.0)]
    pub minimum_commission: f64,
    /// 印花税率，默认万分之五，仅卖出收取，无最低收费。
    #[arg(long = "stamp-tax", default_value_t = 0.0005)]
    pub stamp_tax_rate: f64,
    /// 保留配置兼容性；当前按开盘原价撮合，仅支持 0。
    #[arg(long, default_value_t = 0.0)]
    pub slippage_bps: f64,
    /// 交易单位（股）
    #[arg(long, default_value_t = 100)]
    pub lot_size: u32,
    /// 用 close × 当日因子 / 买入日因子估算含公司行动的持有收益。
    /// 这是复权收益模型，不是现金分红、税费和实际送转股的逐笔记账。
    #[arg(long = "raw", action = ArgAction::SetFalse, help = "仅计算价格收益，不使用复权因子估值")]
    pub adjust_returns: bool,
    /// 将完整 JSON 报告写入文件；不指定则打印到 stdout
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        // 只传程序名，让 clap 填充各字段的默认值，不读取进程命令行参数。
        Self::try_parse_from(["rquant"]).unwrap()
    }
}

impl BacktestConfig {
    pub fn validate(&self) -> Result<()> {
        if self.start > self.end {
            return Err(anyhow!("回测参数无效: 开始日期不能晚于截止日期"));
        }
        if self.slippage_bps != 0.0 {
            return Err(anyhow!("开盘价撮合仅支持 slippage_bps=0"));
        }
        if !self.initial_cash.is_finite() || self.initial_cash <= 0.0 {
            return Err(anyhow!("回测参数无效: 初始资金必须是有限正数"));
        }
        if !self.commission_rate.is_finite()
            || !(0.0..1.0).contains(&self.commission_rate)
            || !self.minimum_commission.is_finite()
            || self.minimum_commission < 0.0
            || !self.stamp_tax_rate.is_finite()
            || !(0.0..1.0).contains(&self.stamp_tax_rate)
            || !self.slippage_bps.is_finite()
            || !(0.0..10_000.0).contains(&self.slippage_bps)
            || self.lot_size == 0
        {
            return Err(anyhow!("回测参数无效: 佣金、印花税、滑点或交易单位无效"));
        }
        Ok(())
    }
}

/// 收盘后上下文，可查询当日及以前日线；返回的订单在下一交易日开盘执行。
pub struct BtContext<'a> {
    date: Date,
    pub init_cash: f64,
    pub cash: f64,
    /// 当日收盘权益。
    pub equity: f64,
    pub positions: &'a BTreeMap<StockSymbol, Position>,
    provider: tokio::sync::Mutex<&'a mut dyn DataProvider>,
}

impl BtContext<'_> {
    pub fn date(&self) -> Date {
        self.date
    }

    pub fn position(&self, symbol: StockSymbol) -> Option<&Position> {
        self.positions.get(&symbol)
    }

    /// 查询股票基础信息；名称、行业为数据源的静态信息，不代表回测日历史状态。
    pub async fn stock_info(&self, symbol: StockSymbol) -> Stock {
        self.provider.lock().await.stock_info(symbol).await
    }

    /// 批量查询股票基础信息，重复代码合并。
    pub async fn stocks_info(&self, symbols: &[StockSymbol]) -> BTreeMap<StockSymbol, Stock> {
        let symbols: Vec<_> = symbols
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let infos = self.provider.lock().await.stocks_info(&symbols).await;
        assert_eq!(infos.len(), symbols.len(), "基础信息批量结果数量不匹配");
        symbols
            .into_iter()
            .zip(infos)
            .map(|(symbol, info)| {
                assert_eq!(symbol, info.symbol, "股票基础信息代码不匹配");
                info.validate().unwrap();
                assert!(info.bars.is_none(), "股票基础信息查询不应返回日线");
                (symbol, info)
            })
            .collect()
    }

    /// 查询当前日期及以前的指数成分历史，通过 composition(date) 获取已生效成分。
    pub async fn index_comp(&self, symbol: &str, start: Date, end: Date) -> IndexHistComp {
        assert!(
            start <= end && end <= self.date,
            "指数成分查询区间无效或包含未来数据"
        );
        self.provider
            .lock()
            .await
            .index_comp(symbol, DateRange::new(start, end))
            .await
    }

    /// 查询当日或历史日期是否有日线且非 ST；不保证下一交易日订单能成交。
    pub async fn is_tradable(&self, symbol: StockSymbol, date: Date) -> bool {
        assert!(date <= self.date, "可交易状态查询不能包含未来数据");
        self.provider.lock().await.is_tradable(symbol, date).await
    }

    /// 批量查询同一闭区间的原始日线；重复代码合并，无行情股票保留空 Vec。
    /// 一次加锁传递整批请求，由数据源内部并发下载。
    pub async fn stocks_bars(
        &self,
        symbols: &[StockSymbol],
        start: Date,
        end: Date,
    ) -> BTreeMap<StockSymbol, Vec<StockBar>> {
        assert!(
            start <= end && end <= self.date,
            "历史查询区间无效或包含未来数据"
        );
        let symbols: Vec<_> = symbols
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let requests: Vec<_> = symbols
            .iter()
            .map(|&s| (s, DateRange::new(start, end)))
            .collect();
        let results = self.provider.lock().await.stocks_bars(&requests).await;
        assert_eq!(results.len(), requests.len(), "日线批量结果数量不匹配");
        symbols
            .into_iter()
            .zip(results)
            .map(|(symbol, bars)| {
                let mut previous = None;
                for bar in &bars {
                    assert!(
                        bar.symbol == symbol && bar.date >= start && bar.date <= end,
                        "批量日线返回请求范围外数据"
                    );
                    assert!(
                        previous.is_none_or(|date| date < bar.date),
                        "批量日线日期未严格升序"
                    );
                    rbt::validate_bar(bar, true).unwrap();
                    previous = Some(bar.date);
                }
                (symbol, bars)
            })
            .collect()
    }

    /// 查询闭区间内的原始日线和复权因子，允许回溯至回测开始日以前。
    /// 不复权价格；区间无行情时返回空 Vec，日期或数据无效时 panic。
    pub async fn stock_bars(&self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        assert!(
            start <= end && end <= self.date,
            "历史查询区间无效或包含未来数据"
        );
        let mut provider = self.provider.lock().await;
        rbt::load_bars(&mut **provider, symbol, start, end, true).await
    }
}

#[derive(Clone, Debug)]
pub enum Order {
    /// 精确股数，须为整手；限价 >= 次日开盘价才成交，不做部分成交。
    /// 信号日与执行日复权因子必须相同，否则原始限价失效。
    BuyLimit {
        symbol: StockSymbol,
        shares: u64,
        price: f64,
    },
    /// 按次日开盘价买入预算内最多整手股数，金额包含佣金。
    BuyAmount {
        symbol: StockSymbol,
        cash_amount: f64,
    },
    /// 精确股数；限价 <= 次日开盘价才成交，不做部分成交。
    /// 信号日与执行日复权因子必须相同，否则原始限价失效。
    SellLimit {
        symbol: StockSymbol,
        shares: u64,
        price: f64,
    },
    /// 按次日开盘价卖出全部可卖持仓；当天新买入部分受 T+1 限制。
    SellAll { symbol: StockSymbol },
    /// 各股票预算 = 本批次开始前现金 × 权重，包含佣金；权重和不得超过 1。
    /// 各成分独立成交或失败，不将失败成分的预算重新分配。
    BuyWeights { weights: BTreeMap<StockSymbol, f64> },
}

/// 每个失败委托（权重单按股票分别报告）的回调信息。
#[derive(Clone, Debug)]
pub struct OrderFailure {
    pub order: Order,
    pub detail: SkippedOrder,
}

/// 按股票独立记录持仓，估值口径由 BacktestConfig::adjust_returns 决定。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Position {
    /// 当前未平仓的买入股数；复权模式下不代表公司行动后的实际股数。
    pub purchased_shares: u64,
    pub market_value: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Trade {
    pub signal_date: Date,
    pub date: Date,
    /// 批次和原始订单下标（从 0 开始）。
    pub batch_index: usize,
    pub order_index: usize,
    pub symbol: String,
    pub side: &'static str,
    pub shares: u64,
    pub price: f64,
    /// 成交金额。复权模式卖出按收益单位结算，可能不等于 shares × price。
    pub notional: f64,
    pub commission: f64,
    /// 卖出印花税，买入恒为 0。
    pub stamp_tax: f64,
    /// 整批结算后的现金；同批成交记录使用同一个值。
    pub cash_after: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EquityPoint {
    pub date: Date,
    pub cash: f64,
    /// 各股票的收盘持仓快照，键为标准股票代码。
    pub positions: BTreeMap<String, Position>,
    pub market_value: f64,
    pub equity: f64,
    pub net_value: f64,
    pub daily_return: f64,
    pub drawdown: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SkippedOrder {
    pub signal_date: Date,
    pub date: Date,
    pub batch_index: usize,
    pub order_index: usize,
    pub symbol: String,
    pub side: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Performance {
    pub final_equity: f64,
    pub total_return: f64,
    /// 以净值观测数 / 252 年计算；无法表示时为 None。
    pub annualized_return: Option<f64>,
    pub max_drawdown: f64,
    /// 样本标准差 × sqrt(252)，少于两条观测时为 None。
    pub annualized_volatility: Option<f64>,
    /// 无风险利率取 0；波动为 0 或样本不足时为 None。
    pub sharpe_ratio: Option<f64>,
    pub total_commission: f64,
    pub total_stamp_tax: f64,
    /// 佣金与印花税合计。
    pub total_fees: f64,
    pub trade_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct BacktestResult {
    pub strategy: String,
    pub symbols: Vec<String>,
    pub start: Date,
    pub end: Date,
    pub config: BacktestConfig,
    pub performance: Performance,
    pub trades: Vec<Trade>,
    pub equity_curve: Vec<EquityPoint>,
    pub skipped_orders: Vec<SkippedOrder>,
}
