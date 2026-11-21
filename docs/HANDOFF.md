# HANDOFF —— 交接给下一个会话的 lead

> **本文件是快照，不是真源。** 真源永远是：
> `AGENTS.md`（坑与"改 X 去哪"）、`docs/04-implementation-plan.md`（里程碑与出口标准）、
> `docs/02-architecture.md`（架构与契约）。每次交接时更新本文件，冲突时以真源为准。
>
> 本快照时间：**2026-11-21 20:40**。`C4.jobs`、`C2.session`、`C3.send`/`C3.notify`（含附件 `ctx.sendImage` / `ctx.sendFile`）、`C1.webhook`、`onStart`/`onStop`（含 `shutdown` 触发点）、`C2.sql`、`C3.file`（含每 bot 磁盘配额）、`onSchedule`（= `onTimer` 的正式名）全部落地，另加 **`C6.device` 的按窗口截图**（`ctx.device.screen({ window })`）、`ctx.session.prompt` 的**超时重发提示**（`reminders`，2026-11-20）、`ctx.files.read` 的**按行范围读**（`startLine`/`endLine`，2026-11-20）、**作业与等待的只读面板**（底部面板「作业」页签 + `bot_jobs`/`bot_waits`，2026-11-20）—— **M7 第 1–5.5 步完成，能力面已无缺口**；`examples/` 三个无凭据示例 2026-11-20 入库，M7 出口标准里「三类示例项目」那一半变成可自动验证的；同日又补齐 **`C3.file` 的递归删除 / 二进制读 / 按行写**、**webhook 路由级超时 `timeoutMs`**、**作业池上限 `limits.maxPendingJobs`** 与 **三个示例的整体 e2e**（`crates/hbot-rpc/tests/examples_e2e.rs`）。同日又接上 **`onStop(crash)` 的触发点**：supervisor 的读循环意外结束（订阅被关 / 入队失败）会经 `ReadLoopExit` + `ReadExitSlot` 传出，rpc 侧按 `StopReason::Crash` 停机并跑 `onStop`（`crates/hbot-rpc/tests/crash_capability.rs`）—— `docs/07` §4.6 里「枚举与渲染就位、但没有可靠检测」的那个缺口已关闭。 同轮又收掉 `docs/07` 里最后两件原本记「还没做」的本地能力：**`ctx.sql` 的读走独立只读连接**（`PRAGMA query_only = ON`，长 SELECT 不再占写锁，`hbot-store/src/bot_sql.rs`）与 **`jobs` 的宿主级预算上限**（`bots.json` 的 `limits.jobBudgetMs`，判据只有 `hbot_core::effective_job_budget_ms` 一份）。 同轮开始做**桌面端的端到端验收（A）/ 长时稳定性压测（B）/ 五分钟上手指南（C）**：`scripts/verify-*.ps1` 五支全过、安装包冒烟 11 项 PASS（`docs/spikes/S14-desktop-acceptance.md`）；压测首轮抓到真 bug——`hbot serve` 少一条事件桥（业务 emit 到 `EventBus`，WS 只认 `event_sink()`，桌面壳靠 `spawn_event_pump` 所以只有无头宿主坏），已补 `forward_events` 并加回归测试；零配置上手也补了两处——`project_create` 现在自带一条 `local` 的 `bots.json` 条目、`bot_list` 合并 `bots.json` 的声明（否则「新建项目 → 点启动」必 -32002、「试跑」页签永远是空的），`docs/quickstart-desktop.md` 是配套的九节指南。压测的**长时轮也已经跑完**：`scripts/soak-serve.ps1 -Minutes 120` 九条判据全 PASS（内存斜率 0.17 MB/10 min、峰值 7.2 MB、p95 41 ms、max 136 ms、回复 1438/1438、error 行 0、db 1.6 MB、WAL 峰值 4.0 MB、强杀后 10 ms 消失），写进 `docs/spikes/S15-soak.md`；五个 commit 之后 CI 在 head `e5fdfad` 上**连续第九次全绿**（run `44808915396`，workflow 级 `success`）。 同轮还按 DSH 的形态补了**可读性基础设施**：17 个 crate、12 个 web feature 目录与 `apps/web/src` 各有一份 `README.md`（职责 / 关键文件 / 边界 / 上下游 / 测试），新增 [`docs/10-plugin-model.md`](10-plugin-model.md) 讲清编译期 seam 与组合根、以及为什么不做运行时 `dlopen`；`AGENTS.md` §2 的目录地图因此扩成四列（多一列「说明书」）。此外把 `docs/07` 里记成「未实测」的 Windows DPI 虚拟化补成了实测（150% 缩放下 Unaware 报 96/逻辑像素、PMv2 报 144/物理像素），并修掉了一条只在 100% 缩放下成立的假断言（坑 141）。


