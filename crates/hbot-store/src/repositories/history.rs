//! 会话历史：把 `inbound` 与 `outbound` 合成一条按时间升序的列表。
//!
//! 生成代码的 `ctx.history(n)` 问的是**这个会话最近说了什么**，而这两类消息分居两张表
//! （`crates/hbot-core/src/bot_state.rs` 的 `HistoryReader`）。存储层负责把两表拼成一张
//! 形状统一的表，并把 `inbound.payload_json` 里的 `"text"` 取出来 —— 之后
//! `hbot-rpc` 才把它翻译成 `HistoryEntry`。
//!
//! ## 排序：升序，且必须稳定
//!
//! 返回的是**最近的 `limit` 条、但最老的在前**（聊天记录的自然阅读顺序）。做法是先让两个
//! 子查询各自按时间**降序**取 `limit` 条（这样"最近"的窗口是对的），`UNION ALL` 之后再
//! 降序取 `limit` 条，最后在 Rust 里 `reverse()`。
//!
//! 同一毫秒有多条时（`received_at` / `created_at` 都是调用方给的时钟，撞车很常见）用
//! `message_id` 作**次级键**：不加它，同一条 SQL 的两次执行可以给出不同顺序，回放/影子
//! 对比会随机的对不上。次级键是字符串比较（UUID 字符串的字典序），方向与主键一致（DESC），
//! 所以 `reverse()` 之后整体是 `(at ASC, message_id ASC)`。
//!
//! ## 为什么在 Rust 里解析 `payload_json`
//!
//! `inbound.payload_json` 是适配器写进去的不透明 JSON（`hbot-adapters::normalize`），
//! `text` 在**顶层**。用 SQLite 的 `json_extract` 会引入一个新的运行时变量
//! （构建产物的 SQLite 有没有开 JSON1），而不值得：解析失败的行只影响它自己的 `text`。
//! 本 crate 没有 `serde_json` 直接依赖（`xtask/src/deps.rs` 只允许内部依赖 hbot-core），
//! 所以这里用一个**只认 JSON、只取顶层 `text` 字符串**的小扫描器：非法 JSON / 缺 `text` /
//! `text` 不是字符串一律返回空串（与端口文档一致：宁可少一段文本，不能让整条历史读失败）。
//!
//! 注意它**不是**通用 JSON 库：不建 `Value`、不保留其它字段、不做数字归一。新增用途请走
//! `hbot-core` 的 `serde_json`，不要把这个扫描器扩成第二个 JSON 实现。

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::ids::{BotId, ConversationId};
use crate::models::HistoryRow;

/// `HistoryRow.direction`：来自 `inbound` 表的一条。
///
/// 与 `hbot_core::HistoryDirection::Inbound.as_str()` 必须同值（上层要按它分支）。
/// 存储层不 import 领域枚举，所以这条别名由 `direction_literals_match_the_core_contract`
/// 钉住 —— 改单边就红。
pub(crate) const DIRECTION_INBOUND: &str = "inbound";
/// `HistoryRow.direction`：来自 `outbound` 表的一条。
pub(crate) const DIRECTION_OUTBOUND: &str = "outbound";

