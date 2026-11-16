//! Agent 侧的 `RunLog`：把"这一轮干了什么"写进 `logs` 表。
//!
//! ## 为什么要有这一层
//!
//! `logs_tail` 读的是 `logs` 表，而 Agent 链路此前**一条都不写**：模型调了哪个工具、
//! 成功还是失败、花了多久，用户在运行日志面板里完全看不到（`crates/hbot-parity` 的
//! 脱敏测试还专门记了这件事）。本模块补上这条通道，规则只有三条：
//!
//! 1. **一步一条**：每次工具调用写一行（成功/失败 + 耗时），不是"开始一行、结束一行、
//!    再补一行摘要"——那会把库刷爆，而 `logs_tail` 默认只取末尾几十行；
//! 2. **失败了必须留痕**：工具自身报错写 `error` 行，整轮失败也写一行；
//! 3. **密钥不许落库**：工具参数来自模型，里面既有文件内容也可能有 token。写库前一律
//!    过 [`redact_json`] / [`redact_text`]（键名命中敏感词、`Bearer xxx`、长十六进制串）。
//!
//! ## 为什么是"可注入的 sink"而不是直接写库
//!
//! `logs.bot_id` 从 0003 起可空：Agent 会话是人机对话（`SessionId`），不是平台会话
//! （`ConversationId`），两者之间没有可反查的外键。所以这一层只定义落点，由装配方注入：
//!
//! * Agent 行 → [`StoreRunLogSink`]（`bot_id = NULL`、`source = 'agent'`）；
//! * 测试或不关心落库 → [`NullRunLogSink`]（默认值，**不写库而不是写错归属**）。
//!
//! `AppState::open` 负责把 [`StoreRunLogSink`] 注入 `AgentManager`；会话 ID 由
//! `EventTurnSink` 以 `[session 3f2a…]` 前缀写进 `line`（`logs` 没有 session 列）。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hbot_agent::{ToolCallRequest, ToolOutput};
use hbot_core::{Clock, SessionId};
use hbot_store::{NewAgentLog, Store};
use serde_json::Value;

/// 脱敏后的占位符（日志里出现它就说明这一段被抹掉了）。
pub const REDACTED: &str = "<redacted>";

/// 参数进日志的字符上限（脱敏之后再截断）。
pub const MAX_ARG_CHARS: usize = 400;

/// 工具输出进日志的字符上限。
pub const MAX_OUTPUT_CHARS: usize = 240;

/// 单个参数**值**进日志的字符上限（先按值截断，再整体截断：否则一段长文件内容
/// 会把同一行里的其它参数全挤出 400 字符，日志就只剩半截正文）。
pub const MAX_ARG_VALUE_CHARS: usize = 120;

/// 整轮错误进日志的字符上限。
pub const MAX_ERROR_CHARS: usize = 400;

/// 连续十六进制串达到这个长度就当成疑似密钥（32 字节 = 64 个 hex 字符最常见，
/// 但 32 个 hex 字符已经是 128 bit，随机碰撞概率足够低）。
pub const MIN_SECRET_HEX_CHARS: usize = 32;

/// `Bearer` 后面至少这么长才算凭据。
///
/// 真实令牌（RFC 6750 的 `b64token`）远长于它；不设下限的话，"a bearer of bad
/// news" 这种正常英文句子会被抹成 "a bearer &lt;redacted&gt; bad news"。
pub const MIN_BEARER_TOKEN_CHARS: usize = 8;

/// 键名里出现这些片段就整段抹掉（小写比较）。
const SECRET_KEY_HINTS: [&str; 8] = [
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "authorization",
    "credential",
    "cookie",
];

/// 这个键名是否指向敏感值。
///
/// `key` 单独判：既要求整键等于 `key`，也接受 `api_key` / `-key` / `.key` 结尾。
/// 不直接用 `contains("key")` 是因为 `keywords` / `monkey` 之类的正常参数会被误杀
/// （工具参数里 `keywords` 很常见）。
pub fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if SECRET_KEY_HINTS
        .iter()
        .any(|hint| lower.contains(hint) && !lower.contains("tokenizer"))
    {
        return true;
    }
    lower == "key" || lower.ends_with("_key") || lower.ends_with("-key") || lower.ends_with(".key")
}

