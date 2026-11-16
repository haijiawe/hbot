# S13 新项目模板 → 第一条回复（试跑计时）与同一会话 100 条保序

- 日期：2026-11-17
- 环境：Intel i5-14600KF（20 逻辑核）/ 31.8 GB RAM / Windows 11 专业版 10.0.26300（build 26300）/
  rustc 1.96.0 / 两个 profile：`cargo test`（dev，`debug=1`，**未优化**）与 `cargo test --release`
  （`lto="thin"`、`codegen-units=1`）/ `CARGO_TARGET_DIR=E:\hbot-target\tryrun`（E 盘剩余 160 GB；
  D 盘曾是 ENOSPC 事故点，本项一次都没碰）
- 目的：用**产品真实路径**回答两件事，两者都是出口标准（M2）里还没人量过的：
  1. 「新建项目模板 → 看到第一条回复」到底要多久？出口标准写的是 **< 3 分钟**，
     这条 3 分钟里哪些段是确定的、哪些段根本不在我们手里？
  2. 同一个会话里连着塞 100 条消息，回复是不是**严格保序**（不乱序、不丢、不重）？
     历史教训是旧项目 `void dispatch` 造成过乱序，所以这条必须用真实队列量，不能靠"设计上串行"背书。
- 方法（可复制的命令见 §2）：`crates/hbot-rpc/tests/tryrun_timing.rs`（新增，4 个用例，无 `#[ignore]`）
  + 已有的 `scripts/headless-startup-probe.ps1`（量进程冷启动那一段）。
- 结论：**A 段有条件通过、B 段通过**。确定性部分（脚手架 → 第一条回复）**中位 0.322 s / 最大 0.328 s**
  （n=5，in-process）；再加上进程冷启动 **35 ms 中位**（n=5）也才 **约 0.36 s**，相对 3 分钟有 ~500× 余量。
  其中 300 ms 还是热重载去抖窗口（§3.3），真正在算的只有约 50–60 ms。
  但 `< 3 分钟` 这条出口真正的时间在**LLM 生成代码 + 人写需求 + 点 UI**，那几段**不在本次测量里**，
  也没有任何自动化测试覆盖 —— 详见 §5 的判定说明。
- 对设计的影响：无代码改动。本项只新增一份测试 + 本文件；数据用来把"3 分钟出口"这句话的边界写清楚
  （它管确定性链路，不管 LLM 延迟），不构成对任何预算的修改。
- 未验证：见 §7（最重要的一条：LLM 生成时间、桌面 Electron 冷启动、真实项目规模的首次编译）。

---

## 1. 三个口径，别混着说

"启动要多久"这句话在本项目里至少有三个不同的意思。混着说会得出互相矛盾的结论，所以先钉死：

| 口径 | 起点 → 终点 | 本项怎么量 | 数值 |
|---|---|---|---|
| **A. 进程已启动后** | 进程/测试已在跑、`AppState` 已打开 → run log 里出现第一条 `[reply] ` | `tryrun_timing.rs`（in-process，走 `Dispatcher`） | **0.322 s 中位**（n=5） |
| **B. 含进程冷启动** | OS 创建 `hbot` 进程 → ws 已 bind 且 ready 行已写出 | `scripts/headless-startup-probe.ps1`（子进程 + 父进程读 stdout） | **35.1 ms 中位**（n=5）；全新数据目录 **54 ms 中位**（n=3） |
| **C. LLM 生成代码** | 用户写完需求 → 模型把 `src/**` 写出来 → 直到能跑 | **本项完全没有量，也没有入口能量** | 未知（模型/网络/提示词决定） |

口径 A 与 B 都只覆盖**确定性部分**。这一点是本文件最重要的结论，不是脚注。

口径 B 里还有一层没量：**桌面 Electron 的冷启动**（Tauri/WebView2/Electron 主进程 + 渲染首帧）。
`headless-startup-probe.ps1` 量的是 `hbot serve`（无 UI）；桌面那一层的历史数字在
`docs/spikes/S2-tauri.md:73,86-87`（878/1264 ms，桌面"进程启动 → 核心可用"），本项没有复测。

---

## 2. 方法：每一步都是产品命令，没有一处直调内部实现

测试文件：`crates/hbot-rpc/tests/tryrun_timing.rs`。四个用例：

