# ADR-0002：Agent 与 Bot 运行时改用 Rust，产品内移除 Node

- 状态：**提议（Proposed）**，等 7 天 Spike 后转 Accepted
- 日期：2026-11-16
- 取代：`0001-tech-stack.md`（Tauri + Node daemon + 低代码节点）
- 相关：`docs/02-architecture.md`、`docs/03-directory.md`、`docs/04-implementation-plan.md`

---

## 1. 背景与变化

ADR-0001 选了 **Tauri 2 + Node 24 daemon + React**，理由是 bot 负载以 I/O 为主、Agent/MCP/插件生态在 TypeScript 最全。

2026-11-16 用户给出两条新约束，改变了产品形态：

1. **先做“Agent 写代码”**：不做节点画布，不做流程编排器；核心体验是“描述需求 → Agent 读写项目代码 → 运行 → 看日志 → 迭代”。
2. **Agent 不能用 Node**：常驻内存太高。

旧项目是 Electron + Node，但没有做过内存基线，**不能引用任何“旧项目占 X MB”的数字**；能确认的是：Node/V8 堆 + Electron 多进程 + 依赖树，比一个 Rust 单核重。新项目要用实测决定是否达标。

产品也随之从“低代码平台”变成“**Agent-first 的 bot 工程环境**”：

- 用户不再拖节点，而是和 Agent 对话；
- Agent 产出的是**真实的 bot 项目代码**（TypeScript），不是图数据；
- 平台协议、连接、队列、存储仍由运行时负责，生成代码只写业务逻辑。

---

## 2. 决策

### 2.1 一句话

**Rust 单核（Agent + Bot + 代码沙箱 + 存储）+ Tauri 2 桌面（直接链接核心，无 sidecar）+ React 19 UI；生成代码用 TypeScript，SWC 去类型后在 QuickJS isolate 里执行。**

### 2.2 具体

| 层 | 选择 | 说明 |
|---|---|---|
| 核心 | Rust（stable，2024 edition） | 一个 `hbot-core` 库 + `hbot` 二进制 |
| 桌面 | Tauri 2 | Rust 直接链接核心，**不用 sidecar**；只有窗口/托盘/更新/密钥 |
| 后台 | 同一个 `hbot` 二进制 | `hbot serve` 暴露本机 WebSocket RPC；systemd 管理 |
| UI | React 19 + TypeScript 6 + Vite 8 + Monaco | 只做聊天、文件树、编辑器、运行/日志、设置 |
| 契约 | Rust 类型为真源，`ts-rs` 生成 TS | 不再手写三份 IPC 契约 |
| Agent | Rust | 循环、工具、审批、上下文、预算、LLM 客户端 |
| LLM | OpenAI 兼容 + Anthropic 原生 | `reqwest` + SSE；工具 schema 用 `schemars` |
| Bot 运行时 | Rust / tokio | 每 bot 一个 supervisor，每会话串行队列，持久化 outbox |
| 平台 | OneBot 11（WS）优先，通用 Webhook 次之 | 飞书/钉钉后续，不写进核心分支 |
| 生成代码 | TypeScript 多文件（`src/**`，相对 import）+ `hbot.d.ts` | 逐文件 SWC 去类型；类型检查在 Monaco 里做 |
| 执行沙箱 | QuickJS（`rquickjs`） | 每 bot 一个 isolate；内存/时间/调用上限 |
| 存储 | SQLite（`rusqlite`，bundled） | 单 writer + 小读池；WAL；迁移内嵌 |
| 密钥 | OS keychain（`keyring`） | 不可用则拒绝保存或显式明文 + 审计 |
| 打包 | Tauri bundler | Windows NSIS、macOS dmg、Linux deb/AppImage + systemd unit |

**产品运行时里没有 Node**：不 `spawn node`、不依赖 `node_modules`、不要求用户装 Node。pnpm 只用于构建 UI。

### 2.3 进程形态

```text
桌面：
  hbot-desktop (Rust + Tauri)
    ├─ 链接 hbot-core（同进程）
    ├─ WebView（React）
    └─ 命令/事件走 Tauri IPC（进程内，无端口）

Linux 后台：
  hbot serve --project <dir> --port 0
    ├─ 同一个 hbot-core
    ├─ 本机 WS RPC（127.0.0.1 + token）
    └─ systemd 管理生命周期
```

