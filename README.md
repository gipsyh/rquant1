# rquant

Rust 多股票日频回测框架。策略可在运行时决定查询和交易哪些股票，行情按需异步获取。内置买入持有、双均线、地量趋势和自适应轮动策略，输出 JSON 结果与 QuantStats HTML 报告。

## 快速开始

需要 Rust 工具链和 uv。先在项目根目录安装 Python 报告依赖；仓库的 `.cargo/config.toml` 默认使用 `.venv/bin/python`。

```bash
uv sync --locked

# 买入持有；多个股票用逗号分隔，也可重复指定 --symbol
cargo run -- bt buy-and-hold --symbol 000001.SZ,600000.SH --allocation 1.0

# 双均线
cargo run -- bt ma-cross --symbol 000001.SZ,600000.SH --short 5 --long 20 --allocation 0.8

# 地量趋势轮动
cargo run -- bt low-turnover-trend --symbol 399101.XSHE

# 自适应轮动：低成交额池内按低价与反转排名选股，风险关闭时清仓
RUST_LOG=debug cargo run -- bt adaptive-rotation
```

公共参数放在策略名称之前，策略参数放在名称之后。股票代码支持 `000001`、`000001.SZ`、`000001.XSHE`；日期支持 `YYYYMMDD` 和 `YYYY-MM-DD`。

CLI 使用 Tushare 数据源，需要具备所调用接口的访问权限。数据源配置与接口列表见 [TushareProvider](src/data/tushare/provider.rs)。查看行情下载日志可在命令前设置 `RUST_LOG=rquant=debug`。

## 内置策略

| 策略                 | 用途与行为                                                               | 实现                                                   |
| -------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------ |
| `buy-and-hold`       | 初始预算等额分配，首次成交后持有；未成交标的继续尝试。                   | [BuyAndHold](src/strategy/buy_and_hold.rs)             |
| `ma-cross`           | 短均线上穿买入、下穿清仓；从回测开始积累历史，初始多头排列不会直接买入。 | [MaCross](src/strategy/ma_cross.rs)                    |
| `low-turnover-trend` | 从历史指数成分中按低成交额选股，用观察池趋势中位数控制新增买入预算。     | [LowTurnoverTrend](src/strategy/low_turnover_trend.rs) |
| `adaptive-rotation`  | 低成交额池内结合低价与反转排名，保留排名缓冲，观察池趋势恶化时清仓。     | [AdaptiveRotation](src/strategy/adaptive_rotation.rs)  |
| `rebound-rotation`   | 低成交额与 3 日回落综合排名，20 日趋势控制进出；默认集中持有 1 只。      | [ReboundRotation](src/strategy/rebound_rotation.rs)    |
| `volume-breakout`    | 历史创业板成分内按放量突破入场，市场广度及价格趋势控制退出。             | [PatternRotation](src/strategy/pattern_rotation.rs)    |
| `strategy-momentum`  | 根据两个参考策略的历史表现动态配置，读取因果模拟账户报告。               | [StrategyMomentum](src/strategy/strategy_momentum.rs)  |

地量策略只卖出落选股票、买入新增股票，保留交集不再平衡。**进攻／防御比例控制新增买入使用的现金，不是组合总仓位目标**；名单不变时，趋势切换不会触发减仓。合格观察池不足时跳过当天信号，保留持仓。

自适应轮动默认在 20 只低成交额候选中持有 6 只，风险观察池为 100 只；5 日趋势中位数超过 0.5% 时进场、低于 0 时清仓。`--max-trend` 可选启用过热退出，默认关闭。2020-01-01 至 2026-09-28 的默认配置实测夏普为 **2.3442**，未达到 3；策略规则、分期结果和研究限制见 [策略说明](docs/adaptive_rotation.md)。

短线反转策略 `rebound-rotation` 的 2026-01-01 至 2026-10-09 独立回测收益为 **55.86%**、最大回撤 **12.58%**；2020–2026 连续回测累计收益 **446.26%**、最大回撤 **26.77%**。**未达到 2026 年 100% 收益目标**。参数经过历史筛选，默认单股持仓，结果不代表样本外收益；逐年结果、敏感性与复现命令见 [策略说明](docs/rebound_rotation.md)。

```bash
cargo run --release -- bt --start 20260101 --end 20261009 rebound-rotation
```

放量突破研究候选 `volume-breakout` 使用同一参数，在 2025、2026（至 10 月 9 日）独立账户中分别获得 **133.94%、115.85%**；从 2025 年连续运行的年度收益为 **133.94%、151.71%**。但 2020–2026 最大回撤达 **59.22%**，且对参数敏感，**不能称为稳定盈利策略**。规则、完整历史、费用及参数敏感性见 [研究说明](docs/volume_breakout.md)。

