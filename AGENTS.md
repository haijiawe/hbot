# AGENTS.md

给进入这个仓库的 Agent / 新人。**只写“在哪改、什么不能做、踩过什么坑”**，不重复 README 的产品介绍。

当前阶段：**M0–M2 主体 + M3/M4/M5 绝大部分已落地，C6 设备能力窄版完成**（1504 个 Rust 测试 / web 400 用例 / 六道闸门全绿）。最新位置与剩余清单见 [`docs/HANDOFF.md`](docs/HANDOFF.md)（它是快照；冲突以本文件与 `docs/04-implementation-plan.md` 为准）。

---

## 0. 三十秒硬规则

1. **产品运行时不允许 Node**：不 `spawn node`、不加 `node_modules` 运行时依赖、不把 Agent 放回 V8。
2. **不做节点/流程引擎**：没有画布、没有触发器解释器、没有 `flows` 表。
3. **只有一个 Agent 循环**：`crates/hbot-agent/src/loop.rs`；不要为子代理/工作流再写第二套骨架。
4. **只有一份契约**：`crates/hbot-rpc/src/dto/`（`command.rs`/`event.rs`/`common.rs`）→ `ts-rs` 生成；不手写 TS DTO、不复制常量。
5. **先落库再处理**：入站消息先写 `inbound`，再入队；不允许“内存里先跑，失败再说”。
6. **每会话串行**：同一 `(bot_id, conversation_id)` 不得并发执行；不同会话可并行。
7. **审批按完整动作**：规则键 `(tool, 规范化参数/路径, sandbox_mode)`；禁止按大类放行。
8. **不静默吞错**：每个错误要么恢复、要么上报、要么显式降级并记录；空 catch 一律红。
9. **代码是唯一真源**：多文件项目、相对 import；没有隐藏的 flow.json / 数据库里的逻辑。禁止裸包名与 npm。
10. **Agent 写入必须走 Diff**：逐块接受/拒绝；用户未保存缓冲区冲突时拒绝自动应用；禁止静默覆盖。
11. **IDE 只有三个 store**：`workspace` / `agent` / `run`；禁止一个 store 直接改另一个，跨域走命令或事件。

---

## 1. 先读什么

| 顺序 | 文件 | 目的 |
|---|---|---|
| 0 | `docs/HANDOFF.md` | **接手时先看这一份**：决策权在 lead、工作方式、六道闸门现状、已完成/剩余、外部依赖。它是快照，冲突以本文件与 `04` 为准 |
| 1 | 本文件 | 规则与地图 |
| 2 | `docs/03-directory.md` | 改动落点、命名、依赖方向 |
| 3 | `docs/02-architecture.md` | 消息链路、Agent、沙箱、存储、安全 |
| 4 | `docs/06-ide.md` | IDE 布局、编辑器、Agent Diff、运行/调试 |
| 5 | `docs/07-bot-runtime-blueprint.md` | **能力面**：6 组能力、四类 bot 缺什么、改能力时读这个 |
| 6 | `docs/08-multi-host.md` | 桌面/无头/手机三宿主，手机端限制（尤其 iOS 后台） |
| 7 | `docs/09-device-capability.md` | 改设备能力（C6）时读这个：闸只有一份、撤销要即时、审计不记内容 |
| 8 | `docs/05-quality-gates.md` | 命令与预算 |
| 9 | `docs/adr/0002-agent-in-rust.md` + `0004-one-core-three-hosts.md` + `0005-capability-ceiling-and-device-grant.md` + `0006-execution-backend.md` | 为什么是 Rust、为什么无 Node、为什么三宿主、能力给不给、跑在哪 |
| 10 | `docs/00-assessment.md` | 旧项目的坑，作为“不变量”证据 |

不要先读 `_research/`：那是旧项目的原始体检报告，是证据不是规范。

---

## 2. 目录地图