- 桌面**不启 sidecar**：少一个进程、少一套握手与崩溃恢复，内存也更低。
- 同一套核心既能被 Tauri 链接，也能被 CLI 链接；UI 传输层有两个实现（Tauri / WS），业务代码只依赖 `Api` trait。
- 浏览器开发模式：`hbot serve` + `pnpm dev` 连 WS，便于 UI 快速迭代。

---

## 3. 为什么不是其他方案

| 方案 | 不选的原因 |
|---|---|
| **Node / Electron（0001）** | 内存问题是本次的核心约束；把 Agent 放进 worker 只是换位置，V8 堆与 Electron 多进程还在 |
| **Node daemon + Rust 壳** | 两套运行时、两套崩溃模型；sidecar 握手/鉴权/更新都要自己写；内存仍是 Node 主导 |
| **Go 核心** | 内存可控、开发快，但 Tauri 是 Rust，等于两语言；QuickJS/SWC 绑定、async 生态不如 Rust 直接；Agent 工具层没有明显优势 |
| **Python 核心** | 打包体积、嵌入解释器、并发与内存都不占优；分发给普通用户更难 |
| **纯 Rust 原生 UI（egui/Slint）** | 内存最低，但 Monaco 级代码编辑器要自造，MVP 成本过高；作为“内存极限”备选保留 |
| **WASM 插件** | 隔离好，但让 Agent 直接写 WAT/Rust 不现实；JS 是当前最合适的生成目标 |
| **保留低代码节点** | 用户明确不做；节点引擎是旧项目最大的复杂度来源之一，删掉能显著缩小范围 |

---

## 4. 内存目标（S0/S2 已实测，见 §4.1）

**先定口径，再看数字。** 本 ADR 的「内存」一律指 **`PrivateMemorySize64`（私有提交，private commit）在进程树上求和**，不用 `WorkingSet64`：

- 同一份 WebView2 DLL 会被 6–7 个子进程各自映射，WorkingSet 把共享页**重复计入**每个进程（实测子进程 340 MB 里有大量共享页），用它当预算等于「同一块代码按窗口数重复收费」，既不可优化也不可比。
- Private 是真正为这个应用新增的提交量，能直接对应「这台机器少了多少可用内存」，也是唯一能拆成「核心 / WebView」两块归因的口径。
- 代价：私有提交不含共享页的物理占用，比任务管理器「内存」列（private working set）略低。**所以文档一律写全 API 名，不写含糊的「RSS」。**

| 指标（private commit） | 目标 | 实测 | 状态 |
|---|---|---|---|
| 核心（`hbot serve` 或桌面 Rust 主进程）空闲 | < 60 MB | **3.4 MB**（serve）/ 13.9 MB（桌面主进程） | 达标，余量 4–17× |
| 桌面整机（Rust 主进程 + WebView2 子进程求和） | < 260 MB | **190–223 MB**，四次中位 206 MB | 达标（余量 ~20%） |
| 每多一个 bot（连接态） | < 6 MB | **0.10 MB**（首个 bot +1.1 MB） | 达标，余量 60× |
| 每个 QuickJS isolate | < 8 MB（堆上限 32 MB） | **0.30 MB**（32 个共 +9.5 MB） | 达标，余量 27× |
| 冷启动到核心可用 | < 800 ms | `hbot serve` **878 ms 中位 / 1264 ms p95**；桌面 **966 ms 中位 / 1330 ms p95**（2026-11-16 分段复测，n=7） | **未达标**，见 §4.2 |
| 二进制 | < 60 MB | **`hbot.exe` 16.2 MB / `hbot-desktop.exe` 33.0 MB** | 达标（SWC 链没有把体积撑爆） |
| 安装包 | < 70 MB | 未测（`bundle.active=false`，M5 才出安装器） | 待 M5 |

原「桌面空闲总 RSS < 220 MB」这条**作废**：按 WorkingSet 实测 382–392 MB，而其中 88–89% 来自 WebView2 子进程，Rust 主进程只有 42 MB。任何 Chromium/WebView2 壳在 WS 口径下都过不了 220 MB，把它留在 ADR 里只会让后续每次评审都拿一个不可达的数字否决一个已达标的方案。

