//! OpenAI 兼容 `/chat/completions`（SSE 流式）客户端。
//!
//! 结构上刻意分成两层：
//!
//! 1. **纯逻辑**（不碰网络、不碰 IO）：
//!    - [`parse_sse_line`]：一行 SSE → 忽略 / 数据 / 结束；
//!    - [`ToolCallAssembler`]：按 `index` 聚合分片到达的 `tool_calls`；
//!    - [`StreamDecoder`]：字节流 → 文本增量 / 工具调用 / usage / `finish_reason`；
//!    - [`build_request_body`]：请求 → OpenAI JSON。
//!
//!    这些都能用字符串直接测，不需要 `httptest`。
//! 2. **薄 HTTP 层**：[`HttpTransport`] + [`OpenAiProvider`]。传输实现可注入，
//!    测试用假传输；真实 `reqwest` 实现见 `未做项`（本 crate 当前不引入 `reqwest`）。
//!
//! 取消：每次收 chunk 前检查 [`CancellationToken`]，取消后立即返回，不再读流。

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use hbot_core::HbotError;
use serde_json::{Value, json};

use super::{
    ChatMessage, Completion, CompletionRequest, FinishReason, Provider, ProviderError, StreamSink,
    ToolCallRequest, Usage,
};
use crate::cancel::CancellationToken;

/// 一行 SSE 的解析结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseLine<'a> {
    /// 空行、注释（`:` 开头）、`event:` / `id:` 等非数据行。
    Ignore,
    /// `data: <payload>`；`payload` 已去掉前缀与首尾空白。
    Data(&'a str),
    /// `data: [DONE]`。
    Done,
}

/// 解析一行 SSE。
///
/// 接受 `data: {...}`、`data:{...}`、带 `\r\n` 的行；不认识的字段返回
/// [`SseLine::Ignore`]，由调用方跳过。
pub fn parse_sse_line(line: &str) -> SseLine<'_> {
    let line = line.trim_end_matches(['\r', '\n']);
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return SseLine::Ignore;
    }
    let Some(rest) = trimmed.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = rest.trim();
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    if payload.is_empty() {
        return SseLine::Ignore;
    }
    SseLine::Data(payload)
}

/// 把字节流切成完整行（UTF-8 可能跨 chunk 边界，所以按字节缓冲）。
#[derive(Debug, Default)]
pub struct SseLineBuffer {
    buffer: Vec<u8>,
}

impl SseLineBuffer {
    /// 新建空缓冲。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回其中所有**完整**行的文本（`\r` 已去掉）。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.buffer[start..end])
                .trim_end_matches('\r')
                .to_string();
            lines.push(line);
            start = end + 1;
        }
        if start > 0 {
            self.buffer.drain(..start);
        }
        lines
    }

    /// 取出没有换行结尾的残留文本（流结束时调用）。
    pub fn take_remainder(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&self.buffer).to_string();
        self.buffer.clear();
        Some(text.trim_end_matches('\r').to_string())
    }
}

#[derive(Debug, Default)]
struct ToolCallSlot {
    id: String,
    name: String,
    arguments: String,
}

/// 按 `index` 聚合流式 `tool_calls` 增量。
///
/// OpenAI 兼容实现会把一个工具调用的 `id` / `name` / `arguments` 拆到多条 chunk 里，
/// `arguments` 是**字符串拼接**（不是 JSON 合并）。这里按 `index` 归并，
/// 最后统一解析参数 JSON。
#[derive(Debug, Default)]
pub struct ToolCallAssembler {
    slots: BTreeMap<u64, ToolCallSlot>,
}

