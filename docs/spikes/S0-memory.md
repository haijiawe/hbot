# S0 内存基线

- 状态：**已实测**（2026-11-16；六项数字全部为真机 release 实测，未测项在 §6 逐条写明）
- 日期：2026-11-16
- 关联：[ADR-0002](../adr/0002-agent-in-rust.md) §4 内存目标、[02 架构](../02-architecture.md) §1.1、[04 计划](../04-implementation-plan.md) S0

## 1. 要回答的问题

ADR-0002 把“桌面空闲总 RSS < 220 MB、核心 < 60 MB”写成目标，但这些数字**还没有实测**。S0 只回答三件事：

1. 空核心（无 bot、无 isolate）常驻多少？
2. 每多一个 bot / 每多一个 QuickJS isolate，增加多少？
3. Tauri 窗口的 WebView 进程占多少（桌面整机的大头）？

**不回答**：旧 Electron 项目的完整对比（需要启动旧应用、写用户数据；当前按只读处理，留到 M6 影子运行再量）。

## 2. 口径

- 工作集（`WorkingSet64`）：任务管理器“内存”列口径，**含共享页**——多进程求和会把同一份 WebView2 DLL 在 6 个进程里各算一次，所以“求和”是本口径的上界。
- 私有内存（`PrivateMemorySize64`）：提交的独占内存，更适合判断“这个进程自身增长”。
- 报告必须标明用哪个口径；比较不同进程时只用同一种。
- 采样窗口：`hbot serve` 空闲用**启动后等 5 秒 → 采 30 秒（200 ms 一次）**；桌面用**等 10 秒（M3）/ 40 秒（M4）→ 采 30 秒（500 ms 一次）**。原文写的“采样 3 秒、200ms 一次”是脚本默认值，本次按任务要求加长窗口（这台机器上 30 秒与 3 秒读数相同，窗口加长只为覆盖重连/GC 周期）。
- 数字保留一位小数（MB）。
- **含不含子进程逐条写在“备注”列**：核心行**不含** `conhost.exe`（stdout/stderr 被重定向时由 Windows 附带创建，约 10.1 MB，与产品内存无关）；桌面行**含**全部子进程（1 个 conhost + 6 个 `msedgewebview2.exe`）。

工具：`scripts/mem-probe.ps1`（只用于测量，不参与产品运行时）。本次给它加了 `-SplitChildren`：把根进程与全部后代**分开**报，`WorkingSetMB`/`PrivateMB` 仍是总数（兼容原口径）。

```powershell
# 单进程
pwsh -File scripts/mem-probe.ps1 -Name hbot -IncludeChildren

# 已知 PID，输出 JSON 便于贴进本文
pwsh -File scripts/mem-probe.ps1 -Id 1234 -Seconds 5 -IncludeChildren -Json

# 主进程 / 子进程分开报（本次新增）
pwsh -File scripts/mem-probe.ps1 -Name hbot-desktop -Seconds 30 -IncludeChildren -SplitChildren -Json
```

## 2.1 测量条件（记录纪律要求，逐条可核对）

| 项 | 值 |
|---|---|
| commit | `ba49346`（`ba4934658f9b6e781c8768b2c5c5c2ad6a10b14b`，master）。测量时工作区另有 123 个未提交改动，都来自并行编辑的其他 agent；与本文数字相关的源码见下一行 |
| 源码一致性 | 全部测量在 `D:\hbot-measure` 副本做；`crates/hbot-desktop/src/lib.rs` 与主仓库**逐字节一致**（SHA256 `B8157714…C0E022`） |
| 构建 | `cargo build --release -p hbot-cli -p hbot-desktop`；`[profile.release]` = `lto="thin"` / `codegen-units=1` / `strip="symbols"`；`CARGO_TARGET_DIR=D:\hbot-target\measure`；全量首建 7m46s |
| 机器 | Intel Core i5-14600KF（14 核 / 20 线程，基频 3.5 GHz）、31.8 GB RAM、WD Blue SN580 1TB NVMe SSD |
| 操作系统 | Windows 11 专业版 10.0.26300（build 26300） |
| Defender | **实时保护开启**（`RealTimeProtectionEnabled=True`）——读数含扫描影响，属真实用户环境，不是干净实验室数字 |
| WebView2 | 运行时 157.0.4341.46 |
| 工具链 | rustc/cargo 1.96.0（`ac68faa20` 2026-05-25）、Node v24.21.0、pnpm 11.7.0、PowerShell 7.6.10 |
| 前端产物 | `apps/web` 的 `pnpm run build`：`index.html` 445 B、主 chunk 266 KB；Monaco 编辑面与 TS worker 是**懒加载** chunk（未打开文件不加载） |

