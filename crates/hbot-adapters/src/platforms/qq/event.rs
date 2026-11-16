//! 网关 WebSocket 帧的解析与上行载荷构造。
//!
//! # 帧格式（`op` 是数字，`d` 是任意 JSON）
//!
//! | `op` | 方向 | 含义 | 处理 |
//! |---|---|---|---|
//! | 0 `DISPATCH` | 下行 | 事件：带 `t`（事件名）/`s`（序号）/`d`（载荷） | 归一化并派发 |
//! | 1 `HEARTBEAT` | 上行 | 心跳，`d` 是最近一次的 `s`（可为 `null`） | 由客户端按时发 |
//! | 2 `IDENTIFY` | 上行 | 首连鉴权 `d:{token,intents,shard}` | 连接后发一次 |
//! | 6 `RESUME` | 上行 | 断线续连 `d:{token,session_id,seq}` | 有会话时优先发它 |
//! | 7 `RECONNECT` | 下行 | 服务端要求重连 | 断开，走 Resume |
//! | 9 `INVALID_SESSION` | 下行 | 会话失效，`d` 为 `false`/`true` | 丢掉 `session_id`/`seq`，下次 Identify |
//! | 10 `HELLO` | 下行 | 握手成功，`d.heartbeat_interval`（**毫秒**） | 记下心跳周期，然后 Identify/Resume |
//! | 11 `HEARTBEAT_ACK` | 下行 | 心跳回执 | 喂看门狗 |
//!
//! `heartbeat_interval` **取服务端值**，不写死。实测 2026-11-16 是 **41250ms**
//! （社区文档常见 45000，别照抄）。
//!
//! # 容错
//!
//! 网关是会加字段的（`d` 里我们只用的那几项之外可能任意）。所以解析按
//! 「需要的字段缺了才报错，其余忽略」的口径：`op` 必须是数字（缺了/不是数字 = 协议
//! 错误，无法推断该做什么），`s`/`t`/`d` 缺失一律容忍。

use std::time::Duration;

use serde_json::{Value, json};

use crate::error::AdapterError;

/// 事件派发（`op:0`）。
pub const OP_DISPATCH: u8 = 0;
/// 心跳（双向，`op:1`）。
pub const OP_HEARTBEAT: u8 = 1;
/// 首连鉴权（`op:2`）。
pub const OP_IDENTIFY: u8 = 2;
/// 断线续连（`op:6`）。
pub const OP_RESUME: u8 = 6;
/// 服务端要求重连（`op:7`）。
pub const OP_RECONNECT: u8 = 7;
/// 会话失效（`op:9`）。
pub const OP_INVALID_SESSION: u8 = 9;
/// 握手帧（`op:10`）。
pub const OP_HELLO: u8 = 10;
/// 心跳回执（`op:11`）。
pub const OP_HEARTBEAT_ACK: u8 = 11;

/// `READY`：首连成功，`d.session_id` 是本次会话 ID。
pub const EVENT_READY: &str = "READY";
/// `RESUMED`：续连成功。
pub const EVENT_RESUMED: &str = "RESUMED";
/// 群消息（**实测事件名**）。
///
/// 2026-11-16 用真凭据抓到：群里 @ 机器人之后，网关下发的是这个事件名，而不是
/// 公开文档/社区资料里写的 `GROUP_AT_MESSAGE_CREATE`。两个名字都要认。
pub const EVENT_GROUP_MESSAGE_CREATE: &str = "GROUP_MESSAGE_CREATE";

