//! Anthropic Messages API（`/v1/messages`，SSE 流式）客户端。
//!
//! 与 [`super::openai`] **并列**：对外产出的仍是 crate 内部那一套事件
//! （[`StreamSink`] / [`Completion`] / [`ToolCallRequest`]），所以 `loop` / `gateway`
//! 一行都不用改。区别只在线上契约。
//!
//! 结构照 `openai.rs` 切成两层：
//!
//! 1. **纯逻辑**（不碰网络、不碰 IO）：
//!    - [`parse_event`]：一条 `data:` 负载 → [`AnthropicEvent`]；
//!    - [`AnthropicStreamDecoder`]：字节流 → 文本增量 / 工具调用 / usage / 结束原因；
//!    - [`build_request_body`]：内部请求 → Anthropic JSON；
//!    - [`messages_endpoint`]：`base_url` → `/v1/messages`。
//! 2. **薄 HTTP 层**：[`AnthropicProvider`]；传输层复用 [`super::openai::HttpTransport`]，
//!    所以真机走 `ReqwestTransport`、测试注入假传输。
//!
//! ## 为什么永远流式
//!
//! 实测（`D:\hbot-target\logs\m3resp.txt`，81 行真实抓包）：非流式 `/v1/messages`
//! 带 `tools` 时上游只回空文本、**没有** `tool_use` 块；流式完整可用。因此请求体
//! 永远带 `"stream": true`，没有非流式分支（也不发 `stream: false`）。
//!
//! ## 与 OpenAI 的差异（都是实测过的，不是猜的）
//!
//! - 结束信号是 `message_stop`，**没有** `data: [DONE]`；
//! - 工具入参是 `input_json_delta.partial_json` 按 `index` **字符串拼接**，
//!   到 `content_block_stop` 才算完整；
//! - 思考块是 `thinking_delta`，**不并进正文**（只数个数并 `tracing::debug!`）；
//! - `message_delta.usage` 里带 `billing_usage.source: "oai_chat"`（OpenAI 后端的外壳），
//!   忽略它，只用 `input_tokens` / `output_tokens`；
//! - `message_delta` 里可能**没有** `stop_sequence`。
//!
//! 取消：每次收 chunk 前检查 [`CancellationToken`]，取消后立即返回，不再读流。

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::HbotError;
use serde_json::{Value, json};

use super::openai::{
    HttpRequest, HttpTransport, ResponseSink, SseLine, SseLineBuffer, parse_sse_line,
};
use super::{
    ChatMessage, Completion, CompletionRequest, FinishReason, Provider, ProviderError, StreamSink,
    ToolCallRequest, Usage,
};
use crate::cancel::CancellationToken;

/// `anthropic-version` 请求头（上游实测值）。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 请求体 `max_tokens` 的默认值。
///
/// Anthropic 的 `max_tokens` 是**必填**字段（不像 OpenAI 可省略），所以这里给一个
/// 保守默认；调用方可以用 [`AnthropicProvider::with_max_tokens`] 或
/// [`CompletionRequest::max_tokens`] 覆盖。
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// `base_url` → Messages 端点。
///
/// 两种拼法都要支持（各有一条单测）：
///
/// - 已以 `/v1` 结尾 → `{base_url}/messages`；
/// - 否则 → `{base_url}/v1/messages`。
///
/// 第二种存在的理由：OpenAI 那条链路要求 `base_url` 自带 `/v1`（`…/v1/chat/completions`），
/// 用户切 provider 时极可能原样复用同一个值，所以不能假设它有、也不能假设它没有。
pub fn messages_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// `message_start` / `message_delta` 里的 token 计数（只取这两个字段）。
///
/// `message_delta.usage` 还带 `cache_creation_input_tokens` / `cache_read_input_tokens`
/// 与 `billing_usage` 等字段 —— 一律忽略（见模块文档）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnthropicUsage {
    /// 输入 token 数。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
}

/// `content_block_start` 里声明的块类型。
#[derive(Clone, Debug, PartialEq)]
pub enum BlockStart {
    /// 正文文本块。
    Text {
        /// `content_block_start` 里自带的初始文本（流式下通常是空串）。
        text: String,
    },
    /// 思考块（不透传，只计数）。
    Thinking {
        /// 初始思考文本（流式下通常是空串）。
        thinking: String,
    },
    /// 工具调用块。
    ToolUse {
        /// 上游给的调用 id（要原样回填到 `tool_result`）。
        id: String,
        /// 工具名。
        name: String,
        /// `content_block_start` 里自带的入参；流式下是 `{}`（后续靠 delta 拼）。
        input: Option<Value>,
    },
    /// 其他块类型（未来新增的，不认识就忽略）。
    Other {
        /// 原始 `type` 字符串。
        kind: String,
    },
}

/// `content_block_delta` 里的增量类型。
#[derive(Clone, Debug, PartialEq)]
pub enum BlockDelta {
    /// 正文增量。
    Text(String),
    /// 思考增量（**不**并进正文）。
    Thinking(String),
    /// 工具入参 JSON 文本分片（按 `index` 字符串拼接）。
    InputJson(String),
    /// 其他增量类型（如 `signature_delta`）。
    Other {
        /// 原始 `type` 字符串。
        kind: String,
    },
}