impl ToolCallAssembler {
    /// 新建空聚合器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条增量。
    ///
    /// 同一个 `index` 的 `id` / `name` 必须一致（重复给相同值是允许的），
    /// 不一致说明流被污染，直接报错而不是猜。
    pub fn push(
        &mut self,
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> Result<(), HbotError> {
        let slot = self.slots.entry(index).or_default();
        if let Some(id) = id.filter(|value| !value.is_empty()) {
            if slot.id.is_empty() {
                slot.id = id.to_string();
            } else if slot.id != id {
                return Err(HbotError::invalid(format!(
                    "tool_calls 增量在 index {index} 上给出冲突的 id：{} vs {id}",
                    slot.id
                )));
            }
        }
        if let Some(name) = name.filter(|value| !value.is_empty()) {
            if slot.name.is_empty() {
                slot.name = name.to_string();
            } else if slot.name != name {
                return Err(HbotError::invalid(format!(
                    "tool_calls 增量在 index {index} 上给出冲突的工具名：{} vs {name}",
                    slot.name
                )));
            }
        }
        if let Some(arguments) = arguments {
            slot.arguments.push_str(arguments);
        }
        Ok(())
    }

    /// 已聚合的调用数。
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 是否没有任何调用。
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 产出完整调用（按 `index` 升序），参数解析成 JSON。
    pub fn finish(&self) -> Result<Vec<ToolCallRequest>, HbotError> {
        let mut calls = Vec::with_capacity(self.slots.len());
        for (index, slot) in &self.slots {
            if slot.id.is_empty() {
                return Err(HbotError::invalid(format!(
                    "tool_calls（index {index}）缺少 id，无法回填结果"
                )));
            }
            if slot.name.is_empty() {
                return Err(HbotError::invalid(format!(
                    "tool_calls（index {index}）缺少工具名"
                )));
            }
            let raw = slot.arguments.trim();
            let arguments = if raw.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(raw).map_err(|err| {
                    HbotError::invalid(format!(
                        "工具 {} 的参数不是合法 JSON（{err}）：{}",
                        slot.name,
                        snippet(raw)
                    ))
                })?
            };
            calls.push(ToolCallRequest {
                id: slot.id.clone(),
                name: slot.name.clone(),
                arguments,
            });
        }
        Ok(calls)
    }
}

/// 把 SSE 字节流解码成 [`Completion`]。
///
/// 用状态机而不是一次性读全响应：UI 需要边生成边显示，取消也需要能打断。
#[derive(Debug, Default)]
pub struct StreamDecoder {
    lines: SseLineBuffer,
    assembler: ToolCallAssembler,
    content: String,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    done: bool,
}

impl StreamDecoder {
    /// 新建解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节；文本增量与 usage 会立刻回调 `sink`。
    pub fn feed(&mut self, chunk: &[u8], sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        for line in self.lines.feed(chunk) {
            self.handle_line(&line, sink)?;
        }
        Ok(())
    }

    /// 结束解码：处理残留行、产出完整工具调用（此时才回调 `sink.on_tool_call`）。
    pub fn finish(mut self, sink: &mut dyn StreamSink) -> Result<Completion, HbotError> {
        if let Some(rest) = self.lines.take_remainder() {
            self.handle_line(&rest, sink)?;
        }
        let tool_calls = self.assembler.finish()?;
        for call in &tool_calls {
            sink.on_tool_call(call);
        }
        let finish_reason = self.finish_reason.take().unwrap_or({
            if tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }
        });
        let usage = self
            .usage
            .take()
            .unwrap_or_default()
            .with_total_from_parts();
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

    fn handle_line(&mut self, line: &str, sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        if self.done {
            return Ok(());
        }
        match parse_sse_line(line) {
            SseLine::Ignore => Ok(()),
            SseLine::Done => {
                self.done = true;
                Ok(())
            }
            SseLine::Data(payload) => self.apply_chunk(payload, sink),
        }
    }