> **2026-11-21 20:40 补充：界面改成 bot 工作台。** 用户要求按参考实现 Mino 的形态改，**右侧 Agent 栏不动**。活动栏上半段换成六个 bot 视角的面（概览 / Bot / 能力 / 会话 / 运行态 / 渠道，`apps/web/src/features/workbench/`），下半段保留原 IDE 视图；`SidebarView` 从 2 值扩成 8 值（`apps/web/src/lib/layout.ts`），默认面改成 `overview`。设计语言由 [11-ui-language-bot-workbench.md](11-ui-language-bot-workbench.md) 给判据（Q1–Q5 + 17 条不可借用项），顺手修掉它指出的三个可达性缺陷：主按钮白字压 accent 只有 3.22 → 拆成 `--accent`（文字 5.50）/ `--accent-fill`（白字 4.74）；`--fg-dim` 当正文 3.68/3.37 → 4.91/4.50；`.status-dot--stopping` 与 `--degraded` 同色同形 → 降级改成空心环。四个面只读，渠道面**压根不读** `bots.json` 的 `token` / `clientSecret`。同轮修掉一个真瑕疵：没打开项目时底部面板与文件树常驻挂载各发一条命令，首屏状态栏必须挂两条 `-32002` 红字 —— 现在 `projectRoot === null` 时一律不发。落地口径见 [06-ide.md](06-ide.md) §2.1；web 测试 47 文件 427 → **49 文件 438 用例**。

---

## 0. 决策权在父代理（最重要的一条）

- **父代理（lead）决策**：优先级、任务拆分、是否派发子代理、验收口径、什么时候算做完。
  **不要为了"先从哪一段开始"回来问用户。** 自己排序、自己开工、自己宣布下一步。
- 只有四种情况停下来问：**不可逆操作 / 安全敏感 / 工作区外副作用 / 需求歧义**。
- 子代理只执行不决策。任何子代理终报都必须由 lead **亲自复跑关键验收**，不过就打回重做。
- 用户明确说过：**"父代理是决策的"**、**"预算不设上限"**、**"先做完吧"**。
- 汇报格式：一张表（单元 / 状态 / 证据：命令 + 退出码 + 数量 / 未过原因）。
  状态汇报要短，不要自问自答式长文 —— 我犯过一次，用户直接叫停。

---

## 1. 项目是什么

Agent-first 的 **bot IDE**：用户在 IDE 里让 Agent 写 TypeScript bot 代码，核心用 Rust 跑这份代码，
目标是支持任何 bot（聊天 / 游戏 / 客服）、三平台桌面 + Linux 无头后台 + 手机（最后一段是设计稿）。

- 新仓库：`E:\HBot-Next`（commit 数见 `git rev-list --count HEAD`、remote = `https://github.com/haijiawe/hbot.git`（2026-11-21 起 public），CI 已连续十次全绿）。
- 旧项目：`E:\H Bot 制造` —— **只读，一个字节都不许改**，只在需要考古时参考（cwd 常常就是它，别手滑）。
- 技术栈（ADR-0002，状态仍 **Proposed**）：Rust 2024 + Tauri 2 同进程（无 sidecar）+
  React 19/TS 仅作 IDE 前端 + QuickJS（rquickjs）+ SQLite（rusqlite，单写者）+ SWC 去类型。
- **产品运行时零 Node、零 Go**。Node 只出现在 `apps/web` 的构建/测试链（vite / vitest / pnpm）。
- 规模：Rust 约 **124k 行**（16 crate + `xtask`）、web **22,667 行**；
  契约 **35 条命令 / 16 个事件 / 9 个 Agent 工具**；迁移 `0001`–`0010`。

---

## 2. 工作方式

1. **先冻结对接面**（签名 / 字段 / 事件名 / 写域）→ 切真正可并行的单元 → **一次性同时派发** →
   集成 → 联调验收 → 报表。契约不冻结就派发，子代理会互相踩。
2. 预算不设上限。
3. 子代理纪律（派发时逐条写进任务书）：
   - 各用独占 `CARGO_TARGET_DIR=E:\hbot-target\<name>`；
   - 长命令重定向到日志文件；
   - 只许 `cargo fmt -p <crate>`（**禁 `--all`**）；
   - 禁 `git add` / `git commit` / `git checkout` / `git stash`；
   - 禁新增依赖；禁 `#[ignore]`、禁删断言、禁放宽闸门；
   - 改 `hbot-core` 公开结构后先 `cargo check --workspace --all-targets` 找下游红点，
     **在终报里列清单交给 lead 补，自己不要改别人的 crate**。
4. **子代理会失败**（2026-11-18 那批：4 个无终报失败且零产物、1 个被在途中间态卡了 90 分钟）。
   所以：**冻结契约后先自己写核心那一侧**，子代理只在真正独立的部分派；拿到"failed"就
   亲自盘点产物、自己接手，别指望换个 prompt 就成功。

---

## 3. 六道闸门（改完必须全绿，缺一不算改完）

```powershell
cd E:\HBot-Next; $env:CARGO_TARGET_DIR='E:\hbot-target\gate'
cargo fmt --all -- --check
cargo run -q -p xtask -- gen --check     # 生成物一致；注意是 run -p xtask --，没有 cargo xtask 别名
cargo run -q -p xtask -- deps            # 依赖方向；当前 17 crate / 57 边 / 0 违规
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo test -j 2 --workspace              # 当前 2053 passed / 0 failed / 128 target
pnpm --dir apps/web run check            # 当前 49 文件 / 438 用例
```

现状：**六道全绿**（2026-11-21 20:40 实测，`rustc 1.100.0`，2053 passed / 128 target / web 49 文件 438 用例）。
**Rust 版本已钉死**：`rust-toolchain.toml` 的 `channel = "1.100.0"` 与 `ci.yml` 四处 `dtolnay/rust-toolchain@1.100.0`
必须一起改（原来两处都写 `stable`，导致 CI 的 clippy 比本地新、只报在 CI，见 `docs/pitfalls.md` 坑 115）。
`apps/web/src/generated/*.ts` **要入库**；
改 `dto` 后必须 `gen`（还要 `gen --check`）再提交。
纯文档提交不需要跑全闸，但要在提交信息里说明"仅文档"。