/// 群 @ 消息（**文档/社区里的旧名**）。
///
/// 实测网关没有发这个名字（发的是 [`EVENT_GROUP_MESSAGE_CREATE`]），保留是为了
/// 兼容按文档实现的部署与未来的重命名。
pub const EVENT_GROUP_AT_MESSAGE_CREATE: &str = "GROUP_AT_MESSAGE_CREATE";
/// 单聊（C2C）消息（需要 `1 << 25` 位）。
pub const EVENT_C2C_MESSAGE_CREATE: &str = "C2C_MESSAGE_CREATE";
/// 频道（公域）@ 消息。
///
/// **未核实**：这一事件所需的 intents 位（社区文档写 `1 << 30`）、`d` 的字段形状
/// （是否有 `channel_id`/`guild_id`/`author.id`）以及它是否与本适配器的
/// `conversation_kind = Channel` 语义一致，都没有在真网关上验证过。
pub const EVENT_AT_MESSAGE_CREATE: &str = "AT_MESSAGE_CREATE";

/// 一个已解析的网关帧。
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayFrame {
    /// 操作码（见本模块的常量）。
    pub op: u8,
    /// 序号 `s`：事件帧才有，心跳要带最近一次的值。
    pub seq: Option<i64>,
    /// 事件名 `t`：只有 `op:0` 有。
    pub event_name: Option<String>,
    /// 载荷 `d`（缺省为 `Value::Null`）。
    pub data: Value,
    /// 整帧原始 JSON（归一化时原样存进 `NormalizedInbound::raw`）。
    pub raw: Value,
}

impl GatewayFrame {
    /// 从一帧文本解析。
    ///
    /// 只要求顶层是对象且 `op` 是整数；`s`/`t`/`d` 缺失都容忍。
    pub fn parse(text: &str) -> Result<Self, AdapterError> {
        let raw: Value = serde_json::from_str(text).map_err(|err| {
            AdapterError::protocol(format!("qq gateway frame is not valid json: {err}"))
        })?;
        let object = raw
            .as_object()
            .ok_or_else(|| AdapterError::protocol("qq gateway frame must be a json object"))?;
        let op = object
            .get("op")
            .and_then(Value::as_u64)
            .and_then(|op| u8::try_from(op).ok())
            .ok_or_else(|| {
                AdapterError::protocol("qq gateway frame is missing a numeric `op` field")
            })?;
        let seq = object.get("s").and_then(Value::as_i64);
        let event_name = object
            .get("t")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|name| !name.is_empty());
        let data = object.get("d").cloned().unwrap_or(Value::Null);
        Ok(Self {
            op,
            seq,
            event_name,
            data,
            raw,
        })
    }

    /// 是不是事件帧（`op:0`）。心跳/ACK 之类的控制帧不该进归一化。
    pub fn is_dispatch(&self) -> bool {
        self.op == OP_DISPATCH
    }

    /// 事件名（`op:0` 才有）。
    pub fn event_name(&self) -> Option<&str> {
        self.event_name.as_deref()
    }

    /// `Hello` 帧里的心跳间隔（单位毫秒 → [`Duration`]）。
    ///
    /// `0`、负数、非数字一律当「没有」—— 用 0ms 心跳会把网关打挂。
    pub fn hello_interval(&self) -> Option<Duration> {
        if self.op != OP_HELLO {
            return None;
        }
        let millis = self
            .data
            .get("heartbeat_interval")
            .and_then(Value::as_u64)?;
        if millis == 0 {
            return None;
        }
        Some(Duration::from_millis(millis))
    }

    /// `d.session_id`（`READY`/`RESUMED` 带，用于断线续连）。
    pub fn session_id(&self) -> Option<&str> {
        self.data
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session| !session.is_empty())
    }
}

/// `op:2` Identify 的载荷。
///
/// `token` 必须是 `QQBot <access_token>` 这个形态（实测被接受），不是裸 token。
pub(crate) fn identify_payload(access_token: &str, intents: u64, shard: [u32; 2]) -> Value {
    json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": format!("QQBot {access_token}"),
            "intents": intents,
            "shard": [shard[0], shard[1]],
        }
    })
}

/// `op:6` Resume 的载荷（断线续连，带上次的 `session_id` 与 `seq`）。
pub(crate) fn resume_payload(access_token: &str, session_id: &str, seq: i64) -> Value {
    json!({
        "op": OP_RESUME,
        "d": {
            "token": format!("QQBot {access_token}"),
            "session_id": session_id,
            "seq": seq,
        }
    })
}

