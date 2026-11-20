# 示例项目

三个可以直接跑的最小项目，各钉住一组能力。它们**不含任何平台凭据**：`bots.json`
里有 token，属用户数据，所以示例只用 `"platform": { "kind": "local" }`
（本地平台，不需要账号）。

| 目录 | 钉住的能力 | 先看什么 |
|---|---|---|
| [`chat`](chat/) | `ctx.kv` + `ctx.history` + `ctx.ai.complete` | 记忆与上下文怎么拼、模型输出为什么不能当指令 |
| [`game`](game/) | `ctx.sql` + `onSchedule` | 迁移为什么在启动时跑、定时器为什么没有 `ctx.sql` |
| [`support`](support/) | `onWebhook` + `ctx.session.prompt` + `ctx.notify` | 密钥只写环境变量名、`prompt` 是注册不是阻塞 |

## 跑起来

```bash
# 只验清单、编译与 hbot.d.ts 契约
cargo run -q -p hbot-cli -- check examples/chat

# 真跑：起一个本地 bot，然后往 local:default 会话发消息
cargo run -q -p hbot-cli -- run examples/chat
```

在 IDE 里直接打开对应目录也一样。`hbot.d.ts` 是生成物（`cargo xtask gen`），
`gen --check` 会拦住漂移 —— 不要手改它。

## 它们不是用户数据

仓库根目录的 `projects/` 被 `.gitignore` 整目录忽略：那里是本机开发夹具，
`bots.json` 里可能有真实的 `token` / `clientSecret`。`examples/` 相反，**故意入库**：
新克隆的人应当能立刻 `check` 通过，这也正是 M7 出口标准「三类示例项目各一个」的
可验证形式。