/// 一条 Anthropic 流事件（只保留我们关心的字段）。
///
/// 这是 [`parse_event`] 的产物：纯数据、可比较，便于单测逐条断言。
#[derive(Clone, Debug, PartialEq)]
pub enum AnthropicEvent {
    /// `message_start`：流开始，通常带 input token 计数。
    MessageStart {
        /// `message.usage`；上游没给就是 `None`（不许因此炸）。
        usage: Option<AnthropicUsage>,
    },
    /// `content_block_start`：某个内容块开始。
    BlockStart {
        /// 块序号（同一响应内唯一）。
        index: u64,
        /// 块类型。
        block: BlockStart,
    },
    /// `content_block_delta`：某个内容块的增量。
    BlockDelta {
        /// 块序号。
        index: u64,
        /// 增量内容。
        delta: BlockDelta,
    },
    /// `content_block_stop`：内容块结束（工具入参这时才完整）。
    BlockStop {
        /// 块序号。
        index: u64,
    },
    /// `message_delta`：结束原因与最终 usage。
    MessageDelta {
        /// 原始 `stop_reason`；可能缺字段。
        stop_reason: Option<String>,
        /// 最终 usage；可能缺字段。
        usage: Option<AnthropicUsage>,
    },
    /// `message_stop`：流正常结束。
    MessageStop,
    /// `ping`：保活，忽略。
    Ping,
    /// `error`：上游明确报错。
    Error {
        /// 错误文案（上游原文）。
        message: String,
    },
    /// 不认识的事件类型。
    Unknown {
        /// 原始 `type` 字符串。
        kind: String,
    },
}

