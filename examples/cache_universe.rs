//! Populate the standard cache with historical index members and daily bars.
use clap::Parser;
use rquant::{
    data::{DataProvider, DiskCacheProvider, tushare::TushareProvider},
    utils::{DateRange, parse_date},
};
use std::collections::BTreeSet;
use time::Date;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    symbol: String,
    #[arg(long, default_value="20190101", value_parser=parse_date)]
    start: Date,
    #[arg(long, value_parser=parse_date)]
    end: Date,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.start <= args.end && args.end <= rquant::utils::latest_rqdate(),
        "Invalid date range"
    );
    let range = DateRange::new(args.start, args.end);
    let mut data = DiskCacheProvider::new(Box::new(TushareProvider::new()), range);
    let hist = data.index_comp(&args.symbol, range).await;
    let members: BTreeSet<_> = hist
        .snapshots()
        .iter()
        .flat_map(|(_, c)| c.weights().keys().copied())
        .collect();
    anyhow::ensure!(!members.is_empty(), "No historical constituents");
    let members: Vec<_> = members.into_iter().collect();
    eprintln!("{} historical members: {}", args.symbol, members.len());
    data.stocks_info(&members).await;
    for (batch, symbols) in members.chunks(32).enumerate() {
        data.stocks_bar(&symbols.iter().map(|&s| (s, range)).collect::<Vec<_>>())
            .await;
        eprintln!(
            "cached {} / {}",
            ((batch + 1) * 32).min(members.len()),
            members.len()
        );
    }
    Ok(())
}
