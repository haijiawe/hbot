# 旧项目代码评估（只依据代码，不依据旧文档）

> 评估对象：`E:\H Bot 制造`（qqbat 0.1.0）。
> 评估方式：只读源码、配置、测试与质量闸门；未把 `docs/`、`README`、历史报告作为判断依据。
> 评估日期：2026-11-16。
> 结论口径：`P0` 只用于「丢消息 / 错发 / 数据损坏 / 安全绕过 / 必然崩溃」；普通性能与可维护性问题记 `P1/P2`。

## 0. 一句话结论

**旧项目不是“烂尾代码”，而是一个测试充分、长期演进的单机 Electron 单体。**
它值得重写的理由不是“代码不能跑”，而是产品边界变了：要从「Windows 单机桌面」变成「三平台桌面 + Linux 后台 + Agent 优先 + 可被 Agent 持续开发」。在这个新边界下，继续给旧结构加能力，成本会高于新建一个边界清晰的核心。

因此建议：**重写，但不是大爆炸式重写。**
先在新仓库建立「Rust 单核 + 可替换桌面壳 + Agent 写代码」的最小闭环，用旧项目的**行为场景**当验收语料（不逐条搬测试、不兼容旧格式），分阶段替换；旧项目在切换完成前保持可运行。

### 0.1 决策变更（2026-11-16）

本文件写完后，用户追加了两条硬约束，产品形态随之改变：

1. **不做节点画布**，先做「Agent 写 bot 代码」；
2. **Agent 不能用 Node**（常驻内存过高）。

因此：

- 技术栈由 [ADR-0001](adr/0001-tech-stack.md)（Tauri + Node daemon + 低代码节点）改为 [ADR-0002](adr/0002-agent-in-rust.md)（Rust 单核、无 Node、QuickJS 沙箱），`0001` 已标 Superseded。
- 下文的 P0/P1 **仍然是旧代码的事实**，继续作为「新运行时必须满足的不变量」的证据；但与节点/流程强相关的条目（等待表、触发器顺序、flow 保存/迁移、撤销栈）降为参考，不要求 1:1 复刻。
- 与消息可靠性、并发、安全、密钥、批量写、契约膨胀相关的条目**全部保留**，并已映射到新验收项（§4.4）。

### 0.2 产品形态再定位：Agent-first Bot IDE（2026-11-16）

用户进一步明确：**这是一个“Agent 实现任何 bot”的项目，代码优先，并且要做成 IDE**。因此：

1. **IDE 是一等产品**，不是聊天框 + 文件树。设计见 [06-ide.md](06-ide.md)；需求见 01 的 FR-19 ~ FR-27。
2. **生成代码是多文件**（`src/**`，相对 import），不再是单文件；仍然不允许 npm。
3. **“任何 bot”靠适配器 SDK**，不在核心写平台分支；MVP 是 OneBot 11 + 通用 Webhook。
4. **调试走插桩构建**（debug build），不是 V8 Inspector；理由是保住每 bot MB 级内存。限制写进 UI，不做假按钮。

对旧项目评估的影响：

- 旧项目的 **UI 复杂度证据仍然有效，而且更重要**：19 个 flow slice 的跨 store 编排、首屏静态加载节点目录、交互测试近零、e2e 不在 vitest include —— 这些是“IDE 不要重复”的直接反例（P1-18/P1-19/P2-4）。
- 旧项目的 **Monaco/布局/懒加载经验可以作为模式参考**，但不搬代码；新 IDE 重新实现，且只有三个 store。
- 旧项目的 6,395 个测试仍然只抽取**行为场景**，不逐条迁移；IDE 的交互测试用 Playwright 新写。

---

## 1. 实测规模与质量基线

这些数字来自实际命令与代码统计，不是文档转述。

| 指标 | 实测值 | 说明 |
|---|---|---|
| Git 跟踪的 TS/TSX 源文件 | 1,065 | 另有测试 TS 534 个 |
| `src/` 跟踪文件 | 1,144 | `src/main` 570、`src/renderer/src` 415、`src/shared` 153、`src/preload` 3 |
| 最大目录 | `src/main/services` 376、`src/renderer/src` 415、`src/main/ipc` 66、`src/shared/nodes` 47 | 业务实现高度集中在 services |
| 测试基线 | 529 passed / 2 skipped 文件；6,395 passed / 4 skipped 用例；122.23s | `node scripts/ensure-sqlite.js node && node scripts/typecheck.mjs && vitest run` 全过 |
| 显式 `any` | 2,230 处 / 158 文件 | `node scripts/check-any.mjs`，基线 2,230 |
| 超长函数 | 3,797 个函数；≥400 行 7 个（assembly 3 / data-table 2 / single-transaction 2 / multi-capability 0） | `node scripts/check-fnlen.mjs` |
| 模块循环 | 1 个环，含 11 个文件 | `node scripts/check-cycles.mjs` |
| 跨文件克隆 | 888 条（type-1 135 / type-2 773）；逻辑克隆 0；声明式重复 15；轻量重复 873 | `node scripts/check-clones.mjs` |
| 主进程直接 `import ... from 'electron'` | 93 处 | `rg '^import .*from ["'"']electron["'"']' src/main` |
| 渲染层 IPC 类型 | `src/renderer/src/types/ipc.ts` 2,188 行 | 与 preload / 主进程手工同步 |
| preload 桥面 | `src/preload/index.ts` 1,416 行 | 大量通道逐个转发 |

**怎么读这些数字：**

- 6,395 个用例全过，说明旧代码有真实行为基线。重写时最危险的做法是“把旧代码当垃圾”，那会丢掉这些行为知识。
- 2,230 处 `any` 与 888 条跨文件克隆说明：类型与结构约束是**事后加上的**，不是一开始就有的边界。新项目必须让边界成为“写错就编译/闸门失败”，而不是靠 review。
- 1 个 11 文件循环说明 `agent-core` 的职责已经互相缠住；这不是“再拆一个文件”能解决的。
- 93 处 Electron import 说明平台能力没有端口化；一旦要 headless，就只能靠 `require` 兜底或第二套装配。

---

## 2. 结构性问题（按严重度）

> 2026-11-16 对消息主链路做逐行复核后，新增 4 个 **P0**（丢消息 / 错发 / 安全绕过 / 交错副作用）。它们不是“未来重构建议”，是当前版本可触发的真实缺陷；详细证据见 `E:\HBot-Next\_research\bot-runtime.md`。

> **在新方向下怎么读**（2026-11-16 决策变更）：P0-1/P0-2/P0-4/P0-7/P0-8/P0-9/P0-10 与 P1 中的消息、并发、审批、密钥、批量写、契约条目**直接适用**，已转成新架构的不变量；P0-3/P0-5/P0-6/P1-8/P1-15/P1-16/P2-4 与节点/流程强相关，新项目不做节点，只保留其中“等待要落库、保存要两段式、撤销要自洽”的通用教训，不要求复刻流程引擎。

### P0-1 启动/重连窗口内的入站消息被静默丢弃

- 官方适配器先启动（`src/main/services/bot/botManager.ts:250`），运行时后创建（`:256`）；这段窗口内翻译器已经在广播。
- `broadcastToClients` 只遍历现存 `clients`，`clients` 为空时**不写日志、不落盘、不缓冲**（`src/main/services/runner/eventBus.ts:806-827`）。
- 运行时重连的 3 秒窗口（`src/main/runtime/FlowRuntime.ts:355-360`）同样丢事件。

**影响**：用户侧表现为“机器人没反应”，且没有任何错误记录可查。

**新项目不变量**：入站事件在适配器收到后**先持久化/入有界队列，再投递**；运行时订阅必须先于适配器连接；断线期间事件不丢，恢复后按序回放；队列满时背压而不是静默丢弃。

### P0-2 本地 EventBus 的 WebSocket 无鉴权，可冒充机器人发消息

