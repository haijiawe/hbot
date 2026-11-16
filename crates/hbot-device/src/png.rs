//! 最小 PNG 编码器：8 位真彩色（RGB）、行过滤器固定 `None`、zlib 用 **stored（不压缩）
//! deflate 块**。
//!
//! # 为什么不引 `png` / `image`
//!
//! 这个 crate 的依赖面要保持在「`hbot-core` + 平台 crate」这一档（见 `docs/09-device-capability.md`
//! §6.1 倾向窄版零新依赖）。PNG 的输出格式是 RFC 2083 里最稳定的那一小块：写 IHDR + 一个 IDAT
//! + IEND 就够了，标准解码器（浏览器、`PIL`、`pngcheck`）都能读。
//!
//! # 真实代价（别指望它是压缩器）
//!
//! 不压缩 → 体积约等于裸像素：1920×1080 一张 ≈ **6.2 MB**（`1920*1080*3` 加 1080 字节行首过滤器）。
//! 所以 [`ScreenshotRequest::max_bytes`](hbot_core::device::ScreenshotRequest) 的默认值必须按这个
//! 量级给：给 1 MB 在 1080p 上会**必然** `TooLarge`（这是设计上要的行为：宁可报错，不给半张图）。
//! 换成 `flate2` 的 stored→fixed-huffman 能小一个数量级，但那要引第三方 crate，属于以后的决定。
//!
//! # 这里只有编码，没有解码
//!
//! 解码器只存在于 `#[cfg(test)]` 的测试里，用途是**证明**编码正确（自己写的编码器自己读回来，
//! 并且校验每个块的 CRC32、zlib 的 adler32、以及像素逐字节相等）。生产路径不解码。

use hbot_core::DeviceError;

/// PNG 文件签名（RFC 2083 §3.1）。
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// CRC-32（PNG 用的那个多项式）的初值。
const CRC_INIT: u32 = 0xffff_ffff;

/// 一个 stored deflate 块最多能放多少字节（长度字段是 `u16`）。
const MAX_STORED_BLOCK: usize = 65_535;

/// 把 8 位 RGB 像素编码成 PNG。
///
/// `pixels` 是行优先、无行填充的 RGB 三元组，长度必须正好是 `width * height * 3`。
///
/// 出错只有一种情况：尺寸为 0 或缓冲区长度对不上（这是调用方的编程错误，不是运行时状态，
/// 但仍返回 [`DeviceError`] 而不是 panic —— 库代码不 panic）。
pub fn encode_rgb(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, DeviceError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|value| value.checked_mul(3))
        .ok_or_else(|| DeviceError::Failed {
            message: format!("PNG 编码参数不合法：{width}x{height} 的尺寸溢出 usize"),
        })?;
    if width == 0 || height == 0 || pixels.len() != expected {
        return Err(DeviceError::Failed {
            message: format!(
                "PNG 编码参数不合法：{width}x{height} 需要 {expected} 字节 RGB，实际给了 {} 字节",
                pixels.len()
            ),
        });
    }

    let stride = width as usize * 3;
    let mut raw: Vec<u8> = Vec::with_capacity(height as usize + pixels.len());
    for row in pixels.chunks_exact(stride) {
        // 过滤器类型 0 = None：存原始字节。截图没有重排的价值（stored 块不会变短）。
        raw.push(0);
        raw.extend_from_slice(row);
    }

    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    write_chunk(&mut out, b"IHDR", &ihdr(width, height));
    write_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// IHDR 的 13 个字节：宽、高、位深 8、颜色类型 2（真彩色）、压缩 0、过滤 0、隔行 0。
fn ihdr(width: u32, height: u32) -> [u8; 13] {
    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8; // 位深
    header[9] = 2; // 颜色类型：2 = 真彩色 RGB
    header[10] = 0; // 压缩方法：0 = deflate
    header[11] = 0; // 过滤方法：0（唯一的合法值）
    header[12] = 0; // 隔行：0 = 不隔行
    header
}

/// 一个 PNG 块：长度（4 字节大端）+ 类型 + 数据 + CRC-32（类型与数据一起算）。
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let state = crc32_update(crc32_update(CRC_INIT, kind), data);
    out.extend_from_slice(&(state ^ 0xffff_ffff).to_be_bytes());
}