| 用例 | 量什么 |
|---|---|
| `template_to_first_reply_timing` | 脚手架 + `ProjectOpen` + 编译参考 + `bot_start` + 注入→首条回复 + 注入→热回复（5 轮） |
| `project_create_to_first_reply_timing` | `Command::ProjectCreate` 一条命令 + `bot_start` + 首条/热回复（5 轮） |
| `hundred_rapid_injects_stay_in_order` | 100 条**不等回复**连发 → 断言严格 `#1..#100` |
| `slow_injects_stay_in_order` | 10 条"每条立刻等回复"慢速对照 → 断言严格 `#1..#10` |

链路与产品路径的对应关系（**没有任何 mock / 假适配器**）：

| 步骤 | 测试里调的东西 | 产品里同一份实现 |
|---|---|---|
| 脚手架 | `hbot_code::manifest::scaffold(dir, name)` | `Command::ProjectCreate` 的 Phase 1 内部就是它（`commands/system.rs::scaffold_project`），另外还多写一份 `hbot.d.ts` |
| 打开项目 | `Command::ProjectOpen { path }` | 同一条 wire 命令（含 `refresh_host_dts`） |
| 编译参考口径 | `Command::Diagnostics { path: None }` | 同一条 wire 命令 → `hbot_code::compile_project`（**release 编译**） |
| 起机器人 | `Command::BotStart { bot_id }` | 同一条 wire 命令（`PlatformRouter` + 真 `SandboxHandler`） |
| 注入 | `Command::BotInject { bot_id, conversation_id, text }` | 同一条 wire 命令 → `Supervisor::inject` → 会话队列 → QuickJS |
| 看回复 | `store.list_logs` 过滤 `[reply] ` / `EventBus` 的 `Event::RunLog` | IDE 日志面板读的同一张表、同一条事件流 |

平台用 `local`（`hbot_adapters` 的本地适配器）：`send` 不联网，把文本按 `[reply] ` 前缀逐行写 run log。
选它是因为它是**唯一能端到端跑通、又不依赖 QQ 凭据/网络**的平台；再往上就是真协议端，属于 M2 的另一项。

两个夹具动作（都**不计入任何分段**，因为它们不是"模板 → 回复"链路的组成部分）：

1. **写 `bots.json`**：`manifest::scaffold` 不写部署配置（它只产项目源码），而 `bot_start` 要读项目根的
   `bots.json` 才起得来。真实用户是在 UI 里填这个表单 —— 那段时间本来就不在 `cargo test` 里。
   `bots.json` 内容只有 `id` + `platform` + `name`，没有 url/凭据。
2. **`AppState::open`**：每轮一个全新临时目录 + 一个全新 `AppState`（任务书要求），它的耗时单独算进
   "整轮墙钟"这一行，不进"合计"。

关于"首次编译算哪一段"：**`ProjectOpen` 不编译**。第一次编译发生在第一条消息的处理路径上
（`app/runtime/sandbox_handler.rs` 的 `compiled()` → `hbot_code::compile_project_debug`，带
`FingerprintCache`）。所以它天然落在「注入 → 第一条回复」里；「注入 → 第二条回复」是缓存命中的热路径。
注意：**这两段的差不是"编译有多贵"** —— 里面最大的一块是 `HOT_RELOAD_DEBOUNCE`(300 ms) 的固定等待，
详见 §3.3。另外用 `Command::Diagnostics` 单独量一次"编译一个干净项目本身要多久"
（release 口径，**不在试跑热路径上**，只作参考）。

复用 `--nocapture` 才有数字：

```powershell
$env:CARGO_TARGET_DIR = "E:\hbot-target\tryrun"
cargo test -p hbot-rpc --test tryrun_timing -- --nocapture          # dev profile（本文件主表）
cargo test --release -p hbot-rpc --test tryrun_timing -- --nocapture # release 对照
```

进程冷启动（口径 B，用已存在的探针，一次实测）：

```powershell
$env:CARGO_TARGET_DIR = "E:\hbot-target\tryrun"
cargo build --release -p hbot-cli
pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Exe E:\hbot-target\tryrun\release\hbot.exe `
     -Runs 5 -Warmup 1 -Plain -Tag s13-plain        # 常规启动（库已存在）
pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Exe E:\hbot-target\tryrun\release\hbot.exe `
     -Cold -Runs 3 -Warmup 0 -Tag s13-cold          # 每次删库 = 全新数据目录
pwsh -NoProfile -File scripts\headless-startup-probe.ps1 -Exe E:\hbot-target\tryrun\release\hbot.exe `
     -Trace -Runs 3 -Warmup 1 -Tag s13-trace        # 分段（有埋点开销，只看结构）
```

原始数据（`_recovery/` 已 gitignore）：`_recovery/s13-test.log`、`_recovery/s13-test-release.log`、
`_recovery/s13-probe-plain.out.txt`、`_recovery/s13-probe-trace.out.txt`、
`_recovery/s13-startup/s13-plain-result.json`、`_recovery/s13-startup/s13-cold-result.json`、
`_recovery/s13-startup/s13-trace-result.json`。中位是 nearest-rank（n=5 取正中间那一个）。

---

## 3. 结果 A：模板 → 第一条回复（5 轮，每轮新临时目录 + 新 AppState）

### 3.1 主表：`scaffold` + `ProjectOpen`（dev profile，n=5）

| 分段 | 中位 (ms) | 最大 (ms) | n |
|---|---|---|---|
| 1 脚手架 `manifest::scaffold`（= `project_create` 的 Phase 1） | **3.5** | 3.9 | 5 |
| 2 打开项目 `project_open`（含 `refresh_host_dts`，**不编译**） | **0.5** | 1.0 | 5 |
| 3 编译 `diagnostics`（release 口径，独立参考，不在试跑热路径） | 0.9 | 1.2 | 5 |
| 4 `bot_start`（local 适配器 + supervisor + 建 `local:default` 会话） | **2.2** | 4.0 | 5 |
| 5 **注入 → 第一条回复**（含 300 ms 去抖窗口 + 首次 debug 编译 + isolate 装载） | **316.2** | 321.8 | 5 |
| 6 注入 → 第二条回复（热路径，无编译） | **2.0** | 2.2 | 5 |
| 合计（1+2+4+5，in-process） | **321.9** | 328.4 | 5 |
| 整轮墙钟（含夹具写 `bots.json` + `AppState::open`） | 354.4 | 362.7 | 5 |

每轮原始值（ms，顺序：脚手架 / 打开 / 编译 / `bot_start` / 首条回复 / 热回复 / 整轮）：

```text
第 1 轮：  3.5 / 1.0 / 0.9 / 4.0 / 319.9 / 1.7 / 353.6
第 2 轮：  2.7 / 0.6 / 1.0 / 2.5 / 316.2 / 1.8 / 354.4
第 3 轮：  3.9 / 0.5 / 1.2 / 2.2 / 309.7 / 2.0 / 362.7
第 4 轮：  3.6 / 0.5 / 0.9 / 1.8 / 315.7 / 2.2 / 352.3
第 5 轮：  3.0 / 0.5 / 0.6 / 2.1 / 321.8 / 2.1 / 356.3
```

### 3.2 产品命令版：`Command::ProjectCreate` 一条命令（dev profile，n=5）

| 分段 | 中位 (ms) | 最大 (ms) | n |
|---|---|---|---|
| 1 `project_create`（脚手架 + 落库 + 打开项目，一条 wire 命令） | **9.5** | 10.9 | 5 |
| 2 `bot_start` | 3.0 | 8.8 | 5 |
| 3 注入 → 第一条回复（含 300 ms 去抖 + 首次 debug 编译） | **312.7** | 317.9 | 5 |
| 4 注入 → 第二条回复（热路径） | 1.8 | 3.2 | 5 |
| 合计（1+2+3） | **328.7** | 331.6 | 5 |

每轮原始值（ms，顺序：`project_create` / `bot_start` / 首条回复 / 热回复）：

```text
第 1 轮：  9.5 / 2.6 / 305.6 / 1.6
第 2 轮：  9.5 / 8.8 / 310.7 / 1.3
第 3 轮：  7.7 / 3.0 / 312.7 / 1.8
第 4 轮： 10.8 / 3.9 / 313.9 / 2.0
第 5 轮： 10.9 / 2.7 / 317.9 / 3.2
```

`project_create`（9.5 ms）比 `scaffold`（3.5 ms）多出来的约 6 ms 是产品命令多做的事：
写 `hbot.d.ts`、`store.insert_project` 落库、把 workspace 切过去。一条命令走完仍然只要十几毫秒。

### 3.3 时间花在哪：**316 ms 里有 300 ms 是"故意等的"去抖窗口，不是编译慢**

这条是本项最容易被误读的地方，所以给足证据。

- 热回复（第二条）只要 **2.0 ms** 中位 —— 这是 QuickJS 里跑一遍模板 handler + 写一行 run log 的真实成本。
- 首条回复 316 ms 与热回复 2 ms 差 ≈ **314 ms**。它**不是**编译成本，主要是一段**固定的等待**：
  `crates/hbot-rpc/src/app/runtime/sandbox_handler.rs:73` 的
  `pub const HOT_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);`。
  `compiled()`（同文件 `:362`）在缓存未命中时先问去抖窗口：`hot.debounce(...)` 对一个**从未见过的指纹**
  走 `_` 分支、返回 `Some(now + 300ms)`（`HotReload::debounce`，`:226-247`），然后
  `tokio::time::sleep_until(...)` 把这条消息**按住 300 ms** 再取一次指纹、才开始编译。
  设计意图是把编辑器的连番保存合并成一次编译（热重载），代价是**每个新指纹的第一条消息都要等满窗口**。
- 三条互相独立的旁证，说明这 310 ms 不是"算出来的"：
  1. **dev 与 release 几乎一样**：316.2 ms（dev 主表）vs 311.4 ms（release，§3.4）。
     如果这段是 Rust 计算，未优化的 dev 构建至少该慢几倍；
  2. **编译本身很便宜**：`Command::Diagnostics`（release 编译同一个模板）只要 **0.5–0.9 ms**；
  3. **数值贴着常量**：本文件一共产生 **20 次首条回复**（dev 5+5、release 5+5），
     全部落在 **304.2–321.8 ms**，即 `300 ms + 4~22 ms`，
     这个"4~22 ms"才是"首次 debug 编译 + isolate 装载 + 执行"的真实开销。
     另外快速连发那条用例的第一条消息也付了它（408 ms 总数 = 300 ms 窗口 + 其后 99 条热处理）。
- 所以"模板 → 第一条回复"的**真实计算量**只有几十毫秒；310 ms 是产品有意加的确定性延迟。
  对 3 分钟这条出口没有影响（0.3 s），但它是"点试跑之后体感要等一下"的唯一解释。
- 顺带一个副作用：`ProjectOpen` 之后**没有任何"预热/预编译"入口** —— 想省掉这 300 ms，
  只能等第一条消息自己走过去抖；这是设计选择，本项只记录，不改（写域也不允许）。

### 3.4 release 对照（同一份测试，`cargo test --release`，n=5）

产品发货是 release，所以这张表更接近用户实际能看到的数字；它同时也是一条**诊断**（见 §3.3）：

| 分段（`scaffold` + `ProjectOpen` 口径） | dev 中位 | **release 中位** | release 最大 |
|---|---|---|---|
| 脚手架 `manifest::scaffold` | 3.5 | **3.3** | 4.6 |
| 打开项目 `project_open` | 0.5 | **0.4** | 0.5 |
| 编译 `diagnostics`（release 编译，参考） | 0.9 | **0.5** | 0.5 |
| `bot_start` | 2.2 | **1.3** | 2.0 |
| 注入 → 第一条回复 | 316.2 | **311.4** | 318.5 |
| 注入 → 第二条回复（热路径） | 2.0 | **1.2** | 1.3 |
| 合计（in-process） | 321.9 | **316.3** | 323.8 |
| 整轮墙钟 | 354.4 | **346.9** | 360.6 |

`ProjectCreate` 口径（release，n=5）：`project_create` **10.0**（最大 19.7）/ `bot_start` **2.1**（3.0）/
首条回复 **306.7**（315.7）/ 热回复 **0.9**（1.1）/ 合计 **316.9**（336.3）。

100 条快速注入（release）：100 条入队 **32.9 ms**，第一条注入 → 第 100 条回复 **385.8 ms**，
序列仍是 `[1..100]`（首 5 `[1,2,3,4,5]`、末 5 `[96,97,98,99,100]`）。慢速 10 条单条耗时
`[340.6, 31.2, 31.1, 31.0, 30.8, 30.9, 31.1, 31.1, 31.0, 31.1]`，序列 `[1..10]`。

**release 只把"算"的部分减半（3.5→3.3、2.2→1.3、2.0→1.2 ms），那 300 ms 一点没动（316.2→311.4）**
—— 这正是 §3.3 判定"它是等出来的、不是算出来的"最强的一条证据。

### 3.5 口径 B：进程冷启动（`scripts/headless-startup-probe.ps1` 实测）

`-Plain`（不打点，量父进程观测到的真实总时长）**n=5**：

| 轮次 | `proc_to_ready_ms` |
|---|---|
| run-01 | 34.70 |
| run-02 | 35.60 |
| run-03 | 35.10 |
| run-04 | 37.30 |
| run-05 | 29.20 |
| **汇总** | **median 35.1 / p95 37.3 / min 29.2 / max 37.3** |

`-Cold`（每次前删库 = 全新数据目录）**n=3**：54.00 / 58.60 / 50.00 → **median 54 / p95 58.6**。

`-Trace`（打点档，**有埋点开销**，只看结构，n=3）：`proc_to_ready` 中位 35 ms，其中
`proc_to_main` 22 ms（OS 装镜像 + CRT + Rust 运行时初始化）、`appstate_open_dur` 11.5 ms
（里面 `store_open_total_dur` 11.1 ms，而 `store_migrations_dur` 只有 0.1 ms）、`ws_bind_dur` 1.5 ms。
`conn_wal_writer` 一项就占 10 ms —— 首次建库那条 WAL 是冷启动里最实的一段，不是空转。

⇒ **进程冷启动 ≈ 35 ms（库已存在）/ 54 ms（全新数据目录）**，与 `docs/spikes/S11-headless-startup.md`
的 24–38 ms / 52–70 ms 同一量级（本机当前负载下取到的是区间上沿）。

### 3.6 两个口径叠起来

| 组成 | 中位 |
|---|---|
| 进程冷启动（headless `hbot serve` → ready） | 35.1 ms |
| 脚手架（`project_create` 一条命令） | 9.5 ms（dev）/ 10.0 ms（release） |
| `bot_start` | 3.0 ms（dev）/ 2.1 ms（release） |
| 注入 → 第一条回复（含 300 ms 去抖窗口） | 312.7 ms（dev）/ 306.7 ms（release） |
| **合计（进程创建 → 第一条回复）** | **≈ 360 ms（dev）/ ≈ 354 ms（release）** |

其中 **300 ms 是热重载去抖窗口**（§3.3），真正"在算"的部分只有 **约 50–60 ms**。

---

## 4. 结果 B：同一会话 100 条严格保序

### 4.1 快速连发 100 条（不等回复）

过程：`scaffold` → `ProjectOpen` → `BotStart` → 取 `local:default` 会话 id →
**不等待任何回复地**连发 100 条 `#1`..`#100`（每条都断言 `Response::Ok`）→
轮询 `store.list_logs` 直到 100 条 `[reply] ` 或 60 s 超时。

