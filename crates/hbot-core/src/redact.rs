//! 通用脱敏：把**密钥**换成 [`REDACTED`]。
//!
//! 为什么需要它：`docs/security/red-team-findings.md` 的 RT-06。`CommandSpec` /
//! `ToolCallRequest` / `InboundMessage` 一旦 `#[derive(Debug)]`，含密钥的参数、环境
//! 变量和入站 payload 就会顺着 `tracing::warn!(?spec, …)`、`thiserror` 的 `#[source]`
//! 链和"发给模型的工具输出"进入 `logs` 表 —— 用户一旦在 `agent.json` 里配了 key，或
//! 入站事件带着 `Authorization` / `auth_token`，那份密钥就永久留在库里。所以这几个
//! 类型**手写 `Debug`**，每个字段都过一遍本模块。
//!
//! 判据只有一份（本文件）：`hbot-agent` / `hbot-adapters` 直接 `use hbot_core::…`，
//! 谁也不许再写第二套白名单。对外两个入口，按"要不要保留调试价值"分档：
//!
//! - [`redact_arg`]：**`Debug` 用**。只认两种证据 —— 敏感**键名**后面的值
//!   （`token=…` / `"api_key": "…"` / `--token=…`）和 `Bearer <token>`。
//!   不做十六进制串猜测：`Debug` 是要给人看的，40 位 commit SHA 不该凭空消失。
//! - [`redact_text`]：**日志正文用**。[`redact_arg`] 之外再抹长十六进制串（与
//!   `hbot-rpc` 的 `app::agent::log` 对齐），用于"整行文本"这种没有键名可依的场景。
//!
//! # 两份实现（该合并，但本轮不合并）
//!
//! `crates/hbot-rpc/src/app/agent/log.rs` 里还有一份**日志专用**脱敏
//! （`is_secret_key` / `redact_text` / `redact_json`），判据是"键名 `contains` 关键词"。
//! 两份应该合并成一份，但 `hbot-rpc` 不在 RT-06 的写域内（另一个单元在改它），
//! 所以本轮只新增本文件，并在 rpc 侧留待合并。两者**行为差异**有两处，都是本文件
//! 刻意的：
//!
//! | 键名 | rpc 日志版（`contains`） | 本文件（切词 + 精确匹配） |
//! |---|---|---|
//! | `max_tokens` | 误抹（含 `token`） | **不抹**（`max` 是计量词） |
//! | `tokenizer` | 不抹（硬编码例外） | 不抹（切词后是 `tokenizer` 一个词） |
//!
//! # 为什么 `max_tokens` 不能抹
//!
//! 它是**数字限额**（`CompletionRequest.max_tokens`、各家 provider 的
//! `max_output_tokens`），抹掉之后调参日志只剩 `max_tokens: <redacted>`，
//! 排查"为什么被截断"时完全读不出来 —— 而它从来不是凭据。判据因此先切词：
//! 只有 `token`/`tokens`/`key`/`keys` 这些词**单独**充当凭据词义时才抹，
//! 且同一键名里出现计量词（`max`/`limit`/`count`/`prompt` …）时按计量单位处理。

use std::fmt;

use serde_json::{Map, Value};

/// 脱敏占位符。
///
/// 全仓库只此一份字面量：`hbot-rpc` 的日志版有一份同名常量，合并时以本处为准。
pub const REDACTED: &str = "<redacted>";

/// 敏感**词**（键名先按 [`is_secret_key`] 的规则切词，再与这里精确比较）。
///
/// 只放"单独出现就一定是凭据"的词：`key` / `keys` 也算（`X-Api-Key`、
/// `access_key`、`signing_key` 都靠它），误抹一个普通键名的代价远小于漏一个密钥。
const SECRET_WORDS: &[&str] = &[
    "token",
    "tokens",
    "secret",
    "secrets",
    "password",
    "passwords",
    "passwd",
    "apikey",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "cookies",
    "key",
    "keys",
    "accesskey",
    "secretkey",
    "privatekey",
    "passphrase",
];

