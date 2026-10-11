"""Rebuild both causal shadow accounts, then replay actual next-open allocation.

Build first: cargo build --release --example rotation_study
The official-calendar report and cache must cover 2020-01-01 through --end.
"""
import argparse
import hashlib
import json
import subprocess
from pathlib import Path
from verify_backtest import metrics
from study_patterns import BASE as BREAKOUT

CORE = {
    "symbol": "399101.XSHE", "top-k": 6, "liquidity-pool": 20,
    "liquidity-lookback": 120, "reversal-lookback": 20, "price-weight": .5,
    "trend-lookback": 5, "breadth-count": 100, "entry-band": .005,
    "exit-band": 0, "retain-buffer": 2, "rebalance-days": 1,
    "allocation": .95, "minimum-listed-days": 120, "history-start": "20000101",
}
META = {"window": 40, "threshold": .05, "satellite-weight": 1,
        "allocation": .8, "rebalance-band": .02}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--calendar-report", required=True, type=Path)
    parser.add_argument("--end", required=True)
    parser.add_argument("--output", type=Path, default=Path("report/strategy_momentum"))
    parser.add_argument("--runner", type=Path, default=Path("target/release/examples/rotation_study"))
    parser.add_argument("--sensitivity", action="store_true")
    parser.add_argument("--prefix-check", action="store_true", help="Rebuild through 2024 and compare all earlier actual fills and equity")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    summary = {"cases": [], "sources_sha256": {
        str(p): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in [Path("src/strategy/strategy_momentum.rs"),
                  Path("src/strategy/adaptive_rotation.rs"), Path("src/strategy/pattern_rotation.rs"),
                  Path("src/engine/execution.rs"), Path(__file__)]
    }}

    def run(name, strategy, config, start="20200101", end=None, costs=None):
        output = args.output / f"{name}.json"
        command = [str(args.runner), "--calendar-report", str(args.calendar_report),
                   "--start", start, "--end", end or args.end, "--cash", "100000",
                   "--commission", str((costs or {}).get("commission", .0003)),
                   "--min-commission", "5", "--stamp-tax", ".0005", "--output", str(output), strategy]
        for key, value in config.items():
            command += [f"--{key}", str(value)]
        subprocess.run(command, check=True, capture_output=True, text=True)
        result = json.loads(output.read_text())
        curve = result["equity_curve"]
        years = {year: metrics([p for p in curve if p["date"].startswith(year)])
                 for year in sorted({p["date"][:4] for p in curve})}
        row = {"name": name, "command": command, "performance": result["performance"],
               "years": years, "sha256": hashlib.sha256(output.read_bytes()).hexdigest()}
        summary["cases"].append(row)
        print(name, "DD", round(result["performance"]["max_drawdown"] * 100, 2),
              {y: round(m["total_return"] * 100, 2) for y, m in years.items()}, flush=True)
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2))
        return output

    core = run("core", "adaptive-rotation", CORE)
    satellite = run("satellite", "volume-breakout", BREAKOUT | {"allocation": .95})
    config = META | {"core-report": core, "satellite-report": satellite}
    run("full", "strategy-momentum", config)
    run("fresh_2025", "strategy-momentum", config, "20250101", "20251231")
    run("fresh_2026", "strategy-momentum", config, "20260101")
    if args.prefix_check:
        core_prefix = run("core_prefix", "adaptive-rotation", CORE, end="20241231")
        satellite_prefix = run("satellite_prefix", "volume-breakout", BREAKOUT | {"allocation": .95}, end="20241231")
        prefix_path = run("prefix", "strategy-momentum", config | {
            "core-report": core_prefix, "satellite-report": satellite_prefix}, end="20241231")
        full = json.loads((args.output / "full.json").read_text())
        prefix = json.loads(prefix_path.read_text())
        for key in ["equity_curve", "trades", "skipped_orders"]:
            assert [p for p in full[key] if p["date"] <= "2024-12-31"] == prefix[key], key
        summary["prefix_check"] = "2020-2024 equity, fills and rejections exactly unchanged after removing all 2025-2026 data from shadow reports"
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2))
    if args.sensitivity:
        for window in [30, 35, 45, 50]:
            run(f"window_{window}", "strategy-momentum", config | {"window": window})
        for threshold in [0, .025, .075, .1]:
            run(f"threshold_{threshold}", "strategy-momentum", config | {"threshold": threshold})
        for allocation in [.7, .9]:
            run(f"allocation_{allocation}", "strategy-momentum", config | {"allocation": allocation})
        for band in [.01, .03, .05]:
            run(f"band_{band}", "strategy-momentum", config | {"rebalance-band": band})
        # Fixed shadow-signal cost assumptions; stress actual account execution costs.
        run("higher_commission", "strategy-momentum", config, costs={"commission": .001})


if __name__ == "__main__":
    main()