| 路径 | 放什么 | 不能放什么 |
|---|---|---|
| `crates/hbot-core` | 实体、端口 trait、错误、ID | 任何 IO、任何 workspace 依赖 |
| `crates/hbot-store` | SQLite、迁移、repository | 业务规则、Agent 逻辑 |
| `crates/hbot-agent` | 循环、上下文、工具、审批、provider | Bot 生命周期、UI DTO |
| `crates/hbot-code` | manifest、SWC 编译、QuickJS、宿主 API | 平台协议、Agent 循环 |
| `crates/hbot-bot` | supervisor、会话队列、outbox、去重 | 具体平台协议 |
| `crates/hbot-workspace` | 项目文件、监听、搜索、符号/源码映射、编辑事务 | UI、Agent 决策 |
| `crates/hbot-adapters` | OneBot/Webhook/Telegram…、keyring、HTTP、fs | 业务编排 |
| `crates/hbot-rpc` | Command/Event、dispatch、WS 服务 | 业务实现 |
| `crates/hbot-cli` | `hbot` 二进制：serve/check/run | 库逻辑 |
| `crates/hbot-desktop` | Tauri 命令、窗口、托盘、更新 | 业务规则 |
| `crates/xtask` | 代码生成、依赖检查、基准 | 运行时逻辑 |
| `apps/web/src/features/*` | explorer/editor/search/problems/agent/run/debug/logs/settings | 跨 feature 内部 import |
| `apps/web/src/lib/layout.ts` | 布局唯一真源 | 组件写死宽度 |
| `apps/web/src/lib/keybindings.ts` | 快捷键唯一真源 | 组件自己绑快捷键 |
| `apps/web/src/stores` | workspace / agent / run 三个 store | store 之间直接 setter |
| `apps/web/src/generated` | Rust 生成的 TS | **手改** |
| `tests/integration` | 真 SQLite + mock 平台 | 真实账号 |
| `tests/parity` | 旧项目行为场景 | 逐条搬旧测试 |
| `docs/` | 设计、ADR、体检 | 运行日志、临时文件 |

依赖方向与“改 X 去哪”见 `docs/03-directory.md` §2、§8。

---

## 3. 命令

```bash
./scripts/check.sh              # fmt + clippy + deny + deps + gen check + nextest + UI
./scripts/check.sh --full       # 再加集成/E2E/打包
cargo xtask gen                 # 重新生成 TS 契约与 hbot.d.ts（生成物要入库）
cargo test -p hbot-agent        # 单 crate
cargo clippy -p hbot-code -- -D warnings
pnpm -C apps/web check          # tsc --noEmit + biome
pnpm -C apps/web test
```

- 不要用 `npm`；UI 包管理是 pnpm。
- 不要在核心 crate 里加 Node 工具链。
- 生成物有 diff 就提交，别只改 Rust 不改 TS。

---

## 4. 四条不变量（改之前先确认没破坏）

### 4.1 消息

```text
adapter → normalize → INSERT inbound (UNIQUE bot_id, external_id)
        → per-conversation bounded queue → isolate handler
        → outbound row → sender retry → mark processed
```

- 队列有界；满时暂停读取，不丢。
- **两层去重，别混成一句话**：
  1. **入库层（exactly-once）**：`UNIQUE(bot_id, external_id)`，同一条平台消息只会插一行，重复投递被 `insert_inbound_if_new` 挡掉。
  2. **执行层（at-least-once）**：崩溃/被杀发生在「handler 执行完」与「`mark_inbound_processed`」之间时，重启后的补偿重放会把**同一行**再执行一次。所以 `ctx.messageId` 是幂等键，**不是**「有副作用的操作一定只发生一次」的保证；要在外部产生副作用的代码自己按 `messageId` 判重。
  说「重复投递只执行一次」时，指的必须是第 1 层；说第 2 层就必须写 at-least-once。
- 被动回复必须绑定 `reply_to`；主动消息不得冒用旧事件。

### 4.2 Agent

- 工具顺序：参数 → 存在 → 沙箱档 → 路径 realpath → 权限 → 审批 → 执行 → 审计。
- 所有命令走 `CommandRunner`；没有“后台特批”。
- 取消要贯穿 provider/工具/HTTP/命令；取消后不再发新请求。
- 预算：步数、token、时长、金额；超限停，不静默续。

### 4.3 契约

- `Command`/`Event` 只在 `hbot-rpc/src/dto/` 定义（`command.rs`/`event.rs`），别在 `common.rs` 或别处加第二个枚举。
- 加字段要兼容旧 UI（serde default / Option）；删字段走版本化。
- `apps/web/src/generated/` 由 `xtask gen` 产出，review 但不要手改。

### 4.4 沙箱

