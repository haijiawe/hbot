# 03 目录与命名（人与 Agent 共用）

- 状态：Draft（与 [ADR-0002](adr/0002-agent-in-rust.md) 对齐）
- 日期：2026-11-16
- 目标：一个新人或一个 Agent，**只看路径就能判断该去哪改**；不靠记忆、不靠口头约定。

---

## 1. 顶层

```text
HBot-Next/
  crates/                 Rust workspace（产品全部运行时）
  apps/
    web/                  React UI（只在这里写 TypeScript 业务代码）
  tests/                  跨 crate 的集成 / E2E / 性能 / parity
  docs/                   设计与决策（本目录）
  scripts/                一次性脚本与闸门入口（不含业务逻辑）
  .github/workflows/      CI
  Cargo.toml              workspace 根
  pnpm-workspace.yaml     只为 apps/web 服务
  AGENTS.md                Agent 入口规则
  README.md               人看的产品与文档地图
```

**没有 `packages/`，没有 `src/` 大平层，没有第二个 `main`。** 包层只在需要独立发布时才建；当前不需要。

---

## 2. Rust workspace

```text
crates/
  hbot-core/             领域模型 + 端口 trait + 错误 + ID；不依赖任何其他 crate
  hbot-store/            SQLite 实现、迁移、repository
  hbot-agent/            Agent 循环、上下文、工具、审批、LLM provider
  hbot-code/             生成代码：manifest、SWC 编译、QuickJS 运行时、宿主 API
  hbot-bot/              平台无关的 bot supervisor、会话队列、outbox、去重
  hbot-workspace/        项目文件、文件监听、全局搜索、符号/源码映射、编辑事务
  hbot-adapters/         OneBot/Webhook/Telegram…、keyring、HTTP 客户端、文件系统
  hbot-rpc/              Command/Event DTO、dispatch、WS 服务
  hbot-cli/              `hbot` 二进制：serve / check / run
  hbot-desktop/          Tauri 命令、窗口、托盘、单实例、更新
  xtask/                 代码生成、依赖检查、基准入口（不属于产品）
```

### 2.1 依赖方向（硬规则）

```text
hbot-cli / hbot-desktop        （唯一组合根 + 传输）
            │
         hbot-rpc              （应用层：Command/Event → 调用服务）
            │
   ┌────────┬─────────┬──────────┬────────────┐
hbot-store hbot-code hbot-bot hbot-agent hbot-workspace
   └────────┴─────────┴──────────┴────────────┘
            │
        hbot-core              （实体 + 端口 + 错误）

hbot-adapters 只依赖 hbot-core（实现平台协议细节）；
hbot-bot 只依赖 hbot-core + hbot-store（自己定义它需要的平台端口）。
```

1. `hbot-core` 只依赖通用库（serde、thiserror、time、uuid），**不依赖 workspace 内其他 crate**。
2. `hbot-agent` 不依赖 `hbot-bot`、`hbot-rpc`、`hbot-desktop`、`hbot-workspace`。
3. `hbot-bot` 不依赖 `hbot-agent`；`hbot-code` 不依赖 `hbot-agent`。
4. `hbot-workspace` 只依赖 `hbot-core`；负责文件/搜索/监听/编辑事务，不含 UI、不含 Agent 决策。
5. `hbot-adapters` 只依赖 `hbot-core`；不得 import 具体服务 crate。
- **端口由消费方定义**：`hbot-bot` 里声明 `trait BotAdapter`（`crates/hbot-bot/src/adapter.rs`，`id/declared_conversations/subscribe/send`），`hbot-adapters` 只提供具体平台实现（`OneBotClient` 等）。两者之间的桥接（把 `OneBot` 的 `NormalizedInbound`/`OutboundSender` 适配成 `BotAdapter`）放在**组合根**（`hbot-rpc/src/bridge/` 或 `hbot-cli`），不放 bot、也不放 adapters —— 否则 bot 就得依赖 adapters，而 adapters 又可能想依赖 bot，形成双向。
6. `hbot-rpc` 可以依赖五个实现 crate，但只做参数校验、编排、错误映射；业务规则不下沉。
7. `hbot-desktop` 不含业务规则；只做“Tauri ↔ core”的适配与窗口/托盘/更新。
9. **ID 只有一份**：`hbot-core::ids`。`hbot-store` 重导出 core 的 ID 类型，不自己定义。
10. **行级 DTO 与领域实体分层**：`hbot-store` 出的是行级 DTO（`config_json: String`、`enabled: bool` 这类落库形状），`hbot-core::domain` 是领域实体；两者之间的 `row_to_domain` 映射放在**使用方**（`hbot-bot`/`hbot-rpc`），不放 store、也不放 core（store 不认识领域规则，core 不认识 SQL）。
11. **写 inbound 前必须有 bot 行与会话行**（外键开着，`PRAGMA foreign_keys=ON`）；适配器落地顺序固定 `bot → conversation → inbound → 入队`，顺序错了就是 `FOREIGN KEY constraint failed`。