/// 解析一条 SSE `data:` 负载。
///
/// 只认 JSON 里的 `type` 字段 —— `event:` 行（[`parse_sse_line`] 会忽略）与
/// `type` 必然一致，所以不依赖它，少一层状态。
pub fn parse_event(payload: &str) -> Result<AnthropicEvent, HbotError> {
    let value: Value = serde_json::from_str(payload).map_err(|err| {
        HbotError::invalid(format!(
            "provider 返回非法 SSE JSON（{err}）：{}",
            snippet(payload)
        ))
    })?;
    // 错误有两种形状：Anthropic 的 `{"type":"error","error":{…}}` 与 OpenAI 的
    // `{"error":{…}}`（兼容壳可能给后者）。两个都按"上游报错"处理。
    if matches!(value.get("error"), Some(error) if error.is_object()) {
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("provider 未给出错误详情");
        return Ok(AnthropicEvent::Error {
            message: message.to_string(),
        });
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(match kind {
        "message_start" => AnthropicEvent::MessageStart {
            usage: value
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(parse_usage),
        },
        "content_block_start" => {
            let index = required_index(&value, "content_block_start")?;
            let block = value.get("content_block").ok_or_else(|| {
                HbotError::invalid(format!(
                    "content_block_start 缺少 content_block：{}",
                    snippet(payload)
                ))
            })?;
            let block_kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let block = match block_kind {
                "text" => BlockStart::Text {
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                "thinking" => BlockStart::Thinking {
                    thinking: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                "tool_use" => BlockStart::ToolUse {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block.get("input").cloned(),
                },
                other => BlockStart::Other {
                    kind: other.to_string(),
                },
            };
            AnthropicEvent::BlockStart { index, block }
        }
        "content_block_delta" => {
            let index = required_index(&value, "content_block_delta")?;
            let delta = value.get("delta").ok_or_else(|| {
                HbotError::invalid(format!(
                    "content_block_delta 缺少 delta：{}",
                    snippet(payload)
                ))
            })?;
            let delta_kind = delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let delta = match delta_kind {
                "text_delta" => BlockDelta::Text(
                    delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                "thinking_delta" => BlockDelta::Thinking(
                    delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                "input_json_delta" => BlockDelta::InputJson(
                    delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                other => BlockDelta::Other {
                    kind: other.to_string(),
                },
            };
            AnthropicEvent::BlockDelta { index, delta }
        }
        "content_block_stop" => AnthropicEvent::BlockStop {
            index: required_index(&value, "content_block_stop")?,
        },
        "message_delta" => AnthropicEvent::MessageDelta {
            stop_reason: value
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string),
            usage: value.get("usage").and_then(parse_usage),
        },
        "message_stop" => AnthropicEvent::MessageStop,
        "ping" => AnthropicEvent::Ping,
        other => AnthropicEvent::Unknown {
            kind: other.to_string(),
        },
    })
}

fn required_index(value: &Value, event: &str) -> Result<u64, HbotError> {
    value.get("index").and_then(Value::as_u64).ok_or_else(|| {
        HbotError::invalid(format!(
            "{event} 缺少 index 字段：{}",
            snippet(&value.to_string())
        ))
    })
}

fn parse_usage(usage: &Value) -> Option<AnthropicUsage> {
    usage.as_object()?;
    Some(AnthropicUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// 块类型（内部状态机用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BlockKind {
    #[default]
    Text,
    Thinking,
    ToolUse,
    Other,
}

/// 一个内容块的状态。
#[derive(Debug, Default)]
struct Block {
    kind: BlockKind,
    /// 工具调用 id（只有 `ToolUse` 有）。
    tool_id: String,
    /// 工具名（只有 `ToolUse` 有）。
    tool_name: String,
    /// 工具入参 JSON 的**字符串**拼接结果（只有 `ToolUse` 有）。
    input_json: String,
    /// 是否收到过 `content_block_stop`。
    stopped: bool,
    /// 已累计的思考字符数（只做日志）。
    thinking_chars: usize,
}

/// 把 SSE 字节流解码成 [`Completion`]。
///
/// 与 `openai::StreamDecoder` 同形：`feed` 边收边回调文本增量，`finish` 才产出
/// 完整工具调用（[`StreamSink::on_tool_call`] 只在 `finish` 里回调一次）。
///
/// 严格性：流里没有 `message_stop` 就结束 → 报错（上游截断不能说成"正常完工"）；
/// 工具入参不是合法 JSON → 报错（**不许**静默变成空对象）。
#[derive(Debug, Default)]
pub struct AnthropicStreamDecoder {
    lines: SseLineBuffer,
    blocks: BTreeMap<u64, Block>,
    content: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    stop_reason: Option<String>,
    saw_message_start: bool,
    saw_message_stop: bool,
    done: bool,
}

impl AnthropicStreamDecoder {
    /// 新建解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节；文本增量立刻回调 `sink`。
    pub fn feed(&mut self, chunk: &[u8], sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        for line in self.lines.feed(chunk) {
            self.handle_line(&line, sink)?;
        }
        Ok(())
    }

    /// 结束解码：处理残留行、校验流完整性、产出完整工具调用。
    pub fn finish(mut self, sink: &mut dyn StreamSink) -> Result<Completion, HbotError> {
        if let Some(rest) = self.lines.take_remainder() {
            self.handle_line(&rest, sink)?;
        }
        if !self.saw_message_start {
            return Err(HbotError::internal(
                "Anthropic 流里没有 message_start：上游没有给出可解析的响应",
            ));
        }
        if !self.saw_message_stop {
            return Err(HbotError::internal(format!(
                "Anthropic 流在 message_stop 之前结束（已收到 {} 个内容块），响应可能被截断",
                self.blocks.len()
            )));
        }
        let tool_calls = self.collect_tool_calls()?;
        let finish_reason = self.finish_reason(tool_calls.is_empty());
        // 上游会先给 message_start 的 usage、再给 message_delta 的终值；这里只在
        // finish 回调一次**终值**，避免 UI 上 token 数来回跳。
        let usage = Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: 0,
        }
        .with_total_from_parts();
        sink.on_usage(usage);
        for call in &tool_calls {
            sink.on_tool_call(call);
        }
        Ok(Completion {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content)
            },
            tool_calls,
            usage,
            finish_reason,
        })
    }

    fn collect_tool_calls(&self) -> Result<Vec<ToolCallRequest>, HbotError> {
        let mut calls = Vec::new();
        for (index, block) in &self.blocks {
            if block.kind != BlockKind::ToolUse {
                continue;
            }
            if !block.stopped {
                return Err(HbotError::invalid(format!(
                    "工具调用块（index {index}）没等到 content_block_stop，入参不完整"
                )));
            }
            if block.tool_id.is_empty() {
                return Err(HbotError::invalid(format!(
                    "工具调用块（index {index}）缺少 id，无法回填结果"
                )));
            }
            if block.tool_name.is_empty() {
                return Err(HbotError::invalid(format!(
                    "工具调用块（index {index}）缺少工具名"
                )));
            }
            let raw = block.input_json.trim();
            let arguments = if raw.is_empty() {
                // 无参工具：上游可能一个 delta 都不发。
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(raw).map_err(|err| {
                    HbotError::invalid(format!(
                        "工具 {} 的参数不是合法 JSON（{err}）：{}",
                        block.tool_name,
                        snippet(raw)
                    ))
                })?
            };
            calls.push(ToolCallRequest {
                id: block.tool_id.clone(),
                name: block.tool_name.clone(),
                arguments,
            });
        }
        Ok(calls)
    }

    fn finish_reason(&self, no_tool_calls: bool) -> FinishReason {
        match self.stop_reason.as_deref() {
            // tool_use：把工具跑完再继续问。
            Some("tool_use") => FinishReason::ToolCalls,
            Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
            Some("max_tokens") => {
                tracing::warn!(
                    output_tokens = self.completion_tokens,
                    "Anthropic 响应被 max_tokens 截断（stop_reason=max_tokens），本轮输出不完整"
                );
                FinishReason::Length
            }
            Some("refusal") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_string()),
            None if no_tool_calls => FinishReason::Stop,
            None => FinishReason::ToolCalls,
        }
    }

    fn handle_line(&mut self, line: &str, sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        if self.done {
            return Ok(());
        }
        match parse_sse_line(line) {
            SseLine::Ignore => Ok(()),
            SseLine::Done => {
                // Anthropic 没有 `[DONE]`；兼容壳若发了，也只当"不再读"，
                // 不冒充 message_stop（finish 会如实报截断）。
                self.done = true;
                Ok(())
            }
            SseLine::Data(payload) => {
                let event = parse_event(payload)?;
                self.apply(event, sink)
            }
        }
    }

    fn apply(&mut self, event: AnthropicEvent, sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        match event {
            AnthropicEvent::MessageStart { usage } => {
                self.saw_message_start = true;
                if let Some(usage) = usage {
                    self.absorb_usage(usage);
                }
                Ok(())
            }
            AnthropicEvent::BlockStart { index, block } => {
                if self.blocks.contains_key(&index) {
                    return Err(HbotError::invalid(format!(
                        "重复的 content_block_start（index {index}）：同一 index 只能开始一次"
                    )));
                }
                let mut state = Block::default();
                match block {
                    BlockStart::Text { text } => {
                        state.kind = BlockKind::Text;
                        if !text.is_empty() {
                            self.content.push_str(&text);
                            sink.on_text_delta(&text);
                        }
                    }
                    BlockStart::Thinking { thinking } => {
                        state.kind = BlockKind::Thinking;
                        state.thinking_chars = thinking.chars().count();
                    }
                    BlockStart::ToolUse { id, name, input } => {
                        state.kind = BlockKind::ToolUse;
                        state.tool_id = id;
                        state.tool_name = name;
                        // 非流式语义：`content_block_start` 直接给了完整 input 对象。
                        // 流式下是 `{}`（或缺失），后续靠 input_json_delta 拼。
                        if let Some(Value::Object(map)) = input
                            && !map.is_empty()
                        {
                            state.input_json =
                                serde_json::to_string(&Value::Object(map)).map_err(|err| {
                                    HbotError::internal(format!("工具入参无法序列化：{err}"))
                                })?;
                        }
                    }
                    BlockStart::Other { kind } => {
                        state.kind = BlockKind::Other;
                        tracing::debug!(index, kind, "忽略不认识的 Anthropic 内容块");
                    }
                }
                self.blocks.insert(index, state);
                Ok(())
            }
            AnthropicEvent::BlockDelta { index, delta } => match delta {
                BlockDelta::Text(text) => {
                    if text.is_empty() {
                        return Ok(());
                    }
                    self.content.push_str(&text);
                    sink.on_text_delta(&text);
                    Ok(())
                }
                BlockDelta::Thinking(thinking) => {
                    if thinking.is_empty() {
                        return Ok(());
                    }
                    let block = self.block_mut(index, "thinking_delta")?;
                    block.thinking_chars += thinking.chars().count();
                    tracing::debug!(
                        index,
                        chars = block.thinking_chars,
                        "Anthropic 思考增量（不透传正文）"
                    );
                    Ok(())
                }
                BlockDelta::InputJson(chunk) => {
                    if chunk.is_empty() {
                        return Ok(());
                    }
                    let block = self.block_mut(index, "input_json_delta")?;
                    if block.kind != BlockKind::ToolUse {
                        return Err(HbotError::invalid(format!(
                            "content_block_delta（index {index}）是 {:?} 块，却收到 input_json_delta",
                            block.kind
                        )));
                    }
                    block.input_json.push_str(&chunk);
                    Ok(())
                }
                BlockDelta::Other { kind } => {
                    tracing::debug!(index, kind, "忽略不认识的 Anthropic 增量");
                    Ok(())
                }
            },
            AnthropicEvent::BlockStop { index } => {
                let block = self.block_mut(index, "content_block_stop")?;
                block.stopped = true;
                if block.kind == BlockKind::Thinking {
                    tracing::debug!(
                        index,
                        chars = block.thinking_chars,
                        "Anthropic 思考块结束（内容不透传）"
                    );
                }
                Ok(())
            }
            AnthropicEvent::MessageDelta { stop_reason, usage } => {
                if let Some(reason) = stop_reason {
                    self.stop_reason = Some(reason);
                }
                if let Some(usage) = usage {
                    self.absorb_usage(usage);
                }
                Ok(())
            }
            AnthropicEvent::MessageStop => {
                self.saw_message_stop = true;
                Ok(())
            }
            AnthropicEvent::Ping => Ok(()),
            AnthropicEvent::Error { message } => {
                Err(HbotError::internal(format!("provider 返回错误：{message}")))
            }
            AnthropicEvent::Unknown { kind } => {
                tracing::debug!(kind, "忽略不认识的 Anthropic 事件");
                Ok(())
            }
        }
    }

    fn block_mut(&mut self, index: u64, event: &str) -> Result<&mut Block, HbotError> {
        self.blocks.get_mut(&index).ok_or_else(|| {
            HbotError::invalid(format!(
                "{event} 指向未开始的 content block（index {index}）"
            ))
        })
    }

    /// 后到的非零值覆盖先到的值：`message_delta` 是终值，`message_start` 只作兜底。
    fn absorb_usage(&mut self, usage: AnthropicUsage) {
        if usage.input_tokens > 0 {
            self.prompt_tokens = usage.input_tokens;
        }
        if usage.output_tokens > 0 {
            self.completion_tokens = usage.output_tokens;
        }
    }
}

/// 构造 Anthropic Messages 请求体。
///
/// 纯函数，便于断言"内部消息怎么映射到线上字段"：
///
/// - `system` 是**顶层字符串**（不是 messages 里的一条）；
/// - `messages[].content` 是**块数组**，文本块 `{type:"text",text}`，
///   assistant 的工具调用是 `{type:"tool_use",id,name,input}`，
///   工具结果是 user 里的 `{type:"tool_result",tool_use_id,content}`；
/// - 连续的工具结果合并进**同一条** user 消息（Anthropic 要求 tool_result
///   紧跟在 assistant 的 tool_use 之后）；
/// - `tools[].input_schema` 就是内部 [`super::ToolSpec::parameters`]；
/// - `max_tokens` 必填：`req.max_tokens` 没给就用 `default_max_tokens`；
/// - 永远 `"stream": true`；**不发** OpenAI 的 `tool_choice`。
pub fn build_request_body(
    req: &CompletionRequest,
    default_max_tokens: u32,
) -> Result<Value, HbotError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for message in &req.messages {
        match message {
            ChatMessage::System { content } => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                if !content.trim().is_empty() {
                    system_parts.push(content.clone());
                }
            }
            ChatMessage::User { content } => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": content }],
                }));
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = content.as_deref().filter(|text| !text.is_empty()) {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for call in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if blocks.is_empty() {
                    return Err(HbotError::invalid(
                        "assistant 消息既没有文本也没有工具调用，无法映射成 Anthropic content 块",
                    ));
                }
                messages.push(json!({ "role": "assistant", "content": blocks }));
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }
    flush_tool_results(&mut messages, &mut pending_tool_results);

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        // 必填字段：没给就用默认，绝不省略。
        "max_tokens": req.max_tokens.unwrap_or(default_max_tokens),
        // 必须流式：非流式带 tools 时上游只回空文本（实测）。
        "stream": true,
    });
    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    Ok(body)
}

