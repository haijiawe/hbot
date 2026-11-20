//! 标准 base64（RFC 4648 §4）编码 —— 设备截图与 `ctx.files.readBytes` 共用。
//!
//! 手写而不是加依赖：一个 30 行的编码器不值得为它新增一棵依赖树，而
//! `xtask/src/deps.rs` 的依赖表是白名单制。只做编码 —— 解码没人用，不写。

/// 标准 base64（RFC 4648 §4）编码，带 `=` 补齐。
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    // 每个 3 字节组变成 4 个字符（最后一组用 `=` 补齐）。
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).map_or(0, |byte| u32::from(*byte));
        let third = chunk.get(2).map_or(0, |byte| u32::from(*byte));
        let triple = (first << 16) | (second << 8) | third;

        out.push(char::from(ALPHABET[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3f) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((triple >> 6) & 0x3f) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(triple & 0x3f) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 §10 的两个标准向量，外加空输入与 1/2 字节的补齐分支。
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_keeps_every_byte_distinct() {
        // 0x00/0xff/0x80 是最容易被手写编码器弄丢的三类字节。
        let bytes: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&bytes);
        assert_eq!(encoded.len(), bytes.len().div_ceil(3) * 4);
        assert!(encoded.chars().all(|c| c.is_ascii_graphic() || c == '='));
        assert_eq!(&encoded[..4], "AAEC");
    }
}
