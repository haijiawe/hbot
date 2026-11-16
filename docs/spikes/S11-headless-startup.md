# S11 headless 启动分段（`hbot serve` 到 ready）

- 日期：2026-11-16
- 环境：Intel i5-14600KF（20 逻辑核）/ 31.8 GB RAM / Windows 11 专业版 10.0.26300 /
  rustc 1.96.0 / `--release`（`lto="thin"`、`codegen-units=1`、`strip="symbols"`）/
  `CARGO_TARGET_DIR=E:\hbot-target\hstart`（构建缓存在 E 盘，D 盘曾是事故点）
- 目的：ADR-0002 §4 里最后一条没达标的预算——**headless `hbot serve` 到 ready < 800 ms**。
  「ready」的定义就是 stdout 那一行 `{"addr":...,"pid":...,"type":"ready"}`。
- 方法（可复制）：
  ```powershell
  $env:CARGO_TARGET_DIR = "E:\hbot-target\hstart"
  cargo build --release -p hbot-cli
  # 非首次建库（默认口径）
  pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Trace -Runs 7 -Warmup 1 -Tag trace-warm
  # 首次建库：每次前删库
  pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Trace -Cold -Runs 7 -Warmup 0 -Tag trace-cold
  # 不打点的对照口径（只有父进程观测到的总时长）
  pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Plain -Runs 9 -Warmup 1 -Tag plain
  ```
  原始数据：`_recovery/headless-startup/*-result.json` + 每次运行的 `.stderr.log` / `.stdout.log`
  （`_recovery/` 已 gitignore）。中位与 p95 都是 nearest-rank（与 `scripts/startup-probe.ps1` 同口径）。
- 结论：**通过（已达预算，且余量 ~21×）**。非首次建库 **24–38 ms 中位 / 28–48 ms p95**，
  首次建库 **52 ms 中位 / 70 ms p95**（n=7）。ADR 里那个「878 ms」不是 headless 数字，
  是桌面「进程启动 → 核心可用」被误抄进 headless 行（证据见 §4）。
- 对设计的影响：ADR-0002 §4 的 headless 行实测值改为本次数据；未做任何性能优化（数据不支持），
  只加了启动打点 + 一条结构回归闸门。

---

## 1. 打点（最小侵入，默认关）

沿用 `crates/hbot-store/src/startup_trace.rs` 的既有开关 `HBOT_STARTUP_TRACE=1`：关掉时是
`Option::None`，零成本；打开时每点一行 `hbot-startup ev=<event> unix_ms=<u64> [detail]` 到 **stderr**
（stdout 只留 ready 那一行 JSON，契约不变）。新增的观测点：

| 层 | 点 / 段 | 位置 |
|---|---|---|
| 进程 | `main_enter` | `crates/hbot-cli/src/main.rs` 第一条语句 |
| 参数 | `cli_parsed` | `Cli::parse()` 之后 |
| 日志 | `tracing_ready` | `init_tracing()` 之后 |
| 运行时 | `rt_built` | tokio runtime `build()` 成功后 |
| 命令 | `run_enter` / `serve_enter` / `plan_ready` | `lib.rs::run`、`serve.rs::run` |
| 装配 | `appstate_open_total` / `app_create_data_dir` / `app_store_open` / `app_agent_manager` | `crates/hbot-rpc/src/app/mod.rs`（既有） |
| 库 | `store_open_total` / `store_migrations` / `store_writer_thread` | `crates/hbot-store/src/db.rs`（既有） |
| 连接 | `store_conn_open` / `store_conn_wal` / `store_conn_pragmas`（按 `role=writer/read0/read1` 分行） | `crates/hbot-store/src/db.rs`（既有） |
| 迁移 | `mig_schema_table` / `mig_read_applied` / `mig_user_version` / `mig_apply_one` / `mig_user_version_fixup` | `crates/hbot-store/src/migrations.rs`（既有） |
| 监听 | `serve_dispatcher` / `serve_ws_bind` | `serve.rs` |
| 就绪 | `serve_log_ready` / `serve_ready_write` / `ready_written` | `serve.rs` |

`crates/hbot-cli` 复用了 store 的实现（`hbot_store::startup_trace::point`），没有抄第三份；
`hbot-desktop` 里那份副本保留不动（它在我的改动边界外，且它有自己的 `setup_*` 事件）。