- `src/main/services/runner/eventBus.ts:684-687` 绑定 `127.0.0.1` 后**连接即加入 clients**，没有 token、Origin、握手校验；`:706-713` 的消息帧直接进 `handleAction`。
- 本机任意进程（包括被投毒的插件子进程）都能订阅全部事件（含 openid），并下发 `send_group_msg` 之类动作，以机器人身份发消息。

**影响**：安全边界为零；“只绑回环地址”不等于“只有本应用能连”。

**新项目不变量**：核心内部事件总线**不进 socket**（进程内 `EventEmitter`/直接调用）；任何跨进程传输必须 token + Origin + 绑定地址 + 消息大小限制；不存在“连接即信任”的通道。

### P0-3 `wait_input` 唤醒条件过宽，群里他人消息可填入

- `src/main/runtime/waitRegistry.ts:228` 的 `matches()` 只在 `chatId`/`chatType` **双方都有值**时才比较，缺一侧即视为匹配。
- `:233` 对 `scope === 'any_in_group'` 直接 `return true`，连 `userId` 都不比。

**影响**：群里 A 触发的等待可能被 B 的下一条消息消费，链带着 B 的内容回复 A —— 错人/错数据。

**新项目不变量**：等待的归属键至少是 `(botId, conversationId, senderId)`；缺任一字段即不匹配；`any_in_group` 必须显式声明，且至少校验 `conversationId`；跨用户唤醒要有测试钉住。

### P0-4 同一会话事件并发进入解释器，导致乱序/交错副作用

- `src/main/runtime/FlowRuntime.ts:401` 用 `void this.dispatchEvent(...)` 甩出异步链；多条消息各自进入解释器。
- 旧解释器的等待表是全局的（`src/main/runtime/FlowInterpreter.ts:447-476`），并发时“谁先结算”无保证。
- 同一会话的“查询 → 扣款 → 回复”式链可以交错，回复顺序与到达顺序不一致。

**影响**：回复乱序；同一业务链可能重复执行副作用。

**新项目不变量**：每个会话一条显式串行队列（有界、可观测、可取消）；入站事件带去重 id；只有显式标记 `parallel` 的分支才并发；同一会话的流程 run 严格串行。

### P0-5 切项目/关窗口不冲刷，最近 ≤1.5s 编辑与撤销栈一起无声丢失

- `src/renderer/src/stores/flow/persistence.ts:106-110`：自动保存是 1500ms 防抖；`:75-82` 的 `markGraphLoaded()` 直接 `clearTimeout` 并清 `dirty`，**不提交**待写内容。
- `src/renderer/src/components/bots/BotManagerPanel.tsx:83-107`：切项目只 `selectBot()` + `project.getMeta()`，没有 `flushFlow()`。
- `src/main/index.ts:422-449`：`before-quit` 只等 `flushStateJournal()` 与 `shutdownTelemetry()`（`:438-445`），不碰 flow。
- 加载/重置会 `clearHistory()`，未保存编辑与整段撤销历史一起消失，且没有提示。

**影响**：用户改完节点立刻切项目或关窗口，配置回退到上一次保存；撤销也回不去。

**新项目不变量**：图编辑先写本地 journal（IndexedDB 或内存 + 定期 RPC 快照），切项目/退出/窗口隐藏前必须 `await flushGraph()`；保存失败阻塞切换并提示；撤销栈按项目持久化，或明确“不跨会话”并在 UI 说明。

### P0-6 flow.json 迁移先覆盖、后校验，备份失败被吞

- `src/main/services/flow/flow.ts:222-226`：迁移前 `historyService.backup()` 包在 `try/catch` 里，`catch {}` 空吞，备份失败照常继续。
- `:227-229`：迁移结果直接 `writeFile(filePath, ...)` 覆盖原文件。
- `:231-232`：覆盖之后才 `flowGraphSchema.safeParse(migrated)`；校验失败抛错时，原稿已经被覆盖。
- 报告称渲染层加载失败会 `reset()` 成空画布；这一点未逐行复核，但“原文件已丢”已由上面三行代码确认。

**影响**：一次不兼容的迁移会让用户的流程文件永久丢失；备份还可能根本没写成功。

**新项目不变量**：迁移 = 读 → schema 校验 → 备份成功 → 写临时文件 → `fsync` → 原子 rename；任一步失败保留原文件；备份失败视为迁移失败；迁移前后各有测试（损坏文件、未来版本、部分字段缺失）。

### P0-7 一次“以后别再问”把本轮所有写/命令都放行

- `src/main/ipc/ai/aiAgentCoreApproval.ts:180-184`：用户点“always”时执行 `approved.add(ruleKind)`，`ruleKind` 是 `write` / `command` / `tool` 这种**大类**（`:151`）。
- `:153` 先用 `approved.has(ruleKind)` 判断，命中就直接放行，**在比较具体 `match` 之前**。
- 落盘的 `allowRules` 反而是精确匹配（`:153` 后半段、`:183-184`），所以重启后行为会变：同一轮内宽，下一轮严。

**触发条件**：用户对 `a.txt` 的写点一次“以后别再问”，同一轮里模型再写 `b.txt` / 任意路径、或跑任意命令，都不再弹窗。

**影响**：审批粒度被静默放大；用户以为只放行了那一个动作。

**新项目不变量**：审批规则键必须是 `(工具/动作, 规范化参数或路径, 沙箱档)` 的完整匹配；`always` 只登记当前完整键；不存在按 `kind` 放行的集合；规则命中要写审计（哪条规则、谁批准）。

### P0-8 桌面与后台的装配分叉，会让同一项目落到不同端口并互相覆盖配置

- 桌面端口解析：`src/main/services/bot/botManager.ts:182` 调 `eventBus.ts:87 resolveUsableTranslatorPort(config, projectPath)`；**不读 `QQBOT_BUS_PORT`**。
- 无头端口解析：`src/main/runtime/headlessAssembly.ts:64-77` 自己实现“显式 > `QQBOT_BUS_PORT` > `.qqbat/project.json`”。
- 端口回写：桌面 `botManager.ts:239-247` 会把实际端口写回 `project.json`；无头**不回写**。
- 全仓 `QQBOT_BUS_PORT` 只出现在 `headlessAssembly.ts:66`、`dockerExport.ts:277`（容器 healthcheck）、`project.ts:229`（模板注释）三处。

**影响**：同一项目在桌面启动和在 Docker/SSH 里启动可能用不同端口；桌面回写 `project.json` 会覆盖容器侧的配置，形成两套真源互相打架。

**新项目不变量**：只有一个 `RuntimeAssembly.start(opts)`；桌面与后台的差异只通过 `Host`（status/log 回调、是否回写端口）表达；端口策略是一个函数，`QQBOT_BUS_PORT` 语义只定义一次。

### P0-9 后台产物顶层 `require("electron")`，Linux 直跑会 `MODULE_NOT_FOUND`

- 构建产物 `out/main/chunks/FlowRuntime-*.js` 顶层有 `let electron = require("electron")`（报告实测），源在 `src/main/services/official/config.ts:2` 与 `src/main/services/infra/safeStorage.ts:2` 的静态 import。
- 部署包靠 `src/main/services/ops/runtimeBundle.ts:30-40` 生成一个 `module.exports = {}` 的假 electron 包来满足它。
- 直接 `node out/main/standalone.js` 且环境没有 electron 时，启动即死；即便用 stub 跑起来，`electron.app` 是 `undefined`，未加 `?.` 的访问会崩。

**影响**：所谓“Linux 后台”依赖安装脚本的兼容层，不是干净的 Node 产物；这也是 P1-2 依赖方向错误的具体后果。

**新项目不变量**：核心 0 个 Electron import、0 个 Node 运行时依赖；平台能力走端口；后台 `hbot serve` 在没有 Electron/WebView/Node 的 Linux 镜像里能直接启动（CI 必测）。

### P0-10 非 Windows 上命令沙箱退化为“名字白名单”，只读档可能直接 `spawn`