- 生成代码没有 `fs/net/process/require/import`；只有 `hbot.*` 白名单。
- 每 isolate 有内存/时间/调用上限；超限中断，进程不死。
- 命令隔离建不出来 → 拒绝执行；**不回退名字白名单**。
- 密钥只经 keyring/环境；日志、错误、审计统一脱敏。

---

## 5. Rust 约定

- edition 2024，`rustfmt` 默认；`clippy -D warnings`。
- 库代码不用 `unwrap()`/`expect()`（测试、`main` 启动可例外）；错误用 `thiserror`，边界转 `ApiError`。
- `unsafe` 必须有 `// SAFETY:` 说明不变量；核心 crate 默认禁止。
- 公开 API 不暴露 `anyhow::Error`；内部可以用。
- 异步：`tokio`；阻塞/CPU（SWC、QuickJS）放专用线程，不堵 reactor。
- 通道一律有界；无界通道视为 bug。
- 时间、随机、UUID 通过 trait 注入，便于测试。
- 日志用 `tracing`，字段化；不要 `println!`。

---

## 6. UI 约定

- 服务端状态用 TanStack Query；Zustand 只放纯 UI 状态。**已定三个 store：**`workspaceStore`（文件/标签/脏/布局）、`agentStore`（消息/Diff/审批）、`runStore`（bot 状态/日志）。三者只经 `lib/ipc` 的 `dispatch`/事件协作，**禁止互相 import 或直接改对方状态**。
- 只经 `lib/ipc.ts` 的 `dispatch`；不直接 `fetch`/WebSocket。
- Monaco、图表等重依赖动态 `import`；首屏不静态引入（当前入口 258 kB / gzip 81 kB，Monaco 走 lazy chunk）。
- **文件命名 kebab-case**（`editor-surface.tsx`、`workspace-store.ts`），组件导出用 PascalCase（`EditorSurface`）；`App.tsx` 例外保留。hook 用 `use-*.ts`。
- 不用 `any`、`@ts-ignore`；确有需要写 `unknown` + 收窄。
- 每个 feature 有错误边界与空状态；不把异常抛到白屏。
- 布局数值只从 `src/lib/layout.ts` 取，快捷键只从 `src/lib/keybindings.ts` 取；组件里不写死 px 与键名。

---

## 7. 已踩过的坑（来自旧项目，别再踩）

1. **启动/重连窗口丢消息**：旧项目先连平台、后建订阅（`botManager.ts:250,256`），`eventBus.ts:806-827` 只遍历现存客户端。新项目必须先建 supervisor/队列，再 connect。
2. **本地总线开成 socket**：旧 `eventBus.ts:684-687` 连接即加入，无鉴权，任意本机进程可冒充机器人。新项目内部总线**进程内**；对外只有 127.0.0.1 + token 的 RPC。
3. **同会话并发**：旧 `FlowRuntime.ts:401` `void dispatch`，同会话两条消息并行、回复乱序。新项目每会话串行是硬约束。
4. **等待表只在内存**：旧 `FlowInterpreter.ts:437-440` 重启即丢、超时靠下一条事件。新项目所有等待/定时落库，调度器主动结算。
5. **审批 always 按大类放行**：旧 `aiAgentCoreApproval.ts:151-184` 对 a.txt 点“别再问”，同轮任意路径都放行。新项目规则键必须含规范化参数。
6. **迁移先覆盖后校验**：旧 `flow.ts:222-232` 备份失败被空 catch 吞掉，覆盖后才 schema 校验。新项目顺序：读 → 校验 → 备份成功 → tmp → fsync → rename。
7. **命令沙箱回退白名单**：旧非 Windows 直接退化名字白名单（`confine.ts:116`、`sandbox.ts:66`）。新项目建不出隔离就拒绝。
8. **后台命令绕开沙箱**：旧 `job_start` 只查写档，`spawn(shell:true)`（`registry.ts:863-887`、`jobs.ts:184-188`）。新项目所有命令同一入口。
9. **`node:vm` 当安全边界**：旧 `sandbox.ts:30-34` 自己承认不是。新项目用 QuickJS isolate，且限制内存/时间。
10. **半成功批量写**：旧 `persistentStore.ts:622-672` 逐键 catch，全部报成功。新项目单事务，全有或全无。
11. **契约三处手写**：旧 preload 1,421 行 / 类型 2,188 行 / 308 invoke，新增改 4–5 处。新项目 Rust 单一真源 + 生成。
12. **首屏静态加载巨型目录**：旧 `nodeCatalog` 把 6,072 行文案拉进首屏。新项目动态 import + 预算。
13. **按行数当复杂度**：旧项目已改为“函数能力分类”。新项目同样看职责，不搞行数 KPI。
14. **Windows 原子替换 EPERM**：目标被占用时 rename 会失败。写入要重试 + 串行化；不要当成“写成功”。
15. **密钥静默明文**：旧 `safeStorage` 不可用时直接写明文。新项目拒绝或显式确认 + 审计。
16. **闸门本地与 CI 语义不同**：旧 `format` 闸门本地只查 `git diff --cached`，不改暂存区等于没查。新 `scripts/check.sh` 必须对全树判定，本地与 CI 同一条命令。
17. **测试配置一收窄就假绿**：旧项目曾把 vitest include 改窄，364 个测试文件静默不跑。新项目的 nextest/Playwright 配置要有“期望测试数对账”，数量突变即红。
18. **测试断言源码文本**：旧仓库 88 个测试读源码字符串，重命名会以离奇断言失败出现。新项目用类型/契约/行为；结构约束交给依赖图与 `cargo`。