**打点开销的实测上界**：同一会话里，打了点的 release 二进制 `-Trace` 24 ms 中位、
不打点的 `-Plain` 23.6 ms 中位（n=7/9），差 < 1 ms；且 trace 跑里
`ready_observed − ready_written` = 0.5–1.5 ms，即父进程读 stdout 的观测开销也 < 2 ms。
所以下面所有分段数字都不是被观测抬起来的。

## 2. 改前 / 改后

| | 改前（ADR 记的值） | 改后（本次实测，n=7） |
|---|---|---|
| headless 到 ready（非首次建库） | **878 ms 中位 / 1264 ms p95** | **24 ms 中位 / 28 ms p95**（低负载窗口）<br>**38 ms 中位 / 48 ms p95**（同一台机被别的任务压满的窗口）<br>**29 / 35**（最终 release 构建复测，见 §3.1 末） |
| headless 到 ready（首次建库，`-Cold`） | 未单独测过 | **32 ms 中位 / 41 ms p95**（较闲窗口）～ **52 / 70**（受压窗口） |
| 分段 | 无（878 是从桌面表抄来的，从来没有 headless 分段） | 见 §3 两张表 |

**「改前」那 878 ms 的来历**（本项最重要的发现，请连同数据一起看）：

- `docs/spikes/S2-tauri.md:73,86-87` 里 878/1264 明确标的是**桌面**「进程启动 → 核心可用
  （`hbot-store opened`）」，同一张表里 IPC 首次可用是 924/1371；
- ADR-0002 §4.2 的表（`docs/adr/0002-agent-in-rust.md:127-133`）同样是桌端口径：进程启动 →
  `setup_enter` 915 ms、`setup_enter` → 核心可用 51 ms、`AppState::open` 只有 33 ms；
- 而 headless 里根本不存在 Tauri/WebView2 那 900 ms。所以 878 从来不是 headless 的测量值，
  是「桌面到核心可用」被搬进了「headless 到 ready」那一行。本次是第一次真正量 headless。

## 3. 分段表（ms，相对 `Process.Start`，中位 / p95）

### 3.1 非首次建库（常规启动，n=7）

两次独立采样（同一台机、不同负载窗口）。第一次窗口机器较闲，第二次被并行构建压着，
`proc_to_main` 从 14 ms 涨到 25 ms —— 这一段是 OS 装镜像 + CRT + Rust 运行时初始化的固定成本，
随负载漂移，不是代码问题。

| 分段 | 采样 A | 采样 B | 说明 |
|---|---:|---:|---|
| **进程启动 → ready 行写出** | **24 / 28** | **38 / 48** | 预算线 800 ms |
| 进程启动 → `main_enter` | 14 / 18 | 25 / 33 | 镜像加载 + CRT + Rust rt init；**不可由本仓代码优化** |
| `main_enter` → `cli_parsed`（clap） | 0 / 0 | 0 / 1 | 参数解析 < 1 ms |
| `cli_parsed` → `tracing_ready` | 0 / 0 | 0 / 0 | `EnvFilter` 构建 < 1 ms |
| `tracing_ready` → `rt_built`（tokio） | 1 / 1 | 1 / 1 | `new_multi_thread` + `enable_all` |
| `rt_built` → `run_enter` → `serve_enter` → `plan_ready` | 0 / 0 | 0 / 1 | 纯分发 |
| **`AppState::open` 合计** | **7.8 / 9.0** | **8.9 / 11.0** | |
| ├ 建数据目录（`create_dir_all`） | 0.2 / 0.3 | 0.3 / 0.3 | 已存在时几乎零成本 |
| ├ **`Store::open` 合计** | **7.5 / 8.6** | **8.4 / 10.4** | |
| │ ├ 写连接 open | 0.2 / 0.3 | 0.4 / 0.5 | |
| │ ├ **写连接 `PRAGMA journal_mode = WAL`** | **6.4 / 7.6** | **7.2 / 8.7** | 见 §5 归因：首次触碰库文件 ⇒ 读头 + WAL 索引恢复 |
| │ ├ 写连接其余 PRAGMA | 0 / 0 | 0 / 0 | `busy_timeout` / `synchronous` / `foreign_keys` |
| │ ├ **迁移合计（短路）** | **0.1 / 0.1** | **0.1 / 0.1** | `schema_migrations` 已满 ⇒ 只做一次查询 |
| │ ├ 只读连接 read0 / read1 | 0.1 + 0.1 | 0.2 + 0.1 | 每条 open + WAL pragma 各 ~0.1 ms |
| │ └ writer 线程启动 | 0.1 / 0.1 | 0.1 / 0.1 | `std::thread::spawn` + 有界 mpsc |
| └ `AgentManager::new`（纯内存） | 0.1 / 0.2 | 0.2 / 0.2 | 不碰磁盘 |
| `Dispatcher::with_state` | 0 / 0 | 0 / 0 | 只 clone `Arc` |
| **WebSocket bind + accept 任务 spawn** | **1.0 / 1.3** | **1.4 / 1.9** | `TcpListener::bind` + `tokio::spawn` |
| `info!` 已就绪日志 | 0 / 0 | 0 / 0 | stderr，非阻塞 |
| **ready 行写入 + flush** | **0 / 0** | **0 / 0** | stdout 一行 200 字节以内 |

