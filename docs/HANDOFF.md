# HANDOFF —— 交接给下一个会话的 lead

> **本文件是快照，不是真源。** 真源永远是：
> `AGENTS.md`（坑与"改 X 去哪"）、`docs/04-implementation-plan.md`（里程碑与出口标准）、
> `docs/02-architecture.md`（架构与契约）。每次交接时更新本文件，冲突时以真源为准。
>
> 本快照时间：**2026-11-19 08:45**（工作树干净）。`C4.jobs`、`C2.session`、`C3.send`/`C3.notify` 都已落地。

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

- 新仓库：`E:\HBot-Next`（103 commit、工作树干净、**没有 git remote**，所以 CI 一次没跑过）。
- 旧项目：`E:\H Bot 制造` —— **只读，一个字节都不许改**，只在需要考古时参考（cwd 常常就是它，别手滑）。
- 技术栈（ADR-0002，状态仍 **Proposed**）：Rust 2024 + Tauri 2 同进程（无 sidecar）+
  React 19/TS 仅作 IDE 前端 + QuickJS（rquickjs）+ SQLite（rusqlite，单写者）+ SWC 去类型。
- **产品运行时零 Node、零 Go**。Node 只出现在 `apps/web` 的构建/测试链（vite / vitest / pnpm）。
- 规模：Rust 约 **124k 行**（16 crate + `xtask`）、web **22,667 行**；
  契约 **35 条命令 / 16 个事件 / 9 个 Agent 工具**；迁移 `0001`–`0007`。

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
cargo run -q -p xtask -- deps            # 依赖方向；当前 17 crate / 56 边 / 0 违规
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo test -j 2 --workspace              # 当前 1778 passed / 0 failed / 117 target
pnpm --dir apps/web run check            # 当前 46 文件 / 417 用例
```

现状：**六道全绿**（2026-11-19 08:33 实测）。`apps/web/src/generated/*.ts` **要入库**；
改 `dto` 后必须 `gen`（还要 `gen --check`）再提交。
纯文档提交不需要跑全闸，但要在提交信息里说明"仅文档"。

---

## 4. 代码地图

`crates/` 下 16 个（`deps` 报 17 个构建单元，多的是 `xtask`）：

| crate | 职责 |
|---|---|
| `hbot-core` | 纯契约与端口（`bot_state.rs` / `bot_ai.rs` / `bot_timer.rs` / `device.rs` / `debug.rs` / `command.rs` / `ports.rs` / `redact.rs`） |
| `hbot-store` | SQLite、迁移（`0001`–`0007`）、仓储（每条 SQL 只出现在 `repositories/`） |
| `hbot-workspace` | 项目文件、路径守卫、搜索、符号 |
| `hbot-code` | SWC 编译、QuickJS isolate、沙箱、宿主 API（`runtime/{host,state,ai,device}_api.rs`）、插桩与调试 |
| `hbot-bot` | 事件 JSON、会话、有界队列、supervisor |
| `hbot-adapters` | 平台适配器（`onebot11` / `qq_official` / `local`） |
| `hbot-command` | 命令隔离（Windows Job/受限令牌、Linux Landlock、CPU/内存/进程数/IO 限额） |
| `hbot-device` | 截屏 / 多显示器 / OCR（含中文）/ 输入 / 剪贴板 / 每屏 DPI（Windows 实现，其它平台 fail-closed） |
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
| `C6.device` | 窄版已实现（Windows，`docs/09`） |
| `C4.jobs`（`ctx.jobs` / `onJob`） | **已实现（2026-11-19，`docs/07` §4.5）** |
| `C2.session`（`bot_waits`） | **已实现（2026-11-19，`docs/07` §5.2）** |
| `C3.send` / `C3.notify` | **已实现（2026-11-19，`docs/07` §3.4）** |
| `C1.webhook` | **未开始** ← 下一步（M7 第 5 步剩下一半） |
| `onStart` / `onStop` / `onSchedule` | **未开始** |
| `E1` npm 打包 / 云端宿主 | **未开始** |

---

## 5. 必须知道的坑（挑最贵的十二条，全表见 `AGENTS.md` 的 82 条）

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
| M7 第 5 步 | 一半 | `C3.send` / `C3.notify`（2026-11-19，`bot_egress` 契约 / 唯一出口复用 `replies` / `notify` 目标钉清单）；**`C1.webhook` 未开始** |

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

**还没做的三件**（不阻塞主线）：宿主级预算上限（手机 30 秒"平台取小"仍在 `docs/08` 的设计里）；
作业状态面板；作业池"上限可配"（现在恒为 256 行 + 每拍认领 `MAX_ISOLATES` 条）。

### B. `C2.session` —— **已落地 2026-11-19**（不要再重做）

落地形状与设计决定写在 `docs/07` §5.2。下一轮碰它时先记住这五条：

- **`prompt` 是注册不是阻塞**（按字面的 `await` 写会把同会话队列死锁，坑 74）；
- 唯一性靠 `0007` 的**部分唯一索引**（坑 75）；
- `claim` / `expire` 各一条 `UPDATE … RETURNING`（坑 76）；
- `onResume` 的 ctx 是第三个上下文，不挂 `kv`/`history`/`device`/`jobs`/`session`（坑 77）；
- `IdGenerator` 加了 `wait_id`，三处测试替身要一起改（坑 78）。

**还没做的三件**：超时**重发提示**（现在只落一行 error）；等待状态面板；把等待转交给别的 bot。

### C. M7 第 5 步 —— `C3.send`/`C3.notify` **已落地 2026-11-19**，`C1.webhook` **未开始**

**已完成那一半**（`docs/07` §3.4 有五层判据表与六条设计决定）：

| 片 | 落点 |
|---|---|
| 契约 | `crates/hbot-core/src/bot_egress.rs`：`SendRequest` / `NotifyRequest` / `NotifyTarget` / `SendError`（`HBOT_SEND_*`）/ `NotifyError`（`HBOT_NOTIFY_*`）/ 四个参数闸 |
| 槽位 | `crates/hbot-code/src/send.rs`（`SendSlot`，一个端口 `SendQueue::send` 同时喂 send 与 notify）+ `runtime/send_api.rs`（`ctx.send` / `ctx.notify` 两个平铺函数） |
| 清单 | `capabilities.send: bool` 与 `capabilities.notify: { conversation, kind }`（`kind` 必填、`unknown` 在加载清单时就拒） |
| 装配 | `crates/hbot-rpc/src/app/runtime/send.rs` 的 `StoreSendQueue`（查/补建 `conversations` 行 → `replies.blocking_send`，`reply_to` 恒 `None`）+ `SandboxHandler::with_send` |
| e2e | `crates/hbot-rpc/tests/send_capability.rs`，7 条 |

**剩下那一半 `C1.webhook` 的形状**（尚未动工，先读这几条再动手）：

- 它是**入站端口 + 鉴权**：外部系统 POST 到本机的某条路由 → 转成一次 isolate 事件。
  `docs/07` §3 的能力表里 `webhook` 与 `message` 并列，但前者没有平台身份（没有 sender、没有 conversation），
  所以**不能**直接塞进 `inbound` 表那条按 `(bot_id, external_id)` 去重的管道 —— 去重键与回复目标都得新设计。
- 应答模型必须先回答 `AGENTS.md` 坑 74：handler 里等外部事件会死锁同会话队列。
  两种可选形状 —— ① 请求即事件（handler 处理完，返回值就是 HTTP 应答体）；② 请求注册 + 后续投递（像 `session.prompt`）。
  选 ①，别选 ②：webhook 调用方在等 HTTP 响应，注册式会把调用方晾在那儿。
- 鉴权不是可选项：`docs/02-architecture.md` §6 的边界是 127.0.0.1 + token。webhook 路由至少要带**密钥**（清单里声明，走 keyring 或环境），
  且**不许**因为"是本机"就免鉴权 —— 本机任意进程都能发 POST。

### D. 其它未做（不阻塞主线）

- `onStart` / `onStop`（生命周期钩子，`docs/07` §4.1 目标形状里已有）；
- `onSchedule`（与 `onTimer` 不是同一件事）；
- 设备完整版：混合 DPI 跨屏拼图、按窗口截图、中文以外的 OCR 语言包；
- `E1` npm 打包、云端执行宿主（`docs/08` §9 / ADR-0006）。

---

## 8. 外部依赖（这些拿不到就别硬等，能绕就绕）

| 需要什么 | 影响 | 现在怎么办 |
|---|---|---|
| **git remote** | CI 一次没跑过（三平台矩阵是设计） | 继续本地跑六道闸门，别在文档里说"CI 绿" |
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
git log --oneline -5                                     # 确认 HEAD 是 3ded642 或更新
$env:CARGO_TARGET_DIR='E:\hbot-target\gate'
cargo test -j 2 --workspace                              # 基线：1778 passed / 117 target
```

1. 读 `docs/07-bot-runtime-blueprint.md` 的 **§3 能力表 + §3.4 send/notify + §5.2 session + §11 落地顺序**（这是本轮的施工图）。
2. 按本文 §7 的 **C** 开工（`C1.webhook`），先冻结 `hbot-core` 侧契约并写单测，
   再决定哪些部分派子代理 —— 参考 §2 第 4 条：**核心那一侧自己写**。
   注意子代理这条路的实测结果：2026-11-19 派的两个（`runlog-virtual` / `device-window`）一个没交终报、
   一个留下编译不过的残骸（`crates/hbot-device/src/windows.rs` 引用了不存在的类型，得 `git checkout` 回滚）。
   **给子代理的活要窄到"只加一个函数 + 它自己的测试"**，且必须在派之前就想清楚怎么验收。
3. 每做完一段：跑六道闸门 → 按 crate 拆 commit（`feat(core)` / `feat(code)` / `feat(rpc)` /
   `feat(web)` / `docs`，生成物并进相关 commit 让 `gen --check` 在每个 commit 都成立）
   → 回填 `docs/07` 对应小节的状态与「设计决定 + 理由」→ 更新 `README` 数字与 `AGENTS.md` 新坑
   → 回来更新本文件（§3 的数字、§4 能力状态、§7 剩余项）。