### 2.2 每个 crate 的内部结构

```text
hbot-core/src/
  lib.rs
  entities/        Project Bot Conversation Message Session Run
  ports/           Store Platform Llm Executor Secrets Clock EventSink
  services/        纯领域服务（无 IO）
  errors.rs
  ids.rs

hbot-store/src/
  lib.rs
  db.rs            连接、pragma、单 writer 任务
  migrations/      0001_init.sql、0002_*.sql（只增不改）
  repositories/    project.rs bot.rs message.rs session.rs ...
  mappers.rs

hbot-agent/src/
  lib.rs
  loop.rs          turn 骨架（唯一循环）
  context/         assemble.rs compress.rs memory.rs
  tools/           fs.rs shell.rs bot.rs logs.rs http.rs ask.rs plan.rs mod.rs
  approvals.rs     规则键、持久化、超时
  providers/       openai.rs anthropic.rs mod.rs
  budget.rs
  prompt.rs

hbot-code/src/
  lib.rs
  manifest.rs      hbot.json 解析与校验
  compile.rs       SWC 去类型、source map
  runtime/         isolate.rs host_api.rs limits.rs errors.rs
  dts.rs           生成 hbot.d.ts

hbot-bot/src/
  lib.rs
  supervisor.rs    bot 生命周期
  session.rs       会话亲和、队列
  queue.rs         有界队列与背压
  outbox.rs        发送重试、死信
  dedupe.rs        入站幂等
  registry.rs       适配器注册表

hbot-workspace/src/
  lib.rs
  project.rs       项目根、hbot.json、路径守卫
  files.rs         读/写/移动/删除（原子写）
  watcher.rs       文件监听、去抖、外部变更分类
  search.rs        ignore + grep-searcher 流式搜索
  symbols.rs       符号大纲、定义位置（Monaco 结果落库/缓存）
  edit.rs          WorkspaceEdit / Diff / 冲突检测
  sourcemap.rs     运行时错误 → 文件:行

hbot-adapters/src/
  platforms/onebot.rs webhook.rs
  secrets/keyring.rs env.rs
  http.rs
  fs.rs

hbot-rpc/src/
  dto/mod.rs       重导出 + 序列化规则 + 命令→data 形状表
  dto/command.rs   Command 枚举（唯一）
  dto/event.rs     Event 枚举（唯一）
  dto/common.rs    Response / ApiError / 载荷结构
  dispatch.rs      唯一入口
  ws.rs            后台模式（auth、限流、背压）

hbot-cli/src/
  main.rs serve.rs check.rs

hbot-desktop/src/
  main.rs commands.rs window.rs tray.rs updater.rs single_instance.rs
```

新增文件先问：**它属于哪个 crate 的哪个职责？** 答不上来就不要建。

---

## 3. React UI

```text
apps/web/
  index.html
  src/
    app/            App.tsx、providers、错误边界、路由
    features/
      explorer/     文件树、项目切换、文件操作菜单
      editor/       Monaco、标签页、面包屑、Diff 编辑器、行内编辑
      search/       全局搜索/替换（流式结果）
      problems/     诊断列表、Fix 动作、跳转
      agent/        对话、计划、工具卡、审批、Diff 审查
      run/          bot 启停、mock 事件、状态、回放
      debug/        断点、变量、调用栈、步进
      logs/         运行日志、过滤、跳源码
      settings/     模型、密钥、沙箱、快捷键、主题
      command-palette/
    components/     跨 feature 的通用组件（少而稳）
    lib/
      api.ts        Api trait
      transport-tauri.ts
      transport-ws.ts
      layout.ts     布局唯一真源（宽度/折叠/分隔条）
      keybindings.ts 快捷键唯一真源
      commands.ts   dispatch 封装
      monaco/       初始化、worker、主题、TS 配置
      format.ts
    stores/
      workspace-store.ts  项目/文件树/标签/脏状态/布局
      agent-store.ts      会话/消息/工具卡/审批/Diff
      run-store.ts        bot 状态/队列/日志/调试
    generated/      由 Rust ts-rs 生成，禁止手改
    styles/
```

规则：

- **一个 feature 一个目录**；feature 内可以有自己的 components/hooks/types。
- 跨 feature 复用先进 `components/` 或 `lib/`，禁止 feature 之间互相 import 内部文件。
- **只有三个 store**（workspace/agent/run），各管各的；禁止一个 store 直接调另一个的 setter，跨域走命令或事件。
- UI 不直接读写磁盘、不直接 `fetch` 后端；文件操作与 API 都经 `api.ts`。
- 全局搜索、文件监听、诊断计算在后端；UI 只渲染结果与发命令。
- 新面板默认 `lazy`；Monaco、Diff、调试面板等重依赖必须动态 import。
- `layout.ts` 与 `keybindings.ts` 是唯一真源；组件不得写死宽度或快捷键。