## 2.2 测量副本与可复现性

1. 复制源码（不在主仓库构建）：

   ```powershell
   robocopy E:\HBot-Next D:\hbot-measure /E /XD target node_modules .git dist .hbot /XF *.pdb
   ```

2. 前端产物（桌面窗口需要 `apps/web/dist`，否则白屏）：

   ```powershell
   cd D:\hbot-measure\apps\web; pnpm install; pnpm run build
   ```

3. release 构建（长命令一律重定向到日志，避免管道写满假死）：

   ```powershell
   $env:CARGO_TARGET_DIR='D:\hbot-target\measure'; cd D:\hbot-measure
   cargo build --release -p hbot-cli -p hbot-desktop *> D:\build-release.log
   ```

4. 为量“启动 → IPC 可用”，曾在副本的 `dispatch` 里临时插一行 `IPC_READY` 日志；**测完已撤回**并重建。撤回后 `hbot-desktop.exe` = 34,644,480 B，与插桩版（34,649,088 B）差 4,608 B；本文桌面内存数字取自干净构建的时间序列（M3/M4），M5 的启动时间取插桩版（只多一行 `if` 判断，不影响进程启动路径）。
5. 副本里另有一个**测量专用** example `crates/hbot-code/examples/s1_isolates.rs`（主仓库没有它，只为量 isolate 增量；不进产物）。

## 3. 实测表

| 场景 | 进程/构建 | 工作集 MB (avg/max) | 私有 MB (avg/max) | 备注 |
|---|---|---|---|---|
| 空核心，无窗口 | `hbot.exe` release，`serve` 空闲 | **9.0 / 9.0** | **3.4 / 3.5** | 基线。3 次复跑一致（9.0/9.0/9.0）；就绪行出现 258–798 ms；不含 conhost（+10.1 MB，与本行无关） |
| 核心 + 1 bot 连接 | 同上 + `project_open` + 1×`bot_start` | **11.0 / 11.0** | **3.9 / 4.0** | 已开项目基线是 9.9 / 3.6–3.7，故首个 bot 增量 **+1.1 WS / +0.3 Private**（含适配器工厂等一次性初始化） |
| 核心 + 8 bot 连接 | 同上 + 8×`bot_start` | **11.7 / 11.8** | **4.6 / 4.7** | 相对已开项目基线 +1.8 WS / +1.0 Private。另测 4 bot：11.3 / 4.2；16 bot：12.4 / 5.3 |
| 核心 + 8 bot + 8 isolate | 各 bot 1 个 worker | **未直接合测**（推算 ≈ 9.9 + 1.8 + 8×0.30 ≈ **14.1** WS） | 推算 ≈ 3.7 + 1.0 + 8×0.30 ≈ **7.1** | 无法从外部让 core 里真的建出 8 个 isolate（见 §6），故拆成两项实测再线性相加：bot 增量见上一行、isolate 增量见下一行。**属推算，不是实测。** |
| 每 QuickJS isolate（独立 bench） | `s1_isolates.exe` release，1 / 8 / 32 个 | 6.8 / 8.2 / 14.5（三档；基线 5.7 / 5.1 / 5.2） | **1.0 / 3.0 / 10.2**（基线 0.7 / 0.6 / 0.7） | **每 isolate ≈ 0.30 MB Private、≈0.29 MB WS，线性**；spawn 1/8/32 个耗时 2.87 / 4.91 / 18.34 ms；worst dispatch 157 µs / 157 µs / 741 µs。基线已含 swc 编译器，不能当 engine 冷启动成本 |
| Tauri 桌面“空窗” | `hbot-desktop.exe` release，启动后 10 s（M3） | **382.5 / ≈410**（三次 379.6 / 382.5 / 401.9） | **205.5 / ≈222**（三次 205.5 / 204.4 / 219.1） | **本应用没有“空窗”形态**：窗口初现即 IDE 外壳（窗口标题就是 `hbot`，t=1 s 时工作集已 364 MB），只能取“启动后 10 s”代表。含 7 个子进程 |
| Tauri 桌面 + IDE 首屏 | 同上，40 s 稳态（M4） | **392.2 / ≈421**（四次 392.2 / 417.3 / 389.2 / 392.4） | **207.4 / ≈224**（四次 207.6 / 222.7 / 207.2 / 190.1） | 同上进程，等 IDE 完全渲染。**未打开项目、未打开文件**（Monaco 懒加载 chunk 未加载），故这是 IDE 外壳的稳态，不是“3 个标签打开”后的值 |
| Tauri 桌面 + IDE 打开（一个项目、3 个标签） | — | **未测** | **未测** | 原表这一行的问题本身没有回答（[06](../06-ide.md) 的“< 220 MB 整机”目标无法判定）。需要 UI 交互打开文件才会加载 Monaco；原因见 §6。上一行（外壳 392.2 WS / 207.4 Private）只能当下界 |

