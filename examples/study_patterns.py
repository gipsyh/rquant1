"""Reproduce the selected breakout and nearby controls with the real Rust engine.

Build first: cargo build --release --example rotation_study
Every case runs one continuous account from 2025, with identical costs unless labeled.
The supplied official-calendar report and local cache must cover the dates.
"""
import argparse
import hashlib
import json
import subprocess
from pathlib import Path
from verify_backtest import metrics

BASE = {
    "symbol": "399006.XSHE", "pattern": "breakout", "rank-by": "volume",
    "lookback": 120, "top-k": 1, "short-window": 5, "exit-window": 10,
    "max-hold-days": 60, "trailing-stop": 0.12, "volume-ratio": 2,
    "min-momentum": 0.2, "min-breadth": 0.5, "exit-breadth": 0.4,
    "allocation": 0.7, "close-strength": 0, "min-turnover": 30000000,
    "minimum-price": 3, "minimum-listed-days": 240, "history-start": "20000101",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--calendar-report", required=True, type=Path)
    parser.add_argument("--end", required=True)
    parser.add_argument("--output", type=Path, default=Path("report/pattern_validation"))
    parser.add_argument("--runner", type=Path, default=Path("target/release/examples/rotation_study"))
    parser.add_argument("--sensitivity", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    cases = [("selected", {}, {})]
    if args.sensitivity:
        cases += [
            ("higher_commission", {}, {"commission": 0.001}),
            ("exposure_60", {"allocation": 0.6}, {}),
            ("exposure_80", {"allocation": 0.8}, {}),
            ("volume_1_5", {"volume-ratio": 1.5}, {}),
            ("volume_2_5", {"volume-ratio": 2.5}, {}),
            ("breadth_40", {"min-breadth": 0.4}, {}),
            ("breadth_60", {"min-breadth": 0.6}, {}),
            ("two_positions", {"top-k": 2}, {}),
        ]
    summary = {"base": BASE, "cases": [], "source_sha256": {
        name: hashlib.sha256(Path(name).read_bytes()).hexdigest()
        for name in ["src/strategy/pattern_rotation.rs", "src/strategy/online_model.rs"]
    }}
    for name, overrides, costs in cases:
        config = BASE | overrides
        output = args.output / f"{name}.json"
        command = [str(args.runner), "--calendar-report", str(args.calendar_report),
                   "--start", "20250101", "--end", args.end, "--output", str(output)]
        for key, value in costs.items():
            command += [f"--{key}", str(value)]
        command += ["pattern-rotation"]
        for key, value in config.items():
            command += [f"--{key}", str(value)]
        subprocess.run(command, check=True, capture_output=True, text=True)
        result = json.loads(output.read_text())
        years = {year: metrics([p for p in result["equity_curve"] if p["date"].startswith(year)])
                 for year in sorted({p["date"][:4] for p in result["equity_curve"]})}
        summary["cases"].append({"name": name, "command": command,
                                 "performance": result["performance"], "years": years})
        print(name, {year: round(m["total_return"] * 100, 2) for year, m in years.items()}, flush=True)
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
