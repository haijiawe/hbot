# HANDOFF —— 交接给下一个会话的 lead

> **本文件是快照，不是真源。** 真源永远是：
> `AGENTS.md`（坑与"改 X 去哪"）、`docs/04-implementation-plan.md`（里程碑与出口标准）、
> `docs/02-architecture.md`（架构与契约）。每次交接时更新本文件，冲突时以真源为准。

---

## 0. 决策权在父代理（最重要的一条）

- **父代理（lead）决策**：优先级、任务拆分、是否派发子代理、验收口径、什么时候算做完。
  **不要为了"先从哪一段开始"回来问用户。** 自己排序、自己开工、自己宣布下一步。
- 只有四种情况停下来问：**不可逆操作 / 安全敏感 / 工作区外副作用 / 需求歧义**。
- 子代理只执行不决策。任何子代理终报都必须由 lead **亲自复跑关键验收**，不过就打回重做。
- 汇报格式：一张表（单元 / 状态 / 证据：命令 + 退出码 + 数量 / 未过原因）。
  状态汇报要短，不要自问自答式长文。

---

## 1. 项目是什么

Agent-first 的 **bot IDE**：用户在 IDE 里让 Agent 写 TypeScript bot 代码，核心用 Rust 跑这份代码，
目标是支持任何 bot（聊天 / 游戏 / 客服）、三平台桌面 + Linux 无头后台 + 手机（最后一段是设计稿）。

- 新仓库：`E:\HBot-Next`（当前 88 个 commit、工作树干净、**没有 git remote**，所以 CI 一次没跑过）。
- 旧项目：`E:\H Bot 制造` —— **只读，一个字节都不许改**，只在需要考古时参考。
- 技术栈（ADR-0002，状态仍 **Proposed**）：Rust 2024 + Tauri 2 同进程（无 sidecar）+
  React 19/TS 仅作 IDE 前端 + QuickJS（rquickjs）+ SQLite（rusqlite，单写者）+ SWC 去类型。
- **产品运行时零 Node、零 Go**。Node 只出现在 `apps/web` 的构建/测试链。

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

---

## 3. 六道闸门（改完必须全绿，缺一不算改完）

```powershell
cd E:\HBot-Next; $env:CARGO_TARGET_DIR='E:\hbot-target\gate'
cargo fmt --all -- --check
cargo run -q -p xtask -- gen --check     # 生成物一致；注意是 run -p xtask --，没有 cargo xtask 别名
cargo run -q -p xtask -- deps            # 依赖方向；当前 17 crate / 56 边 / 0 违规
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo test -j 2 --workspace              # 当前 1504 passed / 108 target
pnpm --dir apps/web run check            # 当前 45 文件 / 400 用例
```

现状：**六道全绿**。`apps/web/src/generated/*.ts` **要入库**；改 `dto` 后必须 `gen` 再提交。
纯文档提交不需要跑全闸，但要在提交信息里说明"仅文档"。

---

## 4. 代码地图

`crates/` 下 16 个（`deps` 报 17 个构建单元，多的是 `xtask`）：

| crate | 职责 |
|---|---|
| `hbot-core` | 纯契约与端口（`device.rs` / `debug.rs` / `command.rs` / `ports.rs` / `redact.rs`） |
| `hbot-store` | SQLite、迁移（`0001`–`0004`）、仓储 |
| `hbot-workspace` | 项目文件、路径守卫、搜索、符号 |
| `hbot-code` | SWC 编译、QuickJS isolate、沙箱、宿主 API、插桩与调试 |
| `hbot-bot` | 事件 JSON、会话、有界队列、supervisor |
| `hbot-adapters` | 平台适配器（`onebot11` / `qq_official` / `local`） |
| `hbot-command` | 命令隔离（Windows Job/受限令牌、Linux Landlock、限额） |
| `hbot-device` | 截屏 / OCR / 输入 / 剪贴板（Windows 实现，其它平台 fail-closed） |
| `hbot-agent` | `ToolGateway` + 9 个工具 + 审批 |
| `hbot-replay` | 回放内核 + 夹具导出/重跑 |
| `hbot-rpc` | DTO、`dispatch`、AppState 装配、TS 生成 |
| `hbot-parity` / `hbot-redteam` / `hbot-bench` | 行为对照 / 红队 / 基准 |
| `hbot-cli` / `hbot-desktop` | `hbot serve` 等子命令 / Tauri 壳 |

- 契约唯一真源：`crates/hbot-rpc/src/dto/{command,event,common}.rs`
  （serde `tag="type"`，type 值 snake_case、字段 camelCase）；错误码 `-32001`..`-32006`。
- 当前规模：**35 条命令 / 16 个事件 / 9 个 Agent 工具**。

---

## 5. 必须知道的坑（挑最贵的九条，全表见 `AGENTS.md` 的 52 条）

