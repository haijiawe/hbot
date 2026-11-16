# S2 Tauri 同进程 + Monaco

- 状态：**已实测（内存与启动）**；按键延迟与文件打开**未测**（原因见 §3）
- 日期：2026-11-16
- 环境：Windows 11 专业版 10.0.26300 / WebView2 运行时 157.0.4341.46 / rustc+cargo 1.96.0 / release（`lto="thin"`、`codegen-units=1`、`strip="symbols"`）/ 窗口 1280×820 / Defender 实时保护开启
- 机器：Intel Core i5-14600KF（14 核 20 线程）、31.8 GB RAM、WD Blue SN580 1TB NVMe
- 关联：[ADR-0002](../adr/0002-agent-in-rust.md) S2、[02 架构](../02-architecture.md) §1、[06 IDE](../06-ide.md) 性能预算、[S0 内存基线](S0-memory.md)

## 1. 目的

验证“Tauri 2 直接链接 Rust 核心（无 sidecar）”这条形态在桌面端成立，并量出 WebView 的真实成本：

1. 单一进程能否跑起窗口 + 核心，命令/事件走 Tauri IPC；
2. Monaco 多文件（3 个 model）打开、编辑、保存是否满足 [06](../06-ide.md) 的预算（按键 p95 < 16ms、1MB 文件 < 150ms）；
3. 空窗口与“IDE 打开一个项目”的工作集，是否落在 S0 判定区间。

## 2. 方法（实际跑的命令）

```powershell
# 0) 只在副本里构建/运行（主仓库有其他 agent 在改）
robocopy E:\HBot-Next D:\hbot-measure /E /XD target node_modules .git dist .hbot /XF *.pdb

# 1) 前端产物（没有 apps/web/dist 窗口会白屏）
cd D:\hbot-measure\apps\web; pnpm install; pnpm run build

# 2) release 构建（长命令一律重定向到日志，别让管道写满）
$env:CARGO_TARGET_DIR='D:\hbot-target\measure'; cd D:\hbot-measure
cargo build --release -p hbot-cli -p hbot-desktop *> D:\build-release.log

# 3) 起桌面壳（直接跑产物；stdout/stderr 落文件）
Start-Process -FilePath D:\hbot-target\measure\release\hbot-desktop.exe -PassThru `
  -RedirectStandardOutput D:\hbot-measure\_measure\logs\desk-out.log `
  -RedirectStandardError  D:\hbot-measure\_measure\logs\desk-err.log

# 4) 采样（另开终端）——主进程 / 子进程分开报
pwsh -File D:\hbot-measure\_measure\scripts\mem-probe.ps1 -Id <pid> -Seconds 30 `
  -IntervalMs 500 -IncludeChildren -SplitChildren -Json
```

封装好的两个脚本（都在测量副本 `D:\hbot-measure\_measure\scripts\`，不进仓库）：

```powershell
# 桌面内存：series（每 1 s 一条曲线）/ window10（启动 10 s 后采 30 s）/ steady40（40 s 后采 30 s）
pwsh -NoProfile -File .\scripts\m3-desktop.ps1 -Mode window10
pwsh -NoProfile -File .\scripts\m3-desktop.ps1 -Mode steady40

# 冷启动：7 次，25 ms 轮询窗口/IPC 就绪时刻
pwsh -NoProfile -File .\scripts\m5-coldstart.ps1 -Runs 7
```

为什么必须按进程树采：本机同时有别的 `msedgewebview2.exe`（其他应用），`-Name msedgewebview2` 会把不相干的进程算进来；脚本按 `ParentProcessId` 从 `hbot-desktop` 递归取后代。

**“可用”的标志**：项目里没有现成的“IPC 就绪”日志（`hbot-rpc` 的 dispatch 路径无 info/debug 埋点，前端也没有可外部观测的信号）。为了让“可用”有客观时刻，在**测量副本**的 `crates/hbot-desktop/src/lib.rs` 的 `#[tauri::command] dispatch` 开头临时插了一行：

```rust
static FIRST_DISPATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
if !FIRST_DISPATCH.swap(true, std::sync::atomic::Ordering::Relaxed) {
    tracing::info!("IPC_READY 首次收到 web 侧 dispatch");
}
```