---

## 4. 代码地图

`crates/` 下 16 个（`deps` 报 17 个构建单元，多的是 `xtask`）：

| crate | 职责 |
|---|---|
| `hbot-core` | 纯契约与端口（`bot_state.rs` / `bot_ai.rs` / `bot_timer.rs` / `bot_sql.rs` / `device.rs` / `debug.rs` / `command.rs` / `ports.rs` / `redact.rs`） |
| `hbot-store` | SQLite、迁移（`0001`–`0007`）、仓储（每条 SQL 只出现在 `repositories/`）、每 bot 的 `bot_sql/*.db`（`bot_sql.rs`） |
| `hbot-workspace` | 项目文件、路径守卫、搜索、符号 |
| `hbot-code` | SWC 编译、QuickJS isolate、沙箱、宿主 API（`runtime/{host,state,ai,device,sql}_api.rs`）、插桩与调试 |
| `hbot-bot` | 事件 JSON、会话、有界队列、supervisor |
| `hbot-adapters` | 平台适配器（`onebot11` / `qq_official` / `local`） |
| `hbot-command` | 命令隔离（Windows Job/受限令牌、Linux Landlock、CPU/内存/进程数/IO 限额） |
| `hbot-device` | 截屏 / 多显示器 / 按窗口截图 / OCR（含中文）/ 输入 / 剪贴板 / 每屏 DPI（Windows 实现，其它平台 fail-closed） |
| `hbot-agent` | `ToolGateway`（八步，`Capability` 是第一步）+ 9 个工具 + 审批 |
| `hbot-replay` | 回放内核 + 夹具导出/不查库重跑 |
| `hbot-rpc` | DTO、`dispatch`（45 个分派臂）、`AppState` 装配、TS 生成、各能力的 `*Source` |
| `hbot-parity` / `hbot-redteam` / `hbot-bench` | 行为对照 / 红队 / 基准 |
| `hbot-cli` / `hbot-desktop` | `hbot serve` / `replay` / `fixture` 等子命令 / Tauri 壳 |

- 契约唯一真源：`crates/hbot-rpc/src/dto/{command,event,common}.rs`
  （serde `tag="type"`，type 值 snake_case、字段 camelCase）；错误码 `-32001`..`-32006`。
- 宿主 API 的唯一真源是四处同批改：`host_api.rs` 的 `HOST_BINDING_KEYS` → `install()` 的 `host.set`
  → `HOST_MODULE_SOURCE` 的 `export const X` + `export default {...}`（按逗号切、不许换行）
  → `HOST_DTS`；再进 `isolate.rs` 的 `build_event`。漏一处就红在别处。

### 各能力当前状态（一句话）

| 能力 | 状态 |
|---|---|
| `C1.message` / `C3.reply` / `C3.http` | 已实现（早于本轮交接） |
| `C2.kv` / `C2.history` | 已实现（2026-11-18，`docs/07` §5.0） |
| `C5.ai`（`ctx.ai.complete`） | 已实现（2026-11-18，`docs/07` §3.3） |
| `C1.timer`（`onTimer`） | 已实现（2026-11-18，`docs/07` §4.4） |
| `C6.device` | 窄版已实现（Windows，`docs/09`）；按窗口截图 2026-11-20 补齐 |
| `C4.jobs`（`ctx.jobs` / `onJob`） | **已实现（2026-11-19，`docs/07` §4.5）** |
| `C2.session`（`bot_waits`） | **已实现（2026-11-19，`docs/07` §5.2）；超时重发提示 `reminders` 2026-11-20 补齐** |
| `C3.send` / `C3.notify` | **已实现（2026-11-19，`docs/07` §3.4）；附件 `ctx.sendImage` / `ctx.sendFile`（OneBot `base64://` 段 + 宿主根内/存在/5 MiB 校验）2026-11-21 补齐，见 §3.4.1** |
| `C1.webhook` | **已实现（2026-11-19，`docs/07` §3.5）** |
| `onStart` / `onStop` | **已实现（2026-11-19，`docs/07` §4.6）；`shutdown` 原因 2026-11-20 接上** |
| `C2.sql`（`ctx.sql.query` / `ctx.sql.exec`） | **已实现（2026-11-20，`docs/07` §5.3；每 bot 一个库文件）** |
| `C3.file`（`ctx.files`） | **已实现（2026-11-20，`docs/07` §3.6；读写分开声明、路径钉项目根、每 bot 磁盘配额 `limits.quotaMb`）** |
| `onSchedule`（旧名 `onTimer`，同一条执行路径） | **已实现（2026-11-20，`docs/07` §4.4.1；两个名字只许写一个）** |
| `E1` npm 打包 / 云端宿主 | **未开始** |

---

## 5. 必须知道的坑（挑最贵的十二条，全表见 `docs/pitfalls.md` 的 142 条）

