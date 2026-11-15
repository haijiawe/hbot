//! 会话 ID 的确定性派生。
//!
//! ## 为什么不用 `IdGenerator::conversation_id()`
//!
//! `conversations` 的幂等键是 `UNIQUE (bot_id, external_id)`，而 `hbot-store` 只暴露
//! 会话写入、没有“按 `(bot_id, external_id)` 读回”的 API。若会话 ID 随机，进程重启后
//! 再次见到同一会话时既读不到旧行、也不能安全地再插一行（UNIQUE 冲突后拿不到旧 ID），
//! 之后写 `inbound` 就会因为外键指向不存在的 `conversations.id` 失败。
//!
//! 因此会话 ID 由 `(bot_id, external_id)` **确定性派生**：同一会话在任何进程、任何
//! 重启后都得到同一个 ID，`ensure_conversation` 的“插入或已存在”就是幂等的。
//!
//! ## 哈希选择
//!
//! 用 FNV-1a 128 位（算法常量固定，跨 Rust 版本、跨平台稳定；`std` 的
//! `DefaultHasher` 明确不保证跨版本稳定，不能用于落库身份）。输入是
//! `bot_id 的 UUID 文本 + 0x00 + external_id`，用 0x00 分隔避免拼接歧义。
//!
//! 输出按 RFC 9562 打上 version 8（自定义）与 RFC 4122 variant 位，`Display` /
//! `FromStr` 与其它 ID 一致。碰撞概率对“单 bot 的会话数”量级可忽略；真出现碰撞时
//! `conversations` 的主键冲突会显式报错，不会被静默吞掉。

use hbot_core::{BotId, ConversationId};
use uuid::Uuid;

/// FNV-1a 128 位的偏移基。
const FNV_OFFSET_BASIS_128: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
/// FNV-1a 128 位的质数。
const FNV_PRIME_128: u128 = 0x0000_0001_0000_0000_0000_0000_0000_013b;

/// 由 `(bot_id, external_id)` 派生稳定的会话 ID。
///
/// 同样的输入永远得到同样的 ID；不同 bot 的同名会话、同一 bot 的不同会话互不相同。
pub fn conversation_id_for(bot_id: &BotId, external_id: &str) -> ConversationId {
    let mut hash = FNV_OFFSET_BASIS_128;
    for byte in bot_id
        .to_string()
        .bytes()
        .chain(std::iter::once(0))
        .chain(external_id.bytes())
    {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }

    let mut bytes = hash.to_be_bytes();
    // RFC 9562 version 8（自定义）+ RFC 4122 variant，让 `Uuid` 的解析/展示行为与其它
    // ID 完全一致，同时不冒充 v4（它不是随机值）。
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ConversationId::from_uuid(Uuid::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use hbot_core::BotId;
    use uuid::Uuid;

    use super::conversation_id_for;

    fn bot(value: u128) -> BotId {
        BotId::from_uuid(Uuid::from_u128(value))
    }

    #[test]
    fn derivation_is_stable() {
        let first = conversation_id_for(&bot(2), "group-42");
        let second = conversation_id_for(&bot(2), "group-42");
        assert_eq!(first, second, "同一会话必须派生出同一个 ID");
    }

    #[test]
    fn different_bots_or_conversations_differ() {
        let base = conversation_id_for(&bot(2), "group-42");
        assert_ne!(base, conversation_id_for(&bot(3), "group-42"));
        assert_ne!(base, conversation_id_for(&bot(2), "group-43"));
        // 拼接歧义：("a\0b") 与两个字段的组合必须不同。
        assert_ne!(
            conversation_id_for(&bot(2), "group-42"),
            conversation_id_for(&bot(2), "group-4"),
        );
    }

    #[test]
    fn id_round_trips_through_text() {
        let id = conversation_id_for(&bot(2), "group-42");
        let text = id.to_string();
        let parsed: hbot_core::ConversationId = text.parse().expect("uuid text parses");
        assert_eq!(parsed, id);
        // version 8 / variant 位：解析后 UUID 版本必须可读，不能被当成随机 v4。
        assert_eq!(id.as_uuid().get_version_num(), 8);
    }
}
