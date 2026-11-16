# S7 · QQ 官方机器人真实握手实测

> 状态：**部分实测**（真实凭据、真实端点、真实 WebSocket）。测于 2026-11-16。
> 机器：Windows 11 26300 / PowerShell 7。探针脚本：`D:\hbot-target\qqprobe.ps1`（仓库外，含 token 不入库）。
>
> 凭据存放：`%APPDATA%\hbot\qq-official.json`（仓库外，仅 `appId` / `clientSecret`）。
> **本文件与任何日志都不得出现 token / secret 的值。**

## 1. 已实测项（逐条有原始输出）

### 1.1 取 token

```
POST https://bots.qq.com/app/getAppAccessToken
Content-Type: application/json
Body: {"appId":"<appId>","clientSecret":"<secret>"}
```

响应：**HTTP 200**，`access_token` 长度 **78**，`expires_in` = **6627**。

要点：
- `expires_in` 返回的是**剩余**秒数，不是固定 7200。平台会复用未过期的 token，所以不能假设「每次请求都给满 7200」。
- 续期判断必须按服务端给的剩余值算（例：剩余 < 300s 才续），不要写死 7200。

### 1.2 取网关地址

```
GET https://api.sgroup.qq.com/gateway
Authorization: QQBot <access_token>
```

响应：**HTTP 200**，`url` = `wss://api.sgroup.qq.com/websocket/`。

要点：path 是 **`/websocket/`**（带尾斜杠），无 query 参数；实现里不要拼第二个 `/`。

### 1.3 WS 握手

| 步骤 | 实测帧 | 结论 |
|---|---|---|
| 连接后首帧 | `{"op":10,"d":{"heartbeat_interval":41250}}` | **心跳间隔 41250ms**，不是 45000；必须用服务端值，不许写死 |
| Identify | `{"op":2,"d":{"token":"QQBot <token>","intents":33554432,"shard":[0,1]}}` | **被接受** |
| READY | `{"op":0,"t":"READY","s":1,"d":{"user":{"username":"游戏机器人"},"session_id":"…"}}` | `intents = 1<<25`（GROUP_AND_C2C_EVENT）足够；`shard:[0,1]` 被接受；`session_id` 存在 |

实测输出（脱敏后原文）：

```
CONNECTED=True state=Open
RECV op=10 Hello heartbeat_interval=41250
SENT op=2 Identify intents=33554432 shard=[0,1]
RECV op=0 event t=READY s=1
  READY user=游戏机器人 session_id_present=True
  tick(no frame)   # 该 bot 未加入任何群，10 秒内无事件
DONE
```

### 1.4 真实事件（决定性修正）

抓到真实群消息与私聊消息后，**两处与常见资料/文档不一致**，实现必须按实测：

**① 群消息事件名是 `GROUP_MESSAGE_CREATE`**（不是 `GROUP_AT_MESSAGE_CREATE`）。两个都要处理，但实测到达的是前者。

**② @ 标记是 `<@OPENID>`（尖括号 + `@` + 32 位 HEX）**，不是 `<@!QQ号>`：

```json
{
  "author": { "id": "336390D2FCA8472A739B0B1B2B6A3A90",
              "member_openid": "336390D2FCA8472A739B0B1B2B6A3A90",
              "member_nickname": "墨白", "member_role": "owner",
              "union_openid": "", "username": "墨白", "bot": false, "avatar": "https://..." },
  "content": "<@196AAC0A1E7A06AC0D5EEB7FCF778303> 你好",
  "group_id": "F9CBDDEE95273F1E42EF42B8C49D3578",
  "group_openid": "F9CBDDEE95273F1E42EF42B8C49D3578",
  "id": "ROBOT1.0_mPc6uqiVajet59qY2BykbzzIFtfjm6dD-GiarTv.LbV6XtSHYaALuuFP22nyUp7Aw-ogVXaf-A-2urhAGGD.puIWNVH.tsgTAG5uUo789u9.BR2w4VhRNMjsB9Rj5DnA",
  "mentions": [ { "bot": true, "id": "196AAC0A1E7A06AC0D5EEB7FCF778303", "is_you": true,
                  "member_nickname": "游戏机器人", "member_role": "admin",
                  "scope": "single", "username": "游戏机器人" } ],
  "message_scene": { "ext": ["msg_idx=REFIDX_...", "auth_token=<redacted>"], "source": "default" },
  "message_type": 0,
  "timestamp": "2026-11-16T08:06:39+08:00"
}
```

私聊 `C2C_MESSAGE_CREATE` 的形状：`author.user_openid` 而非 `member_openid`，无 `group_openid`/`mentions`，`content` 是纯文本。

### 1.5 由实测确定的字段规则

| 字段 | 规则 |
|---|---|
| `d.id`（消息 ID） | **字符串**，形如 `ROBOT1.0_…`（含 `.`/`-`/`!`，长度 100+）。是幂等键，也是被动回复必填的 `msg_id`。**不许用 i64/u64 解析** |
| `group_openid` / `user_openid` | 32 位大写 HEX **字符串**（两套命名空间，换算不回 QQ 号） |
| `author.member_openid` / `author.user_openid` | 同上，是发送者 ID |
| `READY.d.user.id` | **20 位十进制数字字符串**（`17350883629656119813`），与 HEX openid **不同命名空间**，别混用 |
| `d.content` | 含 `<@HEX32>` 标记，需剥除后 trim；是否 @ 了自己用 `mentions[].is_you` 判定，**不要**字符串猜 |
| `d.timestamp` | ISO8601 **带 `+08:00` 偏移**，用带时区解析，失败再回落本地时间并 warn |
| `d.message_scene.ext` | **含 `auth_token=…`，是推送凭据**：落库前必须脱敏为 `<redacted>`，日志/`Debug`/错误信息一律不许出现其值 |
| `d.message_type` | 实测 `0`（未穷举其它取值，标记未核实） |