1. **PowerShell 改 Rust 源码会静默毁文件**。三类事故都发生过：
   `Get-Content -Raw` + `.Replace()` 时双引号里的反引号+字母被当转义（`` `r `` → 回车）；
   `-replace` 的正则/空白差异导致替换**静默不生效**（我做变异验证时没落地，差点误判"测试抓不到"）；
   数组按下标插删行时索引**漂移**（把 `pub mod debug;` 整行删掉、`Self {…}` 丢掉三个字段、
   插出重复的 `ai: None,`）。
   **规则**：改 Rust 源码一律用 `edit` 工具（按字面量匹配）；每次改完 `git diff` 逐 hunk 复查。
2. **新增 `Option<T>` 配置字段时 `new()` 必须从 `config` 取值**，照抄相邻字段的 `None`
   会静默丢掉整个能力接线（`state` 就整条丢过一次，5 条端到端测试全报 `UNAVAILABLE`）。
3. **`Ctx<'js>` 不许 clone 进 `Async` 闭包**，否则进程退出时撞
   `Assertion failed: list_empty(&rt->gc_obj_list)`、退出码 `0xc0000409`。
   绕法：把 `Ctx<'js>` 作**首参**取回（`FromParam` 的 `param_requirement()` 是 `none()`，不占位置参数）。
4. **`String(error)` 多一层 `Error: ` 前缀**，JS 侧断言要用 `error.message`。
5. **`MessageCtx` 里对象属性必须 ``readonly device: DeviceApi;``（名字含空格）**，
   两个 d.ts 扫描器靠"名字不是纯 alnum"跳过它；写成 `device: DeviceApi;` 会同时红两个 crate、
   而报错完全看不出原因。
6. **负向清单测试按字符串匹配**：`HOST_DTS` 里写"`onStart` 仍未实现"这句话本身就让
   `hbot_dts_does_not_promise_unimplemented_apis` 红。要说明没实现就换个说法。
7. **给 `SandboxHandler` 加能力要加五处**：`Config` 字段 / `new()` 里从 config 取 / `worker()`
   里 `set_*` / `start_bot` 字面量 / `hbot-parity/tests/support/mod.rs` 字面量。缺一处就"看着能用、
   其实没接上"。**判据**：每个能力都要有一条"没声明 → 报 `NOT_DECLARED` 而不是 `UNAVAILABLE`"的
   dispatch 级测试，它同时证明接线真的发生了。
8. **`BotRegistry` 里的后台循环必须登记进 `aux`**，`bot_stop` 会先 abort 辅助任务再发停止信号。
   不登记就是泄漏，且它每轮可能正持槽位锁、拖住 supervisor 排空。
9. **等效测"耗时应被抵账"必须用忙等，不能用 `ctx.sleep`** —— `sleep` 会被夹到剩余预算，测不出抵账。
   变异验证时删掉 `deadline.arm(left)` 那条测试必须立刻红。
10. **`include_str!` 的 SQL 做变异验证后复制还原会带回旧 mtime**，cargo 判 up-to-date，
    二进制里还是变异内容、下轮报假红。还原后必须刷 mtime + `cargo clean -p <crate>`。
11. **别用 `git rebase --onto` 当橡皮擦改非 HEAD 提交信息** —— 曾把一整个 commit 从历史里丢掉，
    靠 `git cherry-pick` 才救回。
12. **没真机证据就不要在文档里写"已验证"**。典型：Landlock 的 `handled_access_fs = 0` 意思是
    "什么都不管"，曾把放行掩码当接管掩码，限制层装上了等于没装，而所有测试都是绿的。

---

## 6. 已完成（不要重做）

| 段 | 进度 | 说明 |
|---|---|---|
| M0 地基 | 全绿 | 16 crate 骨架、单组合根、契约链、六道闸门、ts-rs 生成物入库 |
| M1 Agent + IDE | ~95% | Agent 写代码、Diff 逐块接受、编译回灌、崩溃不影响其它 bot、关窗继续跑 |
| M2 真实平台 | ~80% | QQ 官方适配器**真机端到端**（DB 有 4 入站 / 4 出站证据）；OneBot 只对假网关验过 |
| M3 IDE 深度与调试 | ~95% | 插桩调试 **4.64×**（上限 5×）、条件断点、日志点、回放、夹具导出与不查库重跑、行内编辑 `Ctrl+K`、问题面板 `Fix`/`Ask Agent to fix`、整组撤销 |
| M4 隔离与红队 | ~60% | 红队 8 条 findings 全修 + CPU/内存/进程数/IO 限额；Windows 有隔离；Linux 只交叉编译 |
| M5 打包与后台 | ~70% | Windows NSIS 9.14 MiB、冒烟 11/11；systemd/tray 未真跑 |
| C6 设备能力（窄版） | 完成 | 截屏 / 多显示器 / 原生 OCR（含中文）/ 输入 / 剪贴板 / 限时会话 / 一键撤销 / 审计 / 限速 / 每屏 DPI + Tauri 壳 PMv2 声明 |
| M7 第 1 步 | 完成 | `ctx.kv` / `ctx.history`（`bot_kv` 表、三道判据、参数闸唯一一份） |
| M7 第 2 步 | 完成 | `ctx.ai.complete`（四道判据、每事件 4 次 / 60 秒预算、等待抵账、key 不出核心） |
| M7 第 3 步 | 完成 | `C1.timer`（2026-11-18）+ `C4.jobs`（2026-11-19，`bot_jobs` 表 / `onJob` / 重试退避 / `bot_stop` 取消 / 重启恢复） |
| M7 第 4 步 | 完成 | `C2.session`（2026-11-19，`bot_waits` 表 / `ctx.session.prompt` / `onResume` / 超时结算 / `bot_stop` 取消） |
| M7 第 5 步 | 完成 | `C3.send` / `C3.notify`（2026-11-19）+ `C1.webhook`（2026-11-19，`WebhookServer` 手写 HTTP/1.1 / 三种鉴权失败统一 401 / 返回值即应答） |
| M7 第 5.5 步 | 完成 | `C3.file`（2026-11-20，`docs/07` §3.6，项目内文件读写 / 路径钉根 / 只删空目录）+ `onStart`/`onStop`（2026-11-19，`docs/07` §4.6，含 2026-11-20 接上的 `shutdown` 触发点）+ `C2.sql`（2026-11-20，`docs/07` §5.3，每 bot 一个 `<data_dir>/bot_sql/<bot_id>.db` / 运行时禁 DDL / 迁移不阻断启动）；同日补 `C3.file` 的递归删除 / `readBytes` / 按行写、webhook 路由级超时 `timeoutMs`、作业池上限 `limits.maxPendingJobs` |
| M7 出口标准（示例项目） | 自动化那一半完成 | `examples/chat` / `examples/game` / `examples/support`（无平台凭据，`platform.kind = local`）；`crates/hbot-cli/tests/examples.rs` 逐个跑 `check_project`（清单 → 编译 → `hbot.d.ts` 未漂移）并钉死名字；`cargo xtask gen` 刷新它们的 `hbot.d.ts`、`gen --check` 拦漂移。**整体 e2e 2026-11-20 补齐**：`crates/hbot-rpc/tests/examples_e2e.rs` 把 chat / game / support 三个示例复制进临时项目，走「启动 bot → 发消息 → 断言回复」（chat `/count` 只走 kv、game `/score`+`/top` 真跑 `migrations/0001_init.sql`、support `/ticket` 走 `ctx.session.prompt` → `onResume` → `ctx.sql`）；整条 `M7` 仍不标完成，因为 V10/V11/V15 归 M9/M10 |

