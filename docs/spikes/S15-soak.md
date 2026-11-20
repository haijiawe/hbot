# S15 —— 长时稳定性压测（2026-11-21）

> 目的：回答「这台东西能不能一直开着」。桌面验收（`S14-desktop-acceptance.md`）证明的是「装得上、点得动」，
> 这里证明的是「连续跑两小时、一千多次消息之后，内存不涨、库不膨胀、延迟不劣化」。
> 口径与 `docs/plans/2026-11-21-desktop-acceptance.md` §3.4 的九条判据一致，脚本就是那份计划里的 `B`。

## 1. 被测对象与命令

- 二进制：`E:/hbot-target/desk/release/hbot.exe`（`cargo build --release -p hbot-cli`，`rustc 1.100.0`，Windows x64）。
- 命令：`pwsh -File scripts/soak-serve.ps1 -Minutes 120`（默认 `-IntervalSec 5 -SampleSec 30`）。
- 脚本自建临时数据目录 `_recovery/soak-long/`：一个 `local` 平台的示例项目（`hbot.json` + `bots.json`），
  一份独立的 `--data-dir`，一个随机 token 的 `hbot serve --bind 127.0.0.1:0`，端口从 ready 行里读回来。
- 客户端：`scripts/soak-client.mjs` 经 WS 每 5 秒 `bot_inject` 一条 `soak#<seq>`，等 `run.log` 事件里出现回显，
  记录「注入到事件到达」的毫秒数作为延迟样本。
- 采样：每 30 秒读一次进程的 `PrivateMemorySize64`（不用 `WorkingSet64`，见 `docs/pitfalls.md` 坑 34）、
  `hbot.db` 与 `hbot.db-wal` 的文件大小。两小时共 240 个采样点。

## 2. 判据与实测

| 判据 | 阈值 | 实测 | 结果 |
|---|---|---|---|
| 内存回归 | 每 10 分钟线性斜率 < 10 MB | **0.17 MB / 10 min** | PASS |
| 内存峰值 | < 首采 + 250 MB | 首采 4.4 MB，峰值 **7.2 MB** | PASS |
| 延迟 p95 | < 2 s | **41 ms** | PASS |
| 延迟最大 | < 10 s | **136 ms** | PASS |
| 回复率 | 全部注入都有对应回显 | **1438 / 1438** | PASS |
| 错误行 | 服务端 `error` 级 run.log 为 0，客户端协议错误为 0 | **0 / 0** | PASS |
| 库体积 | `hbot.db` 增长 < 200 MB | 增长 **1.6 MB** | PASS |
| WAL 峰值 | `hbot.db-wal` < 100 MB | 峰值 **4.0 MB** | PASS |
| 收尾 | 强杀后 5 秒内进程消失 | **10 ms** | PASS |

九个判据全部 PASS，脚本退出码 0（`SOAK_LONG_EXIT=0`）。

### 时间线（采样自 `_recovery/soak-long.log`）

| 时刻 | RSS | db | wal |
|---|---|---|---|
| 0 min | 4.4 MB | 0.0 MB | 0.3 MB |
| 30 min | 5.7 MB | 0.5 MB | 4.0 MB |
| 60 min | 6.1 MB | 0.9 MB | 4.0 MB |
| 90 min | 6.7 MB | 1.3 MB | 4.0 MB |
| 119.6 min | 7.2 MB | 1.6 MB | 4.0 MB |

读法：内存从 4.4 涨到 7.2 MB、曲线是平的缓慢爬升（0.17 MB/10 min，两小时合计不到 3 MB），
不是每轮泄漏一块；库随消息数线性增长（1438 条消息 → 1.6 MB，约 1.1 KB/条，含入站、出站、日志三张表）；
**WAL 在 4.0 MB 就停住了**（checkpoint 生效），没有出现「WAL 只涨不落」。

### 5 分钟预热轮（同一脚本，先跑了一轮）

