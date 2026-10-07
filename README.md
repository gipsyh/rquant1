# rquant

Rust 多股票日频回测框架，保留 PyO3 Python 扩展。策略可以在运行时决定交易哪些股票，行情按需异步拉取，不要求预先声明股票池。支持买入持有、双均线金叉/死叉和地量趋势轮动策略，输出组合净值、分股票持仓和买卖成交记录。

## 快速运行

当前仓库的 `.cargo/config.toml` 默认使用 `.venv/bin/python`；

```bash
cargo run -- bt --end 20241231 --cash 100000 \
  buy-and-hold --symbol 000001.SZ --allocation 1.0

# 多股票：也可重复指定 --symbol
cargo run -- bt --start 20240101 --end 20240131 \
  buy-and-hold --symbol 000001.SZ,600000.SH

# 双均线：5 日 SMA 上穿 20 日 SMA 买入，下穿清仓
cargo run -- bt --start 20240101 --end 20241231 --cash 100000 \
  ma-cross --symbol 000001.SZ,600000.SH --short 5 --long 20 --allocation 0.8

cargo run -- --help
cargo run -- bt --help
cargo run -- bt buy-and-hold --help
cargo run -- bt ma-cross --help
cargo run -- bt low-turnover-trend --help
```

`bt` 由两份配置组成：公共参数 `BacktestConfig` 使用 clap flatten，策略参数 `StrategyConfig` 使用子命令。先写公共参数，再写策略名称及其专属参数。支持 `buy-and-hold`（别名 `buy_and_hold`）、`ma-cross`（别名 `ma_cross`）和 `low-turnover-trend`（别名 `low_turnover_trend`）。前两个策略都有 `--symbol`（必填）和 `--allocation`；`ma-cross` 另有 `--short`（默认 5）和 `--long`（默认 20），要求 `0 < short < long`。

公共参数中 `--end` 默认使用最新可取行情日期：北京时间 19:00 前取昨天，19:00 及以后取今天（不跳过非交易日）；`--start` 默认 `20200101`，起止日期均包含在回测区间内。资金、费用、滑点、交易单位、复权开关及输出路径均属于 `BacktestConfig`，应放在策略名称之前。

`--symbol` 是内置策略的目标参数，不是通用策略接口的股票池约束。自定义策略无需提供目标列表，可在回调中决定查询和交易任意股票。

股票代码支持 `000001`、`000001.SZ`、`000001.XSHE` 等现有沪深股票编码；日期支持 `YYYYMMDD` 和 `YYYY-MM-DD`。代码、日期和参数错误会在请求前报错。JSON 和 HTML 自动保存到本次回测的报告子目录，终端输出摘要和目录路径。

可配置 `--commission`（默认 0.0003，即万分之三，买卖双向收取）、`--min-commission`（每笔最低佣金，默认 5 元）、`--stamp-tax`（默认 0.0005，即万分之五，仅卖出收取）、`--slippage-bps`（保留参数，仅支持 0，严格按开盘原价成交）、`--lot-size`（默认 100）。`--raw` 仅关闭账户的复权收益估值，均线信号仍使用复权收盘价。资金不足、停牌或涨停导致没有成交时，报告保留全现金净值和 `skipped_orders`，不会伪造交易。

日志默认级别为 `info`，输出到 stderr。设置 `RUST_LOG=rquant=debug` 后，每次请求下载日线前会记录股票代码和起止日期；命中缓存或读取内存行情时不输出下载日志。

```bash
RUST_LOG=rquant=debug cargo run -- bt --start 20240101 --end 20240131 \
  buy-and-hold --symbol 000001.SZ
```

## QuantStats 报告

`BacktestResult` 是回测和展示后端之间的统一数据结构，`RqReporter` 是报告接口。`BacktestConfig.reporter` 指定后端，默认 `ReporterKind::QuantStats`。运行 `bt` 会在回测完成后保存 JSON，并调用 Python 生成 HTML；报告后端不查询行情，也不重新运行策略。

