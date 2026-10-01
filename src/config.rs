use clap::{Parser, Subcommand};
use rquant::{engine::BacktestConfig, strategy::StrategyConfig};

#[derive(Debug, Parser)]
#[command(name = "rquant", version, about = "rquant量化研究框架")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 回测
    Bt {
        #[command(flatten)]
        backtest: BacktestConfig,
        #[command(subcommand)]
        strategy: StrategyConfig,
    },
}