桌面两行的主进程 / 子进程拆分（`-SplitChildren`；M3 取三次中位，M4 取四次中位）：

| 口径 | 主进程 `hbot-desktop` | 子进程合计（7 个） | 整机（主+子） |
|---|---|---|---|
| M3（10 s） | WS **42.3** / Priv **13.9** | WS **340.3** / Priv **191.6** | WS **382.5** / Priv **205.5** |
| M4（40 s） | WS **42.6** / Priv **14.0** | WS **349.9** / Priv **193.5** | WS **392.3** / Priv **207.4** |

子进程构成（每次都是 7 个）：1 个 `conhost.exe`（10.1 MB WS / 1.6 MB Priv） + 6 个 `msedgewebview2.exe`（WS 约 122、77、68、35、20、10–18 MB；最大那个 Priv 约 77 MB）。

第 4 次复核（干净重建后、40 s 口径）：整机 WS **392.4** / Priv **190.1**，主进程 42.6 / 14.0，子进程 349.9 / 176.1。→ **WS 每次都很稳（389–418），Private 波动更大（190–223）**，因为 WebView2 各进程的私有提交受 GPU/缓存状态影响；引用整机 Private 时应给区间而不是单点。

时间序列（`-Mode series`，1 s 一次，单位 MB 整机工作集）：t=1 s 364.2 ｜ t=3 s 376.7 ｜ t=10 s 376.9 ｜ t=25 s 377.5 ｜ t=30 s 386.3 ｜ t=60 s 390.4。→ 首秒就到 364 MB，之后只缓慢爬升；**不存在“先小后大”的空窗阶段**。

### 3.1 每 bot / 每 isolate 增量是怎么量的

**每 bot（真路径，走 core 自己的 `bot_start`）**

- 项目夹具：`D:\hbot-measure\_measure\data\proj\bots.json`，16 个 bot，`url` 全指向 `ws://127.0.0.1:39001`（该端口**没有**服务端），`limits {memoryMb:32, handlerTimeoutMs:2000}`。
- 驱动：`m2-bots.mjs`（Node 24 内置 WebSocket，无 npm 依赖）→ `auth` → `project_open` → 打 `BASELINE_READY`（外部采基线 10 s）→ 逐个 `bot_start` → 打 `PEAK_READY`（外部采 16 s）→ `bot_list` 校验。
- 关键事实：**连接是异步的**，没有 OneBot 服务端时 `bot_start` 仍返回 ok、`status=running`，适配器在后台按 1/2/4/8 s 退避重连（日志 `onebot connection lost; reconnecting attempt=N`，`os error 10061`）。所以不需要 mock WS 服务端也能测“N 个 bot 的连接态内存”。
- 单次 `bot_start` 延迟 3–5 ms（不等待连接）：1/4/8/16 个 bot 的总耗时 5 / 13 / 24 / 69 ms。
- 斜率：以已开项目基线 9.9 / 3.6–3.7 为原点，N=16 时 12.4 / 5.3 → **0.16 MB WS/bot、0.10 MB Private/bot**；N=8 给 0.23 / 0.12；N=4 给 0.35 / 0.13（越少越受首个 bot 的一次性初始化影响）。取 N=16 的斜率作为“边际成本”。

