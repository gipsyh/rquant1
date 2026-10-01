# rquant

Rust 多股票日频回测框架，保留 PyO3 Python 扩展。策略可以在运行时决定交易哪些股票，行情按需异步拉取，不要求预先声明股票池。当前实现 buy and hold：为目标股票等额分配预算，各自首次买入后持有至结束，输出组合净值、分股票持仓和成交记录。

## 快速运行

需要 Rust 工具链和 Python 环境。当前仓库的 `.cargo/config.toml` 默认使用 `.venv/bin/python`；没有虚拟环境时先运行 `python3 -m venv .venv`，也可通过 `PYO3_PYTHON` 指定解释器。

```bash
cargo run -- bt --end 20241231 --cash 100000 --output result.json \
  buy-and-hold --symbol 000001.SZ --allocation 1.0

# 多股票：也可重复指定 --symbol
cargo run -- bt --start 20240101 --end 20240131 --output portfolio.json \
  buy-and-hold --symbol 000001.SZ,600000.SH

cargo run -- --help
cargo run -- bt --help
cargo run -- bt buy-and-hold --help
```

`bt` 由两份配置组成：公共参数 `BacktestConfig` 使用 clap flatten，策略参数 `StrategyConfig` 使用子命令。先写公共参数，再写策略名称及其专属参数。当前支持 `buy-and-hold`（别名 `buy_and_hold`），其 `BuyAndHoldConfig` 包含 `--symbol`（必填）和 `--allocation`。

公共参数中 `--end` 必填，`--start` 默认 `20200101`，起止日期均包含在回测区间内。资金、费用、滑点、交易单位、复权开关及输出路径均属于 `BacktestConfig`，应放在策略名称之前。

`--symbol` 是内置 buy and hold 策略的目标参数，不是通用策略接口的股票池约束。自定义策略无需提供目标列表，可在回调中决定查询和交易任意股票。

股票代码支持 `000001`、`000001.SZ`、`000001.XSHE` 等现有沪深股票编码；日期支持 `YYYYMMDD` 和 `YYYY-MM-DD`。代码、日期和参数错误会在请求前报错。未指定 `--output` 时，stdout 输出完整 JSON，stderr 输出摘要。

可配置 `--commission`（默认 0.0003）、`--min-commission`（默认 5 元）、`--slippage-bps`（默认 0）、`--lot-size`（默认 100）。`--raw` 关闭复权收益估值。资金不足、停牌或涨停导致没有成交时，报告保留全现金净值和 `skipped_orders`，不会伪造交易。

日志默认级别为 `info`，输出到 stderr。设置 `RUST_LOG=rquant=debug` 后，每次请求下载日线分段前会记录股票代码和起止日期；命中缓存或读取内存行情时不输出下载日志。

```bash
RUST_LOG=rquant=debug cargo run -- bt --start 20240101 --end 20240131 \
  buy-and-hold --symbol 000001.SZ
```

## Python

```bash
source .venv/bin/activate
python -m pip install 'maturin>=1.15,<2'
maturin develop
```

```python
import rquant

result = rquant.backtest_buy_and_hold(
    "000001.SZ", "20240101", "20241231",
    initial_cash=100_000,
    allocation=1.0,
    commission_rate=0.0003,
    minimum_commission=5.0,
    slippage_bps=0.0,
    lot_size=100,
    adjust_returns=True,
    # token="...",  # 显式覆盖；不传则沿用客户端默认值
)
print(result["performance"])
print(result["trades"])

# 可选：安装 pandas 后查看净值表
# import pandas as pd
# curve = pd.DataFrame(result["equity_curve"])
```

Python 便利函数目前接受单只目标股票，内部使用同一个多股票引擎。返回普通 `dict`，日期为 ISO 字符串。数据拉取期间释放 GIL；参数错误为 `ValueError`，网络、权限和行情错误为 `RuntimeError`。

## Rust

```rust,no_run
use rquant::{
    data::{InstrSymbol, tushare::TushareProvider},
    utils::parse_date,
    engine::{BacktestConfig, BacktestEngine},
    strategy::{BuyAndHoldConfig, StrategyConfig},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = Box::new(TushareProvider::new());
    let backtest = BacktestConfig {
        start: parse_date("20240101")?,
        end: parse_date("20241231")?,
        ..Default::default()
    };
    let strategy = StrategyConfig::BuyAndHold(BuyAndHoldConfig {
        symbols: vec![
            "000001.SZ".parse::<InstrSymbol>()?,
            "600000.SH".parse::<InstrSymbol>()?,
        ],
        allocation: 1.0,
    });
    let engine = BacktestEngine::new(backtest)?;
    let result = engine.run(provider, strategy.build()).await?;
    println!("{:?}", result.performance);
    Ok(())
}
```