- `src/main/services/agent-core/acl/confine.ts:116` 在非 `win32` 直接 `return null`（不建隔离）。
- `src/main/services/agent-core/sandbox.ts:66` 的 `FALLBACK_COMMANDS` 只有 6 个名字；`:166` 的 `readOnlyDenial` 主要按命令名拦。
- 结果是 macOS/Linux 的 `workspace-write` 退化成名字白名单；`read-only` 在没有 ACL 时落到普通 `spawn`，只剩名字检查。Windows 缺 PowerShell 导致建不出令牌时同理。

**影响**：换平台即失去隔离；“只读档”在非 Windows 上并不保证只读。

**新项目不变量**：沙箱是平台适配器（Windows 受限令牌 / POSIX `bwrap`/`nsjail`/容器）；**建不出隔离时拒绝执行**，绝不回退成原样运行；`read-only` 在无适配器时拒绝所有命令。

### P1-1 单一 bootstrap 承担了整台机器的装配

`src/main/index.ts:72-309` 的 `bootstrap()` 依次做：

- userData 布局迁移（`:78`）
- 项目根迁移（`:90-102`）
- SQLite 自检与 ABI 兜底（`:105-119`）
- 全局服务容器（`:122`）
- 五组 AI 模块级设置加载（`:127-141`）
- 子代理配置注入（`:146`）
- 全量 IPC 注册（`:148`）
- 项目状态冷恢复（`:161`）
- 定时任务恢复（`:176-189`）
- 内置插件注册（`:192`）
- 市场插件恢复（`:197-212`）
- 凭证迁移（`:234`）
- MCP client/server 启动（`:250-297`）
- 窗口创建与积压事件补发（`:299-308`）

问题不在“长”，而在**启动顺序就是隐式契约**：任何一步失败或顺序变化，都会表现为另一处“看起来没数据”。这类系统很难被 Agent 安全修改，因为改一个初始化点需要理解整条链。

**新项目要求**：一个 composition root，只做“按依赖图装配”；每个子系统自己声明 `init()` / `dispose()`，启动顺序由显式阶段（`config → storage → runtime → rpc → ui`）表达，并且每个阶段可单独测试。

### P1-2 桌面与无头是两套装配，且无头仍然依赖 Electron

> 已升级为 P0-8（端口/配置互相覆盖）与 P0-9（后台产物 require electron）。保留本条作为“装配分叉”的总说明。

- 桌面：`src/main/services/bot/botManager.ts:146-294` 的 `startBot()` 负责快照、插件、配置、端口、官方适配器、平台分支、`FlowRuntime`。
- 无头：`src/main/runtime/headlessAssembly.ts:76-138` 自己重新做一遍 `.env`、端口、翻译器、官方适配器、`FlowRuntime`，**不使用 BotManager**。
- 更关键的是无头链仍然引入了 Electron：`headlessAssembly.ts:6` → `src/main/services/official/ws.ts:4` → `src/main/services/official/config.ts:2` 的 `import * as electron from "electron"`。`src/main/services/bot/botManager.ts:3` 也是顶层 import Electron。

这意味着“Linux 后台”不是独立的 Node 产物，而是需要 Electron 运行时/兼容层才能完整工作。是否能用 `ELECTRON_RUN_AS_NODE` 跑通本文没有实测，不能写成“已失败”；但**依赖方向已经错了**：核心运行时不该依赖桌面壳。

**新项目要求**：核心 crate 0 个 Electron import、0 个 Node 依赖；桌面壳只负责窗口、托盘、密钥、更新；核心与桌面同进程（Rust 单一 `App`），不启 sidecar。用架构闸门钉死。

### P1-3 两套 Agent 循环并存

`src/main/services/agent-engine/loop.ts:17-18` 明确写着新旧两条循环并存，由宿主端口决定；旧循环仍在 `src/main/services/agent-core/core.ts`。两者共用工具、审批、压缩等，但骨架是两份。

后果：新引擎修好的并行、压缩、溢出落盘、检查点、插话，不会自动出现在 `subagent` / `workflow` / `ralph` 等直接调用旧 `runAgentTurn` 的路径上。Agent 行为出现差异时，排查者要先判断“这一轮走的是哪条循环”。

**新项目要求**：**只有一条循环**。需要不同执行模式时，用策略参数（`mode: chat | subagent | workflow`）而不是第二份骨架。

### P1-4 平台分支写进了 BotManager，状态回调复制两份

`botManager.ts:190-228` 用 `if (adapterKind === "lark")` / `if (adapterKind === "dingtalk")` 直接 `new LarkTranslator` / `new DingTalkTranslator`，然后提前 `return`；官方 QQ 走另一条路径（`:229-286`）。

同时 `startBot` 内的 `runtime.onStatusChange`（`:257-274`）与 `attachRuntime` 内的回调（`:310-327`）几乎逐行重复。新增平台时，复制的是“状态机 + 日志 + 端口 + 运行时装配”，而不是只实现一个适配器。

**新项目要求**：`Adapter` 只做协议翻译（入站事件/出站动作/连接生命周期）；`BotSupervisor` 只做状态机与资源编排。新增平台只新增一个 adapter 包与一条注册，不改 supervisor。

### P1-5 MCP 外部工具被标成 `query`，绕过本地写闸门与角色边界

`src/main/mcp/client/manager.ts:105-120` 的注释自己承认：

- 注入的工具“必须是 query，不能是 mutation”，否则会走提案转换而失败；
- 代价是“**MCP 工具绕过本地的写闸门与角色边界**”，一个 filesystem MCP server 拿到 `write_file` 就能写任意路径。

实际字段在 `:120` 是 `category: 'query'`；而模块头 `:18-19` 又写“注入的工具分类是 mutation”。两处自相矛盾，说明设计意图本应是保守，落地时被提案管线倒逼成了 `query`。

`src/main/services/ai/toolGuard.ts:254` 对 `category === 'query'` 在非 `auto` 模式返回 `ask`，在 `auto` 模式返回 `auto`；但 `classifyTool` 不知道“这是外部工具”，也不执行角色写域。`src/main/mcp/client/manager.ts:125-139` 的 handler 直接 `conn.callTool` 转发，**不传 `ctx.signal`、不查 writeGuard、不走沙箱、不声明审批**（`agent-core/legacyTools.ts:135-143` 对非 `job_start` 一律返回 `null`）。因此 `read-only` 档对它无效，取消也不生效。触发前提是用户显式启用 `config/mcp-client.json`（默认 `enabled:false`）。

**触发条件**：启用 MCP client，且工具闸为 `auto`（或该工具在 `approved` 集合里）。此时远端工具可以在没有本地审批、没有角色写域检查的情况下执行任意远端副作用。

**定级**：P1（条件性安全边界）。若产品默认档就是 `auto`，应升 P0；旧代码默认是 `acceptEdits`（`src/main/mcp/mcpToolGuard.ts:28`），所以按 P1 记。

**新项目要求**：外部工具必须是独立类别 `external`，默认 `ask`，永不因为“是 query”而自动放行；外部工具的能力由用户在连接配置里逐项授权，而不是由远端 `inputSchema` 推定。

### P1-6 MCP server 路径的 `write_file` 在 `acceptEdits` 下自动执行

`src/main/mcp/server.ts:181-215` 用 `mcpToolVerdict` 判定，`auto` 直接放行。`classifyTool` 对 MCP 上下文只在 `PROPOSAL_MUTATIONS` 上返回 `ask`（`toolGuard.ts:243`）；`write_file` 属于 `REVERSIBLE_WRITES`（`:190`），在 `acceptEdits` 下走到 `:256` 返回 `auto`。

server 端确实有路径守卫与轮次日志（`server.ts:341-351`、`:357-370`），所以不是“任意路径无保护”；但**外部 IDE 写项目文件不会弹审批**。这与“MCP 是唯一不经 Proposal 的写路径”的注释（`server.ts:107`）一致，是有意取舍，不是纯 bug。

