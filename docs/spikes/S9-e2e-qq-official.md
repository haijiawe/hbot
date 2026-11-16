# S9 — QQ 官方机器人 · 真机端到端（2026-11-16）

这是本项目**第一次**由产品本身（不是协议探针、不是假网关）跑通整条链路：

```
QQ 官方网关 WS
  → 收 GROUP_MESSAGE_CREATE / C2C_MESSAGE_CREATE
  → 归一为 NormalizedInbound（剥 @ 标记，openid 当会话 id）
  → 落库（UNIQUE(bot_id, external_id) 幂等）
  → 每会话串行队列
  → QuickJS isolate 执行 projects/qqdemo/src/main.ts
  → ctx.log / reply()
  → 出站落库（reply_to = QQ 的 msg_id）
  → 被动回复 POST /v2/groups|users/{openid}/messages
```

## 1. 怎么跑的

驱动脚本 `E:\hbot-target\drive-qq2.ps1`（仓库外，属开发工具）**没有绕过任何一层**：

1. `hbot serve --bind 127.0.0.1:0 --token <tok> --data-dir <dir>`，读 stdout 的 `{"addr":…,"type":"ready"}`
2. WS 连上 → 首帧 `{"jsonrpc":"2.0","id":1,"method":"auth","params":{"token":"…"}}`
3. `dispatch {"type":"project_open","path":"…\\projects\\qqdemo"}`
4. `dispatch {"type":"bot_start","botId":"33333333-3333-4333-8333-333333333333"}`
5. 每 4 秒 `dispatch {"type":"logs_tail","botId":…,"limit":60}`

> 脚本 v1 有个 bug 值得记：每次 1 秒超时就新开一个 `ClientWebSocket.ReceiveAsync`，而一个 socket 只允许**一个**挂起接收，于是第二次调用抛 `InvalidOperationException`，脚本卡死。我因此一度误判「产品没收到消息」——实际上产品一直在正常工作。**结论：看不到证据不等于没有发生，要直接查库。**

## 2. 证据（读的是产品自己的 `hbot.db`，只读）

数据库：`E:\hbot-target\qqdemo-data\hbot.db`（WAL 模式，数据当时在 `-wal` 里）。

### 2.1 入站 3 条，全部落库且已处理

| 会话 | `external_id`（= QQ `message_id`） | `processed_at` |
|---|---|---|
| 群 `5e15d40a-c28a-83e1-ba04-3dd318b0e4b5` | `ROBOT1.0_mPc6uqiVajet…` | 1794792092421 |
| 私聊 `2efb7ef5-509e-89bc-928d-87a8d6b07a19` | `ROBOT1.0_.VlvKOAVkSw6…` | 1794792084569 |
| 私聊 `2efb7ef5-509e-89bc-928d-87a8d6b07a19` | `ROBOT1.0_.VlvKOAVkSw6…SS2vxVj…` | 1794792077285 |

两条私聊的 `received_at` 是 1794791916000 / 1794791923000，`processed_at` 依次后移 —— **同一会话严格串行**。

> **一处容易误读的地方（我自己先误判了）**：第 4 条「5+5」的 `received_at` 是 1794792152000
> （09:22:32），`processed_at` 是 1794792313981（09:25:13），差 **161 秒**。查启动日志确认
> 那一轮 `replay_unprocessed_inbound` **没有**输出过 `replayed unprocessed inbound messages`
> —— 也就是说这行在启动重放时**还不存在**，它是 09:25:13 才被写入的。
> 结论：`received_at` 取的是**平台事件里的 `d.timestamp`**，不是我们落库的时刻；
> 上一轮进程在 09:22:45 被杀，QQ 侧把未投递的事件留在队列里、重连后（09:25:13）补投，
> 时间戳仍是原始发送时间。**这不是流水线延迟**，产品收到即处理、当次就回了。
> 以后看到 `processed_at - received_at` 很大，先看是不是平台补投，别直接当成慢。

### 2.2 出站 3 条，全部一次成功

| 回复正文 | `state` | `attempts` | `reply_to` |
|---|---|---|---|
| `收到：1+1` | `sent` | 1 | `ROBOT1.0_mPc6uqiVajet…` |
| `收到：你可以做什么` | `sent` | 1 | `ROBOT1.0_.VlvKOAVkSw6…` |
| `你好，我是用 HBot 写出来的机器人。` | `sent` | 1 | `ROBOT1.0_.VlvKOAVkSw6…SS2vxVj…` |

判定依据：`state=sent` 且 `attempts=1` 表示**平台接受了这次被动回复**（`POST` 返回 200 才会标记 sent，失败会进 `mark_outbound_attempt` / `failed`）。`reply_to` 与入站 `external_id` 逐条对应，说明 `msg_id` 引用透传正确。

### 2.3 `ctx.log` 也进了库

`logs` 表（`level=info`）：`收到一条群消息：1+1`、`收到一条私聊消息：你可以做什么`、`收到一条私聊消息：你好`。

## 3. 这条链验证了哪些设计决定

| 设计决定 | 真机结果 |
|---|---|
| 先落库再入队 | 入站三行齐全，`processed_at` 后置 |
| 每会话串行 + 有界队列 | 两条私聊按 `received_at` 顺序处理 |
| quickjs 隔离执行生成代码 | `main.ts` 里的 `defineBot`/`ctx.log`/`reply` 全部生效 |
| 被动回复必须带 `msg_id` | `reply_to` 逐条透传，平台 200 |
| `msg_seq` 按 `msg_id` 独立计数 | 三条回复（两个不同 `msg_id`）全部成功 |
| 平台差异只在适配器 | 群 / 私聊两种会话归一到同一 `MessageCtx` |

## 4. 实测推翻的公开资料说法（详见 `S7-qq-official.md`）

| 常见说法 | 实测 |
|---|---|
| 群 @ 事件名 `GROUP_AT_MESSAGE_CREATE` | **`GROUP_MESSAGE_CREATE`** |
| @ 标记是 `<@!QQ号>` | **`<@32位HEX>`**，且应看 `mentions[].is_you` |
| 心跳 45s | 服务端 Hello 给 **41250ms**，必须用它 |
| token 有效期固定 7200 | `expires_in` 是**剩余**秒数（实测 6627、3777） |
| `msg_seq` 全局自增 | 只要求**同一 `msg_id` 下不重复** |

## 5. 仍未验证

- 主动推送（`send_group_active`/`send_c2c_active` 恒返回 `Unsupported`，零 HTTP 请求）
- 被动回复的**有效期与次数上限**（窗口过期、重复 `msg_seq`、一次回多条、429 阈值）
- `READY` 事件当前会被归一化器当成消息事件处理，打出
  `WARN qq event has no parsable d.timestamp … event="READY"` —— 不影响功能，但应单独分支
- macOS 的 `write-confine`（声明不实现）与真实 Linux 运行（Landlock 只编译过）
- 真 OneBot（NapCat）网关：`parity_14` 用的是自建网关注入，不是真 NapCat