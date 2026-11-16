//! `platform = "local"` 的原语契约（从**外部**验证：这些符号对其它 crate 是公开的）。
//!
//! 为什么单独在 `tests/` 里再测一遍内部已经测过的格式化：`local` 的回复格式是
//! **跨 crate 的契约** —— `hbot-rpc` 的 `LocalAdapter::send` 调它、IDE 的日志面板按
//! `[reply] ` 前缀解析它。内部的 `#[cfg(test)] mod tests` 只能证明"这个函数自己算得对"，
//! 证明不了"这个符号真的导出了、外部拿到的是同一个实现"。
//!
//! 一并钉住两件容易在重构里丢的事：
//!
//! 1. [`PLATFORM`] 必须还是 `"local"`（`bots.json` 里写的就是这个字面量，改了就断配置）；
//! 2. [`DEFAULT_CONVERSATION_EXTERNAL_ID`] 必须还是 `"local:default"`
//!    （`bot_inject` 的 conversation 行按这个值查，UI/脚本也按它找）。

use hbot_adapters::platforms::local::{
    DEFAULT_CONVERSATION_EXTERNAL_ID, PLATFORM, REPLY_PREFIX, reply_lines,
};

/// `bots.json` 里 `"platform": "local"` 必须精确匹配这个值。
#[test]
fn the_platform_id_is_the_literal_used_in_bots_json() {
    assert_eq!(PLATFORM, "local");
}

/// 试跑用的会话外部 ID 是固定可读值（不是每次启动换一个 UUID）。
#[test]
fn the_default_conversation_id_is_readable_and_stable() {
    assert_eq!(DEFAULT_CONVERSATION_EXTERNAL_ID, "local:default");
    // 含冒号的前缀形态：一眼能看出不是平台推来的真实会话 ID（真实群号/QQ 号是纯数字）。
    assert!(DEFAULT_CONVERSATION_EXTERNAL_ID.contains(':'));
}

/// 回复前缀是日志面板解析契约的一部分。
#[test]
fn the_reply_prefix_is_exactly_bracket_reply_space() {
    assert_eq!(REPLY_PREFIX, "[reply] ");
}

/// 单行回复：一行，带前缀，文本原样（不 trim、不改写）。
#[test]
fn a_single_line_reply_becomes_one_prefixed_line() {
    assert_eq!(reply_lines("收到：你好"), vec!["[reply] 收到：你好"]);
    assert_eq!(
        reply_lines("  前后有空格  "),
        vec!["[reply]   前后有空格  "]
    );
}

/// 多行回复：**每行都带前缀**（第二行不能裸露，否则日志面板看起来像别的来源）。
#[test]
fn every_line_of_a_multi_line_reply_is_prefixed() {
    assert_eq!(
        reply_lines("第一行\n第二行\n第三行"),
        vec!["[reply] 第一行", "[reply] 第二行", "[reply] 第三行"]
    );
}

/// CRLF 只算一个换行，且不会把 `\r` 留在行尾（Windows 上的文本常见）。
#[test]
fn crlf_does_not_leak_a_carriage_return() {
    assert_eq!(reply_lines("a\r\nb"), vec!["[reply] a", "[reply] b"]);
}

/// 空回复不产出任何行：调用方据此决定"不写日志"（而不是写一行空的 `[reply] `）。
#[test]
fn empty_text_produces_no_lines_at_all() {
    assert!(reply_lines("").is_empty());
}

/// 中间的空行保留（多段回复的空行是排版，不是噪声）。
#[test]
fn blank_lines_inside_a_reply_are_kept() {
    assert_eq!(
        reply_lines("a\n\nb"),
        vec!["[reply] a", "[reply] ", "[reply] b"]
    );
}

/// 前缀必须落在**行首**：UI 只看行首来分类，行中出现不算。
#[test]
fn the_prefix_only_ever_appears_at_the_start_of_a_line() {
    for text in ["a", "a\nb", "带 [reply] 字样的正文"] {
        for line in reply_lines(text) {
            assert!(
                line.starts_with(REPLY_PREFIX),
                "每一行都必须以 `{REPLY_PREFIX}` 开头：{line:?}"
            );
        }
    }
}