`pwsh -File scripts/soak-serve.ps1 -Minutes 5`：p95 24 ms、max 58 ms、回复 60/60、内存 4.4 → 5.2 MB、
db 0.2 MB、wal 峰值 3.9 MB、强杀后 4 ms 消失，九条同样全 PASS。短轮的价值是「在一分钟内证伪」，
长轮的价值是「够久才会暴露缓慢增长」—— 两轮都留着，前者快、后者真。

## 3. 压测抓到的真缺陷（已修）

**首轮 5 分钟压测就红了三条判据，根因是同一个**：`hbot serve` 少一条事件桥。

业务代码把事件 emit 到 `AppState` 的 `EventBus`，而 WS 服务只认 `WsServer::event_sink()`，
两者之间必须有人转一趟。桌面壳每个窗口都有一条 `spawn_event_pump`，所以桌面端一直是对的；
无头宿主（`hbot serve`）从来没接过这条线。

症状不是报错：客户端能连上、能发命令、能拿到命令的 `result`，但**一个事件帧都收不到**；
数据与日志都正常落库，于是表现为「面板永远不刷新」。压测里的原始数字是：注入 30 条消息、
库里 31 行回复、客户端 `run.log` 事件 **0** 条 —— 回复率 0/30、延迟样本为空，p95 判据因此直接失败。

修法（commit `5082afc`）：`crates/hbot-cli/src/serve.rs` 加 `forward_events(bus, sink)`，
语义与桌面壳的事件泵一致 —— 广播不补发（订阅之前的事件拿不到，客户端连上后先用快照命令补齐状态）、
`Lagged` 只记一条警告并继续、总线关闭就退出；收尾时 `pump.abort()`。另加一条回归测试钉住它。

这条缺陷值得单列的原因：**它是「没有自动化测试的集成缺口」的典型样本** —— 每个 crate 的单测都绿，
`cargo test --workspace` 全绿，桌面端手测也绿，只有「换个宿主跑同一条链路」才会暴露。

## 4. 边界（这份报告不证明什么）

- **用的是 `local` 平台**，不是真 OneBot。压的是「WS → 命令 → supervisor → 队列 → isolate → 回复 → 事件」
  这一整条核心链路，但它不覆盖平台适配器的网络重连、限流与媒体上传。
- **单 bot、单会话**。多 bot 并发、同 bot 多会话并行的资源曲线没测。
- **强杀收尾**，不是优雅退出。Windows 上独立进程收不到 Ctrl-C，`wait_for_shutdown()` 的 SIGTERM 分支是 `cfg(unix)`；
  优雅路径（`onStop(shutdown)` 跑完再退）由 `crates/hbot-rpc/tests/lifecycle_capability.rs` 覆盖，本脚本不重复。
- **两小时不等于七天**。0.17 MB/10 min 的外推是 24 小时约 24 MB，但不能据此宣称「跑一周没问题」——
  真要这种结论得跑一周，而这份报告只覆盖一次两小时的实测。
- 机器是开发机（Windows 11 x64），不是 CI runner；`PrivateMemorySize64` 的绝对值依赖机器，
  有意义的是**同一台机器上的斜率**。

## 5. 怎么复跑

```powershell
cargo build --release -p hbot-cli
pwsh -File scripts/soak-serve.ps1 -Minutes 5            # 先短跑，一分钟内看结论
pwsh -File scripts/soak-serve.ps1 -Minutes 120          # 正式长跑
```

日志与采样：`_recovery/soak-<tag>/logs/serve.out.log`、`client.out.log`、`samples.csv`、`soak-report.json`；
脚本每 30 秒往控制台打一行 `[soak] <min> min  rss … db … wal …`，结束时打九条判据表并以退出码表示结论。
要指定别的二进制用 `-Bin`，要指定数据目录用 `-DataDir`（默认 `_recovery/soak-<时间戳>`）。