---

## 7. 剩余工作（按建议顺序，A 最优先）

### A. `C4.jobs` —— **已落地 2026-11-19**（不要再重做）

落地形状与设计决定写在 `docs/07` §4.5；这里只记"下一轮碰它时别再踩的六件事"：

- 派发**只许**走 `claim_due_jobs_for_bot`（全局 `claim_due_jobs` 会让 A 跑 B 的作业，见 `AGENTS.md` 坑 68）；
- 作业 tick 是 `SandboxHandler::tick_jobs`，**不能**复用 `tick`（它第一句是"没有计划就 return"，坑 69）；
- `tick_jobs` 里**先 `compiled()` 后 claim**（顺序反了，编译失败会把行钉在 `running` 里直到下次重启，坑 70）；
- 预算按**行**冻结、`retries` 按清单现读（坑 72）；
- `cancel_jobs_for_bot` 只动 `pending`/`running`，终态不碰（坑 73）；
- `hbot_core::JobId` 与 `hbot_store::JobId` 是两个类型，转换用 `from_uuid(*id.as_uuid())`（坑 71）。

**宿主级预算上限 2026-11-21 已落地**：判据只放在 `hbot_core::effective_job_budget_ms(row, ceiling)`（`0` = 不限、结果夹在 `MIN..=MAX`、返回两者较小的那个，**只收紧不放宽**），宿主取值来自 `bots.json` 的 `limits.jobBudgetMs`（`hbot-code::Limits::job_budget_ms()`，缺省 = `MAX_JOB_BUDGET_MS`，所以不写这个字段的部署行为不变）。
作业池"上限可配"**2026-11-20 已落地**：`hbot.json` 的 `limits.maxPendingJobs`（缺省 256 = `hbot_core::MAX_JOBS_PER_BOT`，硬上限 1024，越界报错不夹取），容量闸与预算查询共用同一次 `manifest::load`；e2e 在 `crates/hbot-rpc/tests/jobs_capability.rs` 的 `max_pending_jobs_in_the_manifest_caps_the_queue`。

> **测这条必须用忙等**：`ctx.sleep` 会被剩余预算夹短后**正常返回**，用它测预算会得到假绿（`docs/pitfalls.md` 坑 137）。两条 e2e：`the_host_job_budget_ceiling_cuts_a_longer_declared_job`（上限 1 秒掐断清单里 60 秒、忙等 3 秒的作业，落 `failed` 且不写结束行）与对照组 `a_job_within_the_default_host_budget_runs_to_completion`。

> **2026-11-20 补：作业状态面板已落地**（底部面板「作业」页签，数据来自只读命令 `bot_jobs`/`bot_waits`）。

### B. `C2.session` —— **已落地 2026-11-19**（不要再重做）

落地形状与设计决定写在 `docs/07` §5.2。下一轮碰它时先记住这五条：

- **`prompt` 是注册不是阻塞**（按字面的 `await` 写会把同会话队列死锁，坑 74）；
- 唯一性靠 `0007` 的**部分唯一索引**（坑 75）；
- `claim` / `expire` 各一条 `UPDATE … RETURNING`（坑 76）；
- `onResume` 的 ctx 是第三个上下文，不挂 `kv`/`history`/`device`/`jobs`/`session`（坑 77）；
- `IdGenerator` 加了 `wait_id`，三处测试替身要一起改（坑 78）。

**还没做的**：把等待转交给别的 bot。
> 等待状态面板 2026-11-20 已落地（与作业面板同一个「作业」页签，见上）。