**第三次采样（最终 release 构建，2026-11-16 22:46，n=7）**：`proc_to_ready` **29 / 35**、
`proc_to_main` 18 / 21、`AppState::open` 8.9 / 10.4、`Store::open` 8.4 / 10.1、迁移 0.1、
`conn_wal_writer` 7.3 / 8.9、`ws_bind` 1.1 / 1.5 —— 与 A/B 落在同一区间。之所以特意复测：
这次构建之前，别的 agent 改了 `hbot-code` / `hbot-adapters` / `hbot-rpc`（都在 `hbot-cli`
的依赖图里），镜像从 18,816,512 B 变成 18,926,592 B；复测确认这些外部改动没有移动启动预算。
同一次构建上不打点的 `-Plain` 对照（n=9）是 **25.7 / 29.5**，再次确认打点开销 < 1 ms。

### 3.2 首次建库（`-Cold`，每次前删库，n=7）

下表是**受压窗口**（`proc_to_main` 29 ms）的采样；同一台机较闲窗口（最终构建，`proc_to_main` 17）为
**32 / 41**，其中 `AppState::open` 12.8 / 15.6、`Store::open` 12.4 / 15.2、迁移 2.8 / 3.4、
`conn_wal_writer` 2.4 / 3.1、`conn_open_read0` 0.8 / 1.0、`conn_wal_read0` 6.2 / 7.3。
所以「首次建库」的真实区间是 **32–52 ms 中位 / 41–70 ms p95**，与 §3.1 一样随负载漂移。

| 分段 | 中位 / p95 | 说明 |
|---|---:|---|
| **进程启动 → ready** | **52 / 70** | 受压窗口 |
| 进程启动 → `main_enter` | 29 / 47 | 同 §3.1，负载敏感（本次窗口偏挤） |
| `AppState::open` 合计 | 20.5 / 22.7 | |
| ├ 建数据目录 | 0.3 / 0.4 | 目录真的新建 |
| ├ `Store::open` 合计 | 19.9 / 22.1 | |
| │ ├ 写连接 open / WAL pragma | 0.5 / 0.6，3.2 / 4.7 | 空库，WAL 恢复便宜 |
| │ ├ **迁移合计（真跑 3 条）** | **5.2 / 5.5** | 其中 `CREATE TABLE schema_migrations` 1.7，3 条 DDL 事务合计 1.2 |
| │ ├ **只读连接 read0：open + WAL** | **1.4 / 2.0，8.1 / 9.6** | 迁移刚写了未 checkpoint 的 WAL 帧 ⇒ 第一条后续连接要建 WAL 索引 |
| │ ├ 只读连接 read1：open + WAL | 0.3 / 0.4，0.3 / 0.4 | 索引已在 `-shm` 里，后续连接复用 |
| │ └ writer 线程启动 | 0.1 / 0.1 | |
| └ `AgentManager::new` | 0.2 / 0.2 | |
| WebSocket bind | 1.9 / 2.4 | |

**常规启动的三次采样差 14 ms 全在「进程启动 → `main_enter`」**（14 → 25 → 18），
库那一段（7.5 / 8.4 / 8.4）几乎不动 —— 那才是启动路径自己花掉的时间。
要复现最好挑机器闲的时候，否则请按区间读，别按单次绝对值比。