### 1.6 被动回复：群聊与私聊都实测发送成功

```
POST https://api.sgroup.qq.com/v2/groups/{group_openid}/messages
POST https://api.sgroup.qq.com/v2/users/{user_openid}/messages
Header: Authorization: QQBot <access_token>
Body:   {"content":"…","msg_type":0,"msg_id":"<事件 d.id>","msg_seq":<自增>}
```

实测（2026-11-16 08:28）：

| 场景 | 入站 `msg_id` 长度 | 结果 |
|---|---|---|
| 私聊 `C2C_MESSAGE_CREATE`（`user_openid` 32 位 HEX） | 117 | **HTTP 200** |
| 群聊 `GROUP_MESSAGE_CREATE`（`group_openid` 32 位 HEX） | 137 | **HTTP 200** |

成功响应体形状（两条都一致）：

```json
{
  "id": "ROBOT1.0_vR69Bs2aak3A9rj.0DrRu36gQmgahQzJmchf0ftPjLB6VUPnECDvDf2y5Amw678KV9pnaryxEho9W2pR0NqMhw!!",
  "timestamp": "2026-11-16T08:28:14+08:00",
  "ext_info": { "ref_idx": "REFIDX_…" }
}
```

要点：
- 响应里的 `id` 是**已发出的那条消息**的 ID（`ROBOT1.0_…!!` 或 `ROBOT1.0_…!`），可作为 `SendReceipt` 的外部 ID。
- `msg_id` 长度不固定（实测 117 / 137），**不要**按固定长度校验。
- 我这次探针把 `msg_seq` 当全局计数（私聊用了 1、群聊用了 2，而非各自从 1 开始），**平台照样 200** —— 说明 `msg_seq` 只要求"同一 `msg_id` 下不重复"，但**实现仍应按 `msg_id` 分别计数**（文档口径），不要学我这个探针的偷懒写法。
- **未验证**：窗口过期后的报错形状、同一 `msg_id` 重复 `msg_seq` 的报错形状、一次事件允许回复几条。

### 1.7 仍然可以钉死的常量

- 默认 `intents = 1 << 25`（`GROUP_AND_C2C_EVENT`）。**不要**顺手加 GUILDS / C2C 之外的位——多声明无权限的位会被平台以 close `4914`（invalid intents）/ `4915`（no permission）拒绝。要扩位做成可配项。
- 默认 `shard = [0, 1]`（单分片）。
- 心跳周期**取 `op:10` 的 `heartbeat_interval`**；看门狗阈值 = 该值 × 2。
- Identify 的 `token` 前缀是 `QQBot `（一个空格）。

## 2. 仍未核实（实现时必须在代码注释标「未核实」）

- **被动回复的窗口过期行为与 `msg_seq` 重复的报错形状**（发送本身已验证成功，见 §1.6；但「过期后报什么错」「同一 msg_seq 重发报什么错」「一个事件能回几条」都未验证）。
- **限流阈值与 429 响应形状**。
- **`op:7` / `op:9` 的真实触发条件**（本地假网关可模拟，平台真实行为未知）。
- **`message_type` 的取值集合**（实测只见到 `0`）。
- **`AT_MESSAGE_CREATE`（频道 @ 消息）**：本 bot 未开通频道能力，未见真实帧；字段形状未知。
- **主动推送**（不带 `msg_id`）：本适配器不实现，未测其限额。
- **intents 完整位表**：只实测了 `1<<25` 足够收群与 C2C；其它位未逐个验证。
- **Webhook 模式**：本项目未实现。

## 2.1 已由实测推翻的常见说法

| 常见说法 | 实测 |
|---|---|
| 群 @ 消息事件叫 `GROUP_AT_MESSAGE_CREATE` | 实际到达 `GROUP_MESSAGE_CREATE`（两者都要认） |
| @ 标记形如 `<@!123456>`（频道格式） | 实际是 `<@<32位HEX>>`，且 `mentions[].is_you` 才是判据 |
| 心跳间隔固定 45000ms | 服务端下发 **41250ms**，必须读 `op:10` |
| `expires_in` 固定 7200 | 返回的是**剩余**秒数（实测 6627） |
| 会话/用户 ID 是 QQ 号 | 是 openid（32 位 HEX 或 20 位十进制），**换算不回 QQ 号** |

## 3. 对实现的要求

1. `QqConfig` 必须允许覆盖 endpoints（测试指向本地假网关），但默认值用上表的实测值。
2. token 缓存 + 单飞（并发只取一次）+ 按 `expires_in` 剩余时间提前续期。
3. `Debug` 实现脱敏 `client_secret` 与 `access_token`；任何错误信息与日志都不许出现它们。
4. 本地假网关必须能覆盖：Hello→Identify→READY→事件归一化、心跳与 ACK、Resume（`op:6` 带 `session_id`+`seq`）、`op:9` 后改发 Identify、心跳超时重连、被动回复带 `msg_id` 且 `msg_seq` 递增、主动推送返回 `Unsupported`。
5. 真实冒烟测试单独放在 `tests/qq_real.rs`，标 `#[ignore = "需要真凭据与真网关"]`，只从 `HBOT_QQ_APP_ID` / `HBOT_QQ_APP_SECRET` 读，**不许把凭据写进仓库文件**。

## 4. 与 OneBot 的关键差异（产品层面）

见 `docs/02-architecture.md` §4.2「平台不是二选一」。要点：官方群聊**只有 @ 机器人**才收到消息，会话 ID 是 `group_openid` / `user_openid`（换算不回 QQ 号），主动推送严格限额且本适配器不实现。两个平台并列，不是替代。