| 指标 | 实测 |
|---|---|
| 100 条全部入队（100 次 `dispatch` 的墙钟） | **62.1 ms**（≈0.62 ms/条，无背压；会话队列容量 1024） |
| 第一条注入 → 第 100 条回复可见 | **408.0 ms** |
| 收到的 `[reply] ` 行数 | **100**（恰好，不多不少） |
| 反转后的 `logs.id` | **严格递增**（写入序） |
| 解析出的序号序列 | **`[1, 2, 3, …, 99, 100]`**，与 `1..=100` 逐项相等 |

实际序列（日志里打印出来的原文）：

```text
实际序列首 5：[1, 2, 3, 4, 5]；末 5：[96, 97, 98, 99, 100]；全文长度 100
实际序列前 3 行原文：["收到：#1", "收到：#2", "收到：#3"]
实际序列后 3 行原文：["收到：#98", "收到：#99", "收到：#100"]
```

断言是三条互相独立的硬条件（任一条不满足就 panic，没有放宽）：

1. `rows.len() == 100` —— **无丢失、无重复**（重复会被 `(bot_id, external_id)` 去重挡掉，
   而 `external_id` 每条都不同，所以"重复"只可能来自回复被写两遍）；
2. 反转后 `logs.id` 的相邻对严格递增 —— **写入序**没有被并发写打乱；
3. `indices == (1..=100)` 的 `Vec<usize>` 逐项相等 —— **回复顺序**与注入顺序完全一致。