    fn apply_chunk(&mut self, payload: &str, sink: &mut dyn StreamSink) -> Result<(), HbotError> {
        let value: Value = serde_json::from_str(payload).map_err(|err| {
            HbotError::invalid(format!(
                "provider 返回非法 SSE JSON（{err}）：{}",
                snippet(payload)
            ))
        })?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider 未给出错误详情");
            return Err(HbotError::internal(format!("provider 返回错误：{message}")));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            let usage = parse_usage(usage);
            self.usage = Some(usage);
            sink.on_usage(usage);
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            // `usage`-only 的 chunk（OpenAI 的 include_usage）没有 choices。
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.content.push_str(text);
                sink.on_text_delta(text);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::try_from(position).unwrap_or(u64::MAX));
                    let id = call.get("id").and_then(Value::as_str);
                    let function = call.get("function");
                    let name = function
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str);
                    let arguments = function
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str);
                    self.assembler.push(index, id, name, arguments)?;
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(FinishReason::from_wire(reason));
        }
        Ok(())
    }
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
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

/// 构造 OpenAI 兼容请求体。
///
/// 纯函数，便于断言"消息/工具/采样参数怎么映射到线上字段"。
pub fn build_request_body(req: &CompletionRequest) -> Result<Value, HbotError> {
    let mut messages = Vec::with_capacity(req.messages.len());
    for message in &req.messages {
        messages.push(message_to_json(message)?);
    }
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        // 让兼容实现把 usage 放在最后一个 chunk；不支持的实现会忽略该字段。
        "stream_options": { "include_usage": true },
    });
    if let Some(temperature) = req.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    Ok(body)
}

fn message_to_json(message: &ChatMessage) -> Result<Value, HbotError> {
    Ok(match message {
        ChatMessage::System { content } => json!({ "role": "system", "content": content }),
        ChatMessage::User { content } => json!({ "role": "user", "content": content }),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut value = json!({
                "role": "assistant",
                "content": content.clone().map_or(Value::Null, Value::String),
            });
            if !tool_calls.is_empty() {
                let mut calls = Vec::with_capacity(tool_calls.len());
                for call in tool_calls {
                    calls.push(json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": arguments_text(&call.arguments)?,
                        }
                    }));
                }
                value["tool_calls"] = Value::Array(calls);
            }
            value
        }
        ChatMessage::Tool {
            tool_call_id,
            content,
        } => json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        }),
    })
}

fn arguments_text(arguments: &Value) -> Result<String, HbotError> {
    serde_json::to_string(arguments)
        .map_err(|err| HbotError::internal(format!("工具参数无法序列化：{err}")))
}

/// 一次 HTTP 请求（SSE）。
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// 完整 URL。
    pub url: String,
    /// 请求头（键值原样发送）。
    pub headers: Vec<(String, String)>,
    /// JSON 请求体。
    pub body: String,
    /// 连接/读取总超时。
    pub timeout: Duration,
}

/// 响应字节流出口。
pub trait ResponseSink: Send {
    /// 收到一段响应字节（不保证在行边界）。
    fn on_chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError>;
}

/// HTTP 传输层。
///
/// 抽成 trait 有两个原因：
///
/// 1. 本 crate 不引入 `reqwest`（依赖方向 + 版本锁定，见实施说明）；
/// 2. 测试可以注入假传输，断言"请求长什么样、取消后不发请求"，完全不碰网络。
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// 发起请求，把响应字节按到达顺序喂给 `sink`。
    ///
    /// 约定：必须在 `cancel` 触发后尽快返回错误，不得继续读取响应。
    async fn post(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
        sink: &mut dyn ResponseSink,
    ) -> Result<(), ProviderError>;
}

/// 显式不可用的传输层。
///
/// 真实 `reqwest` 客户端是未做项（不引入网络依赖）；用这个类型可以让
/// [`OpenAiProvider`] 可构造、可接线，但调用会明确失败，**不伪造成功**。
#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledTransport;

#[async_trait]
impl HttpTransport for DisabledTransport {
    async fn post(
        &self,
        _request: HttpRequest,
        _cancel: CancellationToken,
        _sink: &mut dyn ResponseSink,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::fatal(HbotError::denied(
            "真实 HTTP 传输尚未接入（provider 只实现了 SSE 解析与薄封装）",
        )))
    }
}