/// 计量词：与 `token(s)` / `key(s)` 同处一个键名时，后者是**单位**而不是凭据。
///
/// 例：`max_tokens`、`prompt_tokens`、`completion_tokens`、`token_count`、`max_keys`。
const COUNT_WORDS: &[&str] = &[
    "max",
    "min",
    "limit",
    "count",
    "num",
    "number",
    "size",
    "budget",
    "prompt",
    "completion",
    "total",
    "per",
    "used",
    "remaining",
    "window",
    "len",
    "length",
];

/// `Bearer <token>` 里 token 的最短长度（更短的多半是占位符或方案名）。
const MIN_BEARER_TOKEN_CHARS: usize = 8;

/// 连续十六进制串的最短长度（只 [`redact_text`] 用）。
///
/// 32 位起：`sk-…` 这类前缀会先被键名/Bearer 判据处理，这里兜的是"裸 hex 令牌"
/// （32 位 = MD5 长度，真实密钥几乎不会更短）；同时 ≤32 位的数字/短哈希不受影响。
const MIN_SECRET_HEX_CHARS: usize = 32;

// ---------------------------------------------------------------------------
// 键名判据
// ---------------------------------------------------------------------------

/// 这个**键名**是不是在装敏感值。
///
/// 判据是"切词后精确命中 [`SECRET_WORDS`] 里的词"，不是 `contains` 子串：
///
/// - 命中：`token`、`api_key`、`OPENAI_API_KEY`、`access-token`、`clientSecret`、
///   `Authorization`、`X-Auth-Token`、`auth_token`、`password`、`credential(s)`、
///   `cookie(s)`、`secretKey`、`private-key`；
/// - 不命中：`tokenizer`、`monkey`、`keyboard`、`keywords`（都是**一个词**，
///   不会与 `token`/`key` 相等），以及 `max_tokens` / `prompt_tokens` /
///   `token_count` / `max_keys`（凭据词被计量词修饰，见 [`COUNT_WORDS`]）。
///
/// 空键名、纯符号键名一律不命中。
pub fn is_secret_key(key: &str) -> bool {
    let words = words_of(key);
    if words.is_empty() {
        return false;
    }
    let quantified = words
        .iter()
        .any(|word| COUNT_WORDS.contains(&word.as_str()));
    words.iter().any(|word| {
        if !SECRET_WORDS.contains(&word.as_str()) {
            return false;
        }
        // `max_tokens: 4096` 与 `max_keys: 8`：这里的 token/key 是计量单位。
        let is_unit = matches!(word.as_str(), "token" | "tokens" | "key" | "keys");
        !(quantified && is_unit)
    })
}

/// 键名切成小写词：按非字母数字分隔，并按驼峰再切一次。
///
/// `OPENAI_API_KEY` → `[openai, api, key]`；`clientSecret` → `[client, secret]`；
/// `APIKey` → `[api, key]`（大写串尾巴接小写字母处切）。非 ASCII 字母按原样参与
/// 切分（中文键名整段是一个词，不会命中敏感词表）。
fn words_of(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            push_word(&mut words, &mut current);
            continue;
        }
        let prev = index.checked_sub(1).map(|before| chars[before]);
        let next = chars.get(index + 1).copied();
        if is_camel_boundary(prev, ch, next) {
            push_word(&mut words, &mut current);
        }
        current.extend(ch.to_lowercase());
    }
    push_word(&mut words, &mut current);
    words
}

/// 驼峰边界：`client|Secret`（小写转大写）、`API|Key`（大写串转"大写+小写"）。
fn is_camel_boundary(prev: Option<char>, current: char, next: Option<char>) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    if !current.is_uppercase() {
        return false;
    }
    if prev.is_lowercase() || prev.is_numeric() {
        return true;
    }
    match next {
        Some(next) => prev.is_uppercase() && next.is_lowercase(),
        None => false,
    }
}

fn push_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

// ---------------------------------------------------------------------------
// 文本脱敏
// ---------------------------------------------------------------------------