fn flush_tool_results(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let blocks = std::mem::take(pending);
    messages.push(json!({ "role": "user", "content": blocks }));
}

fn snippet(text: &str) -> String {
    let text = text.trim();
    let max = 200;
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// 把 [`AnthropicStreamDecoder`] 接到 [`ResponseSink`] 上，并在每个 chunk 前检查取消。
struct DecoderSink<'a> {
    decoder: &'a mut AnthropicStreamDecoder,
    sink: &'a mut dyn StreamSink,
    cancel: &'a CancellationToken,
}

impl ResponseSink for DecoderSink<'_> {
    fn on_chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        if self.cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        self.decoder
            .feed(chunk, self.sink)
            .map_err(ProviderError::fatal)
    }
}

/// Anthropic Messages provider（薄封装）。
pub struct AnthropicProvider<T: HttpTransport> {
    base_url: String,
    api_key: String,
    transport: T,
    timeout: Duration,
    max_tokens: u32,
}

impl<T: HttpTransport> AnthropicProvider<T> {
    /// 新建；`base_url` 可以是 `https://host` 或 `https://host/v1`（见 [`messages_endpoint`]）。
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, transport: T) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            transport,
            timeout: Duration::from_secs(120),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// 设置总超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 覆盖请求体 `max_tokens` 的默认值（请求里显式给了 `max_tokens` 时仍以请求为准）。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// `/v1/messages` 完整 URL。
    pub fn endpoint(&self) -> String {
        messages_endpoint(&self.base_url)
    }
}