策略配置枚举和 `build()` 位于 `strategy/mod.rs`，`build()` 直接返回 `Box<dyn Strategy>`，`BuyAndHold::new(config)` 直接返回策略实例；配置无效时直接报错（panic）。具体策略的配置与实现放在一起；新增策略时添加对应 config 和枚举变体。`BacktestConfig` 也是引擎实际使用的配置，不再从 CLI 参数复制转换。Rust 的 `Default` 使用 `20200101` 到当前 UTC 日期；CLI 仍要求显式传入 `--end`。

`DataProvider` 使用 `#[async_trait::async_trait]` 定义异步方法，实现时也需要添加该属性，支持 `dyn DataProvider` 动态分发。两个查询方法均使用 `&mut self`，允许数据源直接更新内部状态。分别提供 `trading_days(start, end)` 和 `daily_bars(symbol, start, end)`，直接返回 `Vec<Date>` 和 `Vec<StockDailyBar>`；请求失败或数据无效时直接 panic，空行情正常返回空数组。引擎启动时只取交易日历；策略查询历史、撮合订单和每日持仓估值时，才查询对应股票和日期。

引擎接收 `Box<dyn DataProvider>`，为每次运行创建独立的 `MemCacheProvider::new(provider, start, end)`，并在返回时释放缓存和底层数据源。引擎同时持有 `Box<dyn Strategy>`，可在运行时选择不同策略。`Strategy: Send` 使用 `#[async_trait::async_trait]`，实现策略时也需要添加该属性；只有 `name` 和异步的 `on_trade_day`，没有固定股票池接口。回调直接返回 `Vec<Order>`，遇到不可恢复的错误直接 `expect` / `panic!` 终止本次回测：

```rust,ignore
async fn on_trade_day(
    &mut self,
    ctx: &BtContext<'_>,
) -> Vec<Order> {
    // symbol 可以来自策略当时的判断；无需向引擎预先注册。
    let history = ctx.history(symbol, lookback_start, previous_date)
        .await
        .expect("历史行情查询失败");
    // 在这里根据 history 决定目标股票和订单预算。
    vec![Order::Buy { symbol: selected_symbol, cash_amount: 10_000.0 }]
}
```

`ctx.date()` 是当前回测交易日；`ctx.position(symbol)` 提供单股持仓，`ctx.positions` 提供全部持仓，`ctx.cash` / `ctx.equity` 提供现金和上一收盘权益。`ctx.history(symbol, start, end).await.expect("历史行情查询失败")` 异步查询历史区间，`ctx.bar(symbol, date).await.expect("日线查询失败")` 异步查询某日。策略只可查询当前交易日之前的日线，可以向回测开始日之前回溯；当日及未来查询在发起网络请求前报错。

引擎逐日串行 `.await` 策略回调，上下文直接访问异步数据源和共享行情缓存，兼容 Tokio 单线程和多线程运行时。`BtContext` 通过异步锁访问数据源，历史查询仅需 `&self`。`run` 和 `run_with_data` 仍为异步方法。策略需要满足 `Send + 'static`，即持有自身数据；如需与调用方共享状态，可使用 `Arc<Mutex<_>>`。

`MemCacheProvider` 内部持有 `Box<dyn DataProvider>`，只缓存日线；交易日历每次直接转发。首次查询某股票时，下载构造参数 `start..=end` 的整个区间，之后仅返回调用方请求的日期，停牌或空结果也会缓存。查询回测开始前的历史时，首次下载范围会涵盖该历史区间，之后向外扩展仅补拉缺少的部分。撮合、估值和策略历史查询共用此缓存；未访问的股票不会下载。缓存中即使已有未来日线，Context 仍禁止策略读取当日及未来数据。缓存仅存在于本次回测内存中，不跨运行保留。

订单显式携带 `symbol` 和包含佣金的金额预算 `cash_amount`，同日多笔订单按返回顺序执行、共享现金；超过剩余现金的订单会被拒绝并记录原因，其他订单仍可处理。当前只实现买入订单，卖出和完整调仓尚未实现。