```bash
uv sync --locked

# 默认使用 QuantStats，在 report 下创建本次回测的子目录
cargo run -- bt --start 20250101 --end 20260928 --cash 1000000 low-turnover-trend

# 自定义输出路径；报告参数属于 BacktestConfig，放在策略名称之前
cargo run -- bt --start 20250101 --end 20260928 --cash 1000000 \
  --reporter quantstats --report-output reports \
  low-turnover-trend
```

`report_output` 是报告根目录，默认 `report`。每次回测自动创建“策略名-时间戳”子目录，时间使用报告生成时的北京时间，格式为 `MMDD-HHMMSS`；同秒重名时追加序号，保留之前的结果。例如：

```text
report/
└── low_turnover_trend-1007-163000/
    ├── result.json
    └── report.html
```

`result.json` 包含完整的 `BacktestResult`，HTML 及后续其他报告文件保存在同一子目录中，无需单独指定 JSON 路径。

Python 解释器由 QuantStats 后端自动选择：优先使用当前目录的 `.venv/bin/python`（Windows 为 `.venv/Scripts/python.exe`），否则使用 `python3`。Python 适配脚本嵌入 Rust 二进制，无需部署源码目录。

适配器直接使用 `equity_curve.daily_return`，保留首日及零收益交易日，按 252 个交易日年化、无风险利率 0、复利计算；暂不提供基准。HTML 直接使用 QuantStats 生成的报告，不附加 Rust 引擎摘要。QuantStats 的胜率基于收益周期，不代表平仓交易胜率。单日或全零收益回测只输出无法生成图表的说明；空序列、日期乱序或无效收益明确报错。HTML 暂不展示成交与失败订单明细，这些数据完整保存在 JSON 中。

回测前检查报告依赖；回测结束后先保存 JSON，再生成 HTML。HTML 生成失败时，错误信息包含已保存的 JSON 路径。库调用方可以通过 `save_report` 创建本次回测目录并保存全部报告，也可以将 `BacktestResult` 直接交给 `RqReporter::render` 在指定目录中渲染，引擎自身只计算结果。

```bash
cargo test --lib
uv run --locked python -m unittest discover -s tests -p test_report.py
```

## 双均线策略

`MaCross` 使用收盘价的简单移动平均（SMA）。若前一根日线的短均线 ≤ 长均线、最新一根的短均线 > 长均线，则产生金叉；反方向为死叉。金叉时空仓股票买入，死叉时持仓股票清仓；已有持仓不会因持续多头而重复加仓，初始短均线高于长均线也不会直接买入。

从回测开始日积累数据，每只股票至少有 `long + 1` 根已完成日线才判断交叉。均线按实际日线根数计算，缺失日期不补值；每日仅追加新增历史数据，滚动保留 `long + 1` 根。信号使用截至上一交易日的信息，在下一交易日开盘撮合，回测最后一日的收盘信号不会在当日成交。策略统一使用 `原始价格 × 当日因子` 的 `FactorAdjusted` 日线计算指标；每根新增日线只转换一次，不依赖窗口首尾，也不使用未来复权因子。

买入预算为 `信号日收盘权益 × allocation / 股票数`，包含佣金，并受信号日可用现金限制。策略返回卖出、买入两个批次，下一交易日依次执行；此策略仍保守地只根据已知现金生成买入预算，不预支预期卖款。该比例用于开仓预算，不做每日仓位再平衡。停牌、涨跌停或资金不足造成未成交时，保留买入/清仓目标继续尝试，直到出现反向交叉；期末不强制清仓。

Rust 侧使用 `StrategyConfig::MaCross(MaCrossConfig { symbols, short: 5, long: 20, allocation: 1.0 }).build()`，或直接 `MaCross::new(config)`，与现有引擎和 `rqdata.bin` 缓存配合使用。

