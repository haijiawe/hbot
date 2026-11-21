# hbot-adapters

> 平台适配器与受限 IO：OneBot 11、QQ 官方机器人、本地试跑，加一个受限 HTTP 客户端。

## 概述

只做**协议翻译与传输**：把平台事件归一化成 `NormalizedInbound`，把出站消息编码成平台形状。业务编排（supervisor、队列、落库）在 `hbot-bot`。

## 目录

- [关键文件](#关键文件)
- [三个平台](#三个平台)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/platforms/onebot/client.rs` | OneBot 11 的 WS 客户端：连接、echo 匹配、重连退避、收发 API |
| `src/platforms/onebot/message.rs` | 消息段构造：文本 / 回复 / **附件**（`base64://` 段，5 MiB 上限） |
| `src/platforms/onebot/normalize.rs` | 事件归一化（群 / 私聊、@、图片占位） |
| `src/platforms/onebot/backoff.rs` | 重连退避策略（有上限，且写日志） |
| `src/platforms/qq/` | QQ 官方机器人：`auth`（token 缓存）、`client`、`http`、`normalize`（富媒体事件）、`send` |
| `src/platforms/local/mod.rs` | 本地试跑：不联网，`send` 写成 run log 行（`[reply] ` / `[attachment] `） |
| `src/http.rs` | 受限 HTTP：超时、响应大小上限、重定向与协议限制 |
| `src/normalize.rs` | 平台无关的归一化类型与清洗 |
| `src/error.rs` | `AdapterError`（协议 / 认证 / 限流 / 上限）与到 `HbotError` 的映射 |
| `src/tls.rs` | TLS 根证书与危险配置的拒绝 |

## 三个平台

- **OneBot 11**：WS 长连接 + echo 对应请求；断线无限重连（这也是"读循环意外结束"只能靠假适配器造出来的原因）。
- **QQ 官方**：HTTP + 回调校验；没有媒体上传接口，所以附件出站只在 OneBot 与 local 上成立。
- **local**：给"新建项目就能试跑"用的一档，不联网、不需要账号。

## 边界：能放什么 / 不能放什么

- **能放**：平台协议、传输、重连、鉴权。
- **不能放**：业务编排（`hbot-bot`）、SQL（`hbot-store`）、Agent（`hbot-agent`）。
- 网络授权判据只有一份（`hbot.json` 的 `permissions.http`，在 `hbot-code`）：适配层**不要**再加一条按地址位置的二次拒绝（`AGENTS.md` 坑 48）。

## 上游与下游

- 上游：`hbot-core`（归一化事件、`AttachmentRef`、base64）。
- 下游：`hbot-rpc`（`runtime/onebot.rs` / `runtime/qq.rs` 实现 `BotAdapter`）、`hbot-cli`、`hbot-parity`。

## 测试

`cargo test -p hbot-adapters`：14 个 test target，主要是协议夹具与重连 / 退避行为；附件出站有"段形状 + 缺文件是错误不是降级"的用例。
真网关的用例在 `hbot-parity/tests/parity_14_real_onebot_gateway.rs`（自建 WS 服务端，不需要真账号）。

## 已知限制

- 没有 Telegram / Discord 等其它平台实现。
- QQ 官方接口没有媒体上传，附件出站不支持该平台。