---

## 4. 命名

| 对象 | 规则 | 例 |
|---|---|---|
| crate | kebab-case | `hbot-agent` |
| Rust 文件/模块 | snake_case | `host_api.rs` |
| Rust 类型 | PascalCase | `MessageContext` |
| Rust 函数/变量 | snake_case | `spawn_bot` |
| TS 文件 | kebab-case | `approval-card.tsx` |
| React 组件 | PascalCase | `ApprovalCard` |
| hook | `use-` 前缀文件，`useX` 导出 | `use-bot-status.ts` |
| Command 变体 | PascalCase，序列化 snake_case | `BotStart` → `bot_start` |
| Event 名 | 点分小写 | `agent.delta` |
| SQL 表/列 | snake_case 单数 | `inbound`、`bot_id` |
| 迁移 | `NNNN_topic.sql` | `0002_add_runs.sql` |
| 文档 | `NN-topic.md` | `03-directory.md` |
| ADR | `NNNN-topic.md` | `0002-agent-in-rust.md` |
| 测试 | 与被测同名 + `_test` / `.test.ts` | `queue_test.rs` |

禁止：`I` 前缀接口、`Utils`/`Helpers` 大杂烩、`Manager` 作为万能后缀（除非确实是生命周期管理者，如 `Supervisor`）、无意义缩写。

---

## 5. 单一真源（只准有一份）

| 事实 | 唯一位置 |
|---|---|
| Command / Event / 错误码 | `crates/hbot-rpc/src/dto/` → 生成 `apps/web/src/generated/` |
| 宿主 API 类型 | `crates/hbot-code/src/dts.rs` → 生成 `hbot.d.ts` |
| DB schema | `crates/hbot-store/src/migrations/*.sql` |
| 平台协议 | `crates/hbot-adapters/src/platforms/*` |
| 工具定义 | `crates/hbot-agent/src/tools/*` + 一处注册表 |
| 沙箱档位与审批规则 | `crates/hbot-agent/src/approvals.rs` |
| 生成代码权限 | `hbot.json` schema（`hbot-code/src/manifest.rs`） |
| 性能预算 | `docs/05-quality-gates.md` |
| IDE 布局 | `apps/web/src/lib/layout.ts` |
| 快捷键 | `apps/web/src/lib/keybindings.ts` |
| IDE 命令清单 | `crates/hbot-rpc/src/dto/`（Command）+ `apps/web/src/lib/commands.ts` |
| 平台能力矩阵 | `crates/hbot-adapters/src/platforms/capabilities.rs` |
| 目录规则 | 本文件 |

**任何“第二份契约”都是 bug**：手写 TS DTO、复制 SQL、在 UI 里重写工具列表，都不允许。

---

## 6. 文件与函数体量

- 文件：软 400 行、硬 800 行；超过要拆，或写明分类（数据表/装配/单事务）。
- 函数：软 60 行、硬 120 行；超过要拆。
- 不按行数评判“复杂度”；看是否**一次修改只碰一件事**。
- 禁止：`unwrap()`/`expect()` 出现在库代码（测试和 `main` 启动可例外）、`unsafe` 无 `// SAFETY:`、空 `catch`、`todo!()` 进主干。

---

## 7. 测试放哪

| 类型 | 位置 |
|---|---|
| Rust 单元 | 同文件 `#[cfg(test)]` 或 crate 内 `tests/` |
| Rust 集成 | `tests/integration/*.rs`（临时目录 + 真 SQLite + mock 适配器） |
| UI 单元 | `apps/web/src/**/*.test.ts(x)` |
| E2E | `tests/e2e/*.spec.ts`（Playwright + tauri-driver 或 WS 模式） |
| 性能 | `tests/perf/*.rs` + `scripts/bench.*` |
| parity | `tests/parity/*.rs`（旧项目行为场景） |

规则：bug 先写失败测试；不长期 `skip`；测试不依赖真实平台账号；时间/随机数注入。

---

## 8. 我要改 X，去哪里