**定级**：P1（策略歧义）。如果产品承诺“外部写入必须确认”，这里就是缺口；如果承诺“可撤回写入自动执行”，则应在 UI 与文档中明确，不要让用户以为所有外部写都要确认。

**新项目要求**：审批策略按“可撤回性 × 来源”二维表达，而不是靠工具名集合猜；MCP server 与 UI 走同一个策略函数，并把这个二维表写进测试。

### P1-7 事件分发是 fire-and-forget（已升级为 P0-4）

原判断为 P1；复核后确认同一会话并发会直接导致乱序与副作用交错，已按 P0-4 记录，详见上文。这里不再重复。

### P1-8 等待状态只在内存，重启即丢；超时靠“下一条事件”结算

`FlowInterpreter.ts:437-440` 自己写明：

- 挂起表在内存，进程重启后等待中的链不会恢复（用户表现为最终无响应）；
- 超时在“下一条事件到达”时结算，尚未接心跳定时器，长时间无人说话时超时会顺延。

复核后更具体：

- `takeExpired` 全仓唯一调用点是 `FlowInterpreter.ts:453` 的 `settleWaitsAndResume`，而它只被 `dispatchEventInner`（`:361`）调用。
- 每秒一次的心跳 `tickSchedules`（`src/main/runtime/FlowRuntime.ts:212-238`）整个函数不访问等待表；无定时触发器时 `:219` 直接 `return 0`。安静会话里的超时分支永远不执行。
- `restoreWaitsFor`（`src/main/runtime/waitRegistry.ts:183`）在生产代码里**零调用**；`.qqbat/runtime/pending-waits.json` 只写不读。测试 `tests/main/runtime/waitPersistence.test.ts:53-81` 直接测了这个函数，却没测“运行时是否调用它”。

**定级**：P1（状态丢失/流程卡死）。如果用户把“等待用户输入”用于客服/游戏关键流程，这是业务级故障。

**新项目要求**：等待与定时器落 SQLite（`waits` / `schedules`），启动时恢复；调度器独立于事件循环；超时由定时器触发，不依赖下一条消息。要么完整恢复，要么明确“重启即放弃”并在 UI 提示，不能留“看起来有恢复”的假象。

### P1-9 数据库节点每次执行都开库、设 PRAGMA、关库

`src/main/services/builtinNodes/databaseWrite.ts:147-150` 每次 `new (require('better-sqlite3'))(...)`；`:195-198` 每次操作 `journal_mode=WAL`、`synchronous=NORMAL`、`busy_timeout=5000`；`:214-219` 每次 `close()`。

注释解释了“不关会泄漏句柄、Windows 上 EBUSY”，这是正确的局部修复，但代价是：每个数据库节点 = 一次打开 + 三次 PRAGMA + 一次关闭。高吞吐流程里这是明显的延迟与锁竞争来源。

**定级**：P1（性能；不是正确性）。单条消息可能只多几毫秒，但并发/批量时会放大。

**新项目要求**：存储层持有长连接/连接池；节点通过 repository 调用，不直接开库；写事务短、批量合并；连接生命周期由 storage 包统一管理。

### P1-10 IPC 契约手工膨胀，参数校验不是强制

- `src/preload/index.ts` 1,421 行、308 个 `invoke`、37 个命名空间；`src/renderer/src/types/ipc.ts` 2,188 行，主/预加载/渲染三处手工同步。
- `tests/guards/ipcSurfaceParity.test.ts:7-11` 自述“三处手写”，AST 对账首跑 228/228；对账只保证“名字一致”，不保证“类型/校验一致”。
- `src/main/ipc/system/_shared.ts:158-174` 的 `installTrustedSenderGuard` 确实包住了 `ipcMain.handle`，来源校验是全覆盖的。
- 但 `ipcHandle` 的 `schema` 是可选参数（`:181-239`），没给 schema 的通道就没有参数校验；实测 zod 覆盖 222/308，约 **86 条通道运行时无 schema**（`ai/mcpClientIpc.ts` 5/0、`ai/aiAgentCore.ts` 5/0、`project/launchData.ts` 23/22）。
- 新增一条带载荷的 IPC 要改 4–5 处；没有代码生成。

**定级**：P1（可维护性 + 输入安全面）。不是“没有守卫”，而是“守卫强度取决于每个作者有没有记得传 schema”。

**新项目要求**：契约只在 `crates/hbot-rpc/src/dto.rs` 定义，serde 负责解码/校验；`dispatch` 是唯一入口；UI 类型由 `ts-rs` 生成，不手写第二份；新增通道不需要改 preload/类型/文档三处。

### P1-11 `node:vm` 被明确承认不是安全边界

`src/main/runtime/sandbox.ts:30-34` 写明：`node:vm` 不是安全机制，只消除已知逃逸；真正硬隔离需要独立进程。`:72-114` 用闭包隐藏 bridge、null 原型 contextify，属于“提高门槛”，不是“边界”。

**定级**：P1（安全；取决于是否执行用户/模型生成的代码）。如果只执行内置节点，风险较低；一旦允许自定义 JS/Python 节点，这就是绕过面。

**新项目要求**：不要用 `vm` 当沙箱。用户代码/插件在独立 worker 或子进程，带资源限制、只读挂载、无网络默认；策略上明确“这是隔离，不是语言级沙箱”。

### P1-12 官方 QQ 链路没有事件幂等，且重启后无法 resume

- 全仓 `dedupe|idempot` 只命中 lark/dingtalk 两个 adapter；官方链路没有。
- `src/main/services/official/ws.ts:26-27` 在派发前就把 `seq` 前移，因此 op6 resume 不会重发已收事件；代价是 `sessionId/seq` 只在内存（`src/main/services/official/client.ts` 的 `sessions` Map），**进程重启后断线期间的消息永久丢失**。
- 上游一旦重复投递同一个 `eventId`，流程会重复执行。

**新项目要求**：适配器持久化 `session_id + seq` 或平台游标；入站按 `(botId, externalEventId)` 去重；无法保证 exactly-once 时，副作用节点必须幂等，并在 UI/文档说明“最多一次/至少一次”的语义。

### P1-13 插件入口脚本在主进程 `vm` 同步执行

`src/main/services/plugins/pluginScanner.ts:283-290` 为读取插件声明，直接在**主进程**的 `node:vm` 里执行插件 `index.js`，上限 5 秒。一个 `while(true)` 或重 CPU 的插件会阻塞整个主进程，所有机器人一起卡；而 `src/main/runtime/sandbox.ts:30-34` 已承认 `node:vm` 不是安全边界。

**新项目要求**：主进程永不执行插件代码；加载期只读纯 JSON manifest，必要时在独立 worker 预检；执行期 worker/子进程隔离，带超时与崩溃域。

### P1-14 批量写“半成功”，且把失败键报成成功

`src/main/services/persistentStore.ts:622-672` 的 `batchSaveValues` 对每个键单独 `try/catch`，错误只塞进 `errors[]` 而**不 rethrow**（`:645-662`），事务照常提交；返回的 `keysList: Object.keys(dataDict)`（`:671`）把全部输入键都报成成功。

**影响**：调用方拿到“成功 + 若干错误”的自相矛盾结果，数据部分落库，且无法从返回值判断哪些键真的写了。

**新项目要求**：一批写 = 一个事务，任一失败即回滚并抛出；返回值只包含成功的键；批量写与单写走同一 repository，不允许节点绕过。

### P1-15 一次事件只执行第一条命中的触发器

`src/main/runtime/FlowInterpreter.ts:414` 遍历节点，第一条 `matchTrigger` 命中的触发器就 `return true`，其余同时匹配的触发器静默不执行；顺序是 `flow.json` 的节点数组顺序，画布上不可见、不可调。

**新项目要求**：默认仍可“首条命中”，但必须：① 触发器有显式优先级；② 保存时静态检测重叠触发器并报错/警告；③ 需要多链时用显式 `continue`，不靠数组顺序。

### P1-16 并行分支变量覆盖与重试 deadline 重置