**2026-11-20 补：超时重发提示已落地**（`reminders: 0..=3`，缺省 0 = 旧行为）。三个判据：额度与单轮超时落在行上（`0008` 的 `reminders_left` / `timeout_ms`）；新 `deadline` 从 **now** 起算（从旧值加会得到仍过期的时刻，下一拍立刻再发一遍）；`promote` 只认 `reminders_left > 0`、`expire` 带 `AND reminders_left = 0`，同一行不会在同一拍既重发又判超时。

### C. M7 第 5 步 —— **全部已落地 2026-11-19**（`C3.send`/`C3.notify` + `C1.webhook`，不要再重做）

`docs/07` §3.4（send/notify）与 §3.5（webhook）各有五层判据表与设计决定。下一轮碰它们时先记住这几条：

- `send` 复用 `replies` 通道、`reply_to` 恒 `None`；**必须先补建 `conversations` 行**（外键，坑 79），
  补建用 `INSERT OR IGNORE` 而不是嗅探错误文本（坑 80）；
- `notify` 的收件人钉在清单里，调用点改不了（坑 81）；
- webhook **不进 `inbound`**、handler 的返回值就是 HTTP 应答（坑 83）；
- webhook 鉴权的三种失败统一 401，密钥只写环境变量名（坑 84）；
- `Ctx::json_parse` 吃 `Value` 不吃 `String`（坑 85）；`WebhookServer` 的 listener 一旦 move 进接受循环，
  `addr()` 就得靠绑定时存下的 `SocketAddr`（坑 86）。

**2026-11-20 补：路由级超时已落地**。`capabilities.webhook[].timeoutMs`（10..=2000，缺省用
`limits.handlerTimeout`，**只能收紧不能放宽**）一路透传到 worker，取 `min(清单值, 宿主预算)`；
超时走既有 `WebhookError::Timeout` → 504。e2e 在 `crates/hbot-rpc/tests/webhook_capability.rs`
（`a_route_timeout_cuts_a_slow_handler_short`，用**忙等**测，`ctx.sleep` 测不出 —— 坑 109）。

**还没做的两件**（都不阻塞主线）：应答体流式（**刻意不做**：handler 的返回值本来就是 JS 内存里的
字符串，流式只省传输层缓冲，却要新开一套分块 API；上限 256 KiB 写进文档）、反向代理/公网暴露
（手机做不到，见 `docs/08` §4.4）。

**刚落地的那两件（`onStart` / `onStop`，`docs/07` §4.6）**：失败语义**相反** —— `onStart` 抛错就是
启动失败（回滚到 stopped），`onStop` 抛错只落一行 error 日志；预算也不同（`onStart` 用 `handlerMs`，
`onStop` 用 5 秒）。**编译失败时两个钩子都跳过**，绝不能让 `bot_start` 因此失败（坑 88）。
`shutdown` 触发点**已接**（2026-11-20）：切项目、`hbot serve` 收到 Ctrl-C、桌面端退出一律走
`AppState::shutdown()` → `stop_all_with_reason(Shutdown)`，绝不裸调 `bots().stop_all()`（那会跳过
`onStop` 与 webhook 注销，坑 94）。**`crash` 触发点已接（2026-11-20）**：`read_loop` 现在把退出原因
（正常停机 / 订阅断开 / 入站落库失败 / 入队失败）作为 `ReadLoopExit` 返回并写进 `ReadExitSlot`，
`Supervisor::wait_read_loop_exit()` 是唯一读口，`hbot-rpc` 的 `start_inner` 起一个收尾任务等它、
看到「意外」就落一行 error + 按 `StopReason::Crash` 停机（坑 132）。**isolate 崩溃仍然不算 crash**：
它有 last-good 兜底（坑 24）、bot 还活着，那一刻调 `onStop(crash)` 会让清理逻辑跑在活着的 bot 上。
真机侧还没验（OneBot 客户端断线是无限重连，造不出这个事件）。
还剩：`E1` 打包、设备完整版、云端宿主、`M6`。

**刚落地的 `C2.sql`（`docs/07` §5.3）**：隔离靠**一个 bot 一个库文件**（`<data_dir>/bot_sql/<bot_id>.db`），
不是 `WHERE bot_id = ?` —— 后者漏一处就是静默跨 bot 越权（坑 89）。运行时**禁 DDL**，表只能来自
项目 `migrations/*.sql`，在 `bot_start` 里、`onStart` 之前执行一次、按文件名去重（坑 90）；
迁移失败**不阻断启动**（坑 24/88 同一纪律）。`query` 只认 `SELECT`（`WITH … DELETE` 也算写，坑 91），
JS 数字绑定 SQLite 时整数/浮点要分开（坑 92），写超 10000 行先 `rollback()` 再报错（坑 93）。