```bash
cargo run --release -- bt --start 20260101 --end 20261009 volume-breakout
```

## 报告

每次回测在 `report/` 下创建独立目录，终端输出摘要和保存路径。可通过公共参数 `--report-output reports` 更换报告根目录。

```text
report/
└── low_turnover_trend-1007-163000/
    ├── result.json
    └── report.html
```

- `result.json`：完整回测结果，包括每日资产、持仓、成交和失败订单。
- `report.html`：QuantStats 收益分析；成交与失败订单明细请查看 JSON。

回测先保存 JSON，再生成 HTML；报告生成失败时仍可使用已保存的 JSON。报告按 252 个交易日年化，无风险利率取 0，暂无基准比较。QuantStats 胜率按收益周期统计，不代表平仓交易胜率；单日或全零收益回测仅输出说明页。

结果字段见 [BacktestResult](src/engine/mod.rs)，报告接入见 [RqReporter](src/report/mod.rs)。

可用标准库脚本从每日权益独立核算夏普、年化收益和回撤，并按年份及研究分期汇总：

```bash
python3 examples/verify_backtest.py report/<本次回测目录>/result.json
```

## 数据缓存

CLI 将行情与基础信息缓存在项目运行目录的 `rqdata.bin`，供后续回测复用。缓存不会自动刷新；更换数据源、缓存格式不兼容或需要重新获取上游修订数据时，应在没有回测运行的情况下移走或删除该文件后重跑。

同一个缓存文件不支持多个实例并发写入；并行运行时应使用不同工作目录。缓存包含空行情结果，应避免把尚未发布的数据区间提前缓存。实现见 [内存缓存](src/data/cache/mem.rs)和[磁盘缓存](src/data/cache/disk.rs)。

## 回测口径与限制

使用结果前需留意以下假设，具体撮合和账户规则见[引擎实现](src/engine/rbt.rs)与[订单执行](src/engine/execution.rs)：

- 收盘生成信号、下一交易日开盘成交。首日只产生信号，末日信号不执行，期末不强制清仓。
- 仅按开盘原价撮合，不模拟盘中触价、盘口排队、成交容量或部分成交；滑点参数目前只支持 0。
- 默认使用复权收益近似，未逐笔处理现金分红、红利税、配股和送转股。`--raw` 只切换账户收益口径，不关闭内置策略指标的复权。
- 交易数量规则为统一手数模型，未完整覆盖各板块规则；费率在整个回测区间固定，不计算过户费。
- 无日线时沿用上一估值，暂不能区分停牌与数据缺口。收盘仍有持仓且进入退市整理期，或达到／超过已知退市日时，直接 panic，尚未实现退市清算。
- 股票名称、行业和退市日期来自静态基础信息，自定义策略需避免使用回测当时未知的信息。Tushare 日线查询受历史 ST 数据范围限制，不支持早于 2000 年的数据。

## Rust 接入与源码导航

库调用通过 `BacktestEngine::run` 传入数据源与策略；完整运行流程可参考 [CLI 入口](src/main.rs)。自定义策略实现 `Strategy`，自定义或离线数据源实现 `DataProvider`。接口契约、字段和枚举变体直接查阅源码注释：

| 内容                                 | 源码                                           |
| ------------------------------------ | ---------------------------------------------- |
| 回测配置、策略上下文、订单和结果类型 | [engine/mod.rs](src/engine/mod.rs)             |
| 回测循环与绩效计算                   | [engine/rbt.rs](src/engine/rbt.rs)             |
| 订单撮合与账户结算                   | [engine/execution.rs](src/engine/execution.rs) |
| 策略接口与注册                       | [strategy/mod.rs](src/strategy/mod.rs)         |
| 数据源接口、股票和日线类型           | [data/mod.rs](src/data/mod.rs)                 |
| 历史指数成分                         | [data/index.rs](src/data/index.rs)             |
| 报告接口与文件输出                   | [report/mod.rs](src/report/mod.rs)             |

项目保留 PyO3 扩展入口，但当前尚未导出 Python 回测函数；回测请使用 CLI 或 Rust API。

## 开发检查

```bash
cargo test --lib
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

常规测试使用离线数据和本地 mock HTTP 服务。联网测试默认忽略，需要实际访问 Tushare 并消耗接口额度时再运行：

```bash
cargo test data::tushare -- --ignored
```