/// 取某个 bot 某个会话最近 `limit` 条消息，按时间**升序**返回（最老的在前）。
///
/// `limit == 0` 直接返回空表（不进 SQL：`LIMIT 0` 也是空表，但早退省一次往返）。
///
/// 过滤同时带 `bot_id` 与 `conversation_id`：只按会话过滤会让"别的 bot 恰好复用了同一个
/// conversation id"这种脏数据串进来；只按 bot 过滤则会串会话。两列都是表上的现有列，
/// 不需要新索引。
///
/// 走只读连接池即可（`Store::list_history`）：历史是**只追加**的账本，调用方读的是"已经
/// 发生过的对话"，不需要看见自己刚写的那一行 —— 与续发类读（必须与写同序）相反。
pub(crate) fn history(
    conn: &Connection,
    bot_id: &BotId,
    conversation_id: &ConversationId,
    limit: u32,
) -> Result<Vec<HistoryRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    // 两个子查询各取"最近 limit 条"，UNION ALL 后再取一次 limit（取两表合并后的最近窗口）。
    // 子查询里的 ORDER BY 依赖 LIMIT 才有确定语义，SQLite 允许在带 LIMIT 的子查询里排序。
    // 列顺序固定：direction, message_id, external_id, sender_id, text, at。
    let sql = format!(
        "SELECT direction, message_id, external_id, sender_id, text, at FROM (
             SELECT '{DIRECTION_INBOUND}' AS direction,
                    id                      AS message_id,
                    external_id             AS external_id,
                    sender_id               AS sender_id,
                    payload_json            AS text,
                    received_at             AS at
             FROM inbound
             WHERE bot_id = ?1 AND conversation_id = ?2
             ORDER BY received_at DESC, id DESC
             LIMIT ?3
         )
         UNION ALL
         SELECT direction, message_id, external_id, sender_id, text, at FROM (
             SELECT '{DIRECTION_OUTBOUND}' AS direction,
                    id                       AS message_id,
                    COALESCE(reply_to, '')   AS external_id,
                    ''                       AS sender_id,
                    text                     AS text,
                    created_at               AS at
             FROM outbound
             WHERE bot_id = ?1 AND conversation_id = ?2
             ORDER BY created_at DESC, id DESC
             LIMIT ?3
         )
         ORDER BY at DESC, message_id DESC
         LIMIT ?3"
    );

    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            bot_id.to_string(),
            conversation_id.to_string(),
            i64::from(limit)
        ],
        |row| {
            let direction: String = row.get(0)?;
            let message_id: String = row.get(1)?;
            let external_id: String = row.get(2)?;
            let sender_id: String = row.get(3)?;
            let text: String = row.get(4)?;
            let at: i64 = row.get(5)?;
            // 只有 inbound 行需要从 payload_json 里取文本；outbound 行的 `text` 就是列值。
            let text = if direction == DIRECTION_INBOUND {
                text_from_payload(&text)
            } else {
                text
            };
            Ok(HistoryRow {
                direction,
                message_id,
                external_id,
                sender_id,
                text,
                at,
            })
        },
    )?;

    let mut collected = Vec::new();
    for row in rows {
        collected.push(row?);
    }
    // SQL 给的是"最近 limit 条、最新在前"；对外契约是"最近的 limit 条、最老在前"。
    collected.reverse();
    Ok(collected)
}

/// 从 `inbound.payload_json` 里取顶层 `"text"` 的字符串值；取不到一律空串。
///
/// 三种失败都返回 `""`（不是错误）：整段不是合法 JSON、没有 `text` 键、`text` 不是字符串。
/// 理由：payload 是**适配器写的不透明 JSON**，一行坏数据不该让整个历史读失败——那样
/// 生成代码的 `ctx.history()` 会因为一条脏消息而整体不可用。
fn text_from_payload(payload_json: &str) -> String {
    JsonTextScanner::new(payload_json)
        .top_level_text()
        .unwrap_or_default()
}