```powershell
# 逐档跑：N = 1 / 4 / 8 / 16，每档约 60 s
cd D:\hbot-measure\_measure
pwsh -NoProfile -File .\scripts\m2-bots.ps1 -Count 16 *> .\logs\m2b-n16-result.txt
```

**每 isolate（core 之外的独立 bench）**

core 里的 isolate 是**懒创建**的（`SandboxHandler::new` 不建 isolate，第一条消息到来才建，且每 bot 上限 2 个 worker），而 `bot_inject` 要求 `conversationId` 先存在于库中 —— 从外部没法在不造真实 QQ 流量的前提下让 core 建出 8 个 isolate。所以改用 `hbot-code` 自己的 example 直接量“isolate 本体 + 首条消息编译”的成本：

```powershell
cd D:\hbot-measure; $env:CARGO_TARGET_DIR='D:\hbot-target\measure'
cargo build --release -p hbot-code --example s1_isolates *> D:\build-s1isolates.log
pwsh -NoProfile -File D:\hbot-measure\_measure\scripts\m2-isolates.ps1 -Count 32
```

`m2-isolates.ps1` 的做法：起 `s1_isolates.exe`（`HBOT_S1_ISOLATES=N`），它先冷编译一个 1 文件项目（让 engine 先加载，打 `BASELINE_READY` 保活 14 s）→ 外部采 10 s 基线 → 它再 spawn N 个 isolate 并各派发一条消息（走“首条消息编译项目”的真实路径，打 `ISOLATES=N READY` 保活 26 s）→ 外部再采 20 s。增量 = 峰值 − 基线。

**桌面 / 冷启动**：命令与时间点见 [S2-tauri.md](S2-tauri.md) §2。

### 3.2 二进制体积（release，clean 构建）

| 产物 | 字节数 | MB |
|---|---|---|
| `hbot.exe`（`hbot-cli` 的产物名，**不叫 `hbot-cli.exe`**） | 16,976,384 | **16.2** |
| `hbot-desktop.exe` | 34,644,480 | **33.0** |

`[profile.release]` = `lto="thin"` / `codegen-units=1` / `strip="symbols"`。桌面产物 33.0 MB 里是 Rust 核心 + Tauri 壳；**WebView2 运行时不在里面**（由系统提供，157.0.4341.46），所以“装上就能跑”的整包体积要另算 runtime。

## 4. 判定

- 核心（无窗口）< 60 MB、每 bot < 6 MB、每 isolate < 8 MB：目标成立，ADR 可转 Accepted（还需 S1/S2 数字）。
- 核心 60–120 MB：先查是否重复持有连接/缓存/日志缓冲，再决定是否接受。
- 核心 > 120 MB 或窗口整机 > 300 MB：按 ADR-0002 §11 的重估信号处理（换 UI / 换引擎），不要靠“优化一下”糊过去。

### 4.1 结论（2026-11-16 实测）

| 判定项 | 阈值 | 实测 | 结论 |
|---|---|---|---|
| 核心（无窗口） | < 60 MB | **9.0 MB WS / 3.4 MB Private** | **达标**，余量 6.7× |
| 每 bot 增量 | < 6 MB | **0.16 MB WS / 0.10 MB Private**（N=16 斜率；首个 bot +1.1 MB） | **达标**，余量约 37× |
| 每 isolate 增量 | < 8 MB | **0.30 MB Private / 0.29 MB WS**（1/8/32 三档线性） | **达标**，余量约 27× |
| 整机（桌面，含 WebView2） | < 300 MB | **WS 求和 382.5（10 s）/ 392.3（40 s，四测中位）MB；Private 求和 205.5（10 s）/ 190–223（40 s，四测）MB** | **看口径：Private 达标，WorkingSet 不达标** |