达不到时按顺序重估：① WebView 换原生 UI；② QuickJS 换 `deno_core`（内存更高但 async 完整）；③ 回退 0001。

### 4.1 实测结论（S0/S2，commit `ba49346`）

- **Rust 核心这一侧的假设全部成立，且余量很大**：9 MB WS / 3.4 MB private 的空闲核心、每 bot 0.10 MB、每 isolate 0.30 MB。当初「用 Rust 砍掉 Node 运行时约几十 MB」的判断得到验证——**核心已经小到不再是内存议题**。
- **整机内存完全由 WebView2 决定**：382.5 MB WS 里有 340.3 MB 在 7 个子进程（1 conhost + 6 `msedgewebview2.exe`），而且 **t=1 s 就到 364 MB，没有「先小后大」的空窗**——不是 Monaco 懒加载造成的，是 WebView2 自身基线。
- 因此 §11 的那条判断（「桌面内存的大头是 WebView，不是 Node」）从推论升级为实测：**想再往下压，唯一动作是换原生 UI，Rust 侧再优化没有意义。**
- 测量条件、脚本与原始日志：`docs/spikes/S0-memory.md`、`docs/spikes/S2-tauri.md`（含机器、Defender 状态、采样窗口、副本构建路径）。

### 4.2 冷启动未达标怎么处理

2026-11-16 用 `scripts/startup-probe.ps1` 做了**分段复测**（`HBOT_STARTUP_TRACE=1`，桌面 release，7 次，同一台机器）：

| 分段 | 中位 | p95 | 说明 |
|---|---:|---:|---|
| 进程启动 → 窗口出现 | 525 ms | 774 ms | `MainWindowHandle != 0` |
| 进程启动 → `setup_enter` | 915 ms | 1264 ms | Tauri runtime + WebView2 初始化完成、进入 `setup` |
| `setup_enter` → 核心可用 | **51 ms** | 65 ms | 包含 `AppState::open` |
| 进程启动 → 核心可用 | **966 ms** | 1330 ms | `hbot-store opened` |
| 进程启动 → IPC 首次可用 | 1049 ms | 1425 ms | web 侧第一次 `dispatch` |
| 其中 `AppState::open` | 33 ms | 57 ms | 建库 + 迁移 + 3 条连接 |
| 其中 store 迁移 | 5.6 ms | 24 ms | 已有数据，非首次建库 |

**结论：核心只占 33 ms，915 ms 花在 Tauri/WebView2 初始化和进入 `setup` 之前。** 800 ms 这个数字当初是按「核心可用」定的，但桌面形态的「核心可用」必然排在 WebView2 初始化之后；继续优化 SQLite/迁移不会让这个数字明显下降。

处置（按顺序，不许静默放宽）：

1. **把预算拆成两个口径**：headless `hbot serve` 到 ready < 800 ms（当前 878 ms，需继续查首次迁移与进程初始化）；桌面到 IPC 首次可用 < 1500 ms（当前 1049 ms，达标）。两句话都写进性能表，不再用一个数字同时代表两种形态。
2. **桌面端再压 800 ms 之前先证明能压**：需要把「核心启动」从 Tauri `setup` 里挪到 `setup` 之前（例如在 `run()` 开头先 `spawn` 核心初始化，窗口与核心并行）并实测；只改 SQLite 无效。
3. 若第 2 步做完仍 > 1000 ms，就把桌面目标正式改为 1000 ms，并在此处写明「因为 WebView2 初始化不可控」，同时保留「到可编辑」单独计量。

「到可编辑」没有客观信号（Monaco ready 不可外部观测），1049 ms 是「到 IPC 首次可用」的下界，不作为验收线。首次建库/首次装 WebView2 的路径单独测量，不与已有数据混在一起。

---

## 5. 代价（必须承认）