/// `Debug` 用的保守脱敏：只抹"键名指向的值"与 `Bearer <token>`。
///
/// 保留非敏感文本原样（这一点是硬要求：`Debug` 失去可读性等于失去调试能力）。
///
/// ```
/// # use hbot_core::redact::{redact_arg, REDACTED};
/// assert_eq!(redact_arg("--token=sk-live-1234"), format!("--token={REDACTED}"));
/// assert_eq!(redact_arg("Authorization: Bearer sk-live-1234"), format!("Authorization: {REDACTED}"));
/// assert_eq!(redact_arg("max_tokens=4096"), "max_tokens=4096");
/// assert_eq!(redact_arg("tokenizer=cl100k"), "tokenizer=cl100k");
/// ```
pub fn redact_arg(text: &str) -> String {
    redact(text, false)
}

/// 日志正文用的脱敏：[`redact_arg`] 之外再抹长十六进制串。
///
/// 这是与 `hbot-rpc` 的 `app::agent::log::redact_text` 对应的行为（那份实现应该
/// 合并到本函数，见模块文档）。日志行没有字段名可依，所以额外兜"裸 hex 令牌"；
/// 代价是 32 位以上的十六进制哈希（如完整的 git object id）也会被抹掉 ——
/// 日志可以接受，`Debug` 不接受，所以两者分成两个入口。
pub fn redact_text(text: &str) -> String {
    redact(text, true)
}

fn redact(text: &str, hex_runs: bool) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    collect_bearer_ranges(text, &mut ranges);
    collect_named_value_ranges(text, &mut ranges);
    if hex_runs {
        collect_hex_ranges(text, &mut ranges);
    }
    if ranges.is_empty() {
        return text.to_owned();
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in merged {
        out.push_str(&text[cursor..start]);
        out.push_str(REDACTED);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// `Bearer <token>`：抹 token，保留方案名（`Authorization: Bearer <redacted>` 更好读）。
fn collect_bearer_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    // 小写化只用于定位：`to_ascii_lowercase` 不改变字节偏移，非 ASCII 部分原样保留。
    let lowered = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(relative) = lowered[from..].find("bearer") {
        let at = from + relative;
        let after = at + "bearer".len();
        from = after;
        // 词边界：前面不能是字母数字（`Bearer` 必须是独立词）。
        let boundary = match text[..at].chars().next_back() {
            Some(prev) => !prev.is_alphanumeric(),
            None => true,
        };
        if !boundary {
            continue;
        }
        let token_start = skip_blanks(text, after);
        if token_start == after {
            continue; // `Bearer` 后面必须有空白
        }
        let Some((start, end)) = plain_value_span(text, token_start) else {
            continue;
        };
        if end - start >= MIN_BEARER_TOKEN_CHARS {
            ranges.push((start, end));
            from = end.max(after);
        }
    }
}

/// `key=value` / `"key": value` / `--key value` 形式的敏感值。
fn collect_named_value_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    for (at, ch) in text.char_indices() {
        if ch == '=' || ch == ':' {
            collect_delimited_value(text, at, ch, ranges);
        } else if ch.is_whitespace() {
            collect_flag_value(text, at, ranges);
        }
    }
}

/// `key=value` / `key: value`。
fn collect_delimited_value(
    text: &str,
    at: usize,
    delimiter: char,
    ranges: &mut Vec<(usize, usize)>,
) {
    let Some(key_start) = key_start_before(text, at) else {
        return;
    };
    let key = trim_key(&text[key_start..at]);
    if key.is_empty() || !is_secret_key(key) {
        return;
    }
    let value_start = skip_blanks(text, at + delimiter.len_utf8());
    let Some((start, end)) = value_span(text, value_start) else {
        return;
    };
    if text[start..end].eq_ignore_ascii_case("bearer") {
        // 方案名本身不是秘密：连它后面的 token 一起抹，免得短 token 漏网。
        let next_start = skip_blanks(text, end);
        if let Some((_, next_end)) = value_span(text, next_start) {
            ranges.push((start, next_end));
        }
        return;
    }
    ranges.push((start, end));
}