#[async_trait]
impl<T: HttpTransport> Provider for AnthropicProvider<T> {
    async fn complete(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<Completion, HbotError> {
        req.cancel.check()?;
        let body = build_request_body(&req, self.max_tokens)?;
        let body = serde_json::to_string(&body)
            .map_err(|err| HbotError::internal(format!("请求体无法序列化：{err}")))?;
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
        ];
        if !self.api_key.is_empty() {
            headers.push(("x-api-key".to_string(), self.api_key.clone()));
        }
        let request = HttpRequest {
            url: self.endpoint(),
            headers,
            body,
            timeout: self.timeout,
        };
        let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
        let mut decoder = AnthropicStreamDecoder::new();
        let transport_result = {
            let mut adapter = DecoderSink {
                decoder: &mut decoder,
                sink,
                cancel: &req.cancel,
            };
            match tokio::time::timeout(
                self.timeout,
                self.transport
                    .post(request, req.cancel.clone(), &mut adapter),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(ProviderError::timeout("provider 请求", timeout_ms)),
            }
        };
        transport_result.map_err(HbotError::from)?;
        req.cancel.check()?;
        decoder.finish(sink)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    /// 把若干 `event:` + `data:` 对拼成一段 SSE 字节流。
    fn sse(events: &[(&str, &str)]) -> String {
        let mut out = String::new();
        for (name, payload) in events {
            out.push_str("event: ");
            out.push_str(name);
            out.push_str("\ndata: ");
            out.push_str(payload);
            out.push_str("\n\n");
        }
        out
    }

    /// 记录型 sink。
    #[derive(Default)]
    struct RecordingSink {
        deltas: Vec<String>,
        tool_calls: Vec<ToolCallRequest>,
        usage: Vec<Usage>,
    }

    impl StreamSink for RecordingSink {
        fn on_text_delta(&mut self, delta: &str) {
            self.deltas.push(delta.to_string());
        }

        fn on_tool_call(&mut self, call: &ToolCallRequest) {
            self.tool_calls.push(call.clone());
        }

        fn on_usage(&mut self, usage: Usage) {
            self.usage.push(usage);
        }
    }

    /// 解码一段完整流（一次性喂入）。
    fn decode(stream: &str) -> (Completion, RecordingSink) {
        let mut decoder = AnthropicStreamDecoder::new();
        let mut sink = RecordingSink::default();
        decoder
            .feed(stream.as_bytes(), &mut sink)
            .expect("正常流不该报错");
        let completion = decoder.finish(&mut sink).expect("正常流应能收尾");
        (completion, sink)
    }

    fn text_stream() -> String {
        sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"type":"message","model":"m","usage":{"input_tokens":11,"output_tokens":0},"content":[]}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"，世界"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":5},"delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ])
    }

    #[test]
    fn url_rule_appends_v1_when_missing() {
        assert_eq!(
            messages_endpoint("https://z1rry.cyou"),
            "https://z1rry.cyou/v1/messages"
        );
        assert_eq!(
            messages_endpoint("https://z1rry.cyou/"),
            "https://z1rry.cyou/v1/messages"
        );
        assert_eq!(
            messages_endpoint("http://127.0.0.1:8080/api"),
            "http://127.0.0.1:8080/api/v1/messages"
        );
    }

    #[test]
    fn url_rule_keeps_existing_v1() {
        assert_eq!(
            messages_endpoint("https://z1rry.cyou/v1"),
            "https://z1rry.cyou/v1/messages"
        );
        assert_eq!(
            messages_endpoint("https://z1rry.cyou/v1/"),
            "https://z1rry.cyou/v1/messages"
        );
    }

    #[test]
    fn text_stream_decodes_deltas_and_end_turn() {
        let (completion, sink) = decode(&text_stream());
        assert_eq!(completion.text(), "你好，世界");
        assert!(completion.tool_calls.is_empty());
        assert_eq!(completion.finish_reason, FinishReason::Stop);
        assert_eq!(sink.deltas, vec!["你好".to_string(), "，世界".to_string()]);
        assert_eq!(sink.usage.len(), 1, "usage 只在 finish 回调一次终值");
        // message_start 给 input，message_delta 给 output。
        assert_eq!(completion.usage.prompt_tokens, 11);
        assert_eq!(completion.usage.completion_tokens, 5);
        assert_eq!(completion.usage.total_tokens, 16);
        assert!(sink.tool_calls.is_empty());
    }

    #[test]
    fn thinking_blocks_are_not_merged_into_text() {
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"先想一想"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":9},"delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let (completion, sink) = decode(&stream);
        assert_eq!(completion.text(), "答案", "思考内容不得并进正文");
        assert!(
            sink.deltas.iter().all(|delta| delta != "先想一想"),
            "思考增量不得回调给 UI：{:?}",
            sink.deltas
        );
    }

    #[test]
    fn tool_use_is_assembled_from_input_json_deltas() {
        // 与 D:\hbot-target\logs\m3resp.txt 同形（省略了思考块的多条增量）。
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":63,"output_tokens":0},"content":[]}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The user wants me to write hello"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_bf503cf7b9644970a13e816d","name":"write_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"path\": \"notes.txt\""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":", \"content\": \"hello\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"input_tokens":58,"cache_read_input_tokens":256,"output_tokens":97,"billing_usage":{"source":"oai_chat","semantic":"openai","openai_usage":{"prompt_tokens":314,"completion_tokens":97,"total_tokens":411}}},"delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let (completion, sink) = decode(&stream);
        assert_eq!(completion.finish_reason, FinishReason::ToolCalls);
        assert_eq!(completion.tool_calls.len(), 1);
        let call = &completion.tool_calls[0];
        assert_eq!(call.id, "call_bf503cf7b9644970a13e816d");
        assert_eq!(call.name, "write_file");
        assert_eq!(
            call.arguments,
            json!({"path": "notes.txt", "content": "hello"})
        );
        assert_eq!(sink.tool_calls.len(), 1);
        // billing_usage 必须被忽略：只用 input_tokens / output_tokens。
        assert_eq!(completion.usage.prompt_tokens, 58, "message_delta 终值优先");
        assert_eq!(completion.usage.completion_tokens, 97);
        assert_eq!(completion.usage.total_tokens, 155);
        assert_eq!(completion.text(), "", "思考和工具块都不该产生正文");
        assert_eq!(completion.content, None, "没有正文时 content 必须是 None");
    }

    #[test]
    fn two_tool_uses_are_kept_apart_by_index() {
        // 两个工具并行：index 1 / 2，入参分片**交错**到达。
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_a","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_b","name":"list_dir","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src\"}"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":12},"delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let (completion, _sink) = decode(&stream);
        assert_eq!(completion.tool_calls.len(), 2);
        // 按 index 升序产出。
        assert_eq!(completion.tool_calls[0].name, "read_file");
        assert_eq!(completion.tool_calls[0].arguments, json!({"path": "a.txt"}));
        assert_eq!(completion.tool_calls[1].name, "list_dir");
        assert_eq!(completion.tool_calls[1].arguments, json!({"path": "src"}));
    }

    #[test]
    fn invalid_tool_json_is_an_error_not_an_empty_object() {
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_bad","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not json"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let mut decoder = AnthropicStreamDecoder::new();
        let mut sink = RecordingSink::default();
        decoder
            .feed(stream.as_bytes(), &mut sink)
            .expect("流本身合法");
        let error = decoder.finish(&mut sink).expect_err("坏 JSON 必须报错");
        assert!(matches!(error, HbotError::Invalid { .. }), "应是 Invalid");
        assert!(
            error.to_string().contains("不是合法 JSON"),
            "错误信息要说清原因：{error}"
        );
        assert!(sink.tool_calls.is_empty(), "解析失败时不得回调半个工具调用");
    }

    #[test]
    fn max_tokens_finish_reason_is_length() {
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":2,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半截"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":4096},"delta":{"stop_reason":"max_tokens"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let (completion, _sink) = decode(&stream);
        assert_eq!(completion.finish_reason, FinishReason::Length);
        assert_eq!(completion.text(), "半截");
        assert_eq!(completion.usage.completion_tokens, 4096);
    }

    #[test]
    fn message_start_without_usage_does_not_panic() {
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"chatcmpl-x","content":[]}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let (completion, sink) = decode(&stream);
        assert_eq!(completion.text(), "ok");
        assert_eq!(completion.usage, Usage::default());
        assert_eq!(sink.usage.len(), 1);
    }

    #[test]
    fn sse_lines_split_at_every_byte_still_decode() {
        // 逐字节喂入：每行都被切成任意小块，`SseLineBuffer` 必须自己拼回。
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"中文分片"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":3},"delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let mut decoder = AnthropicStreamDecoder::new();
        let mut sink = RecordingSink::default();
        for byte in stream.as_bytes() {
            decoder.feed(&[*byte], &mut sink).expect("逐字节喂入");
        }
        let completion = decoder.finish(&mut sink).expect("finish");
        assert_eq!(completion.text(), "中文分片");
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].arguments, json!({"path": "a.txt"}));
    }

    #[test]
    fn truncated_stream_without_message_stop_is_an_error() {
        let stream = sse(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半截"}}"#,
            ),
        ]);
        let mut decoder = AnthropicStreamDecoder::new();
        let mut sink = RecordingSink::default();
        decoder.feed(stream.as_bytes(), &mut sink).expect("feed");
        let error = decoder
            .finish(&mut sink)
            .expect_err("没有 message_stop 必须报错");
        assert!(matches!(error, HbotError::Internal { .. }));
        assert!(
            error.to_string().contains("message_stop"),
            "错误信息要指出截断：{error}"
        );
    }

    #[test]
    fn stream_without_message_start_is_an_error() {
        let mut decoder = AnthropicStreamDecoder::new();
        let mut sink = RecordingSink::default();
        let error = decoder
            .feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\n", &mut sink)
            .and_then(|()| decoder.finish(&mut sink))
            .expect_err("没有 message_start 必须报错");
        assert!(matches!(error, HbotError::Internal { .. }));
        assert!(error.to_string().contains("message_start"));
    }

    #[test]
    fn request_body_uses_top_level_system_and_anthropic_tools() {
        let req = CompletionRequest {
            model: "deepseek-test".to_string(),
            messages: vec![ChatMessage::system("你是 hbot"), ChatMessage::user("hi")],
            tools: vec![super::super::ToolSpec {
                name: "read_file".to_string(),
                description: "读取文件".to_string(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            temperature: Some(0.5),
            max_tokens: Some(512),
            cancel: CancellationToken::new(),
        };
        let body = build_request_body(&req, DEFAULT_MAX_TOKENS).expect("body");
        assert_eq!(body["model"], "deepseek-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 512);
        // system 是顶层字符串，不在 messages 里。
        assert_eq!(body["system"], "你是 hbot");
        let messages = body["messages"].as_array().expect("messages 是数组");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        // tools 是扁平的 name/description/input_schema，没有 OpenAI 的 function 包装。
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["description"], "读取文件");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(
            body["tools"][0].get("function").is_none(),
            "不能发 OpenAI 的 function 包装"
        );
        assert!(body.get("tool_choice").is_none(), "不能发 tool_choice");
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn request_body_keeps_max_tokens_required() {
        let req = CompletionRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            cancel: CancellationToken::new(),
        };
        let body = build_request_body(&req, DEFAULT_MAX_TOKENS).expect("body");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["max_tokens"], 4096);
        // 没有工具时不该出现 tools / tool_choice。
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        // 空 system 不占位。
        assert!(body.get("system").is_none());
    }

    #[test]
    fn request_body_maps_tool_use_and_merges_tool_results() {
        let req = CompletionRequest {
            model: "m".to_string(),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("做两件事"),
                ChatMessage::Assistant {
                    content: Some("我先并行调两个".to_string()),
                    tool_calls: vec![
                        ToolCallRequest {
                            id: "call_a".to_string(),
                            name: "read_file".to_string(),
                            arguments: json!({"path": "a.txt"}),
                        },
                        ToolCallRequest {
                            id: "call_b".to_string(),
                            name: "list_dir".to_string(),
                            arguments: json!({"path": "src"}),
                        },
                    ],
                },
                ChatMessage::tool_result("call_a", "a 的内容"),
                ChatMessage::tool_result("call_b", "src 的列表"),
            ],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            cancel: CancellationToken::new(),
        };
        let body = build_request_body(&req, 4096).expect("body");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(
            messages.len(),
            3,
            "user + assistant + 合并后的 tool_result user"
        );
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][0]["text"], "我先并行调两个");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "call_a");
        assert_eq!(assistant["content"][1]["input"]["path"], "a.txt");
        assert_eq!(assistant["content"][2]["type"], "tool_use");
        assert_eq!(assistant["content"][2]["name"], "list_dir");
        // 两条工具结果必须合并进**同一条** user 消息。
        let results = &messages[2];
        assert_eq!(results["role"], "user");
        let blocks = results["content"].as_array().expect("content 块数组");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_a");
        assert_eq!(blocks[0]["content"], "a 的内容");
        assert_eq!(blocks[1]["tool_use_id"], "call_b");
    }

    /// 假传输的共享状态（可在 provider 消费传输后继续断言）。
    #[derive(Default)]
    struct TransportState {
        chunks: Vec<Vec<u8>>,
        requests: Vec<HttpRequest>,
        status: Option<u16>,
    }

    #[derive(Clone, Default)]
    struct ScriptedTransport {
        state: Arc<Mutex<TransportState>>,
    }

    impl ScriptedTransport {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            let transport = Self::default();
            transport
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .chunks = chunks;
            transport
        }

        fn with_status(status: u16) -> Self {
            let transport = Self::default();
            transport
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .status = Some(status);
            transport
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.state
                .lock()
                .map(|state| state.requests.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post(
            &self,
            request: HttpRequest,
            cancel: CancellationToken,
            sink: &mut dyn ResponseSink,
        ) -> Result<(), ProviderError> {
            let (chunks, status) = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| ProviderError::fatal(HbotError::internal("测试锁中毒")))?;
                state.requests.push(request);
                (std::mem::take(&mut state.chunks), state.status)
            };
            if let Some(status) = status {
                return Err(ProviderError::from_status(status, "scripted failure"));
            }
            for chunk in chunks {
                cancel.check().map_err(|_| ProviderError::cancelled())?;
                sink.on_chunk(&chunk)?;
            }
            Ok(())
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn provider_sends_anthropic_headers_and_stream_body() {
        let transport = ScriptedTransport::new(vec![text_stream().into_bytes()]);
        let handle = transport.clone();
        let provider =
            AnthropicProvider::new("https://z1rry.cyou", "sk-test-0123456789abcdef", transport);
        let mut sink = RecordingSink::default();
        let completion = provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");
        assert_eq!(completion.text(), "你好，世界");

        let requests = handle.requests();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert_eq!(sent.url, "https://z1rry.cyou/v1/messages");
        let header = |name: &str| {
            sent.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-api-key"), Some("sk-test-0123456789abcdef"));
        assert_eq!(header("anthropic-version"), Some(ANTHROPIC_VERSION));
        assert_eq!(header("content-type"), Some("application/json"));
        assert_eq!(header("accept"), Some("text/event-stream"));
        assert!(
            header("authorization").is_none(),
            "Anthropic 用 x-api-key，不发 Bearer"
        );
        let body: Value = serde_json::from_str(&sent.body).expect("请求体是 JSON");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[tokio::test]
    async fn provider_endpoint_accepts_base_url_with_v1() {
        let transport = ScriptedTransport::new(vec![text_stream().into_bytes()]);
        let handle = transport.clone();
        let provider = AnthropicProvider::new("https://z1rry.cyou/v1/", "k", transport);
        let mut sink = RecordingSink::default();
        provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");
        assert_eq!(handle.requests()[0].url, "https://z1rry.cyou/v1/messages");
    }

    #[tokio::test]
    async fn provider_omits_api_key_header_when_empty() {
        let transport = ScriptedTransport::new(vec![text_stream().into_bytes()]);
        let handle = transport.clone();
        let provider = AnthropicProvider::new("https://example.test", "", transport);
        let mut sink = RecordingSink::default();
        provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");
        assert!(
            handle.requests()[0]
                .headers
                .iter()
                .all(|(key, _)| !key.eq_ignore_ascii_case("x-api-key")),
            "没有 key 时不得带空的 x-api-key 头"
        );
    }

    #[tokio::test]
    async fn provider_does_not_call_transport_when_already_cancelled() {
        let transport = ScriptedTransport::new(vec![text_stream().into_bytes()]);
        let handle = transport.clone();
        let provider = AnthropicProvider::new("https://example.test", "k", transport);
        let req = request();
        req.cancel.cancel();
        let mut sink = RecordingSink::default();
        let error = provider
            .complete(req, &mut sink)
            .await
            .expect_err("必须被取消");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert!(handle.requests().is_empty(), "取消后不得发起任何请求");
    }

    #[tokio::test]
    async fn provider_maps_retryable_status() {
        let transport = ScriptedTransport::with_status(503);
        let provider = AnthropicProvider::new("https://example.test", "k", transport);
        let mut sink = RecordingSink::default();
        let error = provider
            .complete(request(), &mut sink)
            .await
            .expect_err("必须失败");
        assert!(matches!(error, HbotError::Internal { .. }), "5xx 应可重试");
    }
}
