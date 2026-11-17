# HBot Next

**Agent-first 的 bot IDE。** 你和 Agent 描述需求，它在多文件项目里写代码，你在 IDE 里审查 Diff、编辑、搜索、运行、调试；平台连接、消息队列、存储、沙箱由 Rust 运行时负责。产物是能在任何平台上线的 bot。

- 不是低代码画布，也不是通用 IDE：它只服务 bot 项目。
- 代码是唯一真源；Agent 的每次写入都是可审查的 Diff。
- 产品运行时**不含 Node**；生成代码是 TypeScript（多文件、相对 import），在 QuickJS 隔离区执行。
- 桌面 Tauri 2（Rust 直接链接核心，无 sidecar），Linux 后台复用同一个核心。

---

## 为什么重写

旧项目（Electron + Node，`E:\H Bot 制造`）不是废弃代码：类型检查与 6,395 个测试全过，质量闸门齐全。但它的边界是长期累积出来的：

- 单主进程承担迁移、SQLite、IPC、Agent、插件、MCP、窗口；
- 桌面与 headless 两套装配，headless 仍依赖 Electron；
- Agent 新旧两套循环并存；
- preload/IPC/渲染三处手写契约（309 通道、2,188 行类型）；
- 节点解释器 + 19 个 flow store slice + 事件 fire-and-forget，可靠性与性能都受限。

新目标是三平台桌面 + Linux 后台 + Agent 优先 + 低内存，继续在旧结构上改，成本高于重建。完整证据与不变量见 [`docs/00-assessment.md`](docs/00-assessment.md)。

---

## 技术栈

| 层 | 选择 |
|---|---|
| 核心 | Rust（stable，2024 edition），tokio |
| 桌面 | Tauri 2（同进程链接核心） |
| UI | React 19 + TypeScript 6 + Vite 8 + Monaco + Tailwind 4 |
| 契约 | Rust DTO → `ts-rs` 生成 TS |
| Agent | Rust：循环、工具、审批、上下文、预算 |
| LLM | OpenAI 兼容 + Anthropic（`reqwest` + SSE） |
| Bot | Rust supervisor，每会话串行队列，持久化 inbound/outbound |
| 平台 | OneBot 11 + 通用 Webhook 优先，适配器 SDK 扩展到其他平台 |
| 生成代码 | 多文件 TypeScript（相对 import）→ 逐文件 SWC 去类型 → QuickJS 模块加载器 |
| IDE | Monaco 编辑器、文件树、搜索、问题、Diff 审查、运行/日志、插桩调试 |
| 存储 | SQLite（rusqlite bundled，WAL，单 writer） |
| 密钥 | OS keychain（`keyring`） |
| 打包 | Tauri bundler（NSIS / dmg / deb+AppImage） |

决策与取舍：[`docs/adr/0002-agent-in-rust.md`](docs/adr/0002-agent-in-rust.md)（取代 `0001`）。

---

## 架构一句话

```text
Tauri 2 (Rust)
  ├─ hbot-core：Agent / Bot / 代码沙箱 / SQLite / 调度
  └─ WebView：React IDE（文件树、Monaco、搜索、问题、Diff、运行/调试、日志）

Linux：hbot serve（同一核心，本机 WS RPC，systemd）
```

消息主链路：**适配器 → 幂等落库 → 每会话有界队列 → QuickJS handler → 持久化 outbox → 发送**。
先落库再处理；每会话串行；重复投递只执行一次；队列满背压不丢。

细节：[`docs/02-architecture.md`](docs/02-architecture.md)。

---

## 状态

