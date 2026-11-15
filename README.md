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
| 技术栈 ADR | Proposed（`0002`，7 天 Spike 后转 Accepted） |
| 架构 / 目录 / 计划 / 闸门 | Draft（`02`–`05`） |
| IDE 设计 | Draft（`06`） |
| 代码 | **尚未开始**（M0 第一周任务见 `04` §9） |
| 可运行的 `package.json` / `Cargo.toml` | 还没有；下面命令是目标形态 |

> 现在是设计阶段。不要照着本文去 `pnpm install` —— 仓库里还没有实现。

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
| [`docs/spikes/`](docs/spikes/) | 实测记录（内存、QuickJS、启动、打包） |
| [`AGENTS.md`](AGENTS.md) | Agent/新人入口：规则、地图、命令、坑 |
| [`_research/`](_research/) | 旧项目四份只读审查原始证据（不是产品文档） |

---

## 目录（目标形态）

```text
crates/            Rust workspace（core/store/agent/code/bot/workspace/adapters/rpc/cli/desktop/xtask）
apps/web/          React IDE（generated/ 由 Rust 生成）
tests/             integration / e2e / perf / parity / security
docs/              设计与决策
scripts/           闸门与一次性脚本
AGENTS.md          Agent 入口
```

规则与“改 X 去哪”：[`docs/03-directory.md`](docs/03-directory.md)。

---

## 命令（M0 落地后）

```bash
./scripts/check.sh          # 静态检查 + 单测（本地与 CI 同一套）
./scripts/check.sh --full   # 集成 + E2E + 打包 smoke
cargo xtask gen             # 生成 TS 契约与 hbot.d.ts
cargo run -p hbot-cli -- serve --project ./examples/hello
pnpm -C apps/web dev        # 浏览器模式，连本地 serve
pnpm -C apps/web tauri dev  # 桌面模式
```

---

## 非目标

节点画布、插件市场、旧格式/旧插件兼容、多用户、SSH/终端/git 客户端、语音视频、扩展市场、远程开发、MCP（MVP 后）。

---

## 内存与性能

目标：桌面空闲总 RSS < 220 MB（核心 < 60，WebView < 160），每 bot < 6 MB，冷启动 < 800 ms。全部是**待 Spike 实测**的目标，不是已达成数字。预算与测量方法见 [`docs/05-quality-gates.md`](docs/05-quality-gates.md) §4。

---

## 许可

待定（建议 MIT 或 Apache-2.0；发布前定）。