## 地量趋势轮动策略

```bash
cargo run -- bt --start 20191125 --end 20260928 --cash 1000000 \
  low-turnover-trend --symbol 399101.XSHE
```

`LowTurnoverTrend` 每日使用信号日已生效的指数成分（默认中小综指 `399101.XSHE`）。筛选上市满 120 个自然日、当日有日线且非 ST 的股票，以最近 120 根日线的平均成交额从低到高排名，同值按股票代码排序。前 100 只合格股票构成风险观察池，其中前 6 只组成持仓名单。股票历史不足或窗口内成交额非正时排除；观察池不足 100 只或没有已生效成分时，跳过当天信号，不生成清仓单。接口失败和数据异常仍直接 panic。

观察池各股票用 `FactorAdjusted` 收盘价计算 `close / MA15 - 1`，取中位数。初始为防御状态，严格超过 +0.5% 转进攻，严格低于 -0.5% 转防御，边界和中间区域保持原状态。每根日线成交量必须为有限正数，在校验时断言，不通过过滤零成交量来拼接窗口。

首个信号日就回溯历史，新进入指数的股票也立即预热。历史不足时向前扩大查询区间，直到取得所需根数，或到达上市日与 `--history-start` 中较晚的日期；该参数默认 `20000101`，与当前 Tushare 历史 ST 数据起点一致。后续按股票只追加新增日期，滚动保存所需根数；成分离开后再次进入时补齐中间历史，不重复复权。日线与指数成分查询均不得超过信号日。

调仓比较实际持仓与新名单：保留交集不动，第一批用 `SellAll` 清仓落选股票，第二批用 `BuyWeights` 买入新增股票。进攻状态平分第二批开始时全部可用现金的 95%；防御状态平分该现金的 `95% × defensive_fraction`（默认 23.75%），预算包含佣金。**这些比例是新增买入的现金预算，不是账户总仓位目标**。名单不变时不因趋势切换或持仓市值漂移交易，不提供 `rebalance_tolerance` 或 `TargetWeights`。

卖出失败不阻止后续批次，但失败卖单不产生现金；买入失败的预算留在现金中，不重新分配。次日仍根据实际持仓重新判断，已买入少量的股票不会继续补仓。没有新增股票时不产生买入批次；回测末日信号不执行。

策略参数：`--top-k`（6）、`--liquidity-lookback`（120）、`--trend-lookback`（15）、`--breadth-count`（100）、`--trend-band`（0.005）、`--defensive-fraction`（0.25）、`--minimum-listed-days`（120）、`--history-start`（20000101）。现金保留基准固定为 5%。Rust 可用 `LowTurnoverTrend::new(LowTurnoverTrendConfig::default())` 创建策略。

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

策略配置枚举和 `build()` 位于 `strategy/mod.rs`，`build()` 直接返回 `Box<dyn Strategy>`，`BuyAndHold::new(config)` 直接返回策略实例；配置无效时直接报错（panic）。具体策略的配置与实现放在一起；新增策略时添加对应 config 和枚举变体。`BacktestConfig` 也是引擎实际使用的配置，不再从 CLI 参数复制转换。默认值仅在 clap 属性中定义，Rust 的 `Default` 通过 clap 解析空参数生成配置，不读取进程命令行参数。Rust 的 `Default` 和 CLI 均使用 `20200101` 到 `utils::latest_rqdate()`，截止日期以北京时间 19:00 为分界。

`DataProvider` 使用 `#[async_trait::async_trait]` 定义异步方法，实现时也需要添加该属性，支持 `dyn DataProvider` 动态分发。查询方法均使用 `&mut self`，允许数据源直接更新内部状态。提供 `trading_days(range)`、`stock_bar(symbol, range)` 和 `index_comp(symbol, range)`（`range: DateRange`，为日期闭区间），分别返回 `Vec<Date>`、`StockHistBar` 和 `IndexHistComp`；请求失败或数据无效时直接 panic，日线无行情时仍返回带请求区间的空历史。引擎启动时只取交易日历；策略查询历史、撮合订单和每日持仓估值时，才查询对应股票和日期。