/// 抹掉文本里的疑似密钥（`Bearer xxx`、`token=xxx` 形式、长十六进制串）。
pub fn redact_text(text: &str) -> String {
    redact_hex_runs(&redact_named_values(&redact_bearer(text)))
}

/// 递归脱敏一个 JSON 值：敏感键名整段替换，字符串值做文本脱敏。
pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    if is_secret_key(key) {
                        (key.clone(), Value::String(REDACTED.to_owned()))
                    } else {
                        (key.clone(), redact_json(item))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        Value::String(text) => Value::String(redact_text(text)),
        other => other.clone(),
    }
}

/// 工具参数进日志的形状（按值截断 + 脱敏 + 整体截断）。
pub fn describe_arguments(arguments: &Value) -> String {
    // `serde_json::Map` 默认是 BTreeMap：这里再排一次序，保证同样的参数写出同样的行
    // （日志 diff 与测试断言都需要稳定顺序）。
    let bounded = truncate_values(arguments, MAX_ARG_VALUE_CHARS);
    let redacted = sort_keys(&redact_json(&bounded));
    truncate_chars(&redacted.to_string(), MAX_ARG_CHARS)
}

/// 工具输出进日志的形状（脱敏 + 截断）。
pub fn describe_output(output: &ToolOutput) -> String {
    let text = redact_text(&output.content);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&text, MAX_OUTPUT_CHARS)
}

/// 一次工具调用的日志行：工具名 + 成功/失败 + 耗时（+ 脱敏参数/输出）。
///
/// 返回 `(level, line)`：成功是 `info`，失败是 `error`（`logs_tail` 按它着色，用户
/// 一眼能看到哪一步炸了）。
pub fn tool_result_line(
    call: &ToolCallRequest,
    output: &ToolOutput,
    elapsed: Duration,
) -> (&'static str, String) {
    let millis = elapsed.as_millis();
    let arguments = describe_arguments(&call.arguments);
    if output.is_error {
        (
            "error",
            format!(
                "agent 工具 {} 失败（{millis} ms）参数 {arguments} 输出 {}",
                call.name,
                describe_output(output)
            ),
        )
    } else {
        (
            "info",
            format!(
                "agent 工具 {} 成功（{millis} ms）参数 {arguments}",
                call.name
            ),
        )
    }
}

/// 整轮失败的日志行。
pub fn turn_failure_line(error: &str) -> String {
    format!(
        "agent 本轮失败：{}",
        truncate_chars(&redact_text(error), MAX_ERROR_CHARS)
    )
}

/// 整轮取消的日志行（不是失败，别让用户以为炸了）。
pub fn turn_cancel_line() -> String {
    "agent 本轮已取消".to_owned()
}

/// 会话前缀里保留的字符数。UUID 前 8 位足够在一个面板里辨认会话。
pub const SESSION_PREFIX_CHARS: usize = 8;

/// 把会话 ID 转成日志行前缀：`[session 3f2a1b9c…] `。
///
/// `logs` 没有 session 列（见 0003 迁移注释）：UI 只需要"这行属于哪个会话"，
/// 放进 `line` 前缀比为一个展示字段扩表更小。
pub(crate) fn session_prefix(session_id: &SessionId) -> String {
    let full = session_id.to_string();
    let short: String = full.chars().take(SESSION_PREFIX_CHARS).collect();
    if short.chars().count() < full.chars().count() {
        format!("[session {short}…] ")
    } else {
        format!("[session {short}] ")
    }
}

/// `RunLog` 的落点。
///
/// 实现必须是**不阻塞调用方**的：`TurnSink` 的回调是同步 `fn`，跑在 tokio 任务里，
/// 在回调里等 SQLite 写会把整轮对话卡住。`StoreRunLogSink` 因此把写库丢给
/// `spawn_blocking`，落库失败只打 `tracing::error`（日志丢一行不该让对话失败）。
pub trait RunLogSink: Send + Sync + 'static {
    /// 追加一行；`level` 取 `"info"` / `"warn"` / `"error"`（与 `logs.level` 同词表）。
    fn append(&self, level: &str, line: String);
}