/// zlib 流（RFC 1950）+ 若干 stored deflate 块（RFC 1951 §3.2.4）+ adler32。
///
/// 头两个字节 `78 01`：CM=8（deflate）、CINFO=7（32K 窗口）、FLEVEL=0（最快）、
/// FCHECK 让 `(0x78 << 8 | 0x01) % 31 == 0`（规范要求的校验）。
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let blocks = data.len().div_ceil(MAX_STORED_BLOCK).max(1);
    let mut out: Vec<u8> = Vec::with_capacity(data.len() + blocks * 5 + 6);
    out.push(0x78);
    out.push(0x01);
    let mut chunks = data.chunks(MAX_STORED_BLOCK).peekable();
    if chunks.peek().is_none() {
        // 空数据也要有一个（最后一个）空 stored 块，否则 deflate 流没有终止标记。
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        while let Some(chunk) = chunks.next() {
            // BFINAL = 是否是最后一块；BTYPE = 00（stored）。
            out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// CRC-32 的查表（编译期生成，运行期没有初始化、没有全局可变状态）。
const CRC32_TABLE: [u32; 256] = crc32_table();

/// 生成 CRC-32 查表（多项式 0xEDB88320，反射写法）。
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// 续算 CRC-32：`state` 是上一次的结果（首块用 [`CRC_INIT`]）。
fn crc32_update(state: u32, bytes: &[u8]) -> u32 {
    let mut current = state;
    for &byte in bytes {
        let slot = ((current ^ byte as u32) & 0xff) as usize;
        current = CRC32_TABLE[slot] ^ (current >> 8);
    }
    current
}

/// zlib 的 adler32 校验和。
fn adler32(data: &[u8]) -> u32 {
    const MODULUS: u32 = 65_521;
    // 5552 是「累加多少次不会溢出 u32」的经典上界，分块后才取模。
    const BLOCK: usize = 5552;
    let mut low: u32 = 1;
    let mut high: u32 = 0;
    for chunk in data.chunks(BLOCK) {
        for &byte in chunk {
            low += byte as u32;
            high += low;
        }
        low %= MODULUS;
        high %= MODULUS;
    }
    (high << 16) | low
}

#[cfg(test)]
mod tests {
    //! 解码器只在这里存在：用来证明编码器的输出**真的**能被读回正确像素。

    use super::*;

    /// 2×2 已知像素：红、绿、蓝、白。
    const KNOWN_2X2: [u8; 12] = [
        255, 0, 0, // (0,0) 红
        0, 255, 0, // (1,0) 绿
        0, 0, 255, // (0,1) 蓝
        255, 255, 255, // (1,1) 白
    ];

    #[test]
    fn encodes_a_known_2x2_image_that_a_minimal_decoder_reads_back() {
        let png = encode_rgb(2, 2, &KNOWN_2X2).expect("2×2 编码应成功");
        let (width, height, pixels) = decode(&png);
        assert_eq!((width, height), (2, 2));
        assert_eq!(pixels, KNOWN_2X2, "解码回来的像素必须逐字节等于输入");
    }

    #[test]
    fn ihdr_is_rgb8_non_interlaced() {
        let png = encode_rgb(2, 2, &KNOWN_2X2).expect("编码应成功");
        let header = ihdr_of(&png);
        assert_eq!(&header[0..4], &2u32.to_be_bytes(), "宽");
        assert_eq!(&header[4..8], &2u32.to_be_bytes(), "高");
        assert_eq!(header[8], 8, "位深 8");
        assert_eq!(header[9], 2, "颜色类型 2（真彩色）");
        assert_eq!(header[10], 0, "压缩方法 deflate");
        assert_eq!(header[11], 0, "过滤方法 0");
        assert_eq!(header[12], 0, "不隔行");
    }

    #[test]
    fn starts_with_the_png_signature_and_ends_with_iend() {
        let png = encode_rgb(2, 2, &KNOWN_2X2).expect("编码应成功");
        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn rejects_a_pixel_buffer_whose_length_does_not_match() {
        let error = encode_rgb(2, 2, &KNOWN_2X2[..11]).expect_err("长度不符应当报错");
        assert_eq!(error.code(), "HBOT_DEVICE_FAILED");
        assert!(encode_rgb(0, 2, &[]).is_err(), "宽度 0 应当报错");
        assert!(encode_rgb(2, 0, &[]).is_err(), "高度 0 应当报错");
    }

    #[test]
    fn splits_large_images_across_multiple_stored_deflate_blocks() {
        // 200×200×3 + 200 = 120_800 字节，超过一个 stored 块（65_535），
        // 这条用例专抓「分块边界写错」——那种错只在大图上才会露。
        let width = 200u32;
        let height = 200u32;
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push((x % 256) as u8);
                pixels.push((y % 256) as u8);
                pixels.push(((x + y) % 256) as u8);
            }
        }
        let png = encode_rgb(width, height, &pixels).expect("大图编码应成功");
        let (decoded_width, decoded_height, decoded) = decode(&png);
        assert_eq!((decoded_width, decoded_height), (width, height));
        assert_eq!(decoded, pixels);
    }

    /// 读 IHDR 数据（13 字节），顺带校验所有块的 CRC 都成立。
    fn ihdr_of(png: &[u8]) -> Vec<u8> {
        assert_eq!(&png[..8], &SIGNATURE, "签名");
        let mut position = 8;
        while position < png.len() {
            let (kind, data, next) = next_chunk(png, position);
            if kind == *b"IHDR" {
                return data.to_vec();
            }
            position = next;
        }
        panic!("没有 IHDR");
    }

    /// 从 `position` 读一个块，返回（类型、数据、下一块的起点），并校验 CRC-32。
    fn next_chunk(png: &[u8], position: usize) -> ([u8; 4], &[u8], usize) {
        let length = u32::from_be_bytes(
            png[position..position + 4]
                .try_into()
                .expect("长度字段应当有 4 字节"),
        ) as usize;
        let kind: [u8; 4] = png[position + 4..position + 8]
            .try_into()
            .expect("类型字段应当有 4 字节");
        let data = &png[position + 8..position + 8 + length];
        let stored_crc = u32::from_be_bytes(
            png[position + 8 + length..position + 12 + length]
                .try_into()
                .expect("CRC 字段应当有 4 字节"),
        );
        let computed = crc32_update(crc32_update(CRC_INIT, &kind), data) ^ 0xffff_ffff;
        assert_eq!(
            computed,
            stored_crc,
            "块 `{}` 的 CRC-32 不对",
            String::from_utf8_lossy(&kind)
        );
        (kind, data, position + 12 + length)
    }

    /// 极简解码器：只认本模块写出来的形状（RGB8 / filter 0 / 全 stored 块）。
    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], &SIGNATURE, "签名");
        let mut position = 8;
        let mut header: Option<Vec<u8>> = None;
        let mut idat: Vec<u8> = Vec::new();
        let mut saw_iend = false;
        while position < png.len() {
            let (kind, data, next) = next_chunk(png, position);
            match &kind {
                b"IHDR" => header = Some(data.to_vec()),
                b"IDAT" => idat.extend_from_slice(data),
                b"IEND" => saw_iend = true,
                other => panic!("意外的块 `{}`", String::from_utf8_lossy(other)),
            }
            position = next;
        }
        assert_eq!(position, png.len(), "块长度之和必须正好等于文件长度");
        assert!(saw_iend, "缺 IEND");

        let header = header.expect("缺 IHDR");
        assert_eq!(header.len(), 13, "IHDR 必须 13 字节");
        assert_eq!(header[8], 8, "位深");
        assert_eq!(header[9], 2, "颜色类型");
        assert_eq!(header[12], 0, "隔行");

        let width = u32::from_be_bytes(header[0..4].try_into().expect("宽"));
        let height = u32::from_be_bytes(header[4..8].try_into().expect("高"));
        let raw = inflate_stored(&idat);
        let stride = width as usize * 3;
        assert_eq!(
            raw.len(),
            height as usize * (1 + stride),
            "解压后的裸像素长度"
        );

        let mut pixels = Vec::with_capacity(height as usize * stride);
        for row in raw.chunks_exact(1 + stride) {
            assert_eq!(row[0], 0, "行过滤器只写了 None（0）");
            pixels.extend_from_slice(&row[1..]);
        }
        (width, height, pixels)
    }

    /// 只解 stored 块（BTYPE=00），并校验 `NLEN` 与 adler32。
    fn inflate_stored(zlib: &[u8]) -> Vec<u8> {
        assert_eq!(&zlib[..2], &[0x78, 0x01], "zlib 头");
        let mut position = 2;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let header = zlib[position];
            position += 1;
            let is_final = header & 0x01 == 0x01;
            assert_eq!(header & 0x06, 0, "本编码器只写 stored 块（BTYPE=00）");
            let length = u16::from_le_bytes([zlib[position], zlib[position + 1]]);
            let complement = u16::from_le_bytes([zlib[position + 2], zlib[position + 3]]);
            position += 4;
            assert_eq!(!length, complement, "NLEN 必须是 LEN 的反码");
            let end = position + length as usize;
            out.extend_from_slice(&zlib[position..end]);
            position = end;
            if is_final {
                break;
            }
        }
        let stored_adler = u32::from_be_bytes(
            zlib[position..position + 4]
                .try_into()
                .expect("adler32 应当有 4 字节"),
        );
        position += 4;
        assert_eq!(position, zlib.len(), "adler32 之后不该再有字节");
        assert_eq!(stored_adler, adler32(&out), "adler32 不对");
        out
    }
}
