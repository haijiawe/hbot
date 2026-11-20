# AGENTS.md

给进入这个仓库的 Agent / 新人。**只写“在哪改、什么不能做、踩过什么坑”**，不重复 README 的产品介绍。

当前阶段：**M0–M2 主体 + M3/M4/M5 绝大部分已落地，C6 设备能力窄版完成，M7 第 1–5.5 步（`kv`/`history`/`ai`/`onSchedule`（旧名 `onTimer`）/`jobs`/`session`/`sql`）、`C3.send`/`C3.notify`、`C1.webhook`、`onStart`/`onStop` 与 `C3.file` 全部落地**（2006 个 Rust 测试 / 125 个 target / web 427 用例 / 六道闸门全绿）。最新位置与剩余清单见 [`docs/HANDOFF.md`](docs/HANDOFF.md)（它是快照；冲突以本文件与 `docs/04-implementation-plan.md` 为准）。

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
| 1.5 | [`docs/pitfalls.md`](docs/pitfalls.md) | **131 条坑全表**（编号稳定）：改任何热点模块前先按编号搜一遍 |
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

## 7. 已踩过的坑

全表（131 条）在 **[`docs/pitfalls.md`](docs/pitfalls.md)** —— 那里编号稳定，代码注释与
设计文档里的「坑 66」「坑 69」都指向它。**新增一条写进那个文件**，不要在这里追加。

改代码之前至少扫一眼下面 12 条：它们全是「不报错、只是悄悄失效」那一类，出事最难查。

| # | 一句话判据 |
|---|---|
| 19 | `Drop` 里的等待必须有超时，否则宿主假死（`crates/hbot-code/src/runtime/isolate.rs`） |
| 20 | 队列每处理完一条就按会话通知；只在总数为 0 时通知会假死（`crates/hbot-bot/src/queue.rs`） |
| 21 | 待处理队列一律 `ORDER BY created_at, rowid`，UUID 主键不是顺序 |
| 22 | 「会重放 / 会恢复」必须有测试证明，否则它就是一句谎 |
| 23 | 每消息热路径只准 O(1) 判断；全项目指纹不许出现在消息路径上 |
| 30 | 契约三处同批改：`dto` → `cargo xtask gen` → `apps/web` 消费点 |
| 42 | 面向用户代码的类型声明，必须有「运行时真的给出这个类型」的测试 |
| 53 | `Ctx<'js>` 不许存进 `Async` 闭包（进程退出时 GC 断言崩，`0xc0000409`） |
| 59 | `HBOT_*_NOT_DECLARED`（清单没写）与 `HBOT_*_UNAVAILABLE`（宿主没装配）不许混用 |
| 60 | 清单参与热重载指纹：改 `hbot.json` 会重建 isolate |
| 66 | 给 `SandboxHandler` 加能力要加五处；缺一处就是「看着能用、其实没接上」 |
| 69 | 每个「声明了才有」的能力都要自己第一句读清单，不要复用别人的 tick |
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
5. **四处必须同批改，少一处就红在别处**（`crates/hbot-code/src/dts.rs` 的漂移测试会逐处对上）：
   `host_api.rs` 的 `HOST_BINDING_KEYS` → `install()` 里的 `host.set(...)` → `HOST_MODULE_SOURCE` 的 `export const X` + 末尾 `export default { …, X }`（**测试按逗号切这一行**，不许换行、不许在中间插注释）→ `HOST_DTS` 的 `MessageCtx` 属性与 `declare module 'hbot'` 里的模块级声明。
   新绑定还要同时进 `crates/hbot-code/src/runtime/isolate.rs` 的 `build_event`（`event.set("X", …)`），否则 JS 侧拿不到。
6. **给 `MessageCtx` 加对象属性时，名字必须含空格**（`readonly kv: KvApi;`、`readonly device: DeviceApi;`）。
   两个扫描器（`dts.rs` 的 `message_ctx_properties` 与 `hbot-rpc/tests/event_shape_dts.rs`）靠"名字不是纯 alnum"把它跳过；
   规范化成 `kv: KvApi;` 会被当成**入站字段**，两个 crate 同时红、而报错完全看不出是这个原因。
7. `HOST_DTS` 的负向清单（`hbot_dts_does_not_promise_unimplemented_apis`）里，**宽泛条目（`"kv."`、`"fetch("`）在能力落地时要整条删**，
   只有精确条目（`"history(conversationId"`、`"watch("`）才能留 —— 否则它会在能力实现后继续假装"这个没做"。
8. **新绑定的错误码必须能在 d.ts 里点出来**：`the_*_surface_says_presence_is_not_authorization` 这类测试要求
   `HOST_DTS` 的注释里写明该能力的码（`HBOT_STATE_NOT_DECLARED`、`HBOT_AI_UNAVAILABLE`、`HBOT_TIMER_NO_TARGET`…），
   并写清"存在不等于授权"。这不是装饰：模板与 Agent 都照着 d.ts 写代码。

### 新增清单能力（`hbot.json` 的 `capabilities`）

`crates/hbot-code/src/manifest.rs` 的 `Capabilities` 是 `deny_unknown_fields` 的 —— **不认识的字段会让整个项目
加载失败**（这是故意的：一个"声明了但什么都不做"的布尔会让人以为能力已打开）。加一项能力的顺序：

1. 在 `crates/hbot-core` 定义规则与参数闸（唯一一份），带单测；
2. `Capabilities` 加字段（布尔用 `#[serde(default)]`，计划表这类用 `Vec<…>`）并让 `impl Capabilities` 暴露读取方法；
3. `Manifest::validate` 里调 core 的校验（**别在 manifest 里再写一份规则**）；
4. `hbot-code` 的槽位（`AiSlot`/`StateSlot`/`TimerSlot`/`JobSlot` 模式：`Mutex` 内的可替换端口 + `set_*`/`is_*_assembled`）；
5. `hbot-rpc` 侧读清单决定装不装（`*Source` trait + `worker()` 里调 `set_*`，见坑 66）；
6. 端到端测试：`NOT_DECLARED` / `UNAVAILABLE` / 参数非法 / 跨 bot 隔离 / 跨重启存活 各一条。

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
6. **`ctx.ai` 的形态**：现在只支持"一次请求一段文本"（无多轮历史、无流式、无 tool calling）；
   要不要做 `ctx.ai.chat(messages)` / 流式 / 结构化输出，以及每 bot 独立模型设置（现在只有宿主级一份
   `agent.json`），都还没定。
7. **`ctx.ai` 的成本可见性**：`promptTokens`/`completionTokens` 只回到 handler，宿主不落库、UI 不显示，
   所以用户看不到一个项目累计花了多少。要不要进 run log / 成本面板另说。

遇到这些，先看 `docs/04-implementation-plan.md` §10 与 ADR-0002 §7；仍不确定就问，不要默默选一个。

---

## 11. 给 Agent 的交付方式

- 报告用“**改了什么 / 证据 / 没做什么 / 风险**”四段，不写流水账。
- 给文件路径时带行号；给结论时区分“实测 / 读码 / 推测”。
- 大改动先出计划（`update_plan`），再动手；不要边想边改十个文件。
- 失败要说失败：哪一步、什么错误、试过什么、下一步建议。
- 不要为了“让闸门变绿”改测试断言或基准 fixture。