/// 不落库的 sink：装配方没有落点时用它（默认）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullRunLogSink;

impl RunLogSink for NullRunLogSink {
    fn append(&self, _level: &str, _line: String) {}
}

/// 把 Agent 日志写到 `logs` 表：`bot_id = NULL`、`source = 'agent'`。
pub struct StoreRunLogSink {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    /// 上一次 append 派出去的落库任务。
    ///
    /// `logs.id` 是自增主键、`logs_tail` 按 `id DESC` 读，所以**插入顺序就是显示顺序**。
    /// 若两次 append 各自起一个任务，两次 `spawn_blocking` 谁先拿到连接是抢跑的，
    /// 面板上"本轮失败"就可能排到"工具成功"前面（表现是同一毫秒里顺序颠倒）。
    /// 这里把同一 sink 的落库串成一条链：新任务先 `await` 前一个任务的句柄再写。
    /// 与 `AGENTS.md` 第 5 节第 7 条"同一落点的写入串成一条链"是同一个思路。
    tail: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StoreRunLogSink {
    /// 构造一个 Agent 日志 sink（不绑定 bot）。
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            tail: std::sync::Mutex::new(None),
        }
    }
}

impl RunLogSink for StoreRunLogSink {
    fn append(&self, level: &str, line: String) {
        let entry = NewAgentLog {
            level: level.to_owned(),
            line,
            at: self.clock.now().unix_millis(),
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let store = Arc::clone(&self.store);
                // 取走"上一个任务的句柄"，让这次的写排在它后面（见 `tail` 字段的注释）。
                let previous = {
                    let mut tail = self
                        .tail
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.take()
                };
                let task = runtime.spawn(async move {
                    // 等前一条落库完成再写：顺序是 `logs.id` 给的，不是调度给的。
                    if let Some(previous) = previous {
                        let _ = previous.await;
                    }
                    match tokio::task::spawn_blocking(move || store.append_agent_log(entry)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::error!(error = %error, "agent 运行日志落库失败");
                        }
                        Err(error) => {
                            tracing::error!(error = %error, "agent 运行日志落库任务失败");
                        }
                    }
                });
                // 把自己挂到链尾：下一次 append 会先等它。
                let mut tail = self
                    .tail
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *tail = Some(task);
            }
            // 没有运行时（同步上下文直接调用）：宁可慢一步也不能丢。
            Err(_) => {
                if let Err(error) = self.store.append_agent_log(entry) {
                    tracing::error!(error = %error, "agent 运行日志落库失败");
                }
            }
        }
    }
}

/// 稳定顺序：`serde_json` 默认 `BTreeMap` 已经有序，这里只在开了 `preserve_order`
/// 时兜底（编排形状不变，排序是可预测日志的一部分）。
fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: BTreeMap<&String, Value> = BTreeMap::new();
            for (key, item) in map {
                sorted.insert(key, sort_keys(item));
            }
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// 把每个字符串值截到 `max` 个字符（递归）。
fn truncate_values(value: &Value, max: usize) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), truncate_values(item, max)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| truncate_values(item, max))
                .collect(),
        ),
        Value::String(text) => Value::String(truncate_chars(text, max)),
        other => other.clone(),
    }
}