- `src/main/runtime/FlowInterpreter.ts:1609-1613` 的 `execParallel` 合并分支变量时 last-write-wins，同名键静默覆盖，无告警。
- `src/main/runtime/FlowInterpreter.ts:565-600` 的 `resumeFromWait` 在 `expectedPattern` 不匹配且 `maxRetries > 0` 时，每次重试都 `registerWait` 新 id 并把 deadline 重置为完整 timeout，总等待时长变成 `timeout × (retries+1)`。

**新项目要求**：并行分支写入冲突必须检测并报错/隔离命名空间；重试保留原 deadline，只在原窗口内重试。

### P1-17 防抖字段可能在切换节点时把值写进新节点（待实测）

- `src/renderer/src/components/blocks/DebouncedInputs.tsx:18` 每次渲染刷新 `onCommitRef`；`:21-28` 只在 `externalValue` 变化时清定时器；`:39-41` 到点用**最新** ref 提交。
- `src/renderer/src/components/blocks/ConfigFieldsSection.tsx:224` 的字段行 `key={field.key}` 不含 `nodeId`，React 会复用组件。
- 两个节点有同名字段且当前值相同（空串最常见）时，切节点不触发 `externalValue` 变化 → 旧定时器不清 → 300ms 后把上一节点输入写进新节点，并落盘/进生成代码。

**定级**：P1（未实测；条件窄但会写错配置）。报告给了复现步骤。

**新项目不变量**：字段组件 `key` 含 `nodeId`；提交闭包绑定 `nodeId` 并在节点变化时丢弃；切换节点前 `flush/cancel`；加一条“切换后 300ms 内不得写旧值”的回归测试。

### P1-18 首屏静态加载全部节点目录与 6,072 行文案

- `src/renderer/src/components/layout/LeftPanel.tsx:2` 静态 import `NodePalette`。
- `src/renderer/src/lib/node/nodeCatalog.ts:59` → `src/shared/nodes/catalog/registry.ts` → `builtinCatalog`（17 个 spec，约 1 万行）。
- `src/shared/nodes/catalog/build.ts:16,26` 又静态引入 `catalog.en-US.ts`（6,072 行）。
- 画布本身 lazy 了，目录没有；用户停在 launch 首页也要付这份解析/内存成本。

**新项目不变量**：节点目录按类别动态加载；i18n 按 locale 懒加载；初始 bundle 预算用构建报告钉住（见 `docs/05-quality-gates.md`）。

### P1-19 UI 交互测试几乎为零，唯一画布 e2e 永不运行

- 没有 BlockEditor / PropertyDrawer / 拖拽 / 连线 / 撤销的交互测试。
- `tests/e2e/node-flow.spec.ts` 214 行，但 `.spec.ts` 不在 `vitest.config.mts` 的 `include` 里，仓库也没有 Playwright 依赖/脚本 → 从不执行。
- coverage 还排除 `**/stores/**`，而复杂度正在 store。
- 拖拽性能唯一数字只在注释里（263 节点/471 边：开 `onlyRenderVisibleElements` 3.8ms/帧，关 69ms/帧，最慢 241ms）；1000 节点未验证。

**新项目不变量**：画布交互（拖/连/撤销/切换节点）必须有组件测试；核心用户路径有 Playwright e2e；性能测试进 nightly，不靠注释。

### P1-20 “角色写域”是一道恒真闸，注释却声称它在最前

- `src/shared/ai/roleDomains.ts:77-97`：每个角色的 `writeDomains` 都是 `EVERYTHING`。
- `:155-157` 的 `mayRoleWritePath()` 因此恒为 `true`；`mayRoleWrite()` 对未登记能力也返回 `true`（`:110-114`）。
- 唯一调用点在提案路径 `src/main/services/ai/proposal.ts:8`；而 `src/main/ipc/ai/aiAgentCoreApproval.ts:112,127`、`src/main/services/agent-runtime/projectTurn.ts:105` 的注释都说“角色写域闸在最前”。

**影响**：它既不是防线也不是漏洞，是**误导后续维护者/Agent 的恒真闸**；有人会以为越界写会被拦。

**新项目不变量**：MVP 单一角色就**删掉**这道闸并在文档说明“不做角色隔离”；要做多角色时，用真实策略 + 拒绝路径测试，不保留恒真函数。

### P1-21 `job_start` 是第二条命令通道，绕开命令沙箱

- `src/main/services/ai/tools/registry.ts:863-887`：只检查 `sandboxAllowsWrites(currentSandboxMode())`，然后 `startJob({ command, cwd, owner })`。
- `src/main/services/agent-core/jobs.ts:184-188`：`spawn(command, { shell: true })`，不经过 `FORBIDDEN` 元字符检查，也不走 `src/main/services/agent-core/acl/confine.ts` 的受限令牌。
- 源码注释（`registry.ts:855-861`）自己写明“run_command 被拦、job_start 照跑任意命令”，并解释为什么不改成 mutation。

**影响**：`workspace-write` 档下，模型可以通过 `job_start` 跑任意 shell 命令；沙箱承诺在这条路径上不成立。

**新项目不变量**：所有命令（前台/后台/子代理）走同一个 `CommandRunner` 端口，统一沙箱、超时、输出上限、进程树回收；不存在“后台任务”特批通道。

### P1-22 `safeStorage` 不可用时明文保存 API Key，且不提示

- `src/main/services/ai/appAiConfig.ts:97-106`、`src/main/services/ai/projectAiConfig.ts:97`：`safeStorage` 不可用时直接写明文 key。
- 同仓库 `src/main/services/agent-core/speechConfig.ts:169` 的口径是“拒绝以明文保存”，两处不一致。
- MCP client 的 `env` / `headers` 也是明文 JSON。

**影响**：Linux 无 keyring、或 safeStorage 初始化失败时，密钥静默落盘；用户不知道。

**新项目不变量**：无 OS keychain 时**拒绝保存**并提示，或要求用户显式选择“明文（不推荐）”并记录；日志/诊断/导出统一脱敏；密钥永不进项目目录。

### P1-23 用量覆盖而非累加，工作流没有总预算，退出不 abort

- `src/main/services/agent-engine/loop.ts:322`：`usage = next` 覆盖上一段，多轮/多工具后 token 统计失真。
- 旧循环 `src/main/services/agent-core/core.ts` 没有 `onUsage`。
- 工作流默认允许 1000 个代理 × 每个 8 轮模型调用，没有本轮总预算闸。
- `src/main/index.ts:422-503` 的 `before-quit` 杀 jobs/子代理/LSP/终端，但不 abort 正在跑的那一轮。

**新项目不变量**：usage 累加（input/output/cache 分开）；每会话/每工作流有硬预算（步数、token、时长、金额）；退出先 abort 所有 turn 并等待落盘，超时才强杀。

### P1-24 原生依赖只有一个 ABI 位，测试/开发/打包互相踢

- `better-sqlite3` 需要 ABI 137（Node 24）与 ABI 146（Electron 42）两份，但仓库只有一个 `build/Release/better_sqlite3.node`（报告实测 1,919,488 B）。
- `scripts/ensure-sqlite.js` 用“真建一次 `:memory:` 库”探测并重装；`pnpm test` 会把它切到 Node 版，之后 `pnpm dev`/打包必须再切回 Electron 版。

**影响**：新人第一小时必踩；CI 里也容易“测试绿、打包挂”。

**新项目不变量**：只有一个运行时（Rust），不再有“两套 ABI / 切 ABI”的隐式步骤；SQLite 用 `rusqlite` bundled，CI 必须真建一次 `:memory:` 库并跑 WAL 读写，不能只 `cargo check`。

### P1-25 `node-pty` 没有 linux-x64 预编译，Linux 终端必然不可用

- `package.json` 依赖 `node-pty ^1.1.0`；`pnpm-workspace.yaml` 把 `node-pty` 的构建脚本设为 `false`。
- 包内 `prebuilds/` 只有 darwin/win32（arm64+x64），无 linux-x64；不允许源码构建时，Linux 上终端面板只能靠 mock 测。

