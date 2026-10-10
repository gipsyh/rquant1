//! Replay a strategy with a read-only cache and save JSON without generating HTML.
mod support;
use clap::Parser;
use rquant::{
    engine::{BacktestConfig, BacktestEngine, BacktestResult},
    strategy::StrategyConfig,
};
use std::path::PathBuf;
#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "rqdata.bin")]
    cache: PathBuf,
    /// Existing result.json whose official calendar covers the requested period.
    #[arg(long)]
    calendar_report: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[command(flatten)]
    backtest: BacktestConfig,
    #[command(subcommand)]
    strategy: StrategyConfig,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut provider = support::CachedProvider::load(&args.cache)?;
    let calendar: BacktestResult = serde_json::from_slice(&std::fs::read(&args.calendar_report)?)?;
    anyhow::ensure!(
        calendar.start <= args.backtest.start && calendar.end >= args.backtest.end,
        "Calendar report must cover the complete requested period"
    );
    provider.calendar = calendar
        .equity_curve
        .iter()
        .map(|point| point.date)
        .collect();
    anyhow::ensure!(
        !provider.calendar.is_empty() && provider.calendar.windows(2).all(|days| days[0] < days[1]),
        "Calendar report must contain strictly increasing dates"
    );
    let result = BacktestEngine::new(args.backtest)?
        .run(Box::new(provider), args.strategy.build())
        .await?;
    println!("{}", serde_json::to_string(&result.performance)?);
    if let Some(path) = args.output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = serde_json::to_value(&result)?;
        output["research_command"] = serde_json::to_value(std::env::args().collect::<Vec<_>>())?;
        std::fs::write(path, serde_json::to_vec_pretty(&output)?)?;
    }
    Ok(())
}