/// 把 `token=xxx` / `"password":"xxx"` 这类**文本形式**的键值抹掉。
///
/// JSON 参数走的是 [`redact_json`] 的按键脱敏；这里是给自由文本（错误消息、工具
/// 输出）兜底的：provider 的报错经常长成 `401: key=sk-live-...`。
fn redact_named_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        // 找最靠前的一个敏感键名命中。
        let hit = SECRET_KEY_HINTS
            .iter()
            .chain(std::iter::once(&"key"))
            .filter_map(|hint| lower[cursor..].find(hint).map(|at| (cursor + at, *hint)))
            .min_by_key(|(at, _)| *at);
        let Some((start, hint)) = hit else {
            break;
        };
        let key_end = start + hint.len();
        // 键名必须在**词首**（`monkey` 里的 `key` 不算），且后面紧跟分隔符。
        // 三种词首：串首、前一个字符是分隔符（`_` / `-` / `=` / 空格…）、驼峰。
        // 判据细节见 `is_key_start`；`_` 必须算分隔符，否则 RT-08 那族
        // `api_key=` / `access_token=` / `OPENAI_API_KEY=` 整条漏抹。
        if !is_key_start(text, start) || is_under_longer_word(&lower, key_end) {
            out.push_str(&text[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let mut separator = key_end;
        for ch in text[key_end..].chars() {
            if ch.is_whitespace() || matches!(ch, '"' | '\'' | '`') {
                separator += ch.len_utf8();
            } else {
                break;
            }
        }
        let Some(delimiter) = text[separator..].chars().next() else {
            out.push_str(&text[cursor..key_end]);
            cursor = key_end;
            continue;
        };
        if delimiter != '=' && delimiter != ':' {
            out.push_str(&text[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let mut value_start = separator + delimiter.len_utf8();
        for ch in text[value_start..].chars() {
            if ch.is_whitespace() || matches!(ch, '"' | '\'' | '`') {
                value_start += ch.len_utf8();
            } else {
                break;
            }
        }
        let mut value_end = value_start;
        for ch in text[value_start..].chars() {
            if ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | ',' | ';' | '}' | ']' | ')') {
                break;
            }
            value_end += ch.len_utf8();
        }
        out.push_str(&text[cursor..value_start]);
        // `Authorization: Bearer <token>`：`Bearer` 只是方案前缀，真正的凭据在它后面，
        // 由 [`redact_bearer`] 负责。这里把值留原样，否则会写成 `Authorization:
        // <redacted> <redacted>`（把方案名也抹了，读者不知道原本是什么认证方式）。
        let bearer_prefix = text[value_start..value_end].eq_ignore_ascii_case("bearer");
        if value_end > value_start && !bearer_prefix {
            out.push_str(REDACTED);
            cursor = value_end;
        } else {
            out.push_str(&text[value_start..value_end]);
            cursor = value_end;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// 命中位置是否是**键名的词首**。
///
/// 三条成立条件（满足一条即可）：
/// 1. 串首；
/// 2. 前一个字符不是字母数字 —— `_` / `-` / `=` / 空格 / 引号都算分隔符。`_` 必须
///    算分隔符：`api_key=` / `access_token=` / `OPENAI_API_KEY=` 命中的是后半段，
///    前一个字符正是 `_`；把它算成词内字符，下划线这整族写法就漏抹（RT-08）。
/// 3. 驼峰词首：前一个字符是小写字母或数字、当前字符是大写（`clientSecret=` 里命中
///    的 `Secret` 前一个是 `t`）。这条判据必须看**原文**大小写，`lower` 里已经没
///    有驼峰信息了。`monkey` / `tokenizer` / `keywords` 全小写，不受这条影响。
fn is_key_start(text: &str, start: usize) -> bool {
    if start == 0 {
        return true;
    }
    let Some(prev) = text[..start].chars().next_back() else {
        return true;
    };
    if !prev.is_alphanumeric() {
        return true;
    }
    let Some(current) = text[start..].chars().next() else {
        return false;
    };
    !prev.is_uppercase() && current.is_uppercase()
}

/// 命中位置的右边是否还是词内字符（`tokenizer` 里的 `token` 不算键名）。
///
/// 只看字母数字：`_` / `-` / `=` / `:` 都算词尾。`tokenizer` / `keywords` / `monkey`
/// 靠左边（`is_key_start`）排除，`secret_santa` / `api_key=` 靠右边这里把"键名还在
/// 继续"的写法挡掉（`key=` 命中后紧跟 `_` 说明是 `api_key` 的中间，不是键名）。
fn is_under_longer_word(lower: &str, key_end: usize) -> bool {
    lower[key_end..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '-')
}

/// 把 `Bearer <token>` 换成 `Bearer <redacted>`（大小写不敏感）。
fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find("bearer") {
        let start = cursor + offset;
        let keyword_end = start + "bearer".len();
        out.push_str(&text[cursor..keyword_end]);

        // 只认"bearer + 空白 + 凭据"；没空白说明只是普通词，原样留着继续扫。
        let mut value_start = keyword_end;
        for ch in text[keyword_end..].chars() {
            if ch.is_whitespace() {
                value_start += ch.len_utf8();
            } else {
                break;
            }
        }
        if value_start == keyword_end {
            cursor = keyword_end;
            continue;
        }

        let mut value_end = value_start;
        for ch in text[value_start..].chars() {
            if ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';' | '}' | ']' | ')') {
                break;
            }
            value_end += ch.len_utf8();
        }
        // 太短的"凭据"是普通单词（"a bearer of bad news"），不抹 —— 连空白也不动，
        // 否则会把原文的单个空格复制成两个。
        if value_end.saturating_sub(value_start) >= MIN_BEARER_TOKEN_CHARS {
            out.push_str(&text[keyword_end..value_start]);
            out.push_str(REDACTED);
            cursor = value_end;
        } else {
            cursor = keyword_end;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// 达到 [`MIN_SECRET_HEX_CHARS`] 的连续十六进制串换成 `<redacted>`。
fn redact_hex_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0usize;
    let mut run: Option<(usize, usize)> = None; // (start, len)
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_hexdigit() {
            run = match run {
                Some((start, len)) => Some((start, len + 1)),
                None => Some((index, 1)),
            };
        } else if let Some((start, len)) = run.take() {
            if len >= MIN_SECRET_HEX_CHARS {
                out.push_str(&text[copied..start]);
                out.push_str(REDACTED);
                copied = start + len;
            }
        }
    }
    if let Some((start, len)) = run {
        if len >= MIN_SECRET_HEX_CHARS {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = start + len;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// 按字符截断（不切 UTF-8），超长补 `…`。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::TempDir;
    use hbot_core::BotId;
    use hbot_store::{Bot, Project};
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: "call-1".to_owned(),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn bearer_tokens_are_redacted() {
        let line = redact_text("请求头 Authorization: Bearer sk-live-abcdef123456 已发送");
        assert!(!line.contains("sk-live-abcdef123456"), "实际：{line}");
        assert!(line.contains(&format!("Bearer {REDACTED}")), "实际：{line}");
        // 大小写不敏感，且不误伤普通单词。
        assert!(redact_text("BEARER   tok123456789").contains(REDACTED));
        assert_eq!(redact_text("a bearer of bad news"), "a bearer of bad news");
    }

    #[test]
    fn secret_fields_are_redacted_by_key() {
        let value = json!({
            "token": "sk-1",
            "api_key": "sk-2",
            "client_secret": "sk-3",
            "password": "hunter2",
            "authorization": "Basic abc",
            "cookie": "sid=1",
            "path": "src/main.ts",
        });
        let redacted = redact_json(&value);
        for key in [
            "token",
            "api_key",
            "client_secret",
            "password",
            "authorization",
            "cookie",
        ] {
            assert_eq!(
                redacted[key], REDACTED,
                "`{key}` 必须整段抹掉，实际 {}",
                redacted[key]
            );
        }
        assert_eq!(redacted["path"], "src/main.ts", "普通参数不该被动");
    }

    /// RT-08 的写法矩阵：同一种键名写成下划线 / 连字符 / 驼峰三种风格，都必须被抹掉。
    ///
    /// 这是 RT-08 的**回归判据**（红队用例 `f_secret_leak.rs::f06` 只覆盖下划线一族）。
    /// 词内字符判据一旦把 `_` 重新算进去、或驼峰词首判定被删掉，这张表里对应的行立刻
    /// 变红 —— 而它们正是「`RunLog` / `logs` 表长期可见」的那批写法。
    #[test]
    fn underscore_hyphen_and_camel_key_names_are_all_redacted() {
        // 值里不含任何键名片段，避免"抹掉的是值本身"这种假绿。
        let secret = "s3cr3t-9F8e7D6c";
        let lines = [
            // 下划线风格（RT-08 漏抹的那一族）
            format!("api_key={secret}"),
            format!("api_key: {secret}"),
            format!("access_token={secret}"),
            format!("OPENAI_API_KEY={secret}"),
            format!("client_secret={secret}"),
            format!("my_api_key={secret}"),
            format!("database_password={secret}"),
            format!("aws_secret_access_key={secret}"),
            format!("AWS_SECRET_ACCESS_KEY={secret}"),
            // 连字符风格
            format!("x-api-key: {secret}"),
            format!("x-auth-token={secret}"),
            format!("client-secret={secret}"),
            // 驼峰风格
            format!("apiKey={secret}"),
            format!("accessToken={secret}"),
            format!("clientSecret={secret}"),
            format!("dbPassword={secret}"),
            // 裸写法（改动前的既有效果，防止回归）
            format!("token={secret}"),
            format!("Authorization: Bearer {secret}"),
        ];
        for line in &lines {
            let redacted = redact_text(line);
            assert!(
                !redacted.contains(secret),
                "RT-08：`{line}` 的值没被抹掉 → {redacted}"
            );
            assert!(
                redacted.contains(REDACTED),
                "`{line}` 抹了但没留 `{REDACTED}` 标记 → {redacted}"
            );
        }
        // 反向：键名本身要留着（否则日志看不出是哪个字段），普通词的片段不许被误杀。
        assert_eq!(
            redact_text(&format!("api_key={secret}")),
            "api_key=<redacted>"
        );
        assert_eq!(
            redact_text(&format!("clientSecret={secret}")),
            "clientSecret=<redacted>"
        );
        for line in [
            "tokenizer=slow",
            "keywords=rust",
            "monkey=banana",
            "secret_santa",
            "passwords_are_hashed",
        ] {
            assert_eq!(redact_text(line), line, "`{line}` 不该被抹");
        }
    }

    #[test]
    fn ordinary_keys_that_merely_contain_key_survive() {
        // `keywords` / `monkey` / `tokenizer` 都含 "key"/"token" 片段，但不是密钥。
        assert!(!is_secret_key("keywords"));
        assert!(!is_secret_key("monkey"));
        assert!(!is_secret_key("tokenizer"));
        assert!(is_secret_key("key"));
        assert!(is_secret_key("api_key"));
        assert!(is_secret_key("access-key"));
        assert!(is_secret_key("Token"));
    }

    #[test]
    fn long_hex_runs_are_redacted_but_short_ones_survive() {
        let secret: String = "ab".repeat(32); // 64 hex 字符
        let short = "ab".repeat(10); // 20 hex 字符
        let line = redact_text(&format!("digest={secret} short={short}"));
        assert!(!line.contains(&secret), "实际：{line}");
        assert!(line.contains(REDACTED));
        assert!(line.contains(&short), "短的十六进制串（如哈希前缀）保留");
    }

    #[test]
    fn arguments_are_redacted_then_truncated() {
        let arguments = json!({
            "path": "src/main.ts",
            "token": "sk-live-123456",
            "content": "x".repeat(2000),
        });
        let described = describe_arguments(&arguments);
        assert!(!described.contains("sk-live-123456"), "实际：{described}");
        assert!(
            described.contains(REDACTED),
            "密钥字段要留下脱敏标记（不是被整行截断吃掉）：{described}"
        );
        assert!(
            described.chars().count() <= MAX_ARG_CHARS + 1,
            "参数必须截断，实际 {}",
            described.chars().count()
        );
        // 单个值先截断：长 content 不能把 path/token 挤出同一行（否则日志只剩半截正文）。
        assert!(described.contains("src/main.ts"), "实际：{described}");
        assert!(
            described.matches('…').count() >= 1,
            "被截断的值要留省略标记：{described}"
        );
    }

    #[test]
    fn tool_lines_carry_name_outcome_and_duration() {
        let ok = ToolOutput::text("done");
        let (level, line) = tool_result_line(
            &call("fs_read", json!({"path": "a.txt"})),
            &ok,
            Duration::from_millis(12),
        );
        assert_eq!(level, "info");
        assert!(line.contains("fs_read"), "实际：{line}");
        assert!(line.contains("成功"), "实际：{line}");
        assert!(line.contains("12 ms"), "耗时要在行里：{line}");

        let failed = ToolOutput::error("permission denied");
        let (level, line) = tool_result_line(
            &call("shell_run", json!({"command": "ls"})),
            &failed,
            Duration::from_millis(3),
        );
        assert_eq!(level, "error");
        assert!(line.contains("shell_run"));
        assert!(line.contains("失败"));
        assert!(line.contains("3 ms"));
        assert!(line.contains("permission denied"), "失败原因要留下：{line}");
    }

    #[test]
    fn output_secrets_never_reach_the_line() {
        let secret = "f".repeat(64);
        let output = ToolOutput::text(format!("read file: token=supersecret {secret}"));
        let (_, line) = tool_result_line(
            &call("fs_read", json!({})),
            &output,
            Duration::from_millis(1),
        );
        assert!(!line.contains(&secret), "实际：{line}");
        assert!(!line.contains("supersecret"), "实际：{line}");
    }

    #[test]
    fn turn_failure_line_is_redacted() {
        let line = turn_failure_line("provider 拒绝了请求：key=sk-live-9");
        assert!(line.starts_with("agent 本轮失败："), "实际：{line}");
        assert!(!line.contains("sk-live-9"), "实际：{line}");
    }

    /// 端到端：真 `Store` → Agent 行真的进了 `logs` 表，`logs_tail { bot_id: null }` 读得到，
    /// 且 `logs_tail { bot_id: Some(x) }` 看不到（Agent 行不属于任何 bot）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn store_backed_sink_writes_rows_that_logs_tail_can_read() {
        let dir = TempDir::new("agent-run-log");
        let store = Arc::new(Store::open(dir.path().join("hbot.db")).expect("建库"));
        let project_id = hbot_core::ProjectId::from_uuid(uuid::Uuid::new_v4());
        let bot_id = BotId::from_uuid(uuid::Uuid::new_v4());
        store
            .insert_project(&Project {
                id: project_id,
                name: "p".to_owned(),
                path: dir.path().join("project").display().to_string(),
                created_at: 1,
            })
            .expect("插项目");
        store
            .insert_bot(&Bot {
                id: bot_id,
                project_id,
                name: "b".to_owned(),
                platform: "onebot11".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 1,
            })
            .expect("插 bot");

        let sink = StoreRunLogSink::new(Arc::clone(&store), Arc::new(hbot_adapters::SystemClock));
        sink.append("info", "agent 工具 fs_read 成功（1 ms）".to_owned());
        sink.append("error", "agent 本轮失败：boom".to_owned());

        // 落库是异步的（spawn_blocking）：给它一点时间，再按 `logs_tail` 的读法读。
        let mut rows = Vec::new();
        for _ in 0..200 {
            rows = store.list_logs(None, 16).expect("读日志");
            if rows.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(rows.len(), 2, "两条都应该落库，实际 {rows:?}");
        // `list_logs` 是 id DESC（新→旧）。
        assert_eq!(rows[0].level, "error");
        assert!(rows[0].line.contains("boom"));
        assert_eq!(rows[1].level, "info");
        assert!(rows[1].line.contains("fs_read"));
        assert!(rows.iter().all(|row| row.bot_id.is_none()));
        assert!(rows.iter().all(|row| row.source == "agent"));

        // 按 bot 过滤时 Agent 行必须被排除（不能挂到任意 bot 的日志流上）。
        let bot_rows = store.list_logs(Some(&bot_id), 16).expect("读 bot 日志");
        assert!(
            bot_rows.is_empty(),
            "Agent 行不该出现在 bot 日志里：{bot_rows:?}"
        );
    }
}