1. **Rust 开发速度与编译时间**：比 TS 慢；Agent 迭代反馈变长。缓解：workspace 拆 crate、`cargo check` 增量、CI 缓存、只对改动 crate 跑测试。
2. **QuickJS 的边界**：ES 支持与 async 集成不如 V8；只支持项目内相对 import，无 npm 依赖；模块加载器需自写（见 §12）。若两周内 async/await 不稳，换 `deno_core`。
3. **没有 npm 插件生态**：生成代码只能用宿主 API（`reply/send/kv/http/schedule/log`）。这是安全优势，也是能力限制。
4. **三平台命令隔离要自己写**：Windows 受限令牌 + Job Object，macOS `sandbox-exec`/`bwrap`（若可用）；没有适配器就拒绝执行。
5. **Tauri WebView 仍占内存大头**：如果用户要求总内存 < 100 MB，必须换原生 UI，届时编辑器体验要重新设计。
6. **生态与招聘**：Rust + Tauri 的桌面经验比 Electron 少；文档与示例要更完整。

---

## 6. 可逆性

- **壳可换**：UI 只依赖 `Api` 抽象（Tauri 同进程命令 / `hbot serve` 的 WS 两种实现）与 `app_info`；换回 Electron 壳或纯浏览器开发模式都不动核心。
- **执行器可换**：`ScriptRuntime` trait 隔离 QuickJS；可换 `deno_core` 或外部进程。
- **生成目标可扩**：未来加 Python 用子进程执行，不动核心协议。
- **存储可换**：repository trait 隔离 SQLite；但 SQL 方言会泄漏，换库要重写迁移。
- **回退 0001 的代价**：Agent 内存问题会回来；所以回退是“产品妥协”，不是技术等价。

---

## 7. Spike（7 天，先验证再转 Accepted）

| # | 验证 | 通过标准 | 状态 |
|---|---|---|---|
| S0 | 内存基线 | 旧 Electron 项目、Tauri 空壳、Rust 核心各测 **private commit 求和**；记录测量方法与机器 | **核心 + 桌面已测**（`docs/spikes/S0-memory.md`）；旧 Electron 对比留 M6 影子运行 |
| S1 | SWC + QuickJS + host API（含多文件） | 多文件相对 import 能加载；TS 去类型后用 `await` 调 host 异步函数；超时能中断；内存上限能触发；错误栈能映射回 TS；插桩 `__hbot_trace` 可暂停 | **已测**（`docs/spikes/S1-quickjs.md`）：编译 3.1 ms/40.9 ms（1/50 文件）、handler 34–72 µs、isolate 0.30 MB；插桩暂停在 S1 样板里验过 |
| S2 | Tauri 链接核心 + IDE 骨架 | 同进程调用命令、推事件；Monaco 多文件打开/保存/诊断；UI 首屏 < 1 s；无 sidecar | **部分**（`docs/spikes/S2-tauri.md`）：同进程命令/事件、真迁移、整机内存与冷启动已测；Monaco 多文件与首屏未量化 |
| S3 | SQLite + keyring | 迁移可重放；并发写不 `SQLITE_BUSY`；三平台密钥读写或明确拒绝 | 迁移与单 writer 已落地；keyring 未接 |
| S4 | OneBot 连接与队列 | 每会话有序；断线重连不丢；重复 `message_id` 只处理一次；队列满背压 | 队列/重放/退避已落地并测；**真实网关未连** |
| S5 | 命令隔离 | Windows 受限令牌 / POSIX `bwrap` 至少一个平台真跑；无适配器时拒绝 | 进行中（`crates/hbot-command` + ADR-0003） |
| S6 | Linux headless | 无 WebView/Node 依赖；systemd 启停；WS RPC 可连 | 未开始 |

**S1 或 S2 失败**：先修方案；S1 无法在两周内稳定 → `deno_core`；S2 失败 → 评估原生 UI 或 Electron 壳。
**S5 失败**：不是技术栈失败，是能力范围决定——对应平台禁用命令工具，UI 明示。

---

## 8. 重估信号

出现任一条就重新开 ADR：

- 桌面整机 private commit > 320 MB（实测基线 206 MB，留 55%），或核心 > 100 MB（实测 3.4 MB）。**用 WorkingSet 判会永远触发**，见 §4 口径说明。
- WebView2 子进程数或基线内存随版本上涨（同一份实测重复做一次就涨 >20%）：说明整机预算不在我们控制下，需要重新评估壳。
- QuickJS 的 async/await、错误栈、内存限制任一在两周内无法稳定。
- Rust 交付速度低于计划 50%，且连续两个里程碑延期。
- 生成代码必须多文件或第三方依赖才能写真实 bot（当前已支持多文件相对 import；若仍被迫引入 npm 生态，则重估）。
- macOS 公证/签名或 Linux WebKitGTK 分发阻塞超过 2 周。
- 单 bot 消息处理 p95 > 50 ms（本地），且不是生成代码本身的问题。