| 项 | 状态 |
|---|---|
| 旧项目代码体检 | 完成（`docs/00-assessment.md`） |
| 技术栈 ADR | Proposed（`0002`；转 Accepted 还差 QuickJS 长跑、真 OneBot 网关、macOS 隔离） |
| 架构 / 目录 / 计划 / 闸门 / IDE | Draft，已按实现回填实测口径（`02`–`06`） |
| 代码 | **已落地 M0–M2 主体 + M3/M4/M5 绝大部分 + C6 设备能力窄版（Windows）**（`crates/` 下 16 个 crate + `xtask`，`apps/web` 可构建） |
| 测试 | 最近一次 `cargo test --workspace`：**1504 passed / 0 failed**（108 个 target）；`apps/web`：45 文件 400 用例 |
| 真机验证 | QQ 官方群/私聊被动回复已端到端跑通（`docs/spikes/S9-e2e-qq-official.md`）；Windows NSIS 安装包 9.14 MiB、安装冒烟 11/11；Windows 设备能力（截屏/原生 OCR/输入/多显示器）真机验证；DPI 声明 `unaware → per-monitor-v2` 实测 |
| 未完成 | M6 迁移/影子（缺测试账号）、真 NapCat 登录（需扫码且二维码会过期）、Linux/macOS 运行（Landlock 只交叉编译过、一次没真跑）、CI 从未执行（无 remote）、设备混合 DPI 跨屏拼图/按窗口截图（部分已做）/中文以外的 OCR 语言包 |

> 本仓库**没有任何 Go 代码**（0 个 `.go` 文件、0 个 `go.mod`）。Go 只出现在 ADR 的候选对比表里，且未被采用；当前实现是 Rust（核心）+ TypeScript（仅 IDE 前端）。

---

## 文档地图

| 文件 | 内容 |
|---|---|
| [`docs/00-assessment.md`](docs/00-assessment.md) | 旧项目实测证据、P0/P1、重写判定、验收不变量 |
| [`docs/01-requirements.md`](docs/01-requirements.md) | 用户场景、功能/非功能需求、MVP 范围、成功指标 |
| [`docs/adr/0001-tech-stack.md`](docs/adr/0001-tech-stack.md) | 历史决策（Superseded，不据此实施） |
| [`docs/adr/0002-agent-in-rust.md`](docs/adr/0002-agent-in-rust.md) | **当前决策**：Rust 单核、无 Node、QuickJS 沙箱 |
| [`docs/02-architecture.md`](docs/02-architecture.md) | 进程、边界、消息链路、Agent、沙箱、存储、安全、预算 |
| [`docs/03-directory.md`](docs/03-directory.md) | 目录、命名、依赖规则、改哪里、Agent 工作流 |
| [`docs/04-implementation-plan.md`](docs/04-implementation-plan.md) | M0–M6、出口标准、parity、风险、切换/回滚 |
| [`docs/05-quality-gates.md`](docs/05-quality-gates.md) | 一条命令、闸门、测试分层、性能预算、CI |
| [`docs/06-ide.md`](docs/06-ide.md) | **IDE 设计**：布局、编辑器、Agent Diff、运行/调试、搜索、性能 |
| [`docs/07-bot-runtime-blueprint.md`](docs/07-bot-runtime-blueprint.md) | **起 bot 的架构蓝图**：三层分离、6 组能力、四类 bot 缺什么、状态/定时/作业、设备能力（C6）三道闸、网关顺序 |
| [`docs/08-multi-host.md`](docs/08-multi-host.md) | **多宿主与运行位置**：桌面/无头/手机能力矩阵、iOS/Android 后台真相、本机/局域网/云端四档、npm 打包、移动端风险 |
| [`docs/09-device-capability.md`](docs/09-device-capability.md) | **设备能力实现设计**（Draft）：两个消费者一份闸、库表、三层闸落点、`DeviceBackend`、Windows 抓屏/注入选型、会话状态机、契约增量、窄版 vs 完整版 |
| [`docs/adr/0004-one-core-three-hosts.md`](docs/adr/0004-one-core-three-hosts.md) | 决策：一个核心三宿主、手机不做完整 IDE、为什么 QuickJS 对 iOS 也成立 |
| [`docs/adr/0005-capability-ceiling-and-device-grant.md`](docs/adr/0005-capability-ceiling-and-device-grant.md) | 决策：能力上限不砍、默认全关、逐项授权（含键鼠注入的三层闸） |
| [`docs/adr/0006-execution-backend.md`](docs/adr/0006-execution-backend.md) | 决策：运行位置可选（本机/局域网/云端）、单一写者、换位置靠搬家不靠热迁移 |
| [`docs/spikes/`](docs/spikes/) | 实测记录（内存、QuickJS、启动、打包、QQ 官方握手、**QQ 真机端到端**、性能预算） |
| [`AGENTS.md`](AGENTS.md) | Agent/新人入口：规则、地图、命令、坑 |
| [`_research/`](_research/) | 旧项目四份只读审查原始证据（不是产品文档） |