**影响**：三平台目标里，Linux 终端是“界面在、功能死”。

**新项目不变量**：终端是可选能力；启动时探测 + 显式禁用 UI；或换成有 linux 预编译/可源码构建的 PTY 方案，并在 Linux CI 真跑一次 `spawn + echo + exit`。

### P1-26 测试反馈环 340s，主要耗在 jsdom 环境而不是用例

- 报告实测（`--maxWorkers=2`）：531 文件 / 6,399 用例 / 340.90s；其中 `setup` 123.46s、`environment` 253.04s，用例本身仅 137.96s。
- `tests/main` 290 个、`tests/shared` 42 个大多不需要 DOM，却在 jsdom 里跑；全仓快照 0，mock 调用断言仅 352/11,498（3%）。
- `format` 闸门在 CI 无暂存区，实际空转（`scripts/prettier-gate.mjs` 只读 `git diff --cached`）；真正的格式检查是 `.github/scripts/check-ci-format.mjs`。

**影响**：Agent 每次改动都要等 3–6 分钟；反馈慢会直接降低迭代质量。

**新项目不变量**：测试分 `unit` / `integration` / `e2e` / `perf` 四档，默认 `cargo nextest` 只跑单元+契约；`./scripts/check.sh` <90s；UI 格式/类型闸门在 CI 与本地同一条路径，不允许“本地过、CI 空转”。

### P2-1 UI 状态复杂度在 store，而非入口

`src/renderer/src/App.tsx:2-9` 只挂 `ToastProvider` + `Workspace`；`Workspace.tsx:14-108` 只有 109 行。复杂度在 `src/renderer/src/stores/flow/` 的 19 个 slice，以及跨 store 编排（例如启动前保存 flow、切换项目时禁止跨项目应用）。

这是“功能成熟”的体现，但也意味着 UI 行为很难被单测覆盖，更多依赖集成测试。新项目应把**服务端状态**（项目、bot、运行记录）与**编辑器本地状态**（选中、缩放、剪贴板）分开：前者用 RPC 查询缓存，后者才进 Zustand。

### P2-2 大文件与生成物

最大文件包括 `src/shared/nodes/i18n/catalog.en-US.ts` 6,072 行、`src/main/services/ai/capabilityCatalog.generated.ts` 3,168 行、`src/renderer/src/types/ipc.ts` 2,188 行、`src/main/services/ai/tools/registry.ts` 1,620 行、`src/main/runtime/FlowInterpreter.ts` 1,737 行。

其中 catalog 是数据表、registry 是注册表，行数本身不是罪；但 `FlowInterpreter` 同时承担触发匹配、链执行、等待恢复、节点调度，是真正的高变更热点。新项目应把它拆成 `matcher` / `executor` / `wait-store` / `scheduler` 四个可单测单元。

`any` 的分布也说明一件事：2,230 处里 **2,096 处集中在 134 个“从 10/18 打包产物还原”的文件**（94%），首行带 `// 由 10/18 打包产物还原` 标记的源文件共 176 个。所以旧仓库的 `any` 主要是**来源问题**，不是团队习惯；重写时应重新设计接口形状，而不是“逐个补类型”。

### P2-3 其他已确认的小问题（不阻塞重写，但别复制）

| 问题 | 证据 | 影响 | 新项目处理 |
|---|---|---|---|
| `.qqbat/*.db` 只设 WAL，缺 `busy_timeout`/`synchronous` | `src/main/services/baseSqliteStore.ts:63` | 并发写直接 `SQLITE_BUSY` | 统一 PRAGMA，单 writer |
| 每次持久化节点都 `existsSync` 做迁移判断 | `src/main/services/persistentStore.ts:344` | 热路径多余 syscall | 迁移只跑一次（模块级 flag） |
| 单行写也包事务 | `src/main/services/persistentStore.ts:580` | 无谓开销 | 单语句不显式开事务 |
| 同步 `writeFileSync` 整个等待表 | `src/main/runtime/waitRegistry.ts:223` | 主进程磁盘 IO 抖动 | 异步 + 防抖 + 单写链 |
| 等待重试每次重置 deadline | `src/main/runtime/FlowInterpreter.ts:565-600` | 总等待时间被放大 | 保留原 deadline |
| `http_request`/`api_call` 无 deadline | `src/main/runtime/FlowInterpreter.ts:882` | 卡住的请求拖住整条链 | 链级超时 + AbortController |
| cron 的 `dom` 与 `dow` 用 `&&` | `src/main/runtime/scheduler.ts:34` | 抄来的 `0 9 1 * 1` 不按传统 cron 触发 | 显式 AND/OR 语义并写测试 |
| 被动回复上下文按“会话最后一条”兜底 | `src/main/services/runner/eventBus.ts:611-620` | 异步发送可能带错 `event_id` | 按触发链绑定上下文，或走主动消息 |
| 禁用插件只摘触发器，节点仍可执行 | `src/main/services/plugins/pluginLoader.ts:285` | 用户以为已停用 | 禁用时同时摘 handler，UI 标明 |
| 所有项目共用一个 `untrustedWorker` | `src/main/services/dynamicHandlers.ts:55` | 一个插件崩溃影响全部 | 每项目/每插件一个 worker |
| `better-sqlite3` 失败时静默降级到 `node:sqlite` | `src/main/services/infra/sqlite.ts` | 两套 SQL/事务语义不完全一致，行为随机器变 | 要么 fail fast，要么显式告警并限制能力 |
| 插件 `author` 是恒 `undefined` 的死三元 | `src/main/services/plugins/pluginScanner.ts:94` | 作者信息永远为空 | 删掉或正确读取 |
| 自建插件默认 `permissions: ['*']` | `src/main/services/plugins/pluginManifest.ts` | 自建节点等价内置权限，与最小权限叙事相反 | 显式决策并写进文档 |

### P2-4 撤销/历史语义不自洽

- `FlowSnapshot` 只含 `nodes/edges`（`src/renderer/src/stores/flow/types.ts` 注释明写分组不参与历史）。
- 分组操作不压栈；部分系统操作压栈但 undo 不还原（`systemSlice.ts:28` vs `historySlice.ts` 的 undo）→ 半可撤销留下悬空引用。
- 历史深度按节点数缩到 20/30/50，一次编辑按 300ms 空闲切段；大项目里几十次输入就把历史冲干净。
- `.qqbat/history` 50 份快照按“保存次数”裁剪，渲染层零调用 `qqbat.history.*`（preload `index.ts:1063-1068`）→ 时光机实际只覆盖几分钟且没有 UI。

**新项目要求**：定义清楚“快照包含什么”（含分组/系统节点）；所有会改变图的命令都产生可撤销事务；历史按时间/大小两条线裁剪；要么做完整时光机 UI，要么删掉接口。

### P2-5 保存即自动 git commit，且会改用户仓库

- `src/main/services/flow/flow.ts:305` 每次保存后调用 `gitService.autoCommit`。
- `auto`/`manual` 会 `--amend`（`src/main/services/ops/git.ts:252-253`），`other`/`generate` 每次新建 commit；还会自动写用户项目的 `.gitignore`（`:333-347`）。

**新项目要求**：git 集成默认关闭、显式开启；保存与版本控制解耦；绝不自动 amend 用户提交或改用户 `.gitignore`。

---

### P2-6 开发循环与 Agent 易踩坑（实测，供新项目避免）

来自 `_research/arch-quality.md` 的实测，不属于产品缺陷，但直接影响“Agent 能不能稳定干活”：