即“web 侧第一次成功把命令送进核心”= IPC 可用。**测完已撤回该插桩并重建**（`lib.rs` 与主仓库逐字节一致；重建产物 34,644,480 B）。另两个时刻直接从现有日志取：窗口出现 = `MainWindowHandle != 0`；核心可用 = stderr 出现 `hbot-store opened`。

## 3. 结果

| 项 | 目标 | 实测 | 备注 |
|---|---|---|---|
| 空窗口工作集（含 WebView2 子进程） | — | **结构性不可测**；启动后 10 s = 整机 WS **382.5** / Private **205.5** MB | 该应用**没有空窗形态**：窗口初现（t=1 s）就是完整 IDE 外壳、标题已是 `hbot`，整机 WS 已 364 MB。主进程 42.3 / 13.9，7 个子进程 340.3 / 191.6（3 次中位） |
| IDE 首屏（外壳稳态，40 s） | — | 整机 WS **392.3** / Private **190–223**（四次）MB；主进程 42.6 / 14.0；子进程 349.9 / 193.5 | 四次 WS：392.2 / 417.3 / 389.2 / 392.4（中位 392.3）；Private：207.6 / 222.7 / 207.2 / 190.1。**未打开项目、未打开文件**，Monaco 未加载 |
| IDE 打开 3 标签 | < 220 MB 整机 | **未测** | 需要 UI 交互打开文件；见下方“未测原因”。上界参考：外壳稳态已 392.2 MB（WS）/ 207.6 MB（Private），打开文件只会更多（Monaco 懒加载 chunk：`editor-surface` 1.31 MB + `editor.api` 2.74 MB + `ts.worker` 6.93 MB 都要装进渲染进程） |
| 按键 p95 | < 16 ms | **未测** | 见下方“未测原因” |
| 1 MB 文件打开 | < 150 ms | **未测** | 见下方“未测原因” |
| 冷启动到可编辑 | < 1.5 s | **到“可编辑”未测**；进程启动 → **IPC 首次可用 924 ms（中位）/ 1371 ms（p95）**，n=7 | 窗口出现 503 / 926 ms；核心可用（`hbot-store opened`）878 / 1264 ms。25 ms 轮询 → 每数 ±25 ms 量级 |

### 3.1 冷启动原始数据（7 次，单位 ms，25 ms 轮询）

| 轮次 | 窗口出现 | 核心可用（store opened） | IPC 首次可用 |
|---|---|---|---|
| 1 | 926 | 1264 | 1371 |
| 2 | 373 | 683 | 761 |
| 3 | 385 | 676 | 757 |
| 4 | 522 | 961 | 1060 |
| 5 | 364 | 717 | 795 |
| 6 | 503 | 878 | 924 |
| 7 | 666 | 1081 | 1202 |
| **中位** | **503** | **878** | **924** |
| **p95（nearest-rank）** | **926** | **1264** | **1371** |
| min / max | 364 / 926 | 676 / 1264 | 757 / 1371 |

口径说明：

- 每轮前先杀残留进程、等 2 s；`Stopwatch` 从 `Start-Process` 起算。
- 文件缓存是**热的**（同一份产物连跑 7 次）；第一天冷缓存会更慢，可参考 `hbot serve` 首次 798 ms vs 之后 278 ms 的差。
- 数据目录已迁移过（不是“首次建库”），首次运行会多一次 schema 迁移。
- **“IPC 可用”不是“首屏画完”**：它只证明 web 侧第一次把命令送进核心。首屏像素/可编辑时刻仍晚于它，所以 924 ms 是“到可编辑”的**下界**。

### 3.2 2026-11-16 分段复测（`scripts/startup-probe.ps1`）

上一节是「窗口/核心/IPC 三个时间点」；这一节把进程启动到核心可用**拆成分段**，回答「878 ms 里到底谁慢」。构建：桌面 release（`E:\hbot-target\coldstart\release\hbot-desktop.exe`，28,634,624 B），`HBOT_STARTUP_TRACE=1`，7 次，25 ms 轮询窗口 + 首帧标记。