408 ms 这个总数也自洽：首条消息先付 **300 ms 去抖窗口**（§3.3）+ 首次 debug 编译 + isolate 装载，
剩下 99 条走热路径（单条约 1–2 ms），加上 20 ms 的轮询粒度，落在 400 ms 附近。
**如果会话内是并发处理，这个总数会更小，但顺序断言会崩** —— 所以"总数不算特别快"本身就是串行的旁证。

### 4.2 慢速对照 10 条（每条立刻等回复）

| 条数 | 单条耗时 (ms) |
|---|---|
| 1 | 331.2（含 300 ms 去抖窗口 + 首次编译） |
| 2–10 | 31.6 / 31.8 / 29.0 / 33.7 / 32.7 / 33.4 / 33.3 / 33.3 / 34.7 |

实际序列：`[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]` —— 严格递增、无丢失、无重复。

两条值得记的事实：

- 慢速节奏下单条稳定在 **~32 ms**，而快速连发时单条只要 **~1–2 ms**。差值不是乱序，
  是"每条都重新读一次 `list_logs` 再等 20 ms 轮询"的观测成本 —— 说明快速连发时队列确实在
  **连续吞吐**，而不是每条都在等外部触发。
- 慢速第一条 331 ms 与快速那 408 ms 里的首条 ~310 ms 对得上：两次独立进程里，
  "300 ms 去抖 + 首次编译 + 装载"都稳定在 300 ms 出头。

