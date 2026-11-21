# 10 · 插件模型：seam + 槽位

> 一个能力是怎么被"装进去"的，以及为什么本项目刻意不做运行时动态插件。

## 1. 两种"插件"要分开谈

| 形态 | 谁在用 | 本项目的选择 |
|---|---|---|
| **编译期插件**（一个能力 = 一个 crate / 一个端口 trait + 一个实现） | HBot-Next | ✅ 全部能力都这么接 |
| **运行时插件**（dlopen / 动态加载 / 用户脚本注册新的宿主能力） | 某些编辑器与 agent 宿主 | ❌ 明确不做，见 §5 |

所以本文说的"插件"，指的是**编译期可替换的端口实现**，不是"用户能装的扩展包"。

## 2. 三个层次的 seam

### 2.1 domain 层：`hbot-core::ports` 的 5 个端口

| trait | 谁实现 | 为什么需要它 |
|---|---|---|
| `Clock` | `SystemClock`（core 自带）；测试用固定时钟 | 领域层不直接读系统时间，重放与定时才能确定 |
| `IdGenerator` | `UuidIds`（rpc）；测试用计数替身 | 每个实体一个方法，避免"任意 UUID 塞进任意字段" |
| `CommandRunner` | `hbot-command` | 换隔离实现（Job Object → 受限令牌 → cgroup/Landlock）不动调用方 |
| `EventSink<E>` | `hbot-rpc` 的 `WsServer::event_sink()`、桌面事件泵、`hbot serve` 的 `forward_events` | 泛型在负载类型上，core 不认识 UI `Event` |
| `BotState` / `HistoryReader` | `StoreBotState`（rpc） | `ctx.kv` / `ctx.history` 的后端 |

### 2.2 runtime 层：每个能力的"槽位"

`hbot-code` 里每个能力都是同一个形状（`AGENTS.md` §8 第 4 步）：

```text
清单声明（hbot.json capabilities.*）
        │  每次重读，不缓存
        ▼
  SendSlot / StateSlot / AiSlot / JobSlot / WaitSlot / FileSlot / SqlSlot / DeviceSlot
        │  ① 参数闸（core 的 validate_*）
        │  ② 声明闸（HBOT_*_NOT_DECLARED）
        │  ③ 装配闸（HBOT_*_UNAVAILABLE）
        ▼
  端口 trait：SendQueue / BotState / AiClient / JobQueue / WaitQueue / FileBackend / SqlBackend / DeviceBackend
        │
        ▼
  hbot-rpc 的实现（StoreSendQueue / StoreBotState / ProviderAiClient / StoreJobQueue / StoreWaitQueue / ProjectFiles / StoreSqlBackend / DeviceGate+WindowsBackend）
```

两道闸分开报码是刻意的：`NOT_DECLARED` 要用户去改 `hbot.json`，`UNAVAILABLE` 要用户去 IDE 设置里配宿主（`AGENTS.md` 坑 59）。

### 2.3 platform 层：适配器

`hbot-bot::BotAdapter` 是平台 seam：`subscribe` / `send` / `stop`。实现有四个：
`OneBotAdapter`（WS）、`QqAdapter`（官方 HTTP）、`LocalAdapter`（不联网，回复写进 run log）、以及测试里的假适配器（`TestAdapter` / `ClosableAdapter`）。
换平台不改 supervisor / 队列 / outbox 一行 —— 这是 `crates/hbot-adapters` 存在的全部理由。

## 3. 组合根在哪

**只有一处**：`crates/hbot-rpc/src/app/mod.rs` 的 `AppState::open` / `start_bot`。
桌面（`hbot-desktop`）与无头（`hbot-cli serve`）都调同一个 `AppState`，所以"桌面能做的事后台做不了"这种分叉在本项目里不存在，
也**不允许**新加第二个组合根（`AGENTS.md` §0.3 / §9）。

新增能力的接线清单（历史上一共踩过 5 次同名坑，见 `AGENTS.md` 坑 66）：

1. `SandboxHandlerConfig` 加字段；
2. `SandboxHandler::new` 从 `config` 取（写 `None` 会静默丢掉整个能力）；
3. `worker()` 里调 `sandbox.set_*`；
4. `start_bot` 的结构体字面量；
5. `crates/hbot-parity/tests/support/mod.rs` 的字面量（否则 `--all-targets` 编译红）。

## 4. 与 DSH（Cordis）的对照

| 维度 | DSH / Cordis | HBot-Next |
|---|---|---|
| 能力单元 | 一个包（目录），带 `README.md` 与 `cordis.patch.yml` | 一个 crate（目录），带 `README.md`（本仓已补齐 17 份） |
| 组合方式 | 运行时服务名 `ctx.subagents` + Cordis 依赖注入 | 编译期端口 trait + 显式构造注入（`Arc<dyn Trait>`） |
| 声明依赖 | `package.json` 的 `peerDependencies` | `Cargo.toml` 的 path 依赖 + `cargo run -p xtask -- deps` 的方向检查 |
| 加载 | 运行时按配置加载插件 | 链接期静态装配，组合根只有 `AppState` 一处 |
| 用户扩展 | 可装第三方插件 | 改代码 / 加 crate（**无**第三方运行时扩展） |

两者要解决的问题不同：DSH 要"同一个内核配不同的能力组合"，本项目要"同一个产品在三种宿主上行为一致且可审计"。
后者一旦允许运行时加载任意代码，`capabilities` 三层判据（声明 / 设置 / 限时会话）就全部失去意义。

## 5. 为什么不做运行时动态插件

| 相冲突的不变量 | 说明 |
|---|---|
| 无 Node / 无外部运行时（`AGENTS.md` §0.1） | 动态插件要宿主执行任意用户代码；除了 QuickJS 沙箱，本项目不执行任何外部代码 |
| 只有一个 Agent 循环（§0.3） | 允许插件注册"另一个 Agent 循环"等于把这条规则废掉 |
| 只有一份契约（§0.4） | 插件会带来第二套 Command/Event，TS 生成物与 UI 立刻分裂 |
| 安全边界 fail-closed（§4.4） | dlopen 的代码不受 `hbot-command` 的隔离约束，也没法审计 |

如果确实需要"用户自己加行为"，正确的位置是**机器人项目里的 TypeScript**（`src/main.ts` 的 handler），不是宿主插件。

## 6. 想加一个新能力，按这个顺序

1. `hbot-core` 定义规则与参数闸（唯一一份，带单测）；
2. `hbot-code` 的 `Capabilities` 加字段 + 槽位 + JS 桥 + `dts.rs`；
3. `hbot-rpc` 实现端口并接进 `start_bot`；
4. 端到端测试四件：`NOT_DECLARED` / `UNAVAILABLE` / 参数非法 / 跨 bot 隔离；
5. 文档：`docs/07-bot-runtime-blueprint.md` 的能力表与本节 §3 的清单。