### 7.1 新仓库自己已经踩过的坑（2026-11-16，都是实测）

19. **`Drop` 里的无界 `join()` 会把宿主挂死**。`crates/hbot-code/src/runtime/isolate.rs:235`：sandbox 的 worker 线程在 bootstrap 失败后卡在 `recv`，发送端又在 `Drop` 内部，通道永不 disconnect，`join()` 永不返回 → 进程 0% CPU 假死、日志无错。规则：**任何 `Drop` 里的等待必须有超时**（`recv_timeout(handler_timeout + 1s)`），超时就 `error!` 并 detach，绝不 join 到底；`tests/s1_isolate_lifecycle.rs` 是这条的回归。
20. **唤醒只在“全局归零”时通知会造成假死**。`crates/hbot-bot/src/queue.rs:492-494` 只在 `total == 0` 时 `notify_waiters()`，而 `enqueue` 等它：会话 A 满 → 读循环阻塞 → A 腾出容量但别的会话仍有在途 → A 的等待者永不醒，表现是「bot 突然不回消息、日志干净」。规则：**每处理完一条就按会话通知**，且 `enable()` 必须先于 `try_enqueue` 注册。
21. **按 UUID 主键排序 ≠ FIFO**。`Store::list_pending_outbound` 曾是 `ORDER BY id`，恢复时出站顺序被打乱。规则：任何“待处理队列”的读取都要 `ORDER BY created_at, rowid`；UUID 只当主键，不当顺序。
22. **注释声称“下次启动会重放”，但生产零调用**。`crates/hbot-bot/src/supervisor.rs:677` 说消息已落库会重放，实际 `list_unprocessed_inbound` 一个生产调用点都没有，崩溃窗内的消息永远停在未处理。规则：**「会重放/会恢复」这类声称必须有测试证明**，否则它就是一句谎；恢复路径要写清语义（我们不承诺 exactly-once，承诺 **at-least-once** + 幂等键）。
23. **热路径上禁止 O(项目) 操作**。`sandbox_handler` 曾每条消息重算全项目指纹（全量读盘 + blake3）。规则：每消息路径只允许 O(1) 判断 + 内存缓存；文件变化靠 watcher 脏标志，内容指纹按需重算。
24. **编译失败不能让 bot 停摆**。一个 typo 就 `Err` → inbound 不标记 → 每条消息都失败。规则：**保留 last-good 版本继续服务**，错误进 `RunLog` + UI；只有「从来没有成功过」才真报错。
25. **`tauri-build` 需要 `crates/hbot-desktop/icons/icon.ico`**，否则报 `` required for generating a Windows Resource file during tauri-build ``。图标是 PNG payload 的 ICO（6 字节头 + 16 字节目录项 + PNG 体），`icons/` 已入库，别删。
26. **`#[tauri::command]` 函数不能是 `pub`**：crate root 会 `E0255 __cmd__dispatch is defined multiple times`。命令写 `async fn`，只在 `run()` 里注册。
27. **PowerShell 的 .NET API 用进程 CWD**，`Set-Location` 不改它（`[System.IO.File]::WriteAllBytes` 会写到启动目录）。脚本一律用工具的 `workdir`，或全程 `Join-Path $PSScriptRoot`。
28. **长命令把输出写进管道会假死**：`cargo run` 输出没人读，进程卡在写、CPU 0%、看起来像死锁。规则：长任务一律 `*> 日志文件` 再读文件，不要靠终端缓冲区。
29. **`CARGO_TARGET_DIR` 必须按 agent 分目录**（`D:\hbot-target\<line>`）：多个 agent 共用同一个 target 会互相等锁，看起来像“卡住”。
30. **契约改了必须同批改三处**：`dto`（Rust）→ `cargo xtask gen`（TS 生成物入库）→ `apps/web` 消费点。`gen --check` 与 web 的 `tsc` 会各自抓到一半，别只修一半就提交。
31. **调试断点属于 bot，不属于 agent 会话**（`crates/hbot-core/src/debug.rs`，2026-11-16 冻结）。同一份源码在两个 bot 的 isolate 里可以是不同版本（热重载时机不同），断点按会话走会串味。`DebugSink::paused` 是**同步阻塞**的，只能在 isolate worker 线程里调用；实现方必须自带超时（建议 5 分钟）并返回 `Continue`，否则挂死宿主。
32. **命令执行 fail-closed**：`hbot-command` 的能力矩阵实测是「Windows/Linux 有进程树 Containment、**没有写隔离**、macOS 全无」。因此 `read_only` 直接拒、`workspace_write` 也拒（`IsolationUnavailable`）、只有 `danger` 能跑。**不许为了"能用"放松这条**——装不上隔离就是拒绝，放宽等于把旧项目"非 Windows 退化成名字白名单"的坑再踩一遍（旧 `sandbox.ts:66,166`）。
33. **parity 测试要在别人的文件静止时跑**。并行改 `hbot-code` 期间跑 `crates/hbot-parity`，红可能是中间态（例如宿主 `log` 收集被改）：先看失败点是不是"断言没走到"而不是"结果不对"，再决定是打回还是复验。
34. **内存一律按 `PrivateMemorySize64` 在进程树上求和**，不要用 `WorkingSet64`：WebView2 的 DLL 被 6–7 个子进程各自映射，WS 会把共享页重复计入（实测整机 WS 382.5 MB vs Private 205.5 MB）。口径写在 `docs/adr/0002-agent-in-rust.md` §4。
35. **档位与命令白名单是配置，不是常量**：`HBOT_SANDBOX` / `HBOT_COMMAND_ALLOW` 或 `<data_dir>/agent.json` 的 `sandbox` / `allowedPrograms`。写错档位字符串是 `-32002`，**不许静默退回默认档**（那会把 `danger` 悄悄降成 `workspace_write`，用户以为命令能跑却一直被拒）。白名单默认空 = 一个程序都不许，且只放具体程序，别把 `cmd`/`sh`/`powershell` 放进去 —— 放解释器等于绕开"命令不经 shell"这条设计（`crates/hbot-rpc/src/app/agent/config.rs`）。
36. **前端构建不产出 `.map`**：`tauri build` 会把整个 `apps/web/dist` 内嵌进安装包，实测 58.16 MiB 里有 44.20 MiB 是 sourcemap（关掉后 13.96 MiB）。dev server 的 sourcemap 在内存里，不受影响。
37. **运行时的编译必须用 `compile_project_debug`**（`crates/hbot-rpc/src/app/runtime/sandbox_handler.rs`）：只有 debug 构建才插了 `__hbot_trace` 探针。退成 `compile_project`（release）不会报错，只会让 `debug_set_breakpoints` **静默永不命中** —— 这类"不报错的失效"比崩溃难查一个数量级。同时"没有断点就不 arm"是必须的：arm 后 probe-dense handler 实测 4.64×（上限 5×，余量 8%）。`hbot check` 与 diagnostics 反而**应该**用 release 编译（只要类型与错误，不要探针）。
38. **`ctx.log` 在 handler 抛错时也要留下**：日志在 `hbot-code` 的 `RawEvent` 里已经收集了，别让它在 `finish()` 的错误分支被丢掉（旧实现就是这样）。用户看到"我的 bot 出错了，但出错前那三行 log 一条都没有"时，几乎无法定位。
39. **数据目录只有一份算法：`hbot_core::paths::data_dir()`**（2026-11-16 修）。此前有三份拷贝：`hbot-rpc::dispatch`、`hbot-cli::serve` 各算一份给 `%APPDATA%\hbot`，桌面壳用 Tauri 的 `app.path().app_data_dir()` 给 `%APPDATA%\com.hbot.app` —— **同一个产品的桌面端和后台端各开一个库**，`hbot replay` 又按另一份找库，用户会看到"我刚跑过的消息在回放里不见了"。任何新入口（新 CLI 子命令、托盘、服务化）都从这里取，**不许再写第四份**。
40. **数据目录不要绑定 Tauri 的 `identifier`**。`app_data_dir()` 的路径由 identifier 派生，改 identifier 就等于静默搬家（用户数据留在旧目录没人再去看）。identifier 只决定安装/签名身份：`0.1.0` 发布前已从 `com.hbot.app` 改成 `com.hbot.desktop`（原值以 `.app` 结尾，bundler 会告警——与 macOS 的 bundle 扩展名撞车），**发布之后不要再改**，否则升级会变成两个互不相识的应用。
41. **Windows PowerShell 5.1 会把 UTF-8 无 BOM 的 `.ps1` 当 GBK 读**，中文注释直接炸成 `Unexpected token '}'`。`scripts/*.ps1` 必须带 BOM（`EF BB BF`）；用 `write`/`edit` 写完要检查首三字节。同理，**写 JSON 给 Rust 读时不要用 `Set-Content -Encoding UTF8`**（PS 5.1 会加 BOM，而 `serde_json` 不接受 BOM）——用 `[System.IO.File]::WriteAllText($p, $json, (New-Object System.Text.UTF8Encoding $false))`。
42. **类型声明与运行时不符，是最难查的一类 bug**。实例：`hbot.d.ts` 声明 `ctx.text: string`，而 `event_json` 里 `MessageCtx::text()` 是 `Option<&str>`，纯图片入站的 `payload` 没有 `text` → JSON 落成 `null` → 用户代码 `ctx.text.length` 抛异常，而 `tsc` 一个字都不报。规则：**任何面向用户代码的类型声明都要有一条"运行时真的给出这个类型"的测试**（构造缺字段的入站，断言不抛），否则声明就是谎话。
43. **`vitest.config.ts` 的 `include` 只许扩，不许收**。这次为了 `.tsx` 组件测试把 `['src/**/*.test.ts']` 扩成 `['src/**/*.test.ts','src/**/*.test.tsx']`；反过来收窄会让测试文件静默不跑（旧项目踩过 364 个文件的坑，见坑 17）。组件测试靠文件首行 `// @vitest-environment jsdom` 逐个挂，**不要**把全局 `environment` 改成 jsdom：38 个 store 测试是纯逻辑，拉进 jsdom 只会更慢。
44. **并行开发请按 crate 划边界，不要靠"小心"**。共享工作树上，别人把 `crates/hbot-command` 改到不可编译时，`cargo clippy -p <你的 crate>` 也会红（`-D warnings` 作用整个构建图），你会误判成自己的问题。做法：① 每个单元独占 `CARGO_TARGET_DIR`；② 判断自己的对错用 `-p` 且先看失败点是不是"根本没编译到我的代码"；③ 实在被挡住就 `git archive HEAD` 出干净快照、只覆盖自己的文件验证，验完再回真树复跑。
45. **构建缓存会把磁盘吃干净，而症状看起来像代码坏了**。2026-11-16 实测：8 个并行单元各一个 `CARGO_TARGET_DIR`，`D:\hbot-target` 累积到 **49 GB**，D 盘只剩 4.8 GB，于是出现 `ENOSPC: no space left on device`、`rustc-LLVM ERROR: IO failure`、后台任务莫名退出——两个子代理都把它当成自己的 bug 查了一轮。规则：① 构建缓存放**剩余空间大的盘**（本机 `E:` 有 310 GB，`D:` 只有 380 GB 且常被占满），用 `E:\hbot-target\<单元名>`；② 单元收工就删掉自己的 target 目录，别留着；③ 看到 `ENOSPC`/`IO failure` 先 `Get-PSDrive` 看剩余空间，再怀疑代码。
46. **"按 crate 划写域"仍然会让两个人互相堵住**（2026-11-16 踩到）。`cargo` 的编译单元是**整个 crate**，不是文件：`search-io` 把 `crates/hbot-workspace/src/search.rs` 写成中间态时，另一个单元对 `project.rs`/`files.rs` 的改动**一行都验不了**（`cargo test -p hbot-workspace` 直接编译失败），而两边改的文件根本不重叠。规则：① 同一 crate 里并行，**共享文件必须明确归属一个人**，其余人只能读；② 改到一半就把 crate 弄成编译不过的人，**有义务第一时间广播"已可编译"**，别人据此放行；③ 被挡住的一方**不要空等也不要越界代改**——先做能做的（写文档、设计变异脚本、静态自查），并靠"错误是否全部落在他人的文件里"来判读自己的部分是否已经编译通过。
47. **对外错误文案不许带项目根的绝对路径**（RT-01，2026-11-16 修）。`NotFound` 之类的错误会把 `E:\HBot-Next\projects\demo\src\main.ts` 整条路径拼进 message，而这条 message 会作为**工具输出喂给模型**、也会进日志——用户的目录结构因此泄漏进模型上下文与日志文件。规则：面向模型的错误只给**项目内相对路径**（`hbot-workspace` 统一走 `display_rel` / `Workspace::display_path`，`io_error` 收 root 参数）；操作系统的原始错误（`std::io::Error`）可以保留，但要与相对路径拼接后再外传。新增错误构造点时先问一句"这条 message 会不会被模型看到"。