---

## 9. 对既有文档的影响

- `0001-tech-stack.md` 标记为 **Superseded**，保留作为决策历史。
- `02-architecture.md`、`03-directory.md`、`04-implementation-plan.md`、`05-quality-gates.md`、根 `README.md`、`AGENTS.md` 全部按本 ADR 重写。
- `00-assessment.md` 的旧项目 P0/P1 **仍然有效**：它们是“新运行时必须满足的不变量”的证据；与节点/流程强相关的条目降为参考，不要求 1:1 复刻。
- 旧项目的 6,395 个测试不再作为“逐条迁移”的对象，只抽取**行为场景**做 parity（消息有序、重连不丢、重复投递、回复上下文、崩溃隔离、密钥脱敏、命令拒绝等）。

---

## 10. 复核记录（2026-11-16）

**结论：方向可用，但仍是 Proposed。** 这次复核查了官方文档，纠正了 3 个会直接影响实现的假设，补了 4 个设计缺口。没有改技术方向。

### 10.1 已核实（有出处）

| 事实 | 出处 | 对设计的影响 |
|---|---|---|
| `rquickjs::Runtime` 默认 **`!Send`**；只有启用 `parallel` feature 才 `Send + Sync` | <https://docs.rs/rquickjs/latest/rquickjs/struct.Runtime.html> | 不要启用 `parallel`；每个 isolate 固定一个 OS 线程 + `LocalSet`，跨线程只传 handle。不能把 `Runtime`/`Context` 放进 Tauri `State` |
| `Runtime::set_memory_limit` 在启用 `rust-alloc` / 自定义 allocator 时是 **noop** | 同上 | 不启用这些 feature；用默认 allocator + `memory_usage()` 采样兜底 |
| `set_interrupt_handler` 存在，回调返回 `true` 会抛不可捕获异常 | 同上 | 超时用引擎中断，不用“外面杀线程”当唯一手段 |
| `execute_pending_job` / `is_job_pending` 存在 | 同上 | 异步宿主调用需要显式驱动 job 队列 |
| `swc_ecma_transforms_typescript::strip` 存在，但需要 parser/AST/codegen/resolver/fixer 一整串 crate | <https://docs.rs/swc_ecma_transforms_typescript/latest/swc_ecma_transforms_typescript/> | 二进制与冷编译会变大；已把“<30 MB”改成“待实测，初始 60 MB”，并写入“退 oxc / 退 JS+JSDoc”的顺序 |

### 10.2 必须补的设计缺口（已写入 02）

1. **QuickJS 线程模型**：`!Send` + `AsyncContext` + `LocalSet`，每 isolate 一个线程；全局线程上限，不能每 bot 无限起。
2. **SQLite 阻塞模型**：`rusqlite` 同步；写走专用 `std::thread`，读走 `spawn_blocking`，禁止在 async 任务里直接查。
3. **提示注入**：平台消息/日志/网页都是不可信输入；外部内容不得改变权限、沙箱档、审批策略；命令/项目外写/联网一律审批。
4. **命令工具平台矩阵**：macOS `sandbox-exec` 已弃用、Linux `bwrap` 不一定有；不可用就**从工具表移除**，不留“能点但会失败”的按钮。

### 10.3 仍未验证（S0–S3 必须给数字）