| 需求 | 改哪里 | 不要动 |
|---|---|---|
| 新增平台（如飞书） | `hbot-adapters/src/platforms/` 新文件 + `registry.rs` 注册 | supervisor、队列 |
| 新增 Agent 工具 | `hbot-agent/src/tools/` + 注册表 + 审批分类 + 测试 | 循环骨架 |
| 新增宿主 API | `hbot-code/src/runtime/host_api.rs` + `dts.rs` + 权限校验 | UI |
| 新增 DB 表 | 新迁移 + `repositories/` + 需要时加 DTO | 旧迁移 |
| 新增 UI 页面 | `features/<name>/` + 路由 + 命令 | 其他 feature 内部 |
| 新增 IDE 命令 | `hbot-rpc/src/dto/` 加 Command + `dispatch.rs` + `apps/web/src/lib/commands.ts` + 快捷键表 | 在各组件里自己写 IPC |
| 新增编辑器能力 | `features/editor/` + `lib/monaco/`；文件操作走 `hbot-workspace` | 直接调 `fs`/`invoke` |
| 新增调试能力 | `hbot-code/src/runtime/debug.rs` + `hbot-rpc` 事件 + `features/debug/` | 改动 release 构建 |
| 新增平台模板 | `hbot-adapters/src/platforms/<name>/` + 能力矩阵 + 模板夹具 | 在 Agent 提示里硬编码平台分支 |
| 改 Agent 上下文 | `hbot-agent/src/context/assemble.rs` + 评测集 | 在 UI 里拼上下文 |
| 改协议 | `hbot-rpc/src/dto/` → `pnpm gen` → UI 修编译错误 | 手写 DTO |
| 换 LLM 供应商 | `hbot-agent/src/providers/` + 预设 + 测试 | 工具层 |
| 改沙箱限制 | `hbot-code/src/runtime/limits.rs` + 预算文档 | 宿主 API 白名单 |
| 加命令 | `tools/shell.rs` + 审批 + OS 隔离测试 | 另起执行通道 |

---

## 9. 生成物与忽略

```text
target/                 Rust 构建
node_modules/           UI 依赖
dist/                   UI 构建
apps/web/src/generated/ 生成但入库（便于 review 与离线构建）
.hbot/                  运行数据（每项目）
*.db *.db-wal *.db-shm
```

- `.hbot/` 永不入库；项目源码（`bot.ts`/`hbot.json`）入库由用户决定。
- 生成文件头必须写 `// @generated by xtask; do not edit`。

---

## 10. Agent 工作流（进入仓库先读这 8 步）

1. 读 `AGENTS.md` 与本文；确认改动落在哪个 crate/feature。
2. `git status`；不动无关文件。
3. 找到现有同类实现，优先复用；不新增“第二套”。
4. 先写/改测试（失败），再实现。
5. 跑最小闸门：`cargo fmt`、`cargo clippy -p <crate>`、`cargo test -p <crate>`；UI 改动跑 `pnpm check`。
6. 涉及契约：`cargo xtask gen`，把生成物一起提交。
7. 提交前跑 `./scripts/check.sh`（或 `just check`）。
8. 更新受影响的文档与 ADR；不留“以后补”。

禁令：不引入 Node 运行时、不建节点/流程引擎、不手写第二份契约、不静默吞错、不把密钥写日志、不改 `generated/`。

---

## 11. 提交与格式

- Conventional Commits：`feat(agent): ...`、`fix(bot): ...`、`docs(adr): ...`。
- Rust：`cargo fmt`；TS：Biome（单引号、无分号、宽 100）。
- 一次提交一件事；生成物与源码同提交。
- 不提交 `target/`、`node_modules/`、`.hbot/`、数据库文件。

---

## 12. 旧目录 → 新目录（迁移参考）

| 旧 | 新 |
|---|---|
| `src/main/services/bot/` | `crates/hbot-bot/` + `hbot-adapters/platforms/` |
| `src/main/runtime/FlowInterpreter.ts` | 删除；由 `hbot-code` + 生成代码替代 |
| `src/main/services/agent-core/` + `agent-engine/` | `crates/hbot-agent/`（只保留一个循环） |
| `src/main/services/ai/tools/` | `crates/hbot-agent/src/tools/` |
| `src/main/services/ai/tools/registry.ts`（1620 行） | 拆到各工具文件 + 一个注册表 |
| `src/main/services/infra/`（SQLite/文件） | `crates/hbot-store/` + `hbot-adapters/fs.rs` |
| `src/preload/index.ts` + `src/renderer/src/types/ipc.ts` | `hbot-rpc/src/dto/` + 生成 TS |
| `src/renderer/src/components/blocks/`（节点） | 删除 |
| `src/renderer/src/stores/flow/`（19 个 slice） | 删除；编辑器只编辑文件 |
| `src/main/vendor/dsh/` | 按需重写为 Rust（jobs/goal/lsp 不直接搬） |

---

## 13. 目录变更

- 新增顶层目录、新增 crate、改变依赖方向：必须写 ADR 或在现有 ADR 追加“变更记录”。
- 目录不是审美问题：**它决定 Agent 的默认落点**。路径含混，Agent 就会把代码写到错的地方。
- 每个 crate/feature 目录建议有 5–15 行的 `README.md`：放什么、不放什么、一个例子。没有也行，但 `AGENTS.md` 必须能指路。