离线回放使用 `engine.run_with_data(strategy, &[market_a, market_b]).await?`；内部的 `InMemoryProvider` 走同一按需查询路径。

## 数据与成交口径

- 使用 Tushare [`daily`](https://tushare.pro/document/2?doc_id=27)、[`adj_factor`](https://tushare.pro/document/2?doc_id=28)、[`trade_cal`](https://tushare.pro/document/2?doc_id=26)、[`stk_limit`](https://tushare.pro/document/2?doc_id=183)、[`stock_st`](https://tushare.pro/document/2?doc_id=397)。账号需要这些接口权限；错误会向上传递。仅拉取请求范围内的数据；跨年的历史查询按自然年分段，日期升序排列；成交量从手转为股，成交额从千元转为元。未接入市值、磁盘缓存或自动限频调度，沿用客户端已有重试机制。
- `StockDailyBar.st` 按交易日的 `stock_st` 名单填充，覆盖 ST 和 *ST；摘帽后为 `false`。每个非空日线分段按同一股票、日期范围查询 ST 状态，随日线缓存，空日线不额外查询。接口需要 3000 积分起，历史数据从 `20000101` 开始，因此 Tushare 日线查询不支持更早日期；权限不足或响应数据无效直接报错。
- 策略只能查询已完成的历史日线；买入意图在开盘前产生。内置 buy and hold 为每只目标股票分配 `初始资金 × allocation / 股票数` 的独立预算，第一次可成交时买入；未成交的股票次日重试，已成交的股票不再买入，也不在期末卖出。成交价为原始开盘价加买入滑点。
- 预算包含佣金，按 `lot_size` 向下取整，不允许融资。默认交易单位是简化的 100 股模型，尚未完整实现各板块的申报数量规则；例如科创板使用前需自行配置交易单位。佣金为成交额乘费率与最低佣金的较大值，不单独计算过户费。
- 缺少日线或成交量为零时不成交；开盘或滑点后价格触及涨停时保守跳过；缺少有效涨停价也跳过。滑点后价格高于当日最高价时不成交。日频数据不模拟开盘盘口、排队、成交量约束和部分成交。
- 无日线的交易日延续上一估值，买入前保持现金。数据缺口与停牌目前无法区分，退市后也没有强制清算模型，应选择数据完整且符合研究目的的区间。空日线结果作为未成交记录并缓存，全部缺失时保留全现金组合；重复记录、非法价格、复权因子或接口错误会报错。
- 默认采用复权收益模型：每笔持仓市值为 `买入股数 × 当日原始收盘价 × 当日因子 / 买入日因子`，现金余额单独保留。只使用买入日和当日因子，没有以回测期末因子反推买入股数。**这是一种公司行动调整后的收益近似，不是逐笔分红到账、红利税、配股或送转股记账**；`purchased_shares` 表示累计买入股数。`--raw` 只计算未复权价格收益，跨公司行动日会出现价格跳变；当前数据源仍会拉取上述五类数据。

## 输出与指标

完整报告包含 `symbols`（曾下单的股票）、`config`、`performance`、`trades`、`equity_curve`、`skipped_orders`。每日记录包含按股票代码索引的 `positions`、现金、合计持仓市值、总权益、归一化净值、日收益和回撤。`trades` 和 `skipped_orders` 均携带股票代码。所有收益率为小数，例如 `0.05` 代表 5%。

总收益按期末权益 / 初始资金 − 1 计算。首日日收益包含从初始资金到首日收盘的变化及佣金；回撤峰值从初始资金开始。年化收益采用 `(期末权益 / 初始资金)^(252 / 交易日观测数) − 1`，波动率用日收益样本标准差乘 `sqrt(252)`，夏普比率假设无风险利率为零。样本不足或零波动时对应指标为 `null`。期末权益按持仓估值计算，不包含假设卖出的成本。

## 验证

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
# 安装 Python 扩展后
python -m unittest discover -s tests -p 'test_python.py'
```

常规测试使用内存行情和本地 mock HTTP 服务，不消耗 Tushare 配额。覆盖动态选股、未使用股票零请求、重叠查询与空结果缓存、多股票独立估值、共享现金约束、费用与滑点、全现金、复权、停牌、涨停、无未来数据、分年请求、字段顺序变化、缺失/重复数据及接口权限错误。原有在线测试仍默认忽略：`cargo test data::tushare -- --ignored`。