/// `--api-key sk-live-1234`（开关与值之间是空白）。
///
/// 只在键名**以连字符开头**时生效：散文里的 `the token is …` 不会被误抹。
fn collect_flag_value(text: &str, at: usize, ranges: &mut Vec<(usize, usize)>) {
    let Some(key_start) = key_start_before(text, at) else {
        return;
    };
    let raw_key = &text[key_start..at];
    if !raw_key.trim_start().starts_with('-') {
        return;
    }
    let key = trim_key(raw_key);
    if key.is_empty() || !is_secret_key(key) {
        return;
    }
    let value_start = skip_blanks(text, at);
    if value_start == at {
        return;
    }
    if let Some((start, end)) = value_span(text, value_start) {
        ranges.push((start, end));
    }
}

/// 裸十六进制串（≥ [`MIN_SECRET_HEX_CHARS`]）。
fn collect_hex_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_hexdigit() {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            if index - begin >= MIN_SECRET_HEX_CHARS {
                ranges.push((begin, index));
            }
        }
    }
    if let Some(begin) = start {
        if text.len() - begin >= MIN_SECRET_HEX_CHARS {
            ranges.push((begin, text.len()));
        }
    }
}

/// 分隔符往回走，取键名起点。
fn key_start_before(text: &str, at: usize) -> Option<usize> {
    let mut start = at;
    for (index, ch) in text[..at].char_indices().rev() {
        if is_key_char(ch) {
            start = index;
        } else {
            break;
        }
    }
    (start < at).then_some(start)
}

/// 键名可含的字符：字母数字与 `_ - . " '`（覆盖 `--api-key`、`"token"`）。
fn is_key_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '"' | '\'')
}

/// 去掉键名两侧引号与前置连字符：`"--token"` → `token`。
fn trim_key(key: &str) -> &str {
    let key = key.trim();
    let key = key.trim_matches(|ch| ch == '"' || ch == '\'');
    let key = key.trim_start_matches('-');
    key.trim_matches(|ch| ch == '"' || ch == '\'')
}

/// 值区间：带引号取到配对引号（保留引号本身），否则取到分隔/空白。
fn value_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let first = text[from..].chars().next()?;
    if first == '"' || first == '\'' {
        let inner_start = from + first.len_utf8();
        let end = inner_start + text[inner_start..].find(first)?;
        return (end > inner_start).then_some((inner_start, end));
    }
    plain_value_span(text, from)
}

/// 不带引号的值：到空白、`,` `;` `)` `]` `}` 或引号为止。
fn plain_value_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let mut end = from;
    for (index, ch) in text[from..].char_indices() {
        if ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '}' | '&' | '"' | '\'') {
            break;
        }
        end = from + index + ch.len_utf8();
    }
    (end > from).then_some((from, end))
}

fn skip_blanks(text: &str, from: usize) -> usize {
    let mut cursor = from;
    for (index, ch) in text[from..].char_indices() {
        if !ch.is_whitespace() {
            return from + index;
        }
        cursor = from + index + ch.len_utf8();
    }
    cursor
}

// ---------------------------------------------------------------------------
// JSON 脱敏
// ---------------------------------------------------------------------------

/// 递归脱敏 JSON：对象里命中敏感键名的**整个值**换成 [`REDACTED`]，
/// 数组逐项递归，其余字符串过 [`redact_arg`]，数字/布尔/null 原样保留。
///
/// 递归是 RT-06 的硬要求：入站 payload 是任意 JSON，凭据可能藏在
/// `{"headers":{"Authorization":"…"}}` 这种嵌套层里。**不**把整个 payload
/// 打成 `<redacted>`：那等于把日志阉割掉（红队报告也点名了这一点）。
///
/// ```
/// # use hbot_core::redact::{redact_value, REDACTED};
/// # use serde_json::json;
/// let payload = json!({"max_tokens": 4096, "nested": {"Authorization": "Bearer sk-x"}});
/// let safe = redact_value(&payload);
/// assert_eq!(safe["max_tokens"], json!(4096));
/// assert_eq!(safe["nested"]["Authorization"], json!(REDACTED));
/// ```
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => redact_map(map),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::String(text) => Value::String(redact_arg(text)),
        other => other.clone(),
    }
}