指数成分接口返回查询闭区间的历史，通过 `composition(date)` 取得当天已生效的最近一期 `Arc<IndexComp>`，再用 `weights()` 读取股票到权重比例的映射。例如：

```rust,ignore
let history = provider.index_comp("000300.XSHG", rquant::utils::DateRange::new(start, end)).await;
let comp = history.composition(as_of)?;
for (stock, weight) in comp.weights() {
    println!("{stock}: {weight}"); // 0.005 表示 0.5%
}
```

批量历史查询使用 `ctx.stocks_bars(&symbols, start, end).await`，返回 `BTreeMap<StockSymbol, Vec<StockBar>>`；重复代码合并，无行情股票保留空数组，返回原始价格，日期不得超过策略当日。`DataProvider::stocks_bars(&requests)` 的每项请求是 `(StockSymbol, DateRange)`，支持不同股票或缓存缺口使用不同区间，返回 `Vec<StockHistBar>`，每项与请求逐项对应，并保留完整请求区间（包括空行情区间）。数据源默认串行实现；Tushare 固定最多 4 个异步任务并发调用原有 `stock_bar`，不修改 HTTP API，也不拆分整段日线。任务共享连接池，批量下载并发仅由 `.buffered(4)` 控制；任一查询失败仍直接 panic。

内存和磁盘缓存均透传批量查询：合并同一股票的请求，仅把未覆盖区间交给底层，首次访问仍预取整个回测区间。基础信息通过 `stocks_info` 批量查询和缓存：Tushare 将去重后的代码以逗号连接，一次请求 `stock_basic`；先查 `L`，只将仍缺失的代码继续按 `D`、`P` 顺序请求，不再逐股并发查询。返回顺序与输入一致，`stock_info` 复用单元素的 `stocks_info`；空输入不发请求，缺失、重复或非请求代码仍报错。地量趋势策略已使用批量基础信息和日线查询进行首次预热及后续追加；历史根数不足时，保留原有按股票向前补拉逻辑。