1. **PowerShell 改 Rust 源码会静默毁文件**：`Get-Content -Raw` + `.Replace()` 时双引号里的
   反引号+字母被当转义（`` `r `` → 回车），注释被撕成两行、import 替换因 CRLF/LF 不符静默失败。
   改用 `Get-Content` 数组按下标赋值，注释里别写反引号。
2. **`include_str!` 的 SQL 做变异验证后复制还原会带回旧 mtime**，cargo 判 up-to-date，
   二进制里还是变异内容、下轮报假红。还原后必须刷 mtime 并 `cargo clean -p <crate>`。
3. **别用 `git rebase --onto` 当橡皮擦改非 HEAD 提交信息** —— 曾把一整个 commit 从历史里丢掉，
   靠 `git cherry-pick` 才救回。
4. `MessageCtx` 里必须是 ``readonly device: DeviceApi;``（**名字含空格**），
   `hbot-code/src/dts.rs` 与 `hbot-rpc/tests/event_shape_dts.rs` 两个扫描器才会跳过它；
   写成 `device: DeviceApi;` 会同时红两个 crate 且报错看不出原因。
5. **断点与日志点是两张表**（`Breakpoints.files` / `.logs`），不能合成一张带 `kind` 的表 ——
   `set()` 是整份替换，合一表会让"只改条件"与"只改日志点"互相清掉。
   `DebugSink::logged` 有默认空实现，新 sink 忘了覆写时日志点在 UI 里**什么都不出现且不报错**。
6. **资源上限必须 fail-closed**：限额装不上就返回 `HBOT_COMMAND_*_LIMIT`，
   绝不静默降级成无限额。
7. **网络授权判据只有一份**（`hbot.json` 的 `permissions.http`，策略 C：清单写了什么就允许什么，
   含回环/私网）。别在适配层"顺手加一条 SSRF"，两道判据会打架。
8. **防漂移闸门要扫结构化常量表**，不能扫 `object.set("...")` 行首（rustfmt 折行会静默漏键）。
9. 卸载类、隔离类、沙箱类的改动，**没有真机证据就不要在文档里写"已验证"**。
   典型教训：Landlock 的 `handled_access_fs = 0` 意思是"什么都不管"，曾把放行掩码当接管掩码，
   限制层装上了等于没装，而所有测试都是绿的。

---

## 6. 已完成（不要重做）

| 段 | 进度 | 说明 |
|---|---|---|
| M0 地基 | 全绿 | 16 crate 骨架、单组合根、契约链、六道闸门 |
| M1 Agent + IDE | ~95% | Agent 写代码、Diff 逐块接受、编译回灌、崩溃不影响其它 bot、关窗继续跑 |
| M2 真实平台 | ~80% | QQ 官方适配器**真机端到端**（DB 有 4 入站 / 4 出站证据）；OneBot 只对假网关验过 |
| M3 IDE 深度与调试 | ~95% | 插桩调试 **4.64×**（上限 5×）、条件断点、日志点、回放、夹具导出与不查库重跑、行内编辑 `Ctrl+K`、问题面板 `Fix`/`Ask Agent to fix`、整组撤销 |
| M4 隔离与红队 | ~60% | 红队 8 条 findings 全修；Windows 有隔离；Linux 只交叉编译 |
| M5 打包与后台 | ~70% | Windows NSIS 9.14 MiB、冒烟 11/11；systemd/tray 未真跑 |
| C6 设备能力（窄版） | 完成 | 截屏 / 多显示器 / 原生 OCR（含中文）/ 输入 / 剪贴板 / 限时会话 / 一键撤销 / 审计 / 限速 / 每屏 DPI + Tauri 壳 PMv2 声明 |

---

## 7. 剩余（lead 自行按此顺序决策，不必回来问）

**A. M7 能力底座 —— 最大缺口，建议首先做。**
`ctx.kv` / `ctx.history` / `ctx.jobs` / `ctx.session` 全仓 **0 处引用**；`onSchedule` 只在 `d.ts`
里声明过、**没有运行时**；`schedules` 表不存在。后果：现在这产品只能做"被动回复机器人"，
客服 / 游戏 / 社群三类都缺底座。这比继续打磨 IDE 更影响"任何 bot"这个定位。

**B. M6 切换与影子 —— 0%。** 需要独立测试账号 + 真实平台影子跑一周 + 回滚演练，属外部依赖；
没有账号就把这一段挂起，先做 A。

**C. M3 唯一已知会真卡的地方。** `apps/web/src/features/run/run-log.tsx:27` 是 `logs.map()` 全量铺
`<li>`，10k 行就是 10k 个 DOM 节点，**没有虚拟滚动**。M3 那四条出口标准仍缺浏览器侧证据。

**D. M4。** Linux Landlock 只交叉编译过、一次没真跑（本机无 WSL distro）；macOS 完全没有隔离。

**E. M5。** 托盘脚本 `scripts/verify-tray-actions.ps1` 未执行、systemd 未真跑、只有 Windows 实测。

**F. 能力面。** Agent 工具 9 个（旧项目 47 个）；没有插件系统。

---

## 8. 外部依赖（不必问，先绕过；真需要登录/外部机器时提一句）

- **真 NapCat 扫码**：`D:\机器人\QQ JQR\QQ JQR\napcat\NapCat.Shell.Windows.OneKey\`，
  已配 OneBot 11 在 `127.0.0.1:3001`。二维码会过期，用过一次就得重新出码。
  口径：只用这个 WS 接口，适配器一行不改；不写进任何 `Cargo.toml`、不进 CI。
- **`git remote`**：没有，所以六道闸门只在本机跑过。CI 是设计里有、从未执行的。
- **Linux / macOS 机器**：Landlock 与 macOS 隔离的真机证据都缺。
- **M6 测试账号**：真实平台影子跑一周。

---

## 9. 起手三步

1. 读 `E:\HBot-Next\AGENTS.md`（52 条坑 + "改 X 去哪"）与 `docs/04-implementation-plan.md` 的里程碑表。
2. 跑一遍第 3 节的六道闸门确认基线（预期全绿：1504 / web 400 / deps 0 违规 / gen 51 文件一致）。
3. **自己决策并开工**（默认从 A 起），按第 2 节的方式冻结契约、切单元、并行派发。
   不要为"做哪一段"回来问。