---

## 5. 结论

| 出口标准 | 判定 | 依据 |
|---|---|---|
| 同一会话 100 条消息严格保序 | **通过** | 快速 100 条与慢速 10 条都给出 `1..n` 严格递增，`logs.id` 严格递增，无丢失/重复 |
| 模板 → 第一条回复 < 3 分钟 | **有条件通过（只对确定性部分）** | 进程创建 → 第一条回复 **≈ 360 ms 中位**（n=5）；但 3 分钟里的大头（LLM 生成代码）**不在测量范围内** |

`< 3 分钟` 这条出口的诚实说法（这段是本文件的重点，不要只抄上面那行表格）：

- **覆盖到的**：进程冷启动（35 ms）+ 脚手架/`project_create`（9.5 ms）+ 打开项目（0.5 ms）+
  `bot_start`（3 ms）+ 热重载去抖窗口（300 ms，故意等的）+ 首次 debug 编译 +
  第一次执行 + 第一行 `[reply] ` 落库（合计 312.7 ms）。
  这些全是**确定性**的，本机实测合计 **≈ 0.36 s**，相对 3 分钟有 ~500× 余量。
  其中那 300 ms 还是热重载去抖（§3.3），真实计算只有约 50–60 ms。
- **没覆盖的（也就是 3 分钟真正花掉的地方）**：
  1. **LLM 生成代码**：从"用户写完需求"到 `src/**` 真的被写出来。依赖模型、网络、提示词长度，
     本项**一次都没量**，也没有产品命令能单独量这一段；
  2. **人类操作**：点新建项目、填项目名、写需求、看流式输出、点确认。这段是人的时间，不进任何测试；
  3. **桌面 Electron 冷启动**：本项只量了 headless `serve`（无 UI）。桌面那一层的历史数字在
     `docs/spikes/S2-tauri.md`（878/1264 ms，且是旧栈），当前栈没有复测；
  4. **真实项目规模的首次编译**：本次模板只有 1 个 `src/main.ts`（十几行），编译本身 0.5–0.9 ms，
     被 300 ms 去抖窗口完全盖住 —— 所以"项目变大后这 300 ms 会不会被编译盖过去"本次**没测**（见 §7）。

  所以"< 3 分钟"这句话里，**只有最后一段（模板就绪 → 第一条回复）是可以被自动化测试钉住的**；
  剩下的大头只能靠别的手段（LLM 端到端 eval、真机埋点）量。把 0.36 s 当成"3 分钟达标"的证据是错的。

---

## 6. 验收命令与退出码

全部在本机、`CARGO_TARGET_DIR=E:\hbot-target\tryrun` 下跑：