/// 一个只够用的 JSON 扫描器：校验整段文档合法性，并取出顶层对象的 `"text"` 字符串。
///
/// 为什么不是 `serde_json`：本 crate 没有它的直接依赖（`xtask/src/deps.rs` 的依赖白名单
/// 只允许内部 crate），而为了一个字段加一条依赖不值得。为什么不写正则：`text` 的值里可以
/// 出现 `"text"` 转义形态与嵌套对象，正则必然出错。
struct JsonTextScanner<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> JsonTextScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            position: 0,
        }
    }

    /// 解析整段文档；返回顶层 `text` 的字符串值（没有就是 `None`），非法的 JSON 也返回 `None`。
    ///
    /// 顶层必须是对象：历史里 payload 的形状由 `hbot-adapters::normalize::payload_json`
    /// 决定，是 `{"text": …}`。顶层是数组/标量时按"没有 text"处理。
    fn top_level_text(mut self) -> Option<String> {
        self.skip_whitespace();
        if self.peek()? != b'{' {
            return None;
        }
        let found = self.parse_object(true)?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            // 尾随垃圾（`{"text":"a"} x`）不是合法 JSON。
            return None;
        }
        Some(found.unwrap_or_default())
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Option<()> {
        if self.bump()? == expected {
            Some(())
        } else {
            None
        }
    }

    /// 解析一个对象。`top` 为真时收集 `"text"`：后出现的同名键覆盖前面的（与 `serde_json` 一致）。
    fn parse_object(&mut self, top: bool) -> Option<Option<String>> {
        self.expect_byte(b'{')?;
        let mut found: Option<String> = None;
        self.skip_whitespace();
        if self.peek()? == b'}' {
            self.position += 1;
            return Some(found);
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect_byte(b':')?;
            self.skip_whitespace();
            if top && key == "text" && self.peek()? == b'"' {
                found = Some(self.parse_string()?);
            } else {
                // 非 text 键、或 text 但不是字符串：照常校验并跳过（非字符串 = 没有文本）。
                self.parse_value()?;
            }
            self.skip_whitespace();
            match self.peek()? {
                b',' => {
                    self.position += 1;
                }
                b'}' => {
                    self.position += 1;
                    return Some(found);
                }
                _ => return None,
            }
        }
    }

    fn parse_array(&mut self) -> Option<()> {
        self.expect_byte(b'[')?;
        self.skip_whitespace();
        if self.peek()? == b']' {
            self.position += 1;
            return Some(());
        }
        loop {
            self.skip_whitespace();
            self.parse_value()?;
            self.skip_whitespace();
            match self.peek()? {
                b',' => {
                    self.position += 1;
                }
                b']' => {
                    self.position += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    fn parse_value(&mut self) -> Option<()> {
        self.skip_whitespace();
        match self.peek()? {
            b'{' => self.parse_object(false).map(|_| ()),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(|_| ()),
            b't' => self.expect_literal(b"true"),
            b'f' => self.expect_literal(b"false"),
            b'n' => self.expect_literal(b"null"),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Option<()> {
        let end = self.position.checked_add(literal.len())?;
        if self.bytes.get(self.position..end)? == literal {
            self.position = end;
            Some(())
        } else {
            None
        }
    }

    /// 数字：`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`（RFC 8259 的完整形状）。
    fn parse_number(&mut self) -> Option<()> {
        if self.peek()? == b'-' {
            self.position += 1;
        }
        match self.peek()? {
            b'0' => self.position += 1,
            b'1'..=b'9' => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }
        Some(())
    }

    /// 解析字符串（含转义），返回解码后的值。
    ///
    /// 控制字符必须转义（RFC 8259）；`\uXXXX` 的代理对按 UTF-16 规则合并，孤立的代理项
    /// 视为非法（与 `serde_json` 一致）。
    fn parse_string(&mut self) -> Option<String> {
        self.expect_byte(b'"')?;
        let mut decoded = String::new();
        loop {
            match self.bump()? {
                b'"' => return Some(decoded),
                b'\\' => match self.bump()? {
                    b'"' => decoded.push('"'),
                    b'\\' => decoded.push('\\'),
                    b'/' => decoded.push('/'),
                    b'b' => decoded.push('\u{0008}'),
                    b'f' => decoded.push('\u{000c}'),
                    b'n' => decoded.push('\n'),
                    b'r' => decoded.push('\r'),
                    b't' => decoded.push('\t'),
                    b'u' => decoded.push(self.parse_unicode_escape()?),
                    _ => return None,
                },
                byte if byte < 0x20 => return None,
                byte if byte < 0x80 => decoded.push(char::from(byte)),
                byte => {
                    // 多字节 UTF-8：`position` 已在首字节之后，补全整段序列并原样追加。
                    let start = self.position - 1;
                    let end = start.checked_add(utf8_width(byte))?;
                    let slice = self.bytes.get(start..end)?;
                    decoded.push_str(std::str::from_utf8(slice).ok()?);
                    self.position = end;
                }
            }
        }
    }

    /// `\uXXXX`（`position` 已在 `u` 之后），返回解码后的字符。
    fn parse_unicode_escape(&mut self) -> Option<char> {
        let first = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&first) {
            // 高代理项后面必须紧跟 `\uDC00..DFFF`。
            if self.bump()? != b'\\' || self.bump()? != b'u' {
                return None;
            }
            let second = self.parse_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&second) {
                return None;
            }
            let combined =
                0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00);
            char::from_u32(combined)
        } else if (0xDC00..=0xDFFF).contains(&first) {
            // 孤立低代理项。
            None
        } else {
            char::from_u32(u32::from(first))
        }
    }

    /// 读 4 个十六进制数字。
    fn parse_hex4(&mut self) -> Option<u16> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let digit = char::from(self.bump()?).to_digit(16)?;
            value = value
                .checked_mul(16)?
                .checked_add(u16::try_from(digit).ok()?)?;
        }
        Some(value)
    }
}