| 分段 | 中位（ms） | p95（ms） | min / max |
|---|---:|---:|---|
| 进程启动 → 窗口出现 | 525.2 | 774.3 | 341.4 / 774.3 |
| 进程启动 → `setup_enter` | 915.3 | 1264.2 | 648.8 / 1264.2 |
| 进程启动 → `open_begin`（核心初始化开始） | 928.5 | 1283.0 | 657.5 / 1283.0 |
| `open_begin` → 核心可用 | **~37** | ~50 | — |
| 进程启动 → 核心可用 | **965.9** | 1329.6 | 686.2 / 1329.6 |
| 进程启动 → IPC 首次可用（`first_rpc_out`） | 1048.8 | 1424.6 | 735.9 / 1424.6 |
| 其中 `appstate_open_total` | **33.0** | 56.6 | 26.0 / 56.6 |
| 其中 `store_open_total` | 28.2 | 51.4 | 22.1 / 51.4 |
| 其中 `store_migrations` | 5.6 | 24.0 | 3.1 / 24.0 |
| 其中 `store_writer_thread` | 0.1 | 0.2 | 0.1 / 0.2 |

**归因结论**：核心侧 `AppState::open` 只占 33 ms，store 迁移 5.6 ms；**915 ms 花在进入 `setup` 之前**（Tauri runtime + WebView2 初始化 + 窗口创建）。所以「冷启动到核心可用」这个指标在桌面形态下主要量的是 WebView2，而不是 Rust 核心。继续优化 SQLite/迁移不会改善它。

绝对数比 §3.1 的 878 ms 高约 88 ms，且离散度更大（run-04/06 到 1.29–1.33 s）。两次构建、插桩与后台负载都不同，**不把这 88 ms 归因到某一行代码**；可比较的是分段比例（核心 <4%）。

**处置边界（不许静默放宽）**：桌面目标应拆成「headless core ready < 800 ms」与「桌面到 IPC 首次可用 < 1500 ms」两个口径；若要把桌面「核心可用」压到 800 ms，必须先把核心初始化挪到 Tauri `setup` 之前并行启动并实测，只改 store 无效。详见 [ADR-0002 §4.2](../adr/0002-agent-in-rust.md)。

#### 3.2.1 预取插件实测（同一条「挪到 setup 之前」的尝试，2026-11-16 复测）

上一条「必须先把核心初始化挪到 Tauri `setup` 之前」已经有人试过了：`crates/hbot-desktop/src/lib.rs:172-205` 的 `CorePrefetchPlugin`（插件名 `hbot-core-prefetch`）注册在单实例插件之后、窗口创建之前，在 `initialize` 里 `tauri::async_runtime::spawn` 调 `host.core()`，把 `AppState::open` 丢到后台线程，与 WebView2 初始化重叠。

同一份产物（`E:\hbot-target\coldstart\release\hbot-desktop.exe`，28,634,624 B）、`HBOT_STARTUP_TRACE=1`、3 次、25 ms 轮询，实测：

| 分段 | 中位（ms） | p95（ms） | min / max |
|---|---:|---:|---|
| 进程启动 → `setup_enter` | 761.9 | 851.2 | 728.5 / 851.2 |
| 进程启动 → `core_ready`（预取任务完成） | 803.8 | 899.6 | 757.3 / 899.6 |
| `appstate_open_total` | 28.0 | 29.7 | 17.8 / 29.7 |

**结论：预取没能把核心打开藏到 `setup` 之前。** `core_ready` 比 `setup_enter` 还晚约 **42 ms**，也就是说预取线程真正跑起来时窗口创建阶段（约 760 ms）已经过完了；核心本身只值 28 ms，即使完美重叠也最多省下这 28 ms。

**保留它的理由**：预取确实把核心就绪与 `setup` 之后的装配重叠了，属净收益（省掉 `setup` 里的串行 28 ms），因此**保留**，不要当成无效代码删掉。但**不要指望它救 800 ms**：桌面侧的 760 ms 是 Tauri runtime + WebView2 初始化 + 窗口创建，属于宿主成本，Rust 侧无从优化。要真压到 800 ms 以内只有两条路——换原生 UI，或让 WebView2 预热常驻——都不在本轮范围。

复测命令与原始日志：`E:\hbot-target\prefetch-check.log`（3 次）；探针脚本 `scripts/startup-probe.ps1`。