48. **网络授权判据只有一份：`hbot.json` 的 `permissions.http`**（策略 C，2026-11-16 定，见 `docs/02-architecture.md` §6.6）。判据在 `crates/hbot-code/src/permissions.rs`；适配层（`hbot-adapters`）**不再按地址位置**（回环/私网/云元数据）二次拒绝——`FetchOptions::allow_private` 默认 `false` 保留严格口径，而 `hbot-rpc` 的 `AdaptersHttpTransport::new()` 恒置 `true`，因为上游已经判过。别在适配层"顺手加一条 SSRF 检查"：那会立刻造成"清单写了 `127.0.0.1:11434` 却连不上"的两道判据打架，而且**最终解释权在第一道**，第二道只是让人困惑。要收紧就收紧清单。已验证的边界：`read-only` 档仍在授权之前拒（不建 socket）、`http://2130706433/` 这类歧义数字主机仍拒（解析歧义，非位置）。

49. **Landlock 的 `handled_access_fs = 0` 意思是"什么都不管"，不是"什么都不许"**（2026-11-17）。
   `crates/hbot-command/src/landlock.rs` 第一版把**放行掩码**当成了 handle 掩码，于是 `read_only`
   档变成零规则集 —— 限制层装上了却等于没装，而所有测试都是绿的（因为没人从 Linux 内核那侧验）。
   现在拆成两个函数：`write_permissions()`（要放行的位）与 `handled_write_bits()`（要接管的位，
   恒为全集、按 ABI 版本收窄）。另一个同族坑：`pre_exec` 里装载失败要 `_exit(127)` **并**往 stderr
   写标记，父进程必须**同时**看到 127 与标记才改判 `IsolationUnavailable` —— 只判 exit code 会把
   "命令自己退 127"误判成隔离失败。**这台机器没有 Linux 内核（无 WSL distro），本层一次都没真跑过**，
   所以它现在的证据只到"交叉编译 + 纯逻辑单测"，别在 README 或 ADR 里写成"已验证"。