/// UTF-8 首字节对应的序列长度（只对 ≥ 0x80 的合法首字节有意义）。
fn utf8_width(first: u8) -> usize {
    match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::ids::ConversationId;
    use crate::models::{Conversation, NewInbound, NewOutbound};
    use crate::test_support::{
        TempDir, bot_id, bot_id_at, conversation_id, fixed_uuid, inbound_id, new_inbound_at,
        outbound_id, project_id, raw,
    };
    use hbot_core::bot_state::HistoryDirection;

    /// 另起一个会话（用来验证"别的会话不串味"）。
    fn other_conversation(store: &Store) -> ConversationId {
        let id = ConversationId::from_uuid(fixed_uuid(444));
        store
            .insert_conversation(&Conversation {
                id,
                bot_id: bot_id(),
                external_id: "private-7".to_owned(),
                kind: "private".to_owned(),
                created_at: 4,
            })
            .expect("insert other conversation");
        id
    }

    /// 造一条入站消息，`payload_json` 与时间由调用方给。
    fn inbound_with_payload(
        id: u128,
        external: &str,
        payload: &str,
        received_at: i64,
    ) -> NewInbound {
        let mut message = new_inbound_at(id, external, received_at);
        message.payload_json = payload.to_owned();
        message
    }

    /// 造一条出站消息（`text` 与 `created_at` 由调用方给）。
    fn outbound_with_text(id: u128, text: &str, created_at: i64) -> NewOutbound {
        NewOutbound {
            id: outbound_id(id),
            bot_id: bot_id(),
            conversation_id: conversation_id(),
            text: text.to_owned(),
            created_at,
        }
    }

    /// 方向字面量必须与 `hbot_core::HistoryDirection::as_str()` 同值。
    ///
    /// 上层按 `direction` 分支；两边漂移会静默把出站行当成入站行。
    #[test]
    fn direction_literals_match_the_core_contract() {
        assert_eq!(DIRECTION_INBOUND, HistoryDirection::Inbound.as_str());
        assert_eq!(DIRECTION_OUTBOUND, HistoryDirection::Outbound.as_str());
    }

    /// 混合 inbound + outbound：按时间升序返回（最老在前），方向与各列都对得上。
    #[test]
    fn inbound_and_outbound_are_merged_in_ascending_time_order() {
        let dir = TempDir::new("history-merge");
        let store = open_store_with_seed(&dir);
        store
            .insert_inbound_with_sender(
                inbound_with_payload(11, "ext-11", "{\"text\":\"hi\"}", 1_000),
                "user-1".to_owned(),
            )
            .expect("insert inbound 11");
        store
            .enqueue_outbound_with_target(
                outbound_with_text(21, "hello back", 1_100),
                crate::models::OutboundTarget::new("{}", Some("ext-11".to_owned())),
            )
            .expect("enqueue outbound 21");
        store
            .insert_inbound_with_sender(
                inbound_with_payload(12, "ext-12", "{\"text\":\"again\"}", 1_200),
                "user-2".to_owned(),
            )
            .expect("insert inbound 12");

        let rows = store
            .list_history(&bot_id(), &conversation_id(), 10)
            .expect("list history");

        assert_eq!(rows.len(), 3, "两表必须都参与合并");
        assert_eq!(
            rows.iter().map(|row| row.at).collect::<Vec<_>>(),
            vec![1_000, 1_100, 1_200],
            "必须按时间升序（最老在前）"
        );

        assert_eq!(rows[0].direction, DIRECTION_INBOUND);
        assert_eq!(rows[0].message_id, inbound_id(11).to_string());
        assert_eq!(rows[0].external_id, "ext-11");
        assert_eq!(rows[0].sender_id, "user-1");
        assert_eq!(rows[0].text, "hi", "inbound 的 text 从 payload_json 取");

        assert_eq!(
            rows[1].direction, DIRECTION_OUTBOUND,
            "outbound 必须在结果里"
        );
        assert_eq!(rows[1].message_id, outbound_id(21).to_string());
        assert_eq!(
            rows[1].external_id, "ext-11",
            "outbound 的 external_id 是 reply_to"
        );
        assert_eq!(rows[1].sender_id, "", "outbound 没有发送者");
        assert_eq!(rows[1].text, "hello back");

        assert_eq!(rows[2].direction, DIRECTION_INBOUND);
        assert_eq!(rows[2].sender_id, "user-2");
        assert_eq!(rows[2].text, "again");
    }

    /// `limit` 截断时取**最近**的那几条，且返回仍是最老在前。
    #[test]
    fn limit_keeps_the_newest_rows_but_in_ascending_order() {
        let dir = TempDir::new("history-limit");
        let store = open_store_with_seed(&dir);
        for (index, at) in [100_i64, 200, 300, 400, 500].iter().enumerate() {
            let id = u128::try_from(index).expect("index fits") + 31;
            store
                .insert_inbound_with_sender(
                    inbound_with_payload(
                        id,
                        &format!("ext-{index}"),
                        &format!("{{\"text\":\"m{index}\"}}"),
                        *at,
                    ),
                    String::new(),
                )
                .expect("insert inbound");
        }

        let rows = store
            .list_history(&bot_id(), &conversation_id(), 3)
            .expect("list history limited");
        assert_eq!(rows.len(), 3, "limit 是条数上限");
        assert_eq!(
            rows.iter().map(|row| row.at).collect::<Vec<_>>(),
            vec![300, 400, 500],
            "必须是最近的 3 条，且升序"
        );
    }

    /// `limit == 0` 是空表（不是"不限"）。
    #[test]
    fn zero_limit_is_an_empty_list() {
        let dir = TempDir::new("history-zero");
        let store = open_store_with_seed(&dir);
        store
            .insert_inbound_if_new(new_inbound_at(41, "ext-41", 1_000))
            .expect("insert inbound");
        assert!(
            store
                .list_history(&bot_id(), &conversation_id(), 0)
                .expect("zero limit")
                .is_empty(),
            "limit = 0 必须返回空表"
        );
    }

    /// 别的会话 / 别的 bot 的行不许串进来。
    #[test]
    fn rows_from_other_conversations_and_bots_are_excluded() {
        let dir = TempDir::new("history-scope");
        let store = open_store_with_seed(&dir);
        let other_conversation_id = other_conversation(&store);

        // 目标会话的一条。
        store
            .insert_inbound_if_new(inbound_with_payload(
                51,
                "ext-51",
                "{\"text\":\"mine\"}",
                1_000,
            ))
            .expect("insert target inbound");
        // 另一个会话的一条。
        let mut foreign = inbound_with_payload(52, "ext-52", "{\"text\":\"other\"}", 1_100);
        foreign.conversation_id = other_conversation_id;
        store
            .insert_inbound_if_new(foreign)
            .expect("insert foreign inbound");

        // 另一个 bot 在同一个会话 id 上的行（会话行必须存在，外键才放行）。
        let other_bot = bot_id_at(998);
        store
            .insert_bot(&crate::models::Bot {
                id: other_bot,
                project_id: project_id(),
                name: "other-bot".to_owned(),
                platform: "onebot".to_owned(),
                config_json: "{}".to_owned(),
                enabled: true,
                created_at: 5,
            })
            .expect("insert other bot");
        store
            .insert_conversation(&Conversation {
                id: ConversationId::from_uuid(fixed_uuid(445)),
                bot_id: other_bot,
                external_id: "group-42".to_owned(),
                kind: "group".to_owned(),
                created_at: 6,
            })
            .expect("insert other bot conversation");
        let mut foreign_bot_row = inbound_with_payload(53, "ext-53", "{\"text\":\"x\"}", 1_200);
        foreign_bot_row.bot_id = other_bot;
        foreign_bot_row.conversation_id = ConversationId::from_uuid(fixed_uuid(445));
        store
            .insert_inbound_if_new(foreign_bot_row)
            .expect("insert foreign bot inbound");

        let rows = store
            .list_history(&bot_id(), &conversation_id(), 10)
            .expect("list history");
        assert_eq!(rows.len(), 1, "只该有本 bot + 本会话的那一行");
        assert_eq!(rows[0].message_id, inbound_id(51).to_string());
        assert_eq!(rows[0].text, "mine");
    }

    /// 同一毫秒多条时，用 `message_id` 做次级键 —— 顺序必须稳定（两次读一样）。
    #[test]
    fn equal_timestamps_are_ordered_by_message_id_and_are_stable() {
        let dir = TempDir::new("history-tiebreak");
        let store = open_store_with_seed(&dir);
        for id in [61_u128, 62, 63] {
            store
                .insert_inbound_if_new(inbound_with_payload(
                    id,
                    &format!("ext-{id}"),
                    "{\"text\":\"same\"}",
                    5_000,
                ))
                .expect("insert same-ms inbound");
        }

        let first = store
            .list_history(&bot_id(), &conversation_id(), 10)
            .expect("first read");
        let second = store
            .list_history(&bot_id(), &conversation_id(), 10)
            .expect("second read");
        assert_eq!(first, second, "同一毫秒的多条必须每次顺序一致");
        assert_eq!(
            first
                .iter()
                .map(|row| row.message_id.clone())
                .collect::<Vec<_>>(),
            vec![
                inbound_id(61).to_string(),
                inbound_id(62).to_string(),
                inbound_id(63).to_string(),
            ],
            "次级键是 message_id 升序"
        );
    }

    /// inbound 的 `text` 解析：正常取到、缺键空串、非字符串空串、整段非法 JSON 空串。
    ///
    /// 一行坏 payload 只影响它自己的文本，不能让整条历史读失败。
    #[test]
    fn inbound_text_comes_from_the_payload_with_safe_fallbacks() {
        let dir = TempDir::new("history-payload-text");
        let store = open_store_with_seed(&dir);
        for (id, payload) in [
            (71_u128, "{\"text\":\"hi\"}"),
            (72, "{\"text\":\"say \\\"hi\\\"\"}"),
            (73, "{\"senderId\":\"u1\"}"),
            (74, "{\"text\":42}"),
            (75, "not json at all"),
            (76, "{\"text\":\"a\",\"text\":\"b\"}"),
            (77, "{\"nested\":{\"text\":\"inner\"}}"),
            (78, "{\"text\":\"一二三\"}"),
            (79, "{\"text\":\"\\u4f60\\u597d\"}"),
        ] {
            store
                .insert_inbound_if_new(inbound_with_payload(
                    id,
                    &format!("ext-{id}"),
                    payload,
                    1_000 + i64::try_from(id).expect("id fits") - 71,
                ))
                .expect("insert inbound");
        }

        let rows = store
            .list_history(&bot_id(), &conversation_id(), 20)
            .expect("list history");
        let by_id = |id: u128| {
            rows.iter()
                .find(|row| row.message_id == inbound_id(id).to_string())
                .unwrap_or_else(|| panic!("缺少 inbound {id}"))
                .text
                .clone()
        };

        assert_eq!(by_id(71), "hi");
        assert_eq!(by_id(72), "say \"hi\"", "转义必须解开");
        assert_eq!(by_id(73), "", "缺 text 键 → 空串");
        assert_eq!(by_id(74), "", "text 不是字符串 → 空串");
        assert_eq!(by_id(75), "", "整段非法 JSON → 空串");
        assert_eq!(by_id(76), "b", "重复键后者覆盖（与 serde_json 一致）");
        assert_eq!(by_id(77), "", "只认顶层 text，不挖嵌套");
        assert_eq!(by_id(78), "一二三", "多字节 UTF-8 必须原样");
        assert_eq!(by_id(79), "你好", "\\uXXXX 必须解码（含代理对路径）");
    }

    /// payload 扫描器的单元边界：不合法 / 顶层非对象 / 尾随垃圾都按"没有 text"。
    #[test]
    fn payload_scanner_rejects_malformed_documents() {
        for (payload, expected) in [
            ("{\"text\":\"ok\"}", "ok"),
            ("  {\"text\":\"ok\"}  ", "ok"),
            ("{}", ""),
            ("[]", ""),
            ("\"text\"", ""),
            ("null", ""),
            ("{\"text\":\"unterminated", ""),
            ("{\"text\":\"a\"} trailing", ""),
            ("{\"text\":\"\\ud83d\\ude00\"}", "😀"),
            ("{\"text\":\"\\ud83d\"}", ""),
            ("{\"text\":\"\\q\"}", ""),
            ("{\"n\":1.5e-3,\"text\":\"n\"}", "n"),
            ("{\"flag\":true,\"nothing\":null,\"text\":\"t\"}", "t"),
            ("{\"arr\":[1,{\"text\":\"x\"}],\"text\":\"top\"}", "top"),
        ] {
            assert_eq!(
                text_from_payload(payload),
                expected,
                "payload = {payload:?} 的解析结果不对"
            );
        }
    }

    /// 裸连接上验证 history 的 SQL 形状（`limit` 精确截断与 UNION 后的二次截断都生效）。
    #[test]
    fn merged_window_is_capped_by_limit_across_both_tables() {
        let dir = TempDir::new("history-window-cap");
        let store = open_store_with_seed(&dir);
        drop(store);

        let conn = raw(&dir);
        for (id, at) in [(81_u128, 1_000_i64), (82, 2_000), (83, 3_000)] {
            conn.execute(
                "INSERT INTO inbound (id, bot_id, external_id, conversation_id, sender_id, payload_json, received_at)
                 VALUES (?1, ?2, ?3, ?4, '', '{\"text\":\"i\"}', ?5)",
                rusqlite::params![
                    inbound_id(id).to_string(),
                    bot_id().to_string(),
                    format!("ext-{id}"),
                    conversation_id().to_string(),
                    at
                ],
            )
            .expect("insert inbound");
        }
        for (id, at) in [(91_u128, 3_500_i64), (92, 4_000)] {
            conn.execute(
                "INSERT INTO outbound (id, bot_id, conversation_id, text, state, attempts, created_at)
                 VALUES (?1, ?2, ?3, 'o', 'pending', 0, ?4)",
                rusqlite::params![
                    outbound_id(id).to_string(),
                    bot_id().to_string(),
                    conversation_id().to_string(),
                    at
                ],
            )
            .expect("insert outbound");
        }

        let rows = history(&conn, &bot_id(), &conversation_id(), 2).expect("history limit 2");
        assert_eq!(
            rows.iter().map(|row| row.at).collect::<Vec<_>>(),
            vec![3_500, 4_000],
            "limit 必须作用于两表合并后的窗口"
        );
        assert_eq!(rows[0].direction, DIRECTION_OUTBOUND);
        assert_eq!(rows[1].direction, DIRECTION_OUTBOUND);
    }

    /// 打开库并写种子行，返回可用的 `Store`。
    fn open_store_with_seed(dir: &TempDir) -> Store {
        crate::test_support::open_store(dir)
    }
}