> 待确认：`CorePrefetchPlugin` 目前没有断言它的测试（`startup_trace` 只在 `HBOT_STARTUP_TRACE=1` 时产生可见证据）。若将来重构 `run()` 的插件注册顺序，需要重新跑一次上面的探针确认 `core_ready` 仍早于 `setup_exit`。

### 3.3 未测原因（不编数字）

| 未测项 | 原因 |
|---|---|
| 按键 p95 | 原文要求的 Chrome DevTools 录制不适用于 WebView2（需要开远程调试端口，release 构建没开 devtools 特性）；也没有能向窗口注入按键的自动化手段（未接 tauri-driver/WebDriver，且它是 release 产物）。要测必须在 `editor` feature 里埋 `performance.mark` 再人工敲 200 次 —— 属人工操作，本次无法自动化，**不填数** |
| 1 MB 文件打开 | 同上：需要 UI 打开文件才能加载 Monaco 并触发 `file_read`，没有输入注入手段；也没有围绕这条链路加计时埋点 |
| 打开 3 个标签后的内存 | 同上（Monaco 懒加载 chunk 只有真打开文件才下载/解析） |
| “空窗口”基线 | 该应用没有空窗形态（窗口一出现就挂完整 IDE 外壳并立刻 `loadTree()`），无法构造“只有 WebView 和空壳”的对照 |

## 4. 结论

- **“Tauri 2 同进程 + Rust 核心（无 sidecar）”这条形态成立**：窗口、核心、SQLite 在**同一个进程**里跑，命令与事件走 Tauri command/event，无需 sidecar；`hbot-desktop.exe` 首次成功收到 web 侧命令的中位时间是 **924 ms**（p95 1371 ms），窗口出现在 503 ms。
- **内存：主进程很小，整机很大。** Rust/Tauri 主进程 **42 MB WS / 14 MB Private**；含 7 个子进程的整机是 **382–392 MB WS / 190–223 MB Private**。也就是整机内存的 **88%–89%** 来自 WebView2 子进程，与“IDE 打开多少内容”关系不大（空窗 vs 首屏只差约 10 MB）。
- 对 S0 的 300 MB 整机线：**Private 求和达标（中位 ≈207 MB，区间 190–223），WorkingSet 求和超线（382–392 MB，超 27%–31%）**。结论依赖口径，见 [S0 §4.1](S0-memory.md)；ADR 应显式选定口径，不要写含糊的“总 RSS”。
- 对 [06 IDE](../06-ide.md) 的目标：**内存目标按 WS 口径不达标**；**按键 p95 < 16 ms 与 1 MB 文件 < 150 ms 未测，不能判定**；冷启动“到可编辑 < 1.5 s”未测，但“进程启动 → IPC 可用”为 924 ms（下界），仍在预算内。
- **一句话**：形态验证通过、核心侧成本极低；风险集中在 WebView2 的绝对内存占用，而这一点不是本项目实现能优化的。

## 5. 对设计的影响

- 若整机 > 300 MB：按 ADR-0002 §11 评估原生 UI（这是最大的重估信号）。
- 若按键 p95 > 50 ms：先查是否每次按键都触发全量诊断；仍超则记录，M3 再优化。
- 若 Tauri 同进程做不到（例如 IPC 吞吐不足）：退路是核心跑在独立进程 + 本地 socket（`hbot serve` 形态），代价是多一次序列化与生命周期管理。

### 5.1 这次实测对上面三条的补充

- 第三条退路的实测参照：`hbot serve`（独立进程形态）空闲只有 **9.0 MB WS / 3.4 MB Private**。也就是说“核心独立进程 + Tauri 窗口”的整机成本 ≈ 桌面整机 + 9 MB，**几乎不增加内存**；如果将来为了崩溃隔离/热重载要拆进程，内存不是阻力。
- 真正需要决策的是“要不要 Chromium 渲染侧（WebView2）”这笔 340 MB 的固定开销，而不是“核心跑在哪”。
- 按键延迟与文件打开是 [06](../06-ide.md) 的硬预算，本次**没有证据**；M1/M3 之前必须补测（需要 WebView2 远程调试端口或 tauri-driver 自动化），否则 IDE 性能预算等于未验证。