**刚落地的 `C3.file`（`docs/07` §3.6）**：`capabilities.state.files.{read,write}` **读写分开**（坑 97），
路径判据只在槽位层（坑 96）、后端只补符号链接逃逸；四个动词 `read`/`write`/`list`/`remove` 不给裸 fs；
读超限截断并标 `truncated`、写超 1 MiB 报错、非 UTF-8 报错、`remove` 只删空目录（坑 98）。
2026-11-20 补：**递归删除 / 二进制读 / 按行写**都落地了 —— `remove(path, { recursive })`（返回条目数，
逐层 `symlink_metadata`、遇符号链接一律拒、先扫后删、≤1000 条，坑 107）、
`readBytes(path, { maxBytes })`（回来是原样字节的 base64，不做 UTF-8 校验）、
`write(path, text, { startLine, endLine })`（只替换那几行，未触碰行按字节原样复制、行尾不重写，
同目录 tmp + `sync_all` + `rename` 有界重试；坑 108 / 111）。
2026-11-20 晚补：**每 bot 磁盘配额**落地 —— 清单 `limits.quotaMb`（0 = 不限，缺省 0；只判上界不夹取），
账本表 `bot_file_usage`（迁移 `0009`，一个 bot 一行，减到 0 删行），判据是**一条 SQL**
（`ON CONFLICT … WHERE`，因为同一 bot 的会话是并行的，读-判-写三步会让两个写都通过，坑 130）；
`write` 按长度差、`write_range` 先按上界预留再按真实大小 `settle`、`remove` 先数再删再退账。
超限报 `HBOT_FILE_QUOTA`（与 `HBOT_FILE_TOO_LARGE` 是两个码）。
还没做：QQ 官方接口没有媒体上传（附件在 OneBot 已落地，见下面 `C3.send` 那行）；多个附件、音视频/表情分类都不做。

2026-11-20 补：**按行范围读**已落地 —— `ctx.files.read(path, { startLine, endLine })` 逐行流式读（不再整文件进内存），一次 ≤ 10000 行、行号 1 起、`endLine` 含；越界/逆序报错不夹取；`truncated` 表示「后面还有没给你的」（撞字节上限或范围后仍有行），据此翻页。顺带修掉整文件读的截断点可能切在 UTF-8 字符中间、导致合法文件被报「不是 UTF-8」的老洞。

### D. 其它未做（不阻塞主线）

- `onStart` / `onStop`（含 `shutdown` 与 `crash` 三个原因）—— **已落地（2026-11-19 / 2026-11-20，`docs/07` §4.6）**，别再重做；
  `crash` 只由「读循环意外结束」触发，**不把 isolate 崩溃算进来**是刻意的（理由见上面 §7 C 段末尾）；
- `C3.file`：主链路、按行读写、二进制读、递归删除、**每 bot 磁盘配额**（2026-11-20）都已落地，见 `docs/07` §3.6；**附件出站 `ctx.sendImage` / `ctx.sendFile` 也已落地（2026-11-21）**，见 `docs/07` §3.4.1；
- `onSchedule` —— **已落地（2026-11-20，`docs/07` §4.4.1）**：它就是 `onTimer` 的正式名，同一条派发路径、同一个 `capabilities.timer`；两个名字都写会启动失败。别再按「另起一条路径」实现；
- 设备完整版：混合 DPI 跨屏拼图、中文以外的 OCR 语言包（按窗口截图 2026-11-20 已落地，见 `docs/09` §11.1）；
- `E1` npm 打包、云端执行宿主（`docs/08` §9 / ADR-0006）。

---

## 8. 外部依赖（这些拿不到就别硬等，能绕就绕）