1. **原生 ABI 来回翻**：`pnpm test` 会把 `better-sqlite3` 切到 Node ABI，之后 `pnpm dev` 必须再切回 Electron ABI；这是旧仓库最稳定的踩坑点。新项目无 Node/Electron，天然消除。
2. **闸门只查暂存区**：旧 `format` 闸门在本地只读 `git diff --cached`，不改暂存区等于没查；CI 另有一套。新项目要求 `check.sh` 对**全树**判定，本地与 CI 同一语义。
3. **测试 include 收窄会静默丢测试**：旧项目一度只剩 1 个测试文件在跑。新项目的 `nextest`/Playwright 配置必须有“期望数量对账”测试，防止“跑得飞快”的假绿。
4. **测试读源码文本很脆**：旧仓库 88 个测试文件直接 `readFileSync` 断言源码字符串，重命名会以“莫名其妙的字符串失败”出现。新项目用类型/契约/行为测试，结构约束用 `cargo`/依赖图闸门。
5. **生成物不许手改**：旧项目 `*.generated.ts` 与基线 JSON 由脚本产出。新项目的 `ts-rs`/`xtask` 生成物在 CI 里 `--check`，手改即红。
6. **路径含空格曾导致 `node-gyp` 失败**。Rust/Cargo 对此不敏感，但脚本里所有路径仍必须加引号（旧仓库 `H Bot 制造` 就是活例）。

---

## 3. 旧项目做得好的地方（必须继承）

1. **行为测试足够多**：6,395 个用例是重写的验收资产。不要重写测试的“意图”，要重写测试的“实现”。
2. **质量闸门的方向是对的**：克隆、函数体量分类、`any` 棘轮、空 catch、可达率、循环依赖。新项目应保留这些闸门，但用更少、更快的工具实现。
3. **安全教训写进了注释**：MCP 工具闸、只读档、命令沙箱、路径守卫、凭据迁移。这些是血泪，不是文档噪音；新项目要把它们变成不可绕过的单点策略。
4. **失败路径有处理**：重连、超时、原子写、EPERM 重试、窗口关闭时的审批拒绝。这些细节要进新项目的测试清单。
5. **产品功能面完整**：多平台、低代码流程、AI Agent、插件、部署、运维。重写不是做减法到“Hello World”，而是按阶段恢复能力。
6. **边界纪律实测是干净的**：`src/shared / main / renderer / preload` 四个运行时的反向依赖全为 0，renderer 不碰 Node，shared 不碰 DOM，preload 只有桥面；1,061 个模块里文件级循环只有 1 个。新项目应保留这种“边界靠机器守”的做法，而不是靠人记。
7. **“真探测”哲学**：`ensure-sqlite.js` 真建一次 `:memory:` 库验证 ABI、`vendor-audit` 用哈希锁字节、`ipcSurfaceParity` 用 AST 对账而不是文本正则。新项目的闸门要学这一点：**验证真实能力，不验证名字**。
8. **单一真源做得对的地方**：面板几何 `panelGeometry.ts` + clamp 契约、节点目录 76 kind 唯一、工具名唯一。新项目要把同样的纪律用在 IDE 布局/快捷键/命令/平台能力上（见 06）。
9. **闸门编排方式**：一个数组串起全部检查、`--all` 追加测试。新项目用 `scripts/check.sh` 保持同样体验，但每一道都必须在本机可跑、失败信息可定位。

---

## 4. 重写判定

### 4.1 什么时候不该重写

如果目标仍然是「Windows 单机 Electron 应用，功能继续加」，那么**增量重构更划算**：

- 把 `botManager` 的平台分支抽成 adapter；
- 把两套 bootstrap 合成一套；
- 把旧 Agent 循环删掉；
- 把数据库节点改成连接池；
- 把 IPC 契约生成化。

这些都能在现有测试保护下渐进完成，风险远低于重写。

### 4.2 为什么这次值得重写

用户已确认的新边界是：

- Windows / macOS / Linux 三平台桌面；
- Linux 后台（无 GUI）；
- 启动/内存/UI、机器人吞吐稳定性、Agent 执行、构建测试/Agent 开发效率都要；
- 适合聊天、游戏、客服等不同 bot；
- 旧格式与插件不做兼容；
- 完全重新开始，新根目录 `E:\HBot-Next`；
- **不做节点画布，先做「Agent 写 bot 代码」**；
- **Agent 不能用 Node**（常驻内存过高）。

这几条里，**三平台 + Linux 后台 + Agent 优先 + 无 Node 低内存**是旧结构最难补的四条。继续在原结构上改，会同时做“拆单体 + 换壳 + 补 headless + 重做 Agent 运行时”，等于重写，但背负旧契约。新仓库可以从第一天就把边界钉死。

### 4.3 重写方式：并行替换，不做大爆炸

不建议“旧项目停更 → 新项目从零写到功能对等 → 一次性切换”。建议：

1. 旧项目保持可运行，作为行为参照与回滚路径。
2. 新项目先做**核心 + 一条垂直切片**（Agent 读写 `bot.ts` + QuickJS 执行 + mock 消息 + 最小 UI），再接真实 OneBot。
3. 用旧项目的**行为场景**做对照（不导入旧数据、不逐条搬测试），逐项打勾。
4. 每完成一个垂直切片，就做一次真实 bot 的 shadow run（新旧各用独立测试账号，比较输出）。
5. 切换按平台/功能逐步进行；旧项目最终归档，不保留兼容层。

### 4.4 验收的硬标准

- 新核心进程**没有任何 Node / Electron 运行时依赖**。
- **唯一装配**：桌面与后台共用 `App::new(opts)` + `dispatch(Command)`；端口/token/项目路径语义只定义一次，不允许多个项目互相覆盖配置。
- **后台可直接跑**：Linux 无 WebView、无 Node，`hbot serve` + systemd 可启停；CI 必测。
- **无隔离即拒绝**：macOS/Linux 没有 `bwrap`/`nsjail`/容器等适配器时，`workspace-write` 拒绝执行命令，`read-only` 也拒绝；绝不退化成“6 个命令名白名单”。
- 只有一个 Agent 循环、一个 composition root；**没有节点/流程解释器**。
- 等待/定时器重启可恢复；同一会话事件严格有序。
- **启动/重连窗口不丢入站事件**：先落盘/入队，再投递；队列满时背压而非静默丢弃。
- **核心内部事件总线没有未鉴权的本地 socket**；跨进程传输必须有 token + Origin + 绑定地址。
- **等待归属严格**：`(botId, conversationId, senderId)` 缺一不匹配；`any_in_group` 显式且至少校验会话。
- **批量写全有或全无**：不允许“半成功 + 全量报成功”。
- **编辑不因切项目/退出丢失**：切项目、关窗口、崩溃恢复都能拿到最后一次编辑；保存失败阻塞切换并提示。
- **迁移不破坏原文件/DB**：校验、备份、临时文件、原子 rename 全部成功才替换；任一步失败保留原稿。
- **审批“always”只作用于当前完整动作**：规则键含动作 + 规范化参数/路径 + 沙箱档；不存在按 `write`/`command` 大类放行。
- **命令只有一条通道**：前台、后台、子代理全部走同一个 `CommandRunner`，统一沙箱/超时/进程树回收。
- **没有恒真的安全闸**：单一角色就明确删除角色写域，不保留永远返回 `true` 的“防线”。
- 外部工具（MCP/插件）默认 `ask`，不存在“query 所以自动放行”。
- 无 OS keychain 时不静默明文保存密钥。
- 用量累加；每会话/工作流有硬预算；退出先 abort 再落盘。
- 三平台冷启动、空闲内存、吞吐达到 `docs/05-quality-gates.md` 的预算。
- 旧项目 6,395 个用例中，与产品行为相关的**场景**在新项目有对应测试（数量不要求相等，语义要覆盖；见 `04-implementation-plan.md` §4）。

---

## 5. 证据索引（方便复核）

