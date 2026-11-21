# hbot-parity

> 旧项目行为场景的跨 crate 回归：**只有 `tests/`，没有 `src/`**。

## 为什么要单独一个 crate

接缝测试塞进任何一个实现 crate 的 `tests/` 都会逼那个 crate 反向依赖别人（例如"同会话 FIFO"要同时用到 `hbot-bot` 与 `hbot-adapters`）。
单开一个 `publish = false` 的 crate 就不会污染实现方向（`docs/04-implementation-plan.md` §4.1）。

## 目录

- [用例清单](#用例清单)
- [怎么跑](#怎么跑)
- [约定](#约定)

## 用例清单

| 文件 | 钉住什么 |
|---|---|
| `parity_01_same_conversation_is_fifo.rs` | 同会话严格 FIFO、不并发 |
| `parity_02_no_inbound_lost_before_adapter_ready.rs` | 启动窗口不丢消息（先建队列再连平台） |
| `parity_04_passive_reply_keeps_reply_to.rs` | 被动回复必须绑定 `reply_to` |
| `parity_05_bot_crash_does_not_affect_others.rs` | 一个 bot 崩不影响其它 bot |
| `parity_09_secrets_never_reach_logs.rs` | 密钥不进日志 / 错误 |
| `parity_13_backpressure_recovers.rs` | 队列满之后能恢复 |
| `parity_14_real_onebot_gateway.rs` | 真 WS 协议往返（自建网关，不需要真账号） |
| `parity_15_onebot_single_accept.rs` | 同一 bot 只被一个客户端接收 |

## 怎么跑

`cargo test -p hbot-parity`。夹具在 `tests/support/`（含自建 OneBot 网关的服务端半边）。

## 约定

- 这些用例读的不是源码文本，而是行为（`AGENTS.md` 坑 18）。
- 并行开发时若红，先看失败点是不是"断言没走到"（中间态），再决定是不是真回归（坑 33）。
