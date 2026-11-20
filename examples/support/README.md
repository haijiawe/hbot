# 示例：客服工单（webhook + session + notify + sql）

外部系统推工单、聊天里补细节、运营收到提醒。四条能力各管一段：

| 能力 | 用在哪 |
|---|---|
| `onWebhook` | `POST /hook/<botId>/tickets`，**返回值就是 HTTP 应答** |
| `ctx.session.prompt` | 聊天里问一句，下一条消息交给 `onResume` |
| `ctx.notify` | 提醒运营，收件人来自清单而不是调用点 |
| `ctx.sql` | 工单落库 |

## 密钥怎么给

清单里只写**环境变量名**（`secretEnv: "HBOT_TICKET_SECRET"`），密钥本身不进仓库：

```powershell
$env:HBOT_TICKET_SECRET = '换成你自己的长随机串'
```

变量没设时这条路由**一律拒绝**（401），不会退化成「没配就放行」。

## 试一下

```bash
# 起 bot 之后（端口见 IDE 或 hbot serve 的输出），bot id 换成你自己的
curl -sS -X POST http://127.0.0.1:PORT/hook/<botId>/tickets \
  -H "x-hbot-secret: $env:HBOT_TICKET_SECRET" \
  -H 'content-type: application/json' \
  -d '{"contact":"alice","body":"登录一直转圈"}'
```

返回 `{"ok":true}` 且状态码 201 表示工单已落库；运营那边会收到一条提醒。

## 三种失败的含义

| 状态码 | 什么时候 |
|---|---|
| 401 | 路由没声明 / 密钥变量没设 / 密钥不对（三者统一，不区分） |
| 400 | 体不是 JSON 对象，或 `body` 长度不在 1..2000 |
| 500 | handler 抛错（日志里会点名） |

## 会话那一段

`ctx.session.prompt` 是**注册**不是阻塞：它写一行等待、发一句提示、立刻返回。
用户下一条消息由宿主先 `claim` 再交给 `onResume` —— 所以 `onMessage` 里 `await` 之后那段代码不会「等用户」，写在那里会立刻执行。
