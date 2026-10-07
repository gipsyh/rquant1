"""BacktestResult JSON -> QuantStats HTML. Embedded by the Rust backend."""

import html
import json
import math
from pathlib import Path
import sys


def returns_from_result(result, pd):
    points = result["equity_curve"]
    if not points:
        raise ValueError("回测收益序列为空，无法生成报告")
    dates = pd.to_datetime([point["date"] for point in points], format="%Y-%m-%d")
    if dates.has_duplicates or not dates.is_monotonic_increasing:
        raise ValueError("回测日期必须严格递增且不重复")
    values = [point["daily_return"] for point in points]
    if any(value is None or not math.isfinite(value) or value <= -1 for value in values):
        raise ValueError("日收益率必须是大于 -1 的有限数值")
    # 保留第一日及零收益交易日，不从净值 pct_change，也不补自然日。
    return pd.Series(values, index=dates, dtype=float, name=result["strategy"])


def engine_summary(result):
    metrics = [
        ("final_equity", "Final equity", ",.2f"),
        ("total_return", "Total return", ".2%"),
        ("annualized_return", "Annualized return", ".2%"),
        ("max_drawdown", "Maximum drawdown", ".2%"),
        ("annualized_volatility", "Annualized volatility", ".2%"),
        ("sharpe_ratio", "Sharpe ratio", ".3f"),
        ("total_commission", "Commission", ",.2f"),
        ("total_stamp_tax", "Stamp tax", ",.2f"),
        ("total_fees", "Total fees", ",.2f"),
        ("trade_count", "Executed orders", ",d"),
    ]
    rows = []
    for key, label, formatting in metrics:
        value = result["performance"].get(key)
        text = "N/A" if value is None else format(value, formatting)
        rows.append(f"<tr><td>{label}</td><td>{text}</td></tr>")
    valuation = "Corporate-action adjusted returns" if result["config"]["adjust_returns"] else "Raw price returns"
    return (
        '<section style="max-width:960px;margin:24px auto;font-family:sans-serif">'
        '<h2>Rust engine summary</h2>'
        f'<p>Strategy: {html.escape(result["strategy"])}</p>'
        '<p>252 trading days/year; risk-free rate 0; returns include trading fees. '
        'QuantStats independently calculates the analysis that follows; '
        'its win rates describe return periods, not closed trades.</p>'
        f'<p>Valuation: {valuation}</p>'
        f'<table>{"".join(rows)}</table></section>'
    )


def generate(result, output, pd, qs):
    returns = returns_from_result(result, pd)
    summary = engine_summary(result)
    # 全现金和单日回测没有足够的波动样本；不向 QuantStats 填入虚构收益。
    if len(returns) < 2 or (returns == 0).all():
        output.write_text(
            '<!doctype html><html><head><meta charset="utf-8"><title>Backtest report</title></head><body>'
            + summary
            + '<p style="text-align:center">QuantStats charts omitted: fewer than two observations '
            'or all daily returns are zero.</p></body></html>',
            encoding="utf-8",
        )
        return
    qs.reports.html(
        returns,
        benchmark=None,
        rf=0.0,
        periods_per_year=252,
        compounded=True,
        match_dates=False,
        title=f'{html.escape(result["strategy"])} Backtest Report',
        strategy_title=html.escape(result["strategy"]),
        output=str(output),
    )
    document = output.read_text(encoding="utf-8")
    body = document.index(">", document.index("<body")) + 1
    output.write_text(document[:body] + summary + document[body:], encoding="utf-8")


def main():
    import matplotlib
    matplotlib.use("Agg")
    import pandas as pd
    import quantstats as qs

    if sys.argv[1:] == ["--check"]:
        return
    with open(sys.argv[1], encoding="utf-8") as source:
        result = json.load(source)
    generate(result, Path(sys.argv[2]), pd, qs)


if __name__ == "__main__":
    main()
