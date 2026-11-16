//! 本地试跑平台（`bots.json` 里 `platform = "local"`）：**不联网**。
//!
//! 这个平台存在的唯一理由是产品闭环：`onebot11` 要一个真的 OneBot 实现（NapCat /
//! LLOneBot / Lagrange），`qq_official` 要 AppID/AppSecret 且要连腾讯网关 —— 刚生成的
//! 代码想跑一下，必须先搞一个外部端点。`local` 把这一步拿掉：
//!
//! * **入站**：适配器 `subscribe()` 返回一个正常构造、但**永远不发送事件**的订阅。
//!   消息由 `bot_inject` 直接送进 supervisor 的队列（`Supervisor::inject`），不经过
//!   适配器 —— 这正是 `bot_inject` 已有的语义，`local` 只是让它有一个能 `bot_start`
//!   成功的平台。
//! * **出站**：`send()` 不连任何东西，把要发出去的文本按 [`REPLY_PREFIX`] 逐行写进
//!   run log（日志表 + `Event::RunLog`），于是 IDE 的日志面板能看见"bot 会回什么"。
//! * **会话**：`declared_conversations()` 返回空（没有真实事件，也没有可预置的会话）；
//!   试跑用的那一行 `local:default` 由 `bot_start` 幂等建好
//!   （[`DEFAULT_CONVERSATION_EXTERNAL_ID`]）。
//!
//! **不用于生产**：它没有传输、没有签名校验、没有平台侧的任何东西，回复也不会真的
//! 出现在 QQ 上。它只是"本地试跑"的那一档。
//!
//! # 为什么这里只有"原语"而没有 [`BotAdapter`] 实现
//!
//! `hbot-bot` 的 `BotAdapter` 在 `hbot-bot` crate 里，而本 crate 的依赖约束是
//! **只依赖 `hbot-core` + 通用库**（`crates/hbot-adapters/src/lib.rs` 的模块文档：
//! 业务编排在 `hbot-bot`，适配器只做协议翻译与传输）。`onebot11` 与 `qq_official`
//! 也是同一个分层：协议客户端在这里，`BotAdapter` 实现放在组合根
//! `crates/hbot-rpc/src/app/runtime/`（`onebot.rs` / `qq.rs`）。`local` 照抄这条线：
//! 本模块给出平台标识、试跑会话的外部 id、回复行格式这三样"纯数据"，
//! `BotAdapter` 实现见 `crates/hbot-rpc/src/app/runtime/local.rs`。
//!
//! 这样分层还有一个直接好处：`[reply]` 的行格式是本 crate 的**单一真源**，
//! 组合根与测试都调 [`reply_lines`]，不会一边 `[reply]` 一边 `[reply]:`。

/// `local` 平台的标识（`bots.json` 的 `platform` 列 / `bots.platform` 列都要精确匹配它）。
pub const PLATFORM: &str = "local";

/// 本地试跑用的会话外部 id（`conversations.external_id`）。
///
/// 固定值而不是随机值：`bot_start` 每次都要能**幂等**拿到同一行（`(bot_id,
/// external_id)` 是唯一键），重跑一次不该多出一堆会话。`local:` 前缀让人一眼看出
/// 这行不是平台来的（真实平台的会话 id 是 QQ 号 / `group_openid` 这类值）。
pub const DEFAULT_CONVERSATION_EXTERNAL_ID: &str = "local:default";

/// 回复行前缀：`send()` 把每条出站文本写成 `[reply] <文本>` 一行。
///
/// 带前缀而不是裸文本：日志面板里同时有 handler 自己的 `ctx.log` 输出、平台日志、
/// 错误行，裸文本混在一起看不出哪一行是"bot 发出去的"。`[reply] ` 与 handler 的
/// 日志词法上不冲突，测试也能直接断言前缀。
pub const REPLY_PREFIX: &str = "[reply] ";

/// 把一条出站文本拆成要落 run log 的行：**每行一条**，每行都带 [`REPLY_PREFIX`]。
///
/// 多行文本按行拆开（`\r\n` / `\n` 都算换行，`lines()` 顺带吃掉行尾的 `\r`）：
/// 日志表里一行就是一条记录，塞一个带换行的多行字符串进去会让日志面板的渲染与
/// `logs_tail` 的分页都变得可疑。空文本返回空 `Vec`（没有可显示的行，不伪造一条
/// 空回复）；只有换行的文本会得到若干条 `[reply] ` 行，长度信息不丢。
pub fn reply_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| format!("{REPLY_PREFIX}{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_gets_the_prefix() {
        assert_eq!(reply_lines("收到"), vec!["[reply] 收到".to_owned()]);
    }

    #[test]
    fn every_line_of_a_multi_line_reply_gets_its_own_prefix() {
        assert_eq!(
            reply_lines("第一行\n第二行"),
            vec!["[reply] 第一行".to_owned(), "[reply] 第二行".to_owned()]
        );
    }

    #[test]
    fn crlf_is_treated_as_one_line_break_and_the_cr_is_dropped() {
        assert_eq!(
            reply_lines("a\r\nb\r\n"),
            vec!["[reply] a".to_owned(), "[reply] b".to_owned()]
        );
    }

    #[test]
    fn empty_text_produces_no_lines() {
        assert!(reply_lines("").is_empty());
    }

    #[test]
    fn blank_lines_are_kept_so_the_reply_shape_is_visible() {
        assert_eq!(
            reply_lines("a\n\nb"),
            vec![
                "[reply] a".to_owned(),
                "[reply] ".to_owned(),
                "[reply] b".to_owned(),
            ]
        );
    }

    #[test]
    fn the_default_conversation_id_is_a_readable_fixed_value() {
        // 这个值是契约（`bot_start` 幂等建行 + 文档 + UI 都按它找会话），改它等于
        // 让已存在的 `local:default` 行变成孤儿。
        assert_eq!(DEFAULT_CONVERSATION_EXTERNAL_ID, "local:default");
        assert_eq!(PLATFORM, "local");
        assert_eq!(REPLY_PREFIX, "[reply] ");
    }
}