**一句话结论：Rust 核心侧全面达标且余量很大；唯一不达标的是桌面整机，而且不达标的部分几乎全是 WebView2。**

- 按 §4 第三条的字面口径（窗口整机 WorkingSet 求和）**不达标**：382.5 / 392.3 MB 对 300 MB 线超 **27%–31%**。
- 按 Private 求和**达标**：10 s 口径 205.5 MB；40 s 四次为 190.1 / 207.2 / 207.6 / 222.7 MB（中位 ≈207），余量 26%–37%。
- ADR-0002 写的“桌面空闲总 RSS < 220 MB”：WS 口径不成立（382 > 220）；Private 口径**勉强成立**（中位 ≈206，但四次里有一次 222.7 MB 越线，余量 0%–6%）。这条线定得太紧，实际没有工程余量。
- **最危险的一项是 WebView2 子进程**：6 个 `msedgewebview2.exe` + 1 个 conhost 合计 WS 337–374 MB、Private 176–208 MB（四个 40 s 样本），占整机工作集的 **88%–89%**。Rust/Tauri 主进程只有 42 MB WS / 14 MB Private —— 换 Tauri 省掉的是“Electron 主进程 + Node 运行时”那部分，**省不掉 Chromium 渲染侧**。
- 这 300 MB 线在 WorkingSet 口径下对任何 Chromium 壳都不友好（同一份 WebView2 DLL 在 6 个进程里被重复计入）。若 ADR 要一个可判定的验收口径，建议**显式写成“整机 Private 求和 ≤ 300 MB”**（当前中位 ≈207 MB、最差 223 MB，达标），或把 WorkingSet 线抬到 450 MB；两者都不要含糊地写“总 RSS”。**这个口径选择应由 ADR 记录，不能由这份 spike 默认掉。**
- “空窗”这一项在实现上并不存在（窗口一出现就是 IDE 外壳），所以 §3 的两行桌面数字相差只有 10 MB —— 桌面内存的大头与“IDE 打开多少内容”关系不大，与“有没有 WebView2”关系极大。
- 未测项（每 bot 都在收消息、IDE 真的打开 3 个标签、按键延迟、1 MB 文件打开）见 §6，**不要拿本文数字外推这些场景**。

## 5. 记录纪律

- 每次测量写：commit、构建 profile（debug/release）、机器、Windows 版本、是否开启 Defender 实时扫描。
- 不用“开发者机器上大概”当结论；不把 debug 构建的数字写进验收。
- 数字变化 >10% 时，先确认测量条件一致，再怀疑代码。

## 6. 没测到的项（含原因）

| 项 | 状态 | 原因 |
|---|---|---|
| 核心内**真**的 8 bot + 8 isolate 合测 | 未测（只给线性推算） | 每 bot 的 isolate 是懒创建（`SandboxHandler`：第一条消息才建，每 bot ≤2 个 worker），而 `bot_inject` 要求 `conversationId` 先在库里存在 —— 从外部无法在不造真实 QQ 流量/不改代码的前提下让 core 建出 8 个 isolate。拆成 bot 增量（实测）+ isolate 增量（独立 bench 实测）相加，已在表里标“推算” |
| 每 bot **收到消息**后的增量（含 2 个 worker） | 未测 | 同上：需要真实 OneBot 服务端 + 会话。可线性估算：0.16 + 2×0.30 ≈ 0.76 MB WS/bot |
| “空窗口”基线 | 结构性不可测 | 该应用不存在空窗形态：窗口出现即 IDE 外壳并立刻 `loadTree()`，看不出“只有 WebView 和空壳”的状态 |
| IDE 打开项目 + 3 个标签后的内存 | 未测 | 需要 UI 交互（打开文件才会加载 Monaco 懒加载 chunk），当前没有向 WebView2 注入输入的手段，见 [S2](S2-tauri.md) §3 |
| 按键 p95 / 1 MB 文件打开 | 未测 | 同上（无自动化输入 + 无 DevTools/性能埋点），见 [S2](S2-tauri.md) §3 |
| 与旧 Electron 项目对比 | 按计划不做 | S0 范围外，留到 M6 影子运行 |