## 4. 归因：那几十毫秒到底在哪

非首次建库的 24–38 ms 分成四块，没有一块是「浪费」：

1. **14–25 ms：进程启动本身**（`Process.Start` → `main_enter`）。Windows 加载 ~18.8 MB 镜像、
   CRT 初始化、Rust 运行时与静态初始化。这一段在本仓代码之外，唯一能压的是减小镜像/减少依赖
   （把 `hbot-code`（SWC + rquickjs）从 CLI 里 feature-gate 掉），代价与收益见 §6。
2. **6.4–7.3 ms：写连接的 `PRAGMA journal_mode = WAL`**。它是**第一条真正触碰库文件的语句**，
   所以替后面所有语句付了：读库头、打开/扫描 `-wal`、在 `-shm` 里重建 WAL 索引。这是 WAL 的
   崩溃恢复成本，不是可以跳过的检查。
3. **1.0–1.4 ms：`TcpListener::bind` + accept 任务**。绑定必须发生在 ready 之前，否则 ready 撒谎。
4. **其余 ≤ 0.6 ms**：clap、`EnvFilter`、tokio runtime、建目录、`Dispatcher`、`AgentManager`、
   ready 行写入。

**关于「迁移是不是每次都跑 / 有没有 `user_version` 短路」**（本次专门核对了）：

- 有短路，且是数据驱动的：`migrations::apply` 先 `CREATE TABLE IF NOT EXISTS schema_migrations`、
  再 `SELECT version,name FROM schema_migrations`，只对**不在表里**的版本各开一个事务执行；
  最后 `PRAGMA user_version` 只用来做「老库补水位」和 `SchemaTooNew` 校验。
- 常规启动实测 **0.1 ms**（只读一次表）。
- **启动路径里没有 `integrity_check`、没有 `quick_check`、没有 `VACUUM`**（`crates/hbot-store/src/db.rs`
  与 `migrations.rs` 全文检索为空）。所以「全库扫描拖慢启动」这个假设在本仓不成立——
  现在没有这条债，本次也没有引入。

## 5. 做了什么（按任务要求逐条量）

1. **加分段打点**（见 §1）：只加 `HBOT_STARTUP_TRACE=1` 下的 stderr 点，默认路径零成本、
   行为与顺序完全不变（bind → `info!` → 写 ready → `wait_for_shutdown`）。省了 0 ms，
   代价是 4 个文件的十几行代码；**产出是后面所有判断的依据**。
2. **性能优化：一条都没做。** 依据是 §4：剩下的可动项全部 ≤ 0.6 ms，而预算余量 21×。
   任务要求「只做有证据支撑的改动」，这里证据的方向是「不需要改」。
3. **回归闸门**（`crates/hbot-store/src/tests.rs`，3 条确定性断言，不测时长）：
   - `startup_opens_only_the_writer_and_read_connections`：`Store::open` 只允许开
     `1 + READ_CONNECTIONS` 条连接。计数记在**测试线程本地**（`crate::db` 的
     `connection_opens_on_this_thread`），否则并行测试会互相污染成 flaky。
   - `reopening_does_not_rerun_migrations`：第二次打开后 `schema_migrations` 的
     `(version, name, applied_at)` 必须逐行不变。比既有的 `migrations_are_idempotent_across_restarts`
     更强：那条只看行数，而「每次启动重放 + `INSERT OR REPLACE`」同样能保持行数。这里先把
     `applied_at` 改成哨兵值 42 再重开，重放一次就会变。
   - `startup_path_has_no_full_database_scan`：`include_str!` 读 `db.rs` / `migrations.rs`，
     断言启动路径里不出现 `integrity_check` / `quick_check` / `vacuum`（这三样都是 O(库大小)，
     会随用户数据增长拖慢 ready）。
   **闸门验证过会红**：临时在 `Store::open` 里多开一条连接，测试立刻 `left: 4 right: 3` 失败；
   撤回后恢复绿。三条都跑在 `cargo test -p hbot-store` 里，耗时 < 0.1 s，不进 CI 时序、
   不依赖机器快慢 ⇒ 不会 flaky。

## 6. 考虑过但没做的优化（下一个人别重复试）

