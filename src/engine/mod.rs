mod rbt;

use crate::data::{DataProvider, InstrSymbol, StockDailyBar};
use crate::utils::parse_date;
use anyhow::{Result, anyhow};
use clap::{ArgAction, Args};
pub use rbt::BacktestEngine;
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf};
use time::Date;

#[derive(Args, Clone, Debug, Serialize)]
pub struct BacktestConfig {
    /// 开始日期（含），支持 YYYYMMDD 或 YYYY-MM-DD
    #[arg(long, default_value = "20200101", value_parser = parse_date)]
    pub start: Date,
    /// 截止日期（含），支持 YYYYMMDD 或 YYYY-MM-DD
    #[arg(long, value_parser = parse_date)]
    pub end: Date,
    /// 初始资金
    #[arg(long = "cash", default_value_t = 100_000.0)]
    pub initial_cash: f64,
    /// 买入佣金率；不含额外过户费。
    #[arg(long = "commission", default_value_t = 0.0005)]
    pub commission_rate: f64,
    /// 最低佣金（元）
    #[arg(long = "min-commission", default_value_t = 5.0)]
    pub minimum_commission: f64,
    /// 买入成交价 = 当日原始开盘价 × (1 + slippage_bps / 10000)。
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
        Self {
            start: time::macros::date!(2020 - 01 - 01),
            // Rust 调用的默认截止日；CLI 仍要求显式提供 --end。
            end: time::OffsetDateTime::now_utc().date(),
            output: None,
            initial_cash: 100_000.0,
            commission_rate: 0.0003,
            minimum_commission: 5.0,
            slippage_bps: 0.0,
            lot_size: 100,
            adjust_returns: true,
        }
    }
}

impl BacktestConfig {
    pub fn validate(&self) -> Result<()> {
        if self.start > self.end {
            return Err(anyhow!("回测参数无效: 开始日期不能晚于截止日期"));
        }
        if !self.initial_cash.is_finite() || self.initial_cash <= 0.0 {
            return Err(anyhow!("回测参数无效: 初始资金必须是有限正数"));
        }
        if !self.commission_rate.is_finite()
            || !(0.0..1.0).contains(&self.commission_rate)
            || !self.minimum_commission.is_finite()
            || self.minimum_commission < 0.0
            || !self.slippage_bps.is_finite()
            || !(0.0..10_000.0).contains(&self.slippage_bps)
            || self.lot_size == 0
        {
            return Err(anyhow!("回测参数无效: 佣金、滑点或交易单位无效"));
        }
        Ok(())
    }
}

/// 开盘前上下文。历史数据按需查询，禁止查询当日及未来日线。
pub struct BtContext<'a> {
    date: Date,
    pub init_cash: f64,
    pub cash: f64,
    /// 上一交易日收盘权益；首日为初始资金。
    pub equity: f64,
    pub positions: &'a BTreeMap<InstrSymbol, Position>,
    provider: tokio::sync::Mutex<&'a mut dyn DataProvider>,
    adjust_returns: bool,
}

impl BtContext<'_> {
    pub fn date(&self) -> Date {
        self.date
    }

    pub fn position(&self, symbol: InstrSymbol) -> Option<&Position> {
        self.positions.get(&symbol)
    }

    /// 查询任意股票指定区间的已完成日线；允许查询回测开始日之前的数据。
    pub async fn history(&self, symbol: InstrSymbol, start: Date, end: Date) -> Vec<StockDailyBar> {
        assert!(end >= self.date && start > end);
        let mut provider = self.provider.lock().await;
        rbt::load_bars(&mut **provider, symbol, start, end, self.adjust_returns).await
    }

    pub async fn bar(&self, symbol: InstrSymbol, date: Date) -> Result<Option<StockDailyBar>> {
        Ok(self.history(symbol, date, date).await.into_iter().next())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Order {
    /// 为指定股票买入，金额预算包含佣金。预算超过剩余现金时拒绝该笔订单。
    Buy {
        symbol: InstrSymbol,
        cash_amount: f64,
    },
}

/// 按股票独立记录持仓，估值口径由 BacktestConfig::adjust_returns 决定。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Position {
    /// 累计买入股数；复权模式下不代表公司行动后的实际股数。
    pub purchased_shares: u64,
    pub market_value: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Trade {
    pub date: Date,
    pub symbol: String,
    pub side: &'static str,
    pub shares: u64,
    pub price: f64,
    pub commission: f64,
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
    pub date: Date,
    pub symbol: String,
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