- QuickJS 的 `async/await` 与宿主异步调用在真实 bot 代码里的稳定性；错误栈能否映射回 TS 行号。
- ~~SWC 链的实际二进制体积与冷编译时间~~ **已测（S1/S0）**：`hbot.exe` 16.2 MB、`hbot-desktop.exe` 33.0 MB，全量首次 release 构建 7m46s；**没有超出 60 MB 上限，不需要退到 JS+JSDoc**。冷编译 7m46s 贴着 8 min 线，后续靠 sccache/lld 改善。
- ~~Tauri/WebView2 在 Windows 的真实空闲 RSS；220 MB 总目标是否可达~~ **已测（S0/S2）**：见 §4.1。结论是 220 MB 在 WS 口径下不可达、在 private 口径下 206 MB 可达；口径已定。
- `ts-rs` 对内部标签枚举（`#[serde(tag = "type")]`）的生成质量；复杂 Command 是否要手写。**（已落地 40 个生成文件，编译与 `gen --check` 均通过；UI 侧尚未全部改用生成类型）**
- OneBot 11 事件缺少稳定 `message_id` 时的去重键；重连是否重放同一事件。**（已实现整条事件规范化 JSON 的 FNV 兜底键；真实网关是否重放仍未实测）**
- macOS 命令隔离是否可用；不可用时产品能力如何表述。

### 10.4 判定

- **能用**：技术栈没有“根本跑不通”的项；最坏情况是 SWC 退 JS、macOS 禁命令、内存目标上调，都不动产品核心。
- **不完善**：上面 4 个缺口原稿没写透，已补进 02；`04` 的工期偏乐观，已按复核调整。
- **S0/S1/S2 已给数字**（见 §4.1）：核心侧内存、每 bot、每 isolate、二进制体积全部达标且余量大；整机内存达标（private 口径）；**冷启动 878 ms 未达标**，处置写在 §4.2。
- 仍不转 Accepted：`async/await` 在真实 bot 代码里的稳定性、真实 OneBot 网关的重放行为、macOS 命令隔离三项还没有实测。**转 Accepted 的条件收窄到这三项。**

---

## 11. 为什么是“最合适”，不是“最强”

没有“全套最强”的技术栈。性能至少有 6 个轴，每个轴的最强者不同；把每个轴的最强者拼在一起，通常得到一个开发不动、生态缺失、UI 重写的东西。

| 轴 | 当前选择 | 这一轴的“最强” | 为什么不用最强 |
|---|---|---|---|
| 核心运行时（内存/启动/安全） | Rust | C++/Zig 可以更小更快 | 安全、生态、开发速度；Rust 已接近上限 |
| 桌面整机内存 | Tauri 2 + 系统 WebView | 原生 UI（egui/Slint/iced） | Monaco 级编辑器 + 聊天/日志 UI 要重写；**WebView 才是内存大头**，换原生才真正降到 <150 MB |
| UI 渲染 | React 19 | Solid/Svelte/原生 | 生态、Monaco 集成、Agent 熟悉度；本产品 UI 规模差距不大 |
| JS 执行 | QuickJS | V8（`deno_core`）/JavaScriptCore | 内存 + 二进制；bot handler 是 I/O 型，不是 CPU 型 |
| TS 编译 | SWC | esbuild / oxc | esbuild 要子进程；oxc 覆盖较新；SWC 成熟。超预算再换 |
| 开发迭代速度 | Rust | TypeScript / Go | 为了无 Node、单二进制、沙箱；用 sccache/nextest 缓解 |
| Agent 生态 | 自研 Rust 循环 | Python / TS 的 Agent 框架 | 运行时又回到 Node/Python；Rust 生态弱，但可控、无 GC |
| 命令隔离 | OS 原语自研 | 容器（Docker/nsjail） | 桌面不适合；这是**最弱的一环**，所以不可用就禁用工具 |

**关键判断：**

1. **桌面内存的大头是 WebView，不是 Node。** 选 Rust 省的是核心侧（大约几十 MB），不是整机。**S0 实测证实了这条**：整机 206 MB private 里 Rust 主进程只占 13.9 MB，其余全在 WebView2 子进程，而且 t=1 s 就到 364 MB WS（不是 Monaco 懒加载造成的）。所以：目标是 260 MB 时，Rust 核心 + 一个窗口是合理的且余量 20%；**目标若压到 <150 MB，Tauri 也达不到，必须换原生 UI**。
2. **bot 吞吐是 I/O 型。** 瓶颈在平台网络、每会话串行（正确性要求）、SQLite 单写；换 Rust 不会让 LLM 更快，也不会让平台 API 更快。
3. **Agent 执行是 LLM 延迟主导。** Rust 只让文件/命令/编译这些本地工具快；模型本身的速度与语言无关。
4. **选 Rust 的真正理由**：产品运行时无 Node、单二进制分发、Linux 后台同核、沙箱可控、崩溃面小。**不是“Rust 一定快”。**