---

## 目录

```text
crates/            Rust workspace：core / store / agent / code / bot / workspace / adapters /
                   rpc / cli / desktop / command / parity / redteam / bench（+ xtask）
apps/web/          React IDE（generated/ 由 Rust 生成）
docs/              设计与决策（00 体检、01 需求、ADR、02 架构、03 目录、04 计划、05 闸门、
                   06 IDE、07 bot 能力蓝图、08 多宿主与运行位置、spikes 实测、security 红队）
scripts/           闸门、打包、真机验证脚本
packaging/         systemd / logrotate 部署物
AGENTS.md          Agent 入口
```

规则与“改 X 去哪”：[`docs/03-directory.md`](docs/03-directory.md)。

---

## 命令

```bash
# 静态闸门（与 CI 同一套，Windows 本地全部实跑过）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -q -p xtask -- gen --check   # 契约生成物是否漂移（40 个文件）
cargo run -q -p xtask -- deps          # 依赖方向（17 个构建单元，55 条内部边；含 xtask）
pnpm --dir apps/web run check          # tsc + IPC 无 any + vitest + vite build

# 运行
cargo run -p hbot-cli -- serve --bind 127.0.0.1:0 --token <tok> --data-dir <dir>
cargo run -p hbot-cli -- check <项目目录>          # 解析 hbot.json + 真跑 SWC 编译
cargo run -p hbot-cli -- replay <项目目录> --bot <id>   # 回放历史入站，退出码 0/1/2
cargo run -p hbot-desktop                          # 桌面端（需要 apps/web/dist）
pnpm --dir apps/web dev                            # 纯浏览器模式（mock 后端）

# 打包（Windows）
node scripts/build-desktop.mjs
cargo tauri build --bundles nsis
node scripts/smoke-nsis-install.ps1                # 安装/卸载冒烟
```

> `cargo xtask …` 不是 cargo 别名，正确写法是 `cargo run -p xtask -- …`。

---

## 非目标

节点画布、插件市场、旧格式/旧插件兼容、多用户、SSH/终端/git 客户端、语音视频、扩展市场、远程开发、MCP（MVP 后）。

---

## 内存与性能

目标（内存口径 = 进程树 private commit，**不是** WorkingSet，理由见 [ADR-0002](docs/adr/0002-agent-in-rust.md) §4）：桌面整机 < 260 MB，核心 < 60 MB，每 bot < 6 MB。

**已实测**（2026-11-16，i5-14600KF / Win11 / Defender 开）：核心 3.4 MB、每 bot 0.10 MB、每 QuickJS isolate 0.30 MB、桌面整机 private 中位 **206 MB**（其中 Rust 主进程 13.9 MB，WebView2 子进程 191.6 MB）、`hbot.exe` 18.05 MiB、NSIS 安装包 9.14 MiB。

**冷启动要分两个口径，别混**：无头 `hbot serve` 到 ready **36.4 ms 中位 / 50 ms p95**（n=7，达标）；桌面到窗口可交互 **525 ms**、到 IPC 首次可用 **1049 ms**（预算 1500 ms，达标）。这两个数的时间都花在 Tauri/WebView2 初始化上，不是 SQLite。数据见 [`docs/spikes/S0-memory.md`](docs/spikes/S0-memory.md)、[`S2-tauri.md`](docs/spikes/S2-tauri.md)、[`S11-headless-startup.md`](docs/spikes/S11-headless-startup.md)。

---

## 许可

待定（建议 MIT 或 Apache-2.0；发布前定）。