/// `op:1` 心跳的载荷。
///
/// `d` 是最近一次事件的 `s`；还没收到任何事件时是 `null`（协议允许）。
pub(crate) fn heartbeat_payload(seq: Option<i64>) -> Value {
    json!({
        "op": OP_HEARTBEAT,
        "d": seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hello_reads_the_server_interval() {
        // 实测帧（2026-11-16）：41250ms，不是 45000。
        let frame = GatewayFrame::parse(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#)
            .expect("valid hello");
        assert_eq!(frame.op, OP_HELLO);
        assert_eq!(frame.hello_interval(), Some(Duration::from_millis(41_250)));
        assert!(frame.event_name().is_none());
        assert!(!frame.is_dispatch());
    }

    #[test]
    fn parse_dispatch_keeps_seq_event_name_and_raw() {
        let text = r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"abc","user":{"username":"游戏机器人"}}}"#;
        let frame = GatewayFrame::parse(text).expect("valid dispatch");
        assert!(frame.is_dispatch());
        assert_eq!(frame.seq, Some(1));
        assert_eq!(frame.event_name(), Some(EVENT_READY));
        assert_eq!(frame.session_id(), Some("abc"));
        // raw 是整帧，供 NormalizedInbound::raw 原样落库。
        assert_eq!(frame.raw["t"], Value::String("READY".to_owned()));
    }

    #[test]
    fn parse_tolerates_missing_seq_and_data() {
        let frame = GatewayFrame::parse(r#"{"op":11}"#).expect("ack without d");
        assert_eq!(frame.op, OP_HEARTBEAT_ACK);
        assert_eq!(frame.seq, None);
        assert_eq!(frame.data, Value::Null);

        let frame = GatewayFrame::parse(r#"{"op":1,"d":null}"#).expect("heartbeat with null d");
        assert_eq!(frame.data, Value::Null);
    }

    #[test]
    fn parse_rejects_frames_without_a_numeric_op() {
        for text in [
            r#"{}"#,
            r#"{"op":"2"}"#,
            r#"{"op":300}"#,
            r#"[1,2,3]"#,
            "not json",
        ] {
            assert!(
                GatewayFrame::parse(text).is_err(),
                "expected `{text}` to be rejected"
            );
        }
    }

    #[test]
    fn hello_interval_ignores_zero_and_other_ops() {
        let zero = GatewayFrame::parse(r#"{"op":10,"d":{"heartbeat_interval":0}}"#).expect("hello");
        assert_eq!(zero.hello_interval(), None);
        let dispatch =
            GatewayFrame::parse(r#"{"op":0,"d":{"heartbeat_interval":1000}}"#).expect("evt");
        assert_eq!(dispatch.hello_interval(), None);
    }

    #[test]
    fn identify_payload_matches_the_measured_shape() {
        let payload = identify_payload("tok", 1 << 25, [0, 1]);
        assert_eq!(payload["op"], json!(2));
        assert_eq!(payload["d"]["token"], json!("QQBot tok"));
        assert_eq!(payload["d"]["intents"], json!(33_554_432u64));
        assert_eq!(payload["d"]["shard"], json!([0, 1]));
    }

    #[test]
    fn resume_and_heartbeat_payloads_have_the_documented_fields() {
        let resume = resume_payload("tok", "session-1", 7);
        assert_eq!(resume["op"], json!(6));
        assert_eq!(resume["d"]["session_id"], json!("session-1"));
        assert_eq!(resume["d"]["seq"], json!(7));
        assert_eq!(resume["d"]["token"], json!("QQBot tok"));

        assert_eq!(heartbeat_payload(Some(9)), json!({"op": 1, "d": 9}));
        // 还没收到事件时 d 是 null。
        assert_eq!(heartbeat_payload(None), json!({"op": 1, "d": null}));
    }
}