| 候选 | 实测可省 | 为什么不做 |
|---|---:|---|
| 只读连接改成 lazy（第一次查询才开） | 0.2（常规）/ 1.7（首次） | 省不到 1%。代价是**丢掉 fail-fast**：库坏了要等第一条命令才报错，而且 ready 会变成「假可用」 |
| 只读连接不再执行 `journal_mode = WAL` | 0.2（常规）/ 8.1（首次，read0） | 常规省不到 1%。首次那 8.1 ms 不是 pragma 本身，是**第一条后续连接建 WAL 索引**——跳过 pragma 只是把成本挪到第一次查询 |
| 迁移后加 `PRAGMA wal_checkpoint(TRUNCATE)`，让首次建库的 read0 变便宜 | 潜在 ~8（仅首次建库） | 收益只在「人生第一次启动」，且代价未测（一次全库 fsync，Defender 在场时可能反而更慢）；本项目 52 ms 已余量 15× |
| ready 之后再做 `AppState::open`（先 bind 再开库） | ~8（`AppState::open` 全部） | **这就是把指标做假**：ready 的含义是「命令可用」，先 bind 后开库会让第一条命令撞上未就绪状态。任务明确不许为了达标删功能 |
| CLI 不链接 `hbot-code`（SWC + rquickjs），把 `check`/`replay` 拆成子命令动态加载 | 未知，压的是 14–25 ms 的镜像段 | 越界（要动 `crates/hbot-cli/Cargo.toml` 的依赖面与 `hbot-code` 的 feature），且镜像段本来就不是瓶颈；先做需要重新量 |
| `spawn_blocking` 包住 `AppState::open` | **负收益**（+0.1 ms 起） | 现在确实是在 async 上下文里同步开库，但启动期没有任何其他任务在跑，阻塞无害；包一层只多一次线程唤醒 |
| tokio 换 `current_thread` runtime | ~1 ms | 只省 runtime 构建的 1 ms，却拿掉并发（accept loop 与 handler 会互相饿） |

另外核对过任务里担心的两条「同步 IO 串行等待」：**ready 路径上没有读 `.env` / `bots.json` /
`agent.json`**（`AppState::open` 只建目录 + 开库 + 构造纯内存的 `AgentManager`；配置在命令里按需读），
也没有 `block_on` 嵌套。

## 7. 风险与「什么情况下会漏检」

- 本次**没有改动任何迁移或校验行为**（一个字节都没动 `migrations.rs` 的逻辑），所以不存在
  「迁移被跳过 / 错误被吞」的风险面。上述「不改完整性检查」的结论来自检索：现在没有，
  于是也不存在漏检。
- 新闸门的边界：`startup_path_has_no_full_database_scan` 只扫 `db.rs` 与 `migrations.rs` 两个
  文件的源码文本。若有人把全库扫描挪进**新的**启动模块（例如在 `AppState::open` 里另开一个
  `verify.rs`），这条会漏检——那类改动必须靠 review。它的价值在于把「在现有启动文件里顺手加一句
  `PRAGMA integrity_check`」这条路堵死。
- `startup_opens_only_the_writer_and_read_connections` 断言的是 `1 + READ_CONNECTIONS`，
  所以**有意**允许调 `READ_CONNECTIONS` 常量（那是显式决策），只拦「记账之外的连接」。
- 打点的开销上界是实测的 < 1 ms；但**它只应在排障时开**，别把 `HBOT_STARTUP_TRACE=1`
  写进任何生产配置（stderr 会多出十几行）。
- 本机负载会显著影响 `proc_to_main`（14–29 ms）。做前后对比时要么交错采样、要么同一窗口内
  各测 7 次以上，否则会把自己的优化误算成 ±10 ms。

## 8. 未验证

- 首次**安装**后第一次启动的真实磁盘状态（新装用户目录、Defender 首次扫描、SSD 冷缓存）：
  `-Cold` 只删了库文件，没删系统级缓存，所以 32–52 ms 是首次建库的**下界**。
- 慢盘/网络盘上的 WAL 恢复成本（`journal_mode=wal` 在部分网络盘上会直接失败并返回
  `StoreError::JournalMode`，这是既有设计，本次未触碰）。
- 桌面形态（`crates/hbot-desktop`）不属本次范围，878/966 ms 那两行本次没有复测。