**什么时候该换：**

- WebView 空闲 >180 MB，且产品目标 <150 MB → 原生 UI。
- 生成代码出现 CPU 密集场景（大文本解析、图片处理）→ `deno_core`。
- 二进制 >60 MB 或冷编译 >8 分钟 → oxc / JS+JSDoc。
- 开发速度连续两个里程碑低于计划 50% → 重新评估 Go/TS 核心（代价是重写，不轻易做）。

**结论**：这套是“在无 Node + 三平台 + 桌面/后台同核 + Agent 写代码沙箱 + 低内存”这组约束下的**最合适**，不是每个轴的**最强**。如果用户的第一优先级变成“绝对性能”，先换 UI 与 JS 引擎，而不是换语言。

---

## 12. 追加：产品形态改为 IDE（2026-11-16）

### 12.1 决策

用户明确：**这是“Agent 实现任何 bot”的项目，代码优先，并且要做成 IDE**。据此追加三条，不改 §1 的语言/运行时决策：

1. **IDE 是一等产品**，不是“聊天框 + 文件树”。多文件项目、编辑器、搜索、问题、Diff 审查、运行/调试、日志/回放都在范围内（[06-ide.md](../06-ide.md)）。
2. **多文件取代单文件**。生成代码是 `src/**` 多文件，只允许相对 import；QuickJS 模块加载器按需解析；每文件独立 SWC 去类型 + source map。仍然不支持 npm。
3. **“任何 bot”靠适配器 SDK，不靠核心分支**。核心只认平台无关的 bot 模型与能力矩阵；OneBot 11 与通用 Webhook 是 MVP，其他平台后续加，不改 supervisor/队列。

### 12.2 调试：为什么不上 V8

IDE 通常意味着断点调试；QuickJS 没有可用的断点接口，`deno_core` 能提供 V8 Inspector，但会把每 bot 内存从 MB 推到几十 MB，直接违背“多 bot 常驻 + 低内存”。

因此选择**源码级插桩调试**：debug build 在每条语句前插入 `__hbot_trace(file, line, scope)`；命中断点时阻塞 worker，UI 显示文件/行/局部变量，支持 step over/into/out；release build 不含插桩。代价与限制已写入 02 §6.5、06 §6.2：不支持 watch 任意表达式/原生 async 栈；debug 允许 2–5× 变慢。

**这不是“最强调试”**，但它保住了内存模型；如果将来 IDE 的调试体验成为核心卖点且用户接受更高内存，再按 §12.4 重估。

### 12.3 代价

- **多一个 crate**：`hbot-workspace`（文件、监听、搜索、符号/源码映射、编辑事务），依赖只指向 core。
- **IDE 工作量进主线**：M1 从 3 周调到 4 周，M3 从 2 周调到 3 周；总工期 18–22 周（含缓冲）。
- **多文件模块加载器是新风险**：S1 必须验证 QuickJS module loader；失败则退回“编译期打包成单文件”（仍是多文件源码，只是产物合一）。
- **插桩是自研调试器**：变量快照、条件断点、步进语义都要测试；MVP 明确不支持的能力写进 UI，不做“看起来能调试”的假按钮。

### 12.4 重估信号（追加）

- 插桩调试在真实 bot 上不可用（作用域覆盖太差/开销 >10×），且用户把断点列为 P0 → 评估 `deno_core` 或“调试专用 V8 进程”。
- Monaco 在 10k 文件/20 标签下按键 p95 >50ms，优化后仍不达标 → 评估原生编辑器或换 UI 框架。
- 多文件模块加载器在 S1 两周内不稳 → 退回编译期打包，不换语言。
- “任何 bot”出现必须依赖 npm SDK 的平台，且 HTTP/raw 适配无法覆盖 → 重估插件/依赖模型（可能是产品范围问题，不是语言问题）。

**状态**：仍为 **Proposed**；S0–S4 数字出来后再决定是否 Accepted。IDE 形态不改变“无 Node + Rust 核心 + QuickJS 沙箱”的主决策。