fn redact_map(map: &Map<String, Value>) -> Value {
    let mut out = Map::with_capacity(map.len());
    for (key, value) in map {
        let safe = if is_secret_key(key) {
            Value::String(REDACTED.to_owned())
        } else {
            redact_value(value)
        };
        out.insert(key.clone(), safe);
    }
    Value::Object(out)
}

/// 命令行的 argv 脱敏，处理"值与开关分开"的形态。
///
/// `["--api-key", "sk-live-1234"]` 里的值单看是裸串、没有键名可依，所以这里看
/// **上一个参数**：它本身是敏感开关（`--token` / `--api-key` / `--token=`）时，
/// 下一个参数整个抹掉。`--token=sk-…` 这种带等号的形态由 [`redact_arg`] 处理。
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            out.push(REDACTED.to_owned());
            mask_next = false;
            continue;
        }
        if is_bare_secret_flag(arg) {
            mask_next = true;
            out.push(redact_arg(arg));
            continue;
        }
        out.push(redact_arg(arg));
    }
    out
}

/// `--api-key` / `-token` / `--token=` 这种"只有开关、没有值"的参数。
fn is_bare_secret_flag(arg: &str) -> bool {
    let trimmed = arg.trim();
    if !trimmed.starts_with('-') {
        return false;
    }
    let (name, value) = match trimmed.split_once('=') {
        Some((name, value)) => (name, value),
        None => (trimmed, ""),
    };
    let name = trim_key(name);
    // 带值的形态（`--token=sk-…`）交给 `redact_arg`，这里只认"值在下一个参数"。
    value.is_empty() && !name.is_empty() && is_secret_key(name)
}

// ---------------------------------------------------------------------------
// Debug 辅助
// ---------------------------------------------------------------------------

/// `Debug` 辅助：命令参数的脱敏打印（形状与 `Vec<String>` 一致）。
///
/// ```
/// # use hbot_core::redact::{RedactedArgs, REDACTED};
/// let args = vec!["--token=sk-live-1234".to_owned(), "status".to_owned()];
/// let printed = format!("{:?}", RedactedArgs(&args));
/// assert_eq!(printed, format!("[\"--token={REDACTED}\", \"status\"]"));
/// ```
#[derive(Clone, Copy)]
pub struct RedactedArgs<'a>(pub &'a [String]);

impl fmt::Debug for RedactedArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(redact_args(self.0)).finish()
    }
}

/// `Debug` 辅助：`Vec<(String, String)>` 环境变量的脱敏打印。
///
/// 变量**名**原样保留（排查"到底注入了什么"全靠它），值按键名判定：
/// 命中敏感键名 → `<redacted>`；否则过 [`redact_arg`]（防值里嵌 `Bearer …`）。
#[derive(Clone, Copy)]
pub struct RedactedEnv<'a>(pub &'a [(String, String)]);

impl fmt::Debug for RedactedEnv<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for (key, value) in self.0 {
            let safe = if is_secret_key(key) {
                REDACTED.to_owned()
            } else {
                redact_arg(value)
            };
            list.entry(&(key.as_str(), safe));
        }
        list.finish()
    }
}

/// `Debug` 辅助：任意 JSON 的脱敏打印（走 [`redact_value`] 的递归判据）。
///
/// ```
/// # use hbot_core::redact::RedactedJson;
/// # use serde_json::json;
/// let printed = format!("{:?}", RedactedJson(&json!({"token": "sk-live-1234"})));
/// assert!(!printed.contains("sk-live-1234"));
/// assert!(printed.contains("token")); // 字段名还在，调试价值不丢
/// ```
#[derive(Clone, Copy)]
pub struct RedactedJson<'a>(pub &'a Value);