| 需要什么 | 影响 | 现在怎么办 |
|---|---|---|
| ~~git remote~~ | 已有（`haijiawe/hbot`，**2026-11-21 起 public**），**CI 历史上连续十次全绿；最近四次（head `d0132d2` 的 run `44788353950`、head `bbb5914` 的 run `44789828206`、head `e5fdfad` 的 run `44808915396`、head `c84d04d` 的 run `44822773155`）都是 workflow 级 `success`**（macOS job 改手动后默认 `skipped`，不再把整轮记为 `cancelled`） | 最近五次：run `44730056833`（head `b9dc9ba`，首次全绿）、run `44735231807`（head `d0662e4`）、run `44738414777`（head `9648800`）、run `44747851031`（head `20caba5`，每 bot 磁盘配额）与 run `44756454957`（head `9a035b8`，`onStop(crash)` 触发点）。七个 job：`gates` / `web` / `bundle (ubuntu)` / `bundle (windows, nsis)` / `rust (ubuntu-latest)` / `rust (windows-latest)` 全 success（`bundle (macos, dmg)` 仍是 `cancelled` —— 矩阵里没有 macOS runner，属正常）。首跑抓到的 10 处红点全部修完：① CI clippy 1.100 的 `result_large_err`（装箱）；② 工具链钉死（`rust-toolchain.toml` + `ci.yml` 都写 `1.100.0`，见坑 115）；③ ubuntu-only `unused import`；④ 按窗口 OCR 夹具写死像素尺寸（坑 116）；⑤ ubuntu runner 磁盘耗尽（坑 118）；⑥ 设置 watcher 的 mtime 盲点（坑 117）；⑦ Linux 的 `RLIMIT_CPU` 软硬上限相等导致 `SIGXCPU` 永不出现、超限被杀当成正常退出（坑 119）；⑧ CI 上两条**测试自身的平台/时序假设**：把 Windows 的设备能力当跨平台契约（坑 120）、`wait_until` 只等「执行完」不等「落库完」（坑 121）；⑨ 安全用例把解释器写死成 `cmd`，Linux 上先撞 `ProgramNotFound`（坑 123）；⑩ 夹具窗口「按文本量宽度」仍超出 runner 的 1024 宽屏幕，被 `PrintWindow` 裁掉末尾字符（坑 124）；⑪ `create_dir("a.txt/sub")` 在 POSIX 给 `ENOTDIR`、Windows 给 `NotFound`，`io_error` 的兜底把「用户输入错」变成 `Internal`（坑 125，已给 `NotADirectory` 加 `Invalid` 分支）；⑫ `hot_reload_e2e` 的「时间下界」断言前提被 `bot_start` 自己那次 `stat` 满足掉（坑 126，预热前补 `sleep(TTL + 250ms)`）；⑬ `hbot-device` 的 `win32_probe` 断言绝对值 `count() == 0`，而它是进程级计数器、被并行用例抬到 6（坑 127，改成比增量）；⑭ `hbot-bot/tests/runtime.rs` 的 `wait_until` 只等「适配器发出去了」，而 `snapshot().outbox.sent` 是落库**之后**才 `fetch_add` 的（坑 128，等待条件补成「已投递 **且** 已记账」）。**push 必须带 `-c http.proxy=http://127.0.0.1:7897`**，直连 github.com:443 会被重置 |
| ~~GitHub Actions 计费~~ | 2026-11-21 00:23 曾因 private 仓库的 Actions 计费停摆（七个 job 2 秒内失败、无 runner、无日志，annotation 是 `The job was not started because recent account payments have failed or your spending limit needs to be increased`）| **已解决**：仓库改成 public（public 的 Actions 不计费），同一 run 的第 2 次尝试六个真实 job 全 success。判读这类失败用 `GET /repos/<owner>/<repo>/check-runs/<job_id>/annotations`，不用翻日志（会 `BlobNotFound`）|
| **macOS runner 容量** | ``CI 的 `bundle (macos, dmg)``` 未验：这台账号没有 macOS runner | 2026-11-21 读到真正原因（`check-runs/<job_id>/annotations`）：`The configuration 'macos-14-arm64-us-default' is not supported`；换 `macos-13` 后排队 35 分钟等不到 runner。该 job 已改成**手动触发**（Actions 页面 Run workflow，或 Variables 里设 `ENABLE_MACOS_BUNDLE=true`），默认 push 时是 `skipped`（≠ 通过）。有 Mac 机器或升级计划后再开 |
| **真 NapCat 扫码登录** | OneBot 11 只对假网关验过（真事件 JSON 形状、断线重连、`message_id` 语义、at 规则、去重键都没真机证据） | 实物在 `D:\机器人\QQ JQR\QQ JQR\napcat\NapCat.Shell.Windows.OneKey\`，坑与现状见 `docs/spikes/S11-napcat-local-fixture.md`；**二维码会过期、需先确认用户自己的 QQ 是否退出** |
| **Linux / macOS 机器** | Landlock 只交叉编译；macOS 命令隔离全无 | 交叉 `cargo check --target x86_64-unknown-linux-gnu`；别写"已验证" |
| **一份独立测试账号** | M6 迁移/影子（0%） | 跳过；M6 需要真实平台影子跑一周 |
| **有效 LLM key** | 20 条 Agent 评测集（`tests/real_llm_eval.rs`） | 已在 `%APPDATA%\hbot\agent.json`（Anthropic 契约，20/20 PASS，`docs/spikes/S10-llm-eval.md`）；换 key 只需改这个文件 |

**敏感信息位置（都不入库）**：LLM key 在 `%APPDATA%\hbot\agent.json`；QQ 官方凭据同在
`qq-official.json`。仓库里 `grep -r "sk-"` 必须为空。

---

## 9. 起手三步

```powershell
cd E:\HBot-Next
git log --oneline -5                                     # 确认 HEAD 是 shutdown 那一批或更新
$env:CARGO_TARGET_DIR='E:\hbot-target\gate'
cargo test -j 2 --workspace                              # 基线：2053 passed / 128 target（web：49 文件 438 用例）
```

1. 读 `docs/07-bot-runtime-blueprint.md` 的 **§3 能力表 + §3.4 send/notify + §3.5 webhook + §4.5 jobs + §4.6 lifecycle + §5.2 session + §5.3 sql + §11 落地顺序**（这是本轮的施工图）。
2. **M7 第 1–5.5 步已经全部完成**（`kv`/`history`/`ai`/`timer`/`jobs`/`session`/`sql`/`send`/`notify`/`webhook`/`onStart`/`onStop`），
   作业与等待的只读面板也已落地（底部面板「作业」页签，`bot_jobs`/`bot_waits`）。
   下一段按本文 §7 的 **D** 挑：**`E1` npm 打包已定为不做**（ADR-0007），别再把工期算到它上面；
   云端宿主（`M9`）与 `M6` 迁移需要有外部条件。先冻结 `hbot-core` 侧契约并写单测，
   再决定哪些部分派子代理 —— 参考 §2 第 4 条：**核心那一侧自己写**。
   注意子代理这条路的实测结果：2026-11-19 派的两个（`runlog-virtual` / `device-window`）一个没交终报、
   一个留下编译不过的残骸（`crates/hbot-device/src/windows.rs` 引用了不存在的类型，得 `git checkout` 回滚）。
   **给子代理的活要窄到"只加一个函数 + 它自己的测试"**，且必须在派之前就想清楚怎么验收。
3. 每做完一段：跑六道闸门 → 按 crate 拆 commit（`feat(core)` / `feat(code)` / `feat(rpc)` /
   `feat(web)` / `docs`，生成物并进相关 commit 让 `gen --check` 在每个 commit 都成立）
   → 回填 `docs/07` 对应小节的状态与「设计决定 + 理由」→ 更新 `README` 数字与 `AGENTS.md` 新坑
   → 回来更新本文件（§3 的数字、§4 能力状态、§7 剩余项）。