/// 把 [`StreamDecoder`] 接到 [`ResponseSink`] 上，并在每个 chunk 前检查取消。
struct DecoderSink<'a> {
    decoder: &'a mut StreamDecoder,
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

/// OpenAI 兼容 provider（薄封装）。
pub struct OpenAiProvider<T: HttpTransport> {
    base_url: String,
    api_key: String,
    transport: T,
    timeout: Duration,
}

impl<T: HttpTransport> OpenAiProvider<T> {
    /// 新建；`base_url` 形如 `https://api.example.com/v1`。
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, transport: T) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            transport,
            timeout: Duration::from_secs(120),
        }
    }

    /// 设置总超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `/chat/completions` 完整 URL。
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl<T: HttpTransport> Provider for OpenAiProvider<T> {
    async fn complete(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> Result<Completion, HbotError> {
        req.cancel.check()?;
        let body = build_request_body(&req)?;
        let body = serde_json::to_string(&body)
            .map_err(|err| HbotError::internal(format!("请求体无法序列化：{err}")))?;
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
        ];
        if !self.api_key.is_empty() {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ));
        }
        let request = HttpRequest {
            url: self.endpoint(),
            headers,
            body,
            timeout: self.timeout,
        };
        let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
        let mut decoder = StreamDecoder::new();
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
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn sse_line_parsing() {
        assert_eq!(parse_sse_line(""), SseLine::Ignore);
        assert_eq!(parse_sse_line(": keep-alive"), SseLine::Ignore);
        assert_eq!(parse_sse_line("event: message"), SseLine::Ignore);
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(
            parse_sse_line("data: {\"a\":1}"),
            SseLine::Data("{\"a\":1}")
        );
        assert_eq!(
            parse_sse_line("data:{\"a\":1}\r"),
            SseLine::Data("{\"a\":1}")
        );
        assert_eq!(parse_sse_line("data:"), SseLine::Ignore);
    }

    #[test]
    fn line_buffer_handles_split_lines_and_utf8() {
        // 把多字节字符从中间切开，验证按字节缓冲不会产生乱码。
        let raw = "data: {\"a\":\"错误\"}\ndata: [DONE]\n".as_bytes();
        let cut = raw
            .iter()
            .position(|byte| *byte >= 0x80)
            .map(|position| position + 1)
            .expect("样本必须含多字节字符");
        let mut buffer = SseLineBuffer::new();
        let mut lines = buffer.feed(&raw[..cut]);
        assert!(lines.is_empty(), "半行不得输出");
        lines.extend(buffer.feed(&raw[cut..]));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("错误"));
        assert_eq!(lines[1], "data: [DONE]");
        assert!(buffer.take_remainder().is_none());
    }

    #[test]
    fn tool_calls_are_assembled_by_index() {
        let mut assembler = ToolCallAssembler::new();
        assembler
            .push(0, Some("call_a"), Some("read_file"), Some("{\"path\":"))
            .expect("first fragment");
        assembler
            .push(
                1,
                Some("call_b"),
                Some("list_dir"),
                Some("{\"path\":\"src\"}"),
            )
            .expect("second call");
        assembler
            .push(0, None, None, Some("\"a.txt\"}"))
            .expect("second fragment");
        let calls = assembler.finish().expect("assembled");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
        assert_eq!(calls[1].arguments, json!({"path": "src"}));
    }

    #[test]
    fn empty_arguments_become_object() {
        let mut assembler = ToolCallAssembler::new();
        assembler
            .push(0, Some("call"), Some("list_dir"), None)
            .expect("delta");
        let calls = assembler.finish().expect("assembled");
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn conflicting_tool_call_ids_are_errors() {
        let mut assembler = ToolCallAssembler::new();
        assembler
            .push(0, Some("call_a"), Some("read_file"), None)
            .expect("first");
        let error = assembler
            .push(0, Some("call_b"), None, None)
            .expect_err("conflict must fail");
        assert!(matches!(error, HbotError::Invalid { .. }));
    }

    #[test]
    fn decoder_emits_deltas_tool_calls_and_usage() {
        let mut decoder = StreamDecoder::new();
        let mut sink = RecordingSink::default();
        let chunks = [
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n",
            "data: [DONE]\n",
        ];
        for chunk in chunks {
            decoder.feed(chunk.as_bytes(), &mut sink).expect("decode");
        }
        let completion = decoder.finish(&mut sink).expect("finish");
        assert_eq!(completion.text(), "你好");
        assert_eq!(completion.usage.total_tokens, 18);
        assert_eq!(completion.finish_reason, FinishReason::ToolCalls);
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].arguments, json!({"path": "a.txt"}));
        assert_eq!(sink.deltas, vec!["你好".to_string()]);
        assert_eq!(sink.tool_calls.len(), 1);
        assert_eq!(sink.usage.len(), 1);
    }

    #[test]
    fn decoder_splits_multibyte_across_chunks() {
        let mut decoder = StreamDecoder::new();
        let mut sink = RecordingSink::default();
        let line =
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"中文\"}}]}\n".to_string();
        let bytes = line.as_bytes();
        let split = line.find('中').expect("contains chinese") + 1;
        decoder.feed(&bytes[..split], &mut sink).expect("head");
        decoder.feed(&bytes[split..], &mut sink).expect("tail");
        let completion = decoder.finish(&mut sink).expect("finish");
        assert_eq!(completion.text(), "中文");
    }

    #[test]
    fn decoder_rejects_malformed_json() {
        let mut decoder = StreamDecoder::new();
        let mut sink = RecordingSink::default();
        let error = decoder
            .feed(b"data: {not json}\n", &mut sink)
            .expect_err("must fail");
        assert!(matches!(error, HbotError::Invalid { .. }));
    }

    #[test]
    fn decoder_surfaces_provider_error_object() {
        let mut decoder = StreamDecoder::new();
        let mut sink = RecordingSink::default();
        let error = decoder
            .feed(
                b"data: {\"error\":{\"message\":\"rate limited\"}}\n",
                &mut sink,
            )
            .expect_err("must fail");
        assert!(matches!(error, HbotError::Internal { .. }));
    }

    #[test]
    fn request_body_maps_messages_and_tools() {
        let req = CompletionRequest {
            model: "test-model".to_string(),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                },
                ChatMessage::tool_result("call_1", "ok"),
            ],
            tools: vec![super::super::ToolSpec {
                name: "read_file".to_string(),
                description: "读取文件".to_string(),
                parameters: json!({"type": "object"}),
            }],
            temperature: Some(0.2),
            max_tokens: Some(512),
            cancel: CancellationToken::new(),
        };
        let body = build_request_body(&req).expect("body");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        // f32 转 JSON number 会有精度尾巴，按容差比较。
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature 必须是数字");
        assert!(
            (temperature - 0.2).abs() < 1e-6,
            "temperature 应约等于 0.2，实际 {temperature}"
        );
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.txt\"}"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
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

    /// 假传输的共享状态：测试里留一个句柄，可在 provider 消费传输后继续断言。
    #[derive(Default)]
    struct TransportState {
        chunks: Vec<Vec<u8>>,
        requests: Vec<HttpRequest>,
        status: Option<u16>,
    }

    /// 假传输：按脚本回放字节块，并记录收到的请求。
    ///
    /// 内部用 `Arc<Mutex<…>>` 而不是所有权字段，这样测试可以在
    /// `provider.complete()`（会取得传输的所有权）之后继续检查请求。
    #[derive(Clone, Default)]
    struct ScriptedTransport {
        state: Arc<Mutex<TransportState>>,
    }

    impl ScriptedTransport {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            let transport = Self::default();
            if let Ok(mut state) = transport.state.lock() {
                state.chunks = chunks;
            }
            transport
        }

        fn with_status(status: u16) -> Self {
            let transport = Self::default();
            if let Ok(mut state) = transport.state.lock() {
                state.status = Some(status);
            }
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
    async fn provider_end_to_end_through_fake_transport() {
        let transport = ScriptedTransport::new(vec![
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n".to_vec(),
            b"data: [DONE]\n".to_vec(),
        ]);
        let provider = OpenAiProvider::new("https://example.test/v1/", "sk-test", transport)
            .with_timeout(Duration::from_secs(5));
        let mut sink = RecordingSink::default();
        let completion = provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");
        assert_eq!(completion.text(), "hi");
        assert_eq!(sink.deltas, vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn provider_sends_auth_header_endpoint_and_stream_body() {
        let transport = ScriptedTransport::new(vec![b"data: [DONE]\n".to_vec()]);
        let handle = transport.clone();
        let provider = OpenAiProvider::new("https://example.test/v1/", "sk-test", transport);
        let mut sink = RecordingSink::default();
        provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");

        let requests = handle.requests();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert_eq!(sent.url, "https://example.test/v1/chat/completions");
        assert!(
            sent.headers
                .iter()
                .any(|(key, value)| key == "Authorization" && value == "Bearer sk-test"),
            "缺少 Authorization 头：{:?}",
            sent.headers
        );
        assert!(
            sent.body.contains("\"stream\":true"),
            "请求体必须开流式：{}",
            sent.body
        );
        assert_eq!(
            OpenAiProvider::new("https://example.test/v1", "k", DisabledTransport).endpoint(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn provider_omits_authorization_without_api_key() {
        let transport = ScriptedTransport::new(vec![b"data: [DONE]\n".to_vec()]);
        let handle = transport.clone();
        let provider = OpenAiProvider::new("https://example.test/v1", "", transport);
        let mut sink = RecordingSink::default();
        provider
            .complete(request(), &mut sink)
            .await
            .expect("completion");
        let requests = handle.requests();
        assert!(
            requests[0]
                .headers
                .iter()
                .all(|(key, _)| key != "Authorization")
        );
    }

    #[tokio::test]
    async fn provider_does_not_call_transport_when_already_cancelled() {
        let transport = ScriptedTransport::new(vec![b"data: [DONE]\n".to_vec()]);
        let handle = transport.clone();
        let provider = OpenAiProvider::new("https://example.test/v1", "k", transport);
        let req = request();
        req.cancel.cancel();
        let mut sink = RecordingSink::default();
        let error = provider
            .complete(req, &mut sink)
            .await
            .expect_err("must be cancelled");
        assert!(matches!(error, HbotError::Denied { .. }));
        assert!(handle.requests().is_empty(), "取消后不得发起任何请求");
    }

    #[tokio::test]
    async fn provider_maps_retryable_status() {
        let transport = ScriptedTransport::with_status(503);
        let provider = OpenAiProvider::new("https://example.test/v1", "k", transport);
        let mut sink = RecordingSink::default();
        let error = provider
            .complete(request(), &mut sink)
            .await
            .expect_err("must fail");
        assert!(matches!(error, HbotError::Internal { .. }), "5xx 应可重试");
    }

    #[tokio::test]
    async fn provider_times_out_transport() {
        /// 永不返回的传输，用来验证超时映射。
        struct HangTransport;

        #[async_trait]
        impl HttpTransport for HangTransport {
            async fn post(
                &self,
                _request: HttpRequest,
                _cancel: CancellationToken,
                _sink: &mut dyn ResponseSink,
            ) -> Result<(), ProviderError> {
                std::future::pending::<()>().await;
                Ok(())
            }
        }

        let provider = OpenAiProvider::new("https://example.test/v1", "k", HangTransport)
            .with_timeout(Duration::from_millis(20));
        let mut sink = RecordingSink::default();
        let error = provider
            .complete(request(), &mut sink)
            .await
            .expect_err("must time out");
        assert!(matches!(error, HbotError::Timeout { .. }));
    }
}