| 结论 | 代码位置 |
|---|---|
| 单 bootstrap 承担全部装配 | `src/main/index.ts:72-309` |
| SQLite ABI 兜底 | `src/main/index.ts:105-119` |
| 桌面 BotManager 平台分支 | `src/main/services/bot/botManager.ts:190-228` |
| 状态回调复制 | `src/main/services/bot/botManager.ts:257-274`、`:310-327` |
| 串行 stopAllBots | `src/main/services/bot/botManager.ts:400-407` |
| 无头第二套装配 | `src/main/runtime/headlessAssembly.ts:76-138` |
| 无头链引入 Electron | `src/main/runtime/headlessAssembly.ts:6` → `src/main/services/official/ws.ts:4` → `src/main/services/official/config.ts:2` |
| 事件 fire-and-forget | `src/main/runtime/FlowRuntime.ts:401` |
| 固定 3s 重连 | `src/main/runtime/FlowRuntime.ts:355-360` |
| 内存 pending map / 30s 超时 | `src/main/runtime/FlowRuntime.ts:438-460` |
| 一次事件只触发一条链 | `src/main/runtime/FlowInterpreter.ts:371-415` |
| 等待只在内存 / 重启丢失 | `src/main/runtime/FlowInterpreter.ts:437-440` |
| 等待登记快照 | `src/main/runtime/FlowInterpreter.ts:537-553` |
| 每节点开库关库 | `src/main/services/builtinNodes/databaseWrite.ts:147-150`、`:195-219` |
| MCP client 工具标 query 并承认绕过 | `src/main/mcp/client/manager.ts:105-120` |
| MCP client handler 直接转发 | `src/main/mcp/client/manager.ts:133-135` |
| MCP server 工具闸 | `src/main/mcp/server.ts:181-215` |
| MCP server 直接 executeTool | `src/main/mcp/server.ts:372-379` |
| 工具闸判定 | `src/main/services/ai/toolGuard.ts:239-258` |
| 两套 Agent 循环 | `src/main/services/agent-engine/loop.ts:17-18` |
| IPC 来源守卫 | `src/main/ipc/system/_shared.ts:158-174` |
| IPC schema 可选 | `src/main/ipc/system/_shared.ts:181-239` |
| `node:vm` 不是安全边界 | `src/main/runtime/sandbox.ts:30-34` |
| UI 入口极薄 | `src/renderer/src/App.tsx:2-9`、`src/renderer/src/pages/Workspace.tsx:14-108` |
| 启动/重连窗口丢消息 | `src/main/services/runner/eventBus.ts:806-827`；`src/main/services/bot/botManager.ts:250`、`:256` |
| EventBus 本地 WS 无鉴权 | `src/main/services/runner/eventBus.ts:684-687`、`:706-713` |
| 等待唤醒跨用户 | `src/main/runtime/waitRegistry.ts:228`、`:233` |
| 官方链路 seq 只在内存 | `src/main/services/official/ws.ts:26-27`；`src/main/services/official/client.ts` 的 `sessions` |
| 插件入口在主进程 vm | `src/main/services/plugins/pluginScanner.ts:283-290` |
| 批量写半成功 | `src/main/services/persistentStore.ts:622-672` |
| 一次事件只跑首条触发器 | `src/main/runtime/FlowInterpreter.ts:414` |
| 心跳不结算等待 | `src/main/runtime/FlowRuntime.ts:212-238`；`src/main/runtime/FlowInterpreter.ts:453` |
| `restoreWaitsFor` 零调用 | `src/main/runtime/waitRegistry.ts:183`；测试 `tests/main/runtime/waitPersistence.test.ts:53-81` |
| SQLite pragma 不一致 | `src/main/services/baseSqliteStore.ts:63`；对比 `src/main/services/persistentStore.ts:265-268` |
| 所有项目共用一个 worker | `src/main/services/dynamicHandlers.ts:55` |
| cron `dom`/`dow` 用 AND | `src/main/runtime/scheduler.ts:34` |
| 切项目不冲刷 flow | `src/renderer/src/stores/flow/persistence.ts:75-82`、`:106-110`；`src/renderer/src/components/bots/BotManagerPanel.tsx:83-107` |
| 退出不冲刷 flow | `src/main/index.ts:422-449`（只等 journal/telemetry） |
| 迁移先覆盖后校验 | `src/main/services/flow/flow.ts:222-232` |
| 审批 always 按大类放行 | `src/main/ipc/ai/aiAgentCoreApproval.ts:151-153`、`:180-184` |
| 角色写域恒真 | `src/shared/ai/roleDomains.ts:77-97`、`:155-157` |
| `job_start` 绕开命令沙箱 | `src/main/services/ai/tools/registry.ts:863-887`；`src/main/services/agent-core/jobs.ts:184-188` |
| 防抖字段 key 不含 nodeId | `src/renderer/src/components/blocks/DebouncedInputs.tsx:18-41`；`src/renderer/src/components/blocks/ConfigFieldsSection.tsx:224` |
| 首屏静态节点目录 | `src/renderer/src/components/layout/LeftPanel.tsx:2`；`src/shared/nodes/catalog/build.ts:16,26` |
| 画布 e2e 永不运行 | `tests/e2e/node-flow.spec.ts`；`vitest.config.mts` 的 `include` 不含 `.spec.ts` |
| 明文密钥回退 | `src/main/services/ai/appAiConfig.ts:97-106`、`src/main/services/ai/projectAiConfig.ts:97` |
| usage 覆盖而非累加 | `src/main/services/agent-engine/loop.ts:322` |
| 保存即 git commit | `src/main/services/flow/flow.ts:305`；`src/main/services/ops/git.ts:252-253`、`:333-347` |
| 桌面端口解析不读 `QQBOT_BUS_PORT` | `src/main/services/bot/botManager.ts:182`；对比 `src/main/runtime/headlessAssembly.ts:64-77` |
| 桌面回写 `project.json` 端口、无头不回写 | `src/main/services/bot/botManager.ts:239-247`；`src/main/runtime/headlessAssembly.ts` 未回写 |
| 后台 chunk 顶层 `require("electron")` | 产物 `out/main/chunks/FlowRuntime-*.js`；源 `src/main/services/official/config.ts:2`、`src/main/services/infra/safeStorage.ts:2` |
| 用假 electron 包过桥 | `src/main/services/ops/runtimeBundle.ts:30-40` |
| 非 Windows 不建 ACL 隔离 | `src/main/services/agent-core/acl/confine.ts:116` |
| 沙箱退化名字白名单 | `src/main/services/agent-core/sandbox.ts:66`、`:166` |
| 原生模块单 ABI 位 | `scripts/ensure-sqlite.js`；`node_modules/better-sqlite3/build/Release/better_sqlite3.node` |
| `node-pty` 无 linux-x64 预编译 | `node_modules/node-pty/prebuilds/`；`pnpm-workspace.yaml` 的 `node-pty: false` |
| 测试 340.90s，环境占 253s | `node node_modules/vitest/vitest.mjs run --maxWorkers=2` 实测 |
| 格式闸门 CI 空转 | `scripts/prettier-gate.mjs` 只读 `git diff --cached` |

---

## 6. 未验证项（不要当成结论）

- `ELECTRON_RUN_AS_NODE` 下无头链是否实际可跑通：只确认了 import 链与产物顶层 `require("electron")`，没有在无 Electron 的机器上真跑。
- MCP client 在 `auto` 模式下的完整逃逸面：只确认了 `category: 'query'` 与 `classifyTool` 的放行路径，没有做端到端攻击测试。
- 旧项目在 macOS/Linux 的真实表现：本次只在 Windows 上跑了测试，没有三平台矩阵。非 Windows 沙箱退化是从 `confine.ts:116` / `sandbox.ts:166` 读出来的，**未在 Linux 实跑验证**。
- 插件 `node:vm` 的逃逸可达性：未写攻击样例执行；只确认“同进程 + 原型链可见面”这一结构性事实。
- `node-pty` 在 Linux 是否真的完全不可用：依据包内 `prebuilds/` 清单与 `allowBuilds:false` 推断，未在 Linux 真跑终端。
- 旧项目的启动时间、空闲内存、消息吞吐：本次没有做基准测量；新项目必须补，不能引用旧文档数字。