指数代码接受 `000300`、`000300.SH`、`000300.XSHG` 等写法，同一指数共用缓存。Tushare 的 [`index_weight`](https://tushare.pro/document/2?doc_id=96) 按完整自然月请求，额外回溯起点前一个月；保留起点已生效的最近快照以及区间内全部变更，不保留终点之后的数据。上游百分数除以 100 转为比例，保留零权重、不重新归一化；重复成分、无效权重或达到 6000 行的可能截断响应会报错。空历史可以缓存，若查询日及以前没有快照，`composition` 返回错误，不使用未来快照回填。

引擎接收 `Box<dyn DataProvider>`，为每次运行创建独立的 `MemCacheProvider::new(provider, start, end)`，并在返回时释放缓存和底层数据源。引擎同时持有 `Box<dyn Strategy>`，可在运行时选择不同策略。`Strategy: Send` 使用 `#[async_trait::async_trait]`，实现策略时也需要添加该属性；提供 `name`、异步的 `on_trade_day` 和默认空实现的 `on_order_failed`，没有固定股票池接口。收盘回调返回 `Vec<Vec<Order>>`，遇到不可恢复的错误直接 `expect` / `panic!` 终止本次回测：

```rust,ignore
async fn on_trade_day(
    &mut self,
    ctx: &BtContext<'_>,
) -> Vec<Vec<Order>> {
    // symbol 可以来自策略当时的判断；无需向引擎预先注册。
    let bars = ctx.stock_bars(symbol, lookback_start, ctx.date())
        .await;
    let adjusted: Vec<_> = bars.iter().map(|bar| bar.adjusted()).collect();
    // 用 adjusted 计算指标；订单限价使用 bars 中的原始价格。
    vec![
        vec![Order::SellAll { symbol: old_symbol }],
        vec![Order::BuyWeights {
            weights: [(selected_symbol, 0.6), (other_symbol, 0.4)].into(),
        }],
    ]
}
```

`ctx.date()` 是当前回测交易日；`ctx.position(symbol)` 提供单股持仓，`ctx.positions` 提供全部持仓，`ctx.cash` / `ctx.equity` 提供当日收盘后的现金和权益。`ctx.stock_bars(symbol, start, end).await` 查询闭区间，直接返回 `Vec<StockBar>`，包含原始 OHLC 和 `Adjustment::Raw(factor)`；无行情时返回空数组，日期、因子或数据无效时 panic。统一使用区间接口，不另设单日方法。策略可查询当前交易日及以前的日线，可以向回测开始日之前回溯；未来查询在发起网络请求前报错。

`ctx.stock_info(symbol).await` 提供股票基础信息，上市日期可用于上市天数筛选；名称、行业为静态元数据，不代表回测当日历史状态。`ctx.index_comp(symbol, start, end).await` 提供指数成分历史，再通过 `composition(date)` 获取已生效成分，区间不能包含未来日期。

`bar.adjusted()` 返回策略指标统一使用的单根 `StockBar`，OHLC 和涨跌停价按 `原始价格 × 当日因子` 独立换算，结果标记为 `Adjustment::FactorAdjusted`，不属于按窗口基准归一化的前复权或后复权。不修改输入，成交量、成交额、市值及其他非价格字段保持不变。输入须为带有效因子的原始日线，无效价格或因子直接 panic。每根日线只依赖自身价格和因子，分段转换可直接拼接；追加或移除日线不改变其他日线的结果。已复权输入会被拒绝，避免重复复权。指标价格不可直接用作订单限价，交易仍使用原始价格。

`ctx.is_tradable(symbol, date).await` 查询当日或过去某日是否可供策略选入：有当日日线且 `st == false` 才返回 `true`；已查询区间内无日线返回 `false`。`DataProvider::is_tradable` 没有默认实现；生产环境由 `MemCacheProvider` 判断，`DiskCacheProvider` 转发给内存缓存。日期不在覆盖区间时先延长日线覆盖区间，再判断是否有 bar 且非 ST；已覆盖但无 bar 的日期不重复下载，补拉失败仍 panic。原始 `TushareProvider` 不直接提供此判断，需包装缓存后使用。直接调用 `StockHistBar::is_tradable(date)` 时必须处于已覆盖闭区间，否则 panic。此方法只判断日线存在与 ST，不额外筛选成交量、上市天数、涨跌停或订单最终能否成交；日线成交量在数据校验时已要求为有限正数。

引擎逐日串行 `.await` 策略回调，上下文直接访问异步数据源和共享行情缓存，兼容 Tokio 单线程和多线程运行时。`BtContext` 通过异步锁访问数据源，历史查询仅需 `&self`。`run` 和 `run_with_data` 仍为异步方法。策略需要满足 `Send + 'static`，即持有自身数据；如需与调用方共享状态，可使用 `Arc<Mutex<_>>`。

`MemCacheProvider` 内部持有 `Box<dyn DataProvider>`，缓存日线与指数成分；交易日历每次直接转发。首次查询某股票或指数时，下载构造参数 `start..=end` 的整个区间，之后仅返回调用方请求的日期，停牌或空结果也会缓存。查询回测开始前的历史时，首次下载范围会涵盖该历史区间，之后向外扩展仅补拉缺少的部分。指数补拉时会合并成分变更并保留起点基准快照，已缓存成分通过 `Arc` 共享。撮合、估值和策略历史查询共用此缓存；未访问的标的不会下载。缓存中即使已有未来日线，Context 仍禁止策略读取未来数据。这层缓存仅存在于本次回测内存中。

CLI 默认在 Tushare 外包装 `DiskCacheProvider::new(provider, start, end)`，将日线跨运行保存在启动时工作目录的 `rqdata.bin` 中，路径固定在结构体内部。构造时读取已有文件，不存在则立即创建空缓存；每次 Drop 都将全部缓存以 bincode 二进制格式重写，先写同目录临时文件，再原子替换。文件直接序列化完整 `RqData`，包含 `stock: HashMap<StockSymbol, Stock>`，每个 `Stock` 同时保存基础信息及 `bars: Option<StockHistBar>`。`None` 表示尚未查询日线，`Some` 表示区间已查询（允许无行情）。每个 `StockHistBar` 统一保存已查询的日期闭区间 `range` 和日线数组 `bars`；日线属于同一股票，按日期严格升序、无重复且位于对应区间内，空行情仍保留覆盖区间；命中已有范围不请求上游，扩展范围只补拉缺失部分。`stock_info(symbol)` 返回基础信息完整且 `bars` 为 `None` 的 `Stock`；首次日线查询前先获取并缓存基础信息，之后补拉仅更新 `Stock.bars`。Tushare 通过 [`stock_basic`](https://tushare.pro/document/2?doc_id=25) 查询上市、退市或暂停上市股票，缺少名称、上市日期或返回无效数据时 panic。股票基础信息是查询时的静态信息，不代表回测日的历史名称或行业。交易日历不落盘，仍转发到底层数据源。旧 `rdata.ron`、`rqdata.ron` 等文本格式缓存及股票信息、日线分开保存的缓存格式不自动迁移；旧格式缓存需重新下载，读取失败会保留原文件。Rust 调用方可按上面的示例自行包装，引擎本身不强制使用磁盘缓存。

指数历史统一保存在 `RqData.index` 中对应 `Index.comp`，随指数信息一起保存和恢复。首次创建 `Index` 前通过 `index_name(symbol)` 获取真实名称（Tushare 使用 `index_basic`），名称与成分均成功获取后才写入缓存；名称缺失、为空或查询失败会报错。已有指数名称在命中和补拉时保持不变，磁盘加载也拒绝空名称。加载时校验股票代码、日期范围、日线顺序、指数生效日顺序及权重；缓存读取失败、损坏或数据不一致时直接报错并保留原文件；Drop 写入失败会记录错误。该文件只供一个存活的缓存实例使用，并发实例不合并数据。缓存不会自动刷新历史数据，切换数据源、复权口径或需要获取上游修订时，应删除 `rqdata.bin` 后重新下载。强制终止进程等不执行 Drop 的退出方式不会保存本次新增数据。

订单使用单层 `Order` 枚举：

- `BuyLimit { symbol, shares, price }`：限价买入精确整手股数；限价不低于次日开盘价且信号日与执行日复权因子相同才成交。
- `BuyAmount { symbol, cash_amount }`：按次日开盘价买入预算内最多整手股数，预算包含佣金。
- `SellLimit { symbol, shares, price }`：限价卖出精确股数；限价不高于次日开盘价且信号日与执行日复权因子相同才成交。
- `SellAll { symbol }`：按次日开盘价卖出全部可卖持仓，保留当天新买入的 T+1 锁定部分。
- `BuyWeights { weights }`：一个订单含多只股票，每只预算为本批次开始前现金乘权重（含佣金），权重有限、非负且合计不超过 1。各股票独立成交，失败预算不重新分配。

外层批次按顺序执行，内层订单基于同一个批次开始快照统一撮合与结算，是账户语义上的并行，不按数组位置抢占现金。前一批卖出所得可以用于后一批，同批卖出所得不能用于同批买入。候选成交合计超过批次开始现金时，所有占用现金的候选订单一起失败；同一股票卖单合计超过可卖股数时，该股票冲突卖单一起失败。其他不冲突的订单继续执行；精确股数订单不部分成交。当天买入的股数在所有后续批次中均不可卖出，但不影响已有可卖底仓。

每日顺序为执行上一交易日订单、收盘估值、调用 `on_trade_day`。回测首日只产生信号，最后一天产生的订单不执行。失败订单立即结束，记录到 `skipped_orders`，并调用 `on_order_failed(&OrderFailure)`；默认回调不处理，也不会自动延期重试，策略可在后续收盘重新下单。权重订单按失败股票分别回调。

限价单在资源分配前检查信号日与执行日的原始复权因子；因子变化，或任一端缺少有效因子时，该单失败，不占用其他订单的预算或可卖股数。此规则同样适用于 `--raw`。按金额、权重买入及全部卖出使用执行日原始开盘价，不因因子变化自动失败。账户继续沿用下述收益近似模型，尚未实现分红送转的独立记账。

`trades` 和 `skipped_orders` 记录 `signal_date`、执行 `date`、从 0 开始的 `batch_index` 和 `order_index`。同批成交的 `cash_after` 均为整批结算后的余额。

离线回放使用 `engine.run_with_data(strategy, &[market_a, market_b]).await?`；内部的 `InMemoryProvider` 走同一按需查询路径。

## 数据与成交口径

- 使用 Tushare [`daily`](https://tushare.pro/document/2?doc_id=27)、[`adj_factor`](https://tushare.pro/document/2?doc_id=28)、[`trade_cal`](https://tushare.pro/document/2?doc_id=26)、[`stk_limit`](https://tushare.pro/document/2?doc_id=183)、[`stock_st`](https://tushare.pro/document/2?doc_id=397)。账号需要这些接口权限；错误会向上传递。仅拉取请求范围内的数据；每只股票的日线、复权因子、涨跌停价和 ST 状态优先按完整请求区间各查询一次，ST 响应触及上限时才拆分，最终日线按日期升序排列；交易日历仍按年查询；成交量从手转为股，成交额从千元转为元。未接入市值或自动限频调度，沿用客户端已有重试机制。
- `TushareApi` 定义接口的 `MAX_ROWS` 和 `split_on_limit(&Params) -> (Params, Params)`，由 `tushare_apis!` 生成实现，具名方法和直接 `query` 共用规则：`daily`、`daily_basic`、`stock_basic` 各 6000 行，`stk_limit` 5800 行，`stock_st` 1000 行，`suspend_d` 5000 行，`index_basic` 8000 行。`index_weight` 保留项目现有 6000 行请求限制和截断保护，其文档未注明数字上限。`adj_factor`、`namechange`、`trade_cal`、`index_daily` 文档也未注明数字上限，显式配置为 `None`（不代表无限制）。各配置旁附官方文档链接。
- 返回行数达到或超过上限时丢弃可能截断的父响应，调用拆分方法。当前仅 `stock_st` 支持按日期闭区间二分为 `[start, mid]` 和 `[mid + 1天, end]`，子请求顺序执行且可递归拆分，沿用原有重试策略。仅合并完整子响应，合并后的总行数可超过单次上限。不支持拆分的接口默认 panic；ST 区间缺失/无效、单日仍触顶，或携带 `trade_date`、`limit`、`offset` 时也直接 panic。子请求失败或字段不一致不返回部分数据。日线仍逐根检查对应复权因子，缺失即 panic。
- `StockBar.st` 按交易日的 `stock_st` 名单填充，覆盖 ST 和 *ST；摘帽后为 `false`。每个非空日线区间按同一股票、日期范围查询 ST 状态，随日线缓存，空日线不额外查询。接口需要 3000 积分起，历史数据从 `20000101` 开始，因此 Tushare 日线查询不支持更早日期；权限不足或响应数据无效直接报错。
- 遇到明确的分钟频次超限时，仅当前 worker 按 `rate_limit_cooldown`（默认 5 秒）冷却后重试，每次请求最多重试 `rate_limit_max_retries`（默认 20 次，累计等待约 100 秒，不含请求耗时），不暂停其他 worker。普通超时、连接错误及 HTTP 5xx 保留原来的指数退避，使用独立的 `max_retries` 计数；两类重试互不影响，`max_retries = 0` 只关闭普通重试，`rate_limit_max_retries = 0` 只关闭分钟限频重试。权限不足、每日额度耗尽不进入分钟限频重试。`RetryPolicy::none()` 把两类上限都归零，禁用全部重试。
- 策略在收盘后查询包含当天的已完成日线，生成下一交易日开盘订单。内置 buy and hold 为每只目标股票分配 `初始资金 × allocation / 股票数` 的独立预算，第一次可成交时买入；未成交的股票次日重试，已成交的股票不再买入，也不在期末卖出。成交价为下一交易日原始开盘价。
- 预算包含佣金，按 `lot_size` 向下取整，不允许融资。默认交易单位是简化的 100 股模型，尚未完整实现各板块的申报数量规则；例如科创板使用前需自行配置交易单位。费用在 `BacktestConfig` 中配置：买卖佣金均为 `max(notional × commission_rate, minimum_commission)`，默认万分之三、每笔最低 5 元；印花税仅卖出时收取 `notional × stamp_tax_rate`，默认万分之五，无最低收费。买入扣除成交额和佣金，卖出到账为成交额减佣金、印花税。税率在整个回测区间固定使用配置值；当前不计算过户费。
- 开盘撮合只使用开盘价、涨跌停价及原始复权因子，不用当日 high、low、close 或全天成交量决定是否成交。无开盘行情、开盘价无效、缺少有效涨跌停价，或买入触及涨停／卖出触及跌停时失败。限价不满足开盘条件即失败，不等待盘中触价；不模拟盘口排队或成交量容量。日线完整性仍在历史查询和收盘估值时校验。
- 无日线的交易日延续上一估值，买入前保持现金。数据缺口与停牌目前无法区分，退市后也没有强制清算模型，应选择数据完整且符合研究目的的区间。空日线结果作为未成交记录并缓存，全部缺失时保留全现金组合；重复记录、非法价格、复权因子或接口错误会报错。
- 默认采用复权收益模型：每笔持仓市值为 `买入股数 × 当日原始收盘价 × 当日因子 / 买入日因子`，现金余额单独保留。只使用买入日和当日因子，没有以回测期末因子反推买入股数。**这是一种公司行动调整后的收益近似，不是逐笔分红到账、红利税、配股或送转股记账**；`purchased_shares` 表示当前未平仓的买入股数，清仓后删除持仓。卖出使用同一收益单位结算：`notional = Σ(各次买入股数 / 买入日因子) × 卖出日因子 × 卖出价`，再扣除佣金和印花税。限价卖出股数沿用 `purchased_shares` 的买入股数口径，部分卖出按可卖持仓比例扣减收益单位。`Trade.shares` 记录平掉的买入股数，`price` 记录原始开盘成交价；在因子变化时，两者乘积不等于结算金额，应以 `notional` 为准。这沿用收益近似模型，并非真实公司行动后的股数交割。`--raw` 只计算未复权价格收益，跨公司行动日会出现价格跳变；当前数据源仍会拉取上述五类数据。

## 输出与指标

完整报告包含 `symbols`（曾下单的股票）、`config`、`performance`、`trades`、`equity_curve`、`skipped_orders`。每日记录包含按股票代码索引的 `positions`、现金、合计持仓市值、总权益、归一化净值、日收益和回撤。`trades` 和 `skipped_orders` 均携带股票代码及 `side`（`buy` / `sell`）；成交记录的 `notional` 是扣费前结算金额，`commission` 和 `stamp_tax` 分别记录佣金和印花税（买入为 0）；`performance` 中的 `total_commission`、`total_stamp_tax` 和 `total_fees` 分别汇总佣金、印花税及二者合计。所有收益率为小数，例如 `0.05` 代表 5%。

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