| 命令 | 退出码 | 结果 |
|---|---|---|
| `cargo test -p hbot-rpc --test tryrun_timing -- --nocapture` | **0** | `test result: ok. 4 passed; 0 failed; 0 ignored; finished in 4.52s` |
| `cargo clippy -p hbot-rpc --all-targets -- -D warnings` | **0** | `Finished dev profile`，无 warning |
| `cargo fmt -p hbot-rpc -- --check` | **0** | 无 diff（时间线见下） |
| `cargo test --release -p hbot-rpc --test tryrun_timing -- --nocapture`（附加对照，非验收项） | **0** | 4 passed |

`cargo fmt` 那条有一次中间态，如实记录：本项第一次跑它时是 **exit 1**，唯一被报的路径是
`crates/hbot-rpc/tests/hot_reload_e2e.rs`（第 104/153/189/366/686 行，5 处 diff）——
那是**另一个并行任务**的测试文件，属于本项"不许改其他测试文件"的写域边界外，
且当时它已在 HEAD 里（工作区干净）。本项新增的 `tryrun_timing.rs` 单独跑
`rustfmt --check --edition 2024` 一直是 exit 0。该文件随后被它的属主格式化，
final 复跑 `cargo fmt -p hbot-rpc -- --check` 就是 **exit 0**（`git status` 里能看到的
`M crates/hbot-rpc/tests/hot_reload_e2e.rs` 就是那次格式化，不是本项改的）。

---

## 7. 未验证 / 已知缺口

1. **LLM 生成代码的时间完全没有量。** 这是 3 分钟出口里最大的一块，也是唯一没有自动化入口的一块。
   现有测试全部走"模板自带 `src/main.ts`"，链路上没有一次模型调用。要量它得另做端到端 eval
   （`docs/spikes/S10-llm-eval.md` 是另一个口径的东西，不含"从需求到可跑项目"）。
2. **桌面 Electron 冷启动没量。** 只量了 headless `hbot serve`（35 ms）。桌面要加 Electron 主进程
   + 渲染首帧，`docs/spikes/S2-tauri.md` 的 878/1264 ms 是旧栈数字，当前栈未复测。
3. **大项目的首次 debug 编译没量。** 本次是 1 个文件；`Command::Diagnostics` 编同一模板只要 0.5–0.9 ms，
   所以在这台机上它被 300 ms 去抖窗口完全盖住。文件数/依赖涨上去之后编译会不会反超窗口、
   会不会把"首条回复"从 0.31 s 拉到秒级 —— 本次没有扫描（没有造大工程的夹具）。
4. **只有 `local` 平台。** 真协议端（QQ official / OneBot）的入站延迟、去重、限流都没进这条链路。
   任务书明确用 `local` 代替，所以这是口径选择，不是遗漏 —— 但"3 分钟"在真机上还要加协议端的网络时间。
5. **100 条只测了一个数量级。** 会话队列容量是 1024，本项只压到 100 条；再往上（接近容量、
   触发背压 `HbotError::Timeout`）的行为没测。另外 100 条都是同一个会话，多会话并行的保序没测。
6. **300 ms 去抖窗口的归属是"按源码路径推断"的，没有单独打点隔离。** 证据链是三条间接的
   （dev≈release、`Diagnostics` 编译 0.5–0.9 ms、20 次样本全落在 304.2–321.8 ms 且贴住 `HOT_RELOAD_DEBOUNCE`），
   以及 `sandbox_handler.rs:362-386` 的代码路径。要钉死"300 ms 窗口之后那 4–22 ms 里编译占几毫秒、
   isolate 装载占几毫秒"，得在 `compiled()` 里加观测点——那属于 `src/**`，本项写域不允许。
7. **计时主表是 dev profile。** §3.1/§3.2 是 `cargo test`（未优化、`debug=1`），§3.4 已补 release 对照；
   产品按 release 发货，所以读 §3.1/§3.2 时应以 §3.4 的 release 列为准。
8. **`bot_start` 的 8.8 ms 异常值**（§3.2 第 2 轮）没有追因：n=5 里出现一次 4 倍于中位的抖动，
   怀疑是首轮磁盘/Defender 干扰，但没做隔离实验（release 跑里 `bot_start` 最大 3.0 ms，没有复现）。