impl fmt::Debug for RedactedJson<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&redact_value(self.0), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_key_matrix() {
        for key in [
            "token",
            "api_key",
            "apikey",
            "API_KEY",
            "OPENAI_API_KEY",
            "access-token",
            "clientSecret",
            "X-Auth-Token",
            "auth_token",
            "secret",
            "client_secret",
            "password",
            "passwd",
            "authorization",
            "Authorization",
            "credential",
            "credentials",
            "cookie",
            "Cookie",
            "secretKey",
            "private_key",
            "HBOT_LLM_TOKEN",
        ] {
            assert!(is_secret_key(key), "{key} 应该被判为敏感键名");
        }
    }

    #[test]
    fn ordinary_key_matrix() {
        for key in [
            "tokenizer",
            "monkey",
            "keyboard",
            "keywords",
            "max_tokens",
            "prompt_tokens",
            "completion_tokens",
            "token_count",
            "max_keys",
            "model",
            "name",
            "",
            "-",
        ] {
            assert!(!is_secret_key(key), "{key} 不该被误判为敏感键名");
        }
    }

    #[test]
    fn redact_arg_keeps_non_sensitive_text() {
        assert_eq!(redact_arg("status"), "status");
        assert_eq!(redact_arg("max_tokens=4096"), "max_tokens=4096");
        assert_eq!(redact_arg("tokenizer=cl100k"), "tokenizer=cl100k");
        assert_eq!(redact_arg("--verbose"), "--verbose");
        // 40 位 commit SHA 原样保留（Debug 要能读）。
        let sha = "95e6cf0a1b2c3d4e5f60718293a4b5c6d7e8f901";
        assert_eq!(redact_arg(sha), sha);
    }

    #[test]
    fn redact_arg_masks_named_values() {
        assert_eq!(
            redact_arg("--token=sk-live-1234"),
            format!("--token={REDACTED}")
        );
        assert_eq!(
            redact_arg("api_key: sk-live-1234"),
            format!("api_key: {REDACTED}")
        );
        assert_eq!(
            redact_arg(r#""Authorization":"Bearer sk-live-1234""#),
            format!(r#""Authorization":"{REDACTED}""#)
        );
        // 值里带空格时只抹到第一个分隔词（`abc` 留在后面是文本脱敏的已知边界；
        // JSON 形态走 `redact_value`，整个值都会被抹掉）。
        assert!(!redact_arg("Authorization: QRBT abc").contains("QRBT"));
        // 开关与值分开的形态（`redact_args` 之外，文本里也认）。
        assert_eq!(
            redact_arg("--token sk-live-1234"),
            format!("--token {REDACTED}")
        );
    }

    #[test]
    fn redact_text_masks_bare_hex() {
        let token = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let text = format!("got {token} from env");
        assert!(!redact_text(&text).contains(token));
        // 短数字/短哈希不动。
        assert_eq!(redact_text("retry 4096 times"), "retry 4096 times");
    }

    #[test]
    fn redact_value_recurses() {
        let payload = json!({
            "text": "hi",
            "max_tokens": 4096,
            "nested": {"headers": {"Authorization": "Bearer sk-x"}, "list": [{"token": "t0k3n"}]},
        });
        let safe = redact_value(&payload);
        assert_eq!(safe["text"], json!("hi"));
        assert_eq!(safe["max_tokens"], json!(4096));
        assert_eq!(safe["nested"]["headers"]["Authorization"], json!(REDACTED));
        assert_eq!(safe["nested"]["list"][0]["token"], json!(REDACTED));
    }

    #[test]
    fn split_flag_value_is_masked() {
        let args = vec![
            "--api-key".to_owned(),
            "sk-live-1234".to_owned(),
            "status".to_owned(),
        ];
        assert_eq!(
            redact_args(&args),
            vec![
                "--api-key".to_owned(),
                REDACTED.to_owned(),
                "status".to_owned()
            ]
        );
    }
}