50. **条件断点在 JS 侧求值，因此它不是沙箱**（2026-11-17）。条件是一段真的 JS 表达式，在 isolate
   里用 `new Function` + `with(scope)` 求值 —— 那里才有 `ctx`、局部变量与模块作用域。后果：
   ① 条件里**能调宿主函数**（`ctx.reply()` 会真发消息、`ctx.device.ocr()` 会真截屏）；
   ② 给名字赋值不生效（scope 是快照），但改对象属性会真生效。规则：条件永远只在
   "探针判为候选命中且非单步" 时求值（否则又会撞 4.64× 的开销上限），坏条件/抛错/引用不存在的
   名字**一律当没命中**、同一 `file:line` 只 warn 一次、绝不打断 handler。UI 要如实提示上面两条，
   不要写"受限求值器"这种让人以为安全的描述。

51. **断点与日志点是两张表，不是给断点加一个 `kind` 字段**（2026-11-17）。`crates/hbot-core/src/debug.rs` 的
    `Breakpoints` 同时有 `files`（会停下）与 `logs`（只打印），各自 `set()` / `set_logs()` **整份替换单个文件**。
    若合成一张带种类的表，"只改条件"和"只改日志点"会互相清掉对方：UI 单独加一个日志点，同文件所有断点消失。
    因此 `hbot-rpc` 的 `plan_breakpoints` 必须按"这次请求里该文件出现了什么"分别构造两路，**只在非空时才调**对应 setter。
    另一个哨兵：`DebugSink::logged` 有默认空实现（让只想处理暂停的实现不必改），所以真实 sink 忘了覆写时
    **日志点在 UI 里什么都不会出现，且没有任何报错** —— 加新的 sink 实现时先搜一遍 `fn logged`。

