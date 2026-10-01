mod config;

use clap::Parser;
use config::{Cli, Command};
use env_logger::Target;
use rquant::{
    data::tushare::TushareProvider,
    engine::{BacktestConfig, BacktestEngine},
    strategy::StrategyConfig,
};
use std::io::Write;

fn logger_init() {
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            let now = time::OffsetDateTime::now_utc();
            let ts = now
                .format(time::macros::format_description!(
                    "[hour repr:24]:[minute]:[second]"
                ))
                .unwrap();

            let meta_style = env_logger::fmt::style::Style::new().dimmed();
            let level_style = buf.default_level_style(record.level());
            writeln!(
                buf,
                "{meta_style}[{ts} {meta_style:#}{level_style}{}{level_style:#}{meta_style}]{meta_style:#} {}",
                record.level(),
                record.args()
            )
        })
        .format_target(false)
        .target(Target::Stdout)
        .init();
}

#[tokio::main]
async fn main() {
    logger_init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Bt { backtest, strategy } => run_bt(backtest, strategy).await,
    };
    if let Err(err) = result {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}

async fn run_bt(backtest: BacktestConfig, strategy: StrategyConfig) -> anyhow::Result<()> {
    let output = backtest.output.clone();
    let engine = BacktestEngine::new(backtest)?;
    let strategy = strategy.build();
    let provider = Box::new(TushareProvider::new());
    let result = engine.run(provider, strategy).await?;
    let json = serde_json::to_string_pretty(&result)?;
    if let Some(output) = output {
        std::fs::write(&output, json)?;
        eprintln!("报告已写入 {}", output.display());
    } else {
        println!("{json}");
    }
    let p = result.performance;
    eprintln!(
        "期末权益: {:.2} | 总收益: {:.2}% | 最大回撤: {:.2}% | 成交: {} 笔",
        p.final_equity,
        p.total_return * 100.0,
        p.max_drawdown * 100.0,
        p.trade_count
    );
    Ok(())
}
