"""Independently recompute daily-return metrics from an engine result.json.

Usage: python examples/verify_backtest.py report/<run>/result.json
Period rows slice the continuous equity curve; they do not restart the account.
"""

import argparse
import json
import math
import statistics
from pathlib import Path


def metrics(points):
    returns = [point["daily_return"] for point in points]
    volatility = statistics.stdev(returns) if len(returns) > 1 else 0.0
    value = peak = 1.0
    drawdown = 0.0
    for daily_return in returns:
        value *= 1 + daily_return
        peak = max(peak, value)
        drawdown = max(drawdown, 1 - value / peak)
    return {
        "observations": len(returns),
        "total_return": value - 1,
        "annualized_return": value ** (252 / len(returns)) - 1,
        "sharpe": statistics.mean(returns) / volatility * math.sqrt(252)
        if volatility
        else None,
        "max_drawdown": drawdown,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result", type=Path)
    args = parser.parse_args()
    result = json.loads(args.result.read_text())
    curve = result["equity_curve"]
    if not curve:
        raise ValueError("The report contains no equity observations")
    previous = result["config"]["initial_cash"]
    for point in curve:
        observed = point["equity"] / previous - 1
        assert math.isclose(
            observed, point["daily_return"], rel_tol=1e-10, abs_tol=1e-12
        ), f"Daily return disagrees with equity on {point['date']}"
        point["daily_return"] = observed
        previous = point["equity"]
    rows = {"full": metrics(curve)}
    reported = result["performance"]["sharpe_ratio"]
    actual = rows["full"]["sharpe"]
    assert (actual is None and reported is None) or (
        actual is not None
        and reported is not None
        and math.isclose(actual, reported, rel_tol=1e-10, abs_tol=1e-10)
    ), "Sharpe disagrees with the engine"
    for label, subset in [
        ("2020-2023", [p for p in curve if "2020" <= p["date"] < "2024"]),
        ("2024+", [p for p in curve if p["date"] >= "2024"]),
    ]:
        if subset:
            rows[label] = metrics(subset)
    for year in sorted({point["date"][:4] for point in curve}):
        rows[year] = metrics([p for p in curve if p["date"].startswith(year)])
    print(json.dumps(rows, indent=2, ensure_ascii=False, allow_nan=False))


if __name__ == "__main__":
    main()