52. **资源上限必须 fail-closed：装不上就拒绝执行，不是"装不上就放开"**（2026-11-17）。`crates/hbot-command/src/limits.rs`
    与 `containment*.rs` 里，CPU/内存/进程数/IO 四条限额谁的平台原语装不上，就让这次执行返回
    `CommandDenied::{CpuLimit,MemoryLimit,IoLimit,ProcessLimit}`（`HBOT_COMMAND_*_LIMIT`），**绝不静默降级成无限额**。
    下游 `crates/hbot-agent/src/tools/shell.rs` 的 `denied_to_result` 把这四类归到与 `TooLong` 同一类输出（命令真跑了并被杀，
    所以不记 denied 以免污染审批率、也不记 internal 以免误导排障）。`CommandOutput` 多了 `usage: ResourceUsage` 字段，
    加字段会打断所有测试字面量，改 core 公开结构后先 `cargo check --workspace --all-targets` 再报清单。

---

## 8. 新增 X 的步骤

### 新增平台适配器

1. `crates/hbot-adapters/src/platforms/<name>.rs` 实现 `PlatformAdapter`。
2. 在 `registry.rs` 注册；配置 schema 放 `hbot.json` 的平台段。
3. mock 协议 fixture + 集成测试（连接、收、发、重连、去重）。
4. 不改 supervisor/queue；若必须改，先改 trait 并说明。

### 新增 Agent 工具

1. `crates/hbot-agent/src/tools/<name>.rs`：参数 schema（`schemars`）、实现、超时、审计。
2. 注册表登记；`approvals.rs` 归类（read/write/command/network/external）。
3. 拒绝路径测试：无权限、路径逃逸、超时、取消。
4. 更新 `docs/02-architecture.md` §5.2 的表。

### 新增宿主 API

1. `hbot-code/src/runtime/host_api.rs` 实现；参数/返回值 JSON。
2. `dts.rs` 生成类型；跑 `cargo xtask gen`。
3. `manifest.rs` 权限校验（默认拒绝）。
4. 测试：无权限拒绝、超限中断、序列化往返。

### 新增 Command / 表 / 页面

见 `docs/03-directory.md` §8；契约改完必须 `cargo xtask gen`。

---

## 9. 提交前

- [ ] `./scripts/check.sh` 绿。
- [ ] 新行为有测试；bug 有回归测试。
- [ ] 生成物已更新并入库。
- [ ] 没有新增第二套循环/契约/装配。
- [ ] 没有 Node 运行时依赖。
- [ ] 安全边界有拒绝路径测试。
- [ ] 文档/ADR 同步；不留 `TODO` 进库。

提交信息：`type(scope): 说明`，如 `feat(agent): 增加 read_logs 工具`。

---

## 10. 当前未决（别自己拍）

1. QuickJS async 桥接模型（S1 定）。
2. `hbot.json` 权限字段最终形状。
3. `onSchedule` 是否进 MVP。
4. 多项目并发打开策略。
5. 关窗继续跑的默认值。

遇到这些，先看 `docs/04-implementation-plan.md` §10 与 ADR-0002 §7；仍不确定就问，不要默默选一个。

---

## 11. 给 Agent 的交付方式

- 报告用“**改了什么 / 证据 / 没做什么 / 风险**”四段，不写流水账。
- 给文件路径时带行号；给结论时区分“实测 / 读码 / 推测”。
- 大改动先出计划（`update_plan`），再动手；不要边想边改十个文件。
- 失败要说失败：哪一步、什么错误、试过什么、下一步建议。
- 不要为了“让闸门变绿”改测试断言或基准 fixture。
