//! 受限 HTTP 抓取：给 Agent 工具/适配器读网页用的**最小** HTTP/1.1 客户端。
//!
//! 为什么不用 `reqwest`：`docs/03-directory.md` 规定 `hbot-adapters` 只依赖
//! `hbot-core` + 少量通用库；为了一次 GET 拉进整套 TLS/连接池依赖不划算，
//! 而且这里更想要**可控**：超时、大小上限、SSRF 拦截都必须是本模块的硬约束，
//! 不是库的默认值。
//!
//! 限制（**有意为之**，都返回明确错误，不静默降级）：
//!
//! - 只允许 `http://` / `https://`；其它 scheme（`file:`、`ftp:`、`data:` …）
//!   直接 `Blocked`。
//! - `https://` 需要 TLS 后端，本 crate 没链接 → `Unsupported`。
//!   要支持就换带 rustls 的实现，别在这里假装成功。
//! - 不跟随重定向：3xx 直接报错。跟随会把 SSRF 校验绕过去（跳到内网），
//!   需要的话由调用方决定是否重新抓下一个 URL。
//! - 拒绝 userinfo（`http://user:pass@host/`）、内网/回环/链路本地/保留地址、
//!   `localhost`、`*.localhost`、`*.internal`、纯数字主机（`http://2130706433/`
//!   是十进制 127.0.0.1）、IDN（非 ASCII 主机名）。
//! - 响应头 ≤ 64 KiB；正文 ≤ `max_bytes`（超了 `Limit`）；整体 ≤ `timeout`。
//!
//! 注意 DNS rebinding 仍可能绕过"解析后校验"：这里校验解析出来的**所有**地址，
//! 并且只连校验过的 `SocketAddr`（不把主机名交给连接层再解析一次）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::AdapterError;

/// URL 长度上限。
pub const MAX_URL_LEN: usize = 8192;
/// 响应头上限（含状态行）。
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// User-Agent；显式标识以便对端限流/排查。
pub const USER_AGENT: &str = "hbot/0.1";

/// 允许的 URL scheme。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// 明文 HTTP。
    Http,
    /// HTTPS（当前无 TLS 后端，抓取时明确报 `Unsupported`）。
    Https,
}

impl Scheme {
    /// 默认端口。
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// 解析后的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpTarget {
    /// 协议。
    pub scheme: Scheme,
    /// 主机名（已小写；IPv6 已去掉方括号）。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 请求路径（含 query，含前导 `/`，已去掉 fragment）。
    pub path: String,
}

/// 解析并校验 URL 的静态部分（不做 DNS）。
pub fn parse_target(url: &str) -> Result<HttpTarget, AdapterError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::blocked("url must not be empty"));
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(AdapterError::limit(format!(
            "url length {} exceeds {MAX_URL_LEN} bytes",
            trimmed.len()
        )));
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(AdapterError::blocked(
            "url must not contain whitespace or control characters",
        ));
    }

    // scheme 用前缀长度切片（scheme 本身是 ASCII，按字节切安全）。
    let lower = trimmed.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("http://") {
        (Scheme::Http, &trimmed["http://".len()..])
    } else if lower.starts_with("https://") {
        (Scheme::Https, &trimmed["https://".len()..])
    } else {
        let scheme = trimmed.split(':').next().unwrap_or(trimmed);
        return Err(AdapterError::blocked(format!(
            "unsupported url scheme `{scheme}`: only http:// and https:// are allowed"
        )));
    };

    let split_at = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..split_at];
    let path_and_more = &rest[split_at..];
    if authority.is_empty() {
        return Err(AdapterError::blocked("url has no host"));
    }
    if authority.contains('@') {
        return Err(AdapterError::blocked(
            "url must not contain userinfo (user:pass@host)",
        ));
    }

    let (raw_host, explicit_port) = split_authority(authority)?;
    let host = raw_host.to_ascii_lowercase();
    if host.is_empty() {
        return Err(AdapterError::blocked("url has no host"));
    }
    if !host.is_ascii() {
        return Err(AdapterError::blocked(
            "internationalized hostnames are not supported; use punycode",
        ));
    }
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' || ch == ':')
    {
        return Err(AdapterError::blocked(format!(
            "host `{host}` contains characters that are not allowed"
        )));
    }
    check_host_name(&host)?;

    // 字面 IP 先查一遍，省掉 DNS；域名等解析后再查全部地址。
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(AdapterError::blocked(format!(
                "refusing to fetch a private/loopback address: {ip}"
            )));
        }
    } else if is_numeric_host(&host) {
        // `http://2130706433/`、`http://0x7f.1/` 这类"看起来像主机其实像 IP"的写法，
        // 各平台解析规则不一致，直接拒绝。
        return Err(AdapterError::blocked(format!(
            "ambiguous numeric host `{host}` is not allowed; use a dotted-decimal ip or a name"
        )));
    }

    let path = build_path(
        path_and_more,
        scheme == Scheme::Https && authority.is_empty(),
    );
    Ok(HttpTarget {
        scheme,
        host,
        port: explicit_port.unwrap_or_else(|| scheme.default_port()),
        path,
    })
}

/// 拆 `host[:port]`，支持 `[::1]:80`。
fn split_authority(authority: &str) -> Result<(&str, Option<u16>), AdapterError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return Err(AdapterError::blocked("malformed ipv6 host (missing `]`)"));
        };
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = if after.is_empty() {
            None
        } else {
            let Some(port_text) = after.strip_prefix(':') else {
                return Err(AdapterError::blocked(
                    "malformed ipv6 host (trailing characters after `]`)",
                ));
            };
            Some(parse_port(port_text)?)
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port_text)) => Ok((host, Some(parse_port(port_text)?))),
        None => Ok((authority, None)),
    }
}

fn parse_port(text: &str) -> Result<u16, AdapterError> {
    if text.is_empty() || !text.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(AdapterError::blocked(format!(
            "invalid port `{text}` in url"
        )));
    }
    text.parse::<u16>()
        .map_err(|_| AdapterError::blocked(format!("port `{text}` is out of range")))
}

/// 主机名策略：本机名一律拒绝。
fn check_host_name(host: &str) -> Result<(), AdapterError> {
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(AdapterError::blocked(
            "loopback host name `localhost` is not allowed",
        ));
    }
    if host == "metadata.google.internal" || host.ends_with(".internal") {
        return Err(AdapterError::blocked(
            "cloud metadata / internal host names are not allowed",
        ));
    }
    Ok(())
}

/// 纯数字/十六进制"主机名"（各平台会被当成 IP 解析）。
fn is_numeric_host(host: &str) -> bool {
    if host.contains(':') {
        return false; // IPv6 字面量走 parse::<IpAddr>() 分支
    }
    let mut parts = 0usize;
    for part in host.split('.') {
        parts += 1;
        if part.is_empty() {
            return false;
        }
        let numeric = part.chars().all(|ch| ch.is_ascii_digit())
            || (part.len() > 2
                && (part.starts_with("0x") || part.starts_with("0X"))
                && part[2..].chars().all(|ch| ch.is_ascii_hexdigit()));
        if !numeric {
            return false;
        }
    }
    parts >= 1 && host.chars().any(|ch| ch.is_ascii_digit())
}

fn build_path(path_and_more: &str, _dummy: bool) -> String {
    if path_and_more.is_empty() {
        return "/".to_string();
    }
    let mut path = path_and_more.to_string();
    if path.starts_with('?') {
        path.insert(0, '/');
    }
    if let Some(hash) = path.find('#') {
        path.truncate(hash);
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    path
}

/// 内网/保留/回环地址判定。
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // IPv4-mapped（::ffff:127.0.0.1）按 IPv4 规则判。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            let segments = v6.segments();
            let unique_local = segments[0] & 0xfe00 == 0xfc00; // fc00::/7
            let link_local = segments[0] & 0xffc0 == 0xfe80; // fe80::/10
            let site_local = segments[0] & 0xffc0 == 0xfec0; // fec0::/10
            let nat64 = segments[0] == 0x0064 && segments[1] == 0xff9b; // 64:ff9b::/96
            unique_local || link_local || site_local || nat64
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let [a, b, c, _d] = octets;
    a == 0                       // 0.0.0.0/8
        || a == 127              // 127.0.0.0/8
        || a == 10               // 10.0.0.0/8
        || (a == 172 && (16..=31).contains(&b))   // 172.16.0.0/12
        || (a == 192 && b == 168)                 // 192.168.0.0/16
        || (a == 169 && b == 254)                 // 169.254.0.0/16
        || (a == 100 && (64..=127).contains(&b))  // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0)         // 192.0.0.0/24
        || (a == 192 && b == 0 && c == 2)         // TEST-NET-1
        || (a == 198 && (b == 18 || b == 19))     // 198.18.0.0/15
        || (a == 198 && b == 51 && c == 100)      // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)       // TEST-NET-3
        || a >= 224 // 组播 + 保留 + 广播
}

/// 抓取 URL 并返回文本。
///
/// - `timeout`：整体超时（DNS + 连接 + 读写）；
/// - `max_bytes`：正文上限，必须 > 0。
pub async fn http_fetch(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, AdapterError> {
    if max_bytes == 0 {
        return Err(AdapterError::unsupported(
            "max_bytes must be greater than 0",
        ));
    }
    let target = parse_target(url)?;
    if target.scheme == Scheme::Https {
        return Err(AdapterError::unsupported(
            "https:// requires a TLS backend, which hbot-adapters does not link",
        ));
    }
    if timeout.is_zero() {
        return Err(AdapterError::timeout("http fetch", timeout));
    }
    match tokio::time::timeout(timeout, fetch_inner(&target, max_bytes)).await {
        Ok(result) => result,
        Err(_) => Err(AdapterError::timeout("http fetch", timeout)),
    }
}

async fn fetch_inner(target: &HttpTarget, max_bytes: usize) -> Result<String, AdapterError> {
    let addresses = resolve(&target.host, target.port).await?;
    let mut stream = connect_any(&addresses).await?;
    let host_header = if target.port == target.scheme.default_port() {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        target.path
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| AdapterError::transport(format!("http write failed: {err}")))?;
    stream
        .flush()
        .await
        .map_err(|err| AdapterError::transport(format!("http flush failed: {err}")))?;
    read_response(&mut stream, max_bytes).await
}

/// 解析主机名并**校验所有**地址；任何一个落在内网就整体拒绝。
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, AdapterError> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| AdapterError::transport(format!("dns lookup failed for `{host}`: {err}")))?
        .collect();
    if addresses.is_empty() {
        return Err(AdapterError::transport(format!(
            "dns lookup returned no address for `{host}`"
        )));
    }
    if let Some(blocked) = addresses.iter().find(|addr| is_blocked_ip(addr.ip())) {
        return Err(AdapterError::blocked(format!(
            "host `{host}` resolves to a private/loopback address ({})",
            blocked.ip()
        )));
    }
    Ok(addresses)
}

async fn connect_any(addresses: &[SocketAddr]) -> Result<TcpStream, AdapterError> {
    let mut last_error = String::from("no address to connect");
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last_error = format!("{address}: {err}"),
        }
    }
    Err(AdapterError::transport(format!(
        "http connect failed: {last_error}"
    )))
}

/// 读响应（状态行 + 头 + 正文），**状态码必须在读正文之前判**。
async fn read_response<S>(stream: &mut S, max_bytes: usize) -> Result<String, AdapterError>
where
    S: AsyncRead + Unpin,
{
    let mut buffer: Vec<u8> = Vec::with_capacity(8192);
    loop {
        if let Some(head_end) = find_subsequence(&buffer, b"\r\n\r\n") {
            let (status, headers) = parse_head(&buffer[..head_end])?;
            let body_head = buffer[head_end + 4..].to_vec();
            return finish_response(stream, status, &headers, body_head, max_bytes).await;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(AdapterError::limit(format!(
                "http response headers exceed {MAX_HEADER_BYTES} bytes"
            )));
        }
        if !read_more(stream, &mut buffer).await? {
            return Err(AdapterError::transport(
                "connection closed before http response headers completed",
            ));
        }
    }
}

async fn finish_response<S>(
    stream: &mut S,
    status: u16,
    headers: &[(String, String)],
    body_head: Vec<u8>,
    max_bytes: usize,
) -> Result<String, AdapterError>
where
    S: AsyncRead + Unpin,
{
    if (300..400).contains(&status) {
        let location = header(headers, "location").unwrap_or("");
        return Err(AdapterError::unsupported(format!(
            "http redirect ({status}) is not followed (target: {location:?})"
        )));
    }
    if !(200..300).contains(&status) {
        return Err(AdapterError::protocol(format!(
            "http request failed with status {status}"
        )));
    }

    let chunked = header(headers, "transfer-encoding")
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        })
        .unwrap_or(false);
    let content_length =
        match header(headers, "content-length") {
            Some(value) => Some(value.trim().parse::<usize>().map_err(|_| {
                AdapterError::protocol(format!("invalid content-length `{value}`"))
            })?),
            None => None,
        };

    let body = if chunked {
        read_chunked(stream, body_head, max_bytes).await?
    } else if let Some(length) = content_length {
        read_fixed(stream, body_head, length, max_bytes).await?
    } else {
        read_until_eof(stream, body_head, max_bytes).await?
    };
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn parse_head(block: &[u8]) -> Result<(u16, Vec<(String, String)>), AdapterError> {
    let text = std::str::from_utf8(block)
        .map_err(|_| AdapterError::protocol("http response head is not valid utf-8"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(AdapterError::protocol(format!(
            "unexpected http status line `{status_line}`"
        )));
    }
    let status = parts
        .next()
        .ok_or_else(|| AdapterError::protocol("http status line has no status code"))?
        .parse::<u16>()
        .map_err(|_| {
            AdapterError::protocol(format!("invalid http status code in `{status_line}`"))
        })?;

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(AdapterError::protocol(format!(
                "malformed http header `{line}`"
            )));
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(AdapterError::protocol("http header with empty name"));
        }
        headers.push((name, value.trim().to_string()));
    }
    Ok((status, headers))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _value)| key.eq_ignore_ascii_case(name))
        .map(|(_key, value)| value.as_str())
}

/// 定长正文。
async fn read_fixed<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    length: usize,
    max_bytes: usize,
) -> Result<Vec<u8>, AdapterError>
where
    S: AsyncRead + Unpin,
{
    if length > max_bytes {
        return Err(AdapterError::limit(format!(
            "http body declares {length} bytes, exceeding max_bytes {max_bytes}"
        )));
    }
    if body.len() > length {
        body.truncate(length);
    }
    while body.len() < length {
        let mut chunk = [0u8; 8192];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| AdapterError::transport(format!("http read failed: {err}")))?;
        if read == 0 {
            return Err(AdapterError::transport(
                "connection closed before the declared content-length was read",
            ));
        }
        let remaining = length - body.len();
        body.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(body)
}

/// 读到 EOF（HTTP/1.0 风格或 `Connection: close`）。
async fn read_until_eof<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    max_bytes: usize,
) -> Result<Vec<u8>, AdapterError>
where
    S: AsyncRead + Unpin,
{
    if body.len() > max_bytes {
        return Err(AdapterError::limit(format!(
            "http body exceeds max_bytes {max_bytes}"
        )));
    }
    loop {
        let mut chunk = [0u8; 8192];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| AdapterError::transport(format!("http read failed: {err}")))?;
        if read == 0 {
            return Ok(body);
        }
        if body.len() + read > max_bytes {
            return Err(AdapterError::limit(format!(
                "http body exceeds max_bytes {max_bytes}"
            )));
        }
        body.extend_from_slice(&chunk[..read]);
    }
}

/// chunked 正文（`Transfer-Encoding: chunked`）。
async fn read_chunked<S>(
    stream: &mut S,
    mut buffer: Vec<u8>,
    max_bytes: usize,
) -> Result<Vec<u8>, AdapterError>
where
    S: AsyncRead + Unpin,
{
    let mut body: Vec<u8> = Vec::new();
    let mut cursor = 0usize;
    loop {
        // 1) 取 size 行
        let line_end = loop {
            if let Some(offset) = find_subsequence(&buffer[cursor..], b"\r\n") {
                break cursor + offset;
            }
            if !read_more(stream, &mut buffer).await? {
                return Err(AdapterError::transport(
                    "connection closed inside a chunked body",
                ));
            }
        };
        let line = std::str::from_utf8(&buffer[cursor..line_end])
            .map_err(|_| AdapterError::protocol("chunk size line is not valid utf-8"))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| AdapterError::protocol(format!("invalid chunk size `{size_text}`")))?;
        cursor = line_end + 2;
        if size == 0 {
            // 忽略 trailer，正文结束。
            return Ok(body);
        }
        if body.len() + size > max_bytes {
            return Err(AdapterError::limit(format!(
                "http body exceeds max_bytes {max_bytes}"
            )));
        }
        // 2) 取 size 字节 + 结尾 CRLF
        while buffer.len() < cursor + size + 2 {
            if !read_more(stream, &mut buffer).await? {
                return Err(AdapterError::transport("connection closed inside a chunk"));
            }
        }
        body.extend_from_slice(&buffer[cursor..cursor + size]);
        cursor += size + 2;
    }
}

async fn read_more<S>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<bool, AdapterError>
where
    S: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    let read = stream
        .read(&mut chunk)
        .await
        .map_err(|err| AdapterError::transport(format!("http read failed: {err}")))?;
    if read == 0 {
        return Ok(false);
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(true)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn blocked_ip_table() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = blocked.parse().expect("valid ip literal");
            assert!(is_blocked_ip(ip), "{blocked} should be blocked");
        }
        for allowed in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "2606:4700::1111"] {
            let ip: IpAddr = allowed.parse().expect("valid ip literal");
            assert!(!is_blocked_ip(ip), "{allowed} should be allowed");
        }
    }

    #[test]
    fn parse_target_rejects_dangerous_forms() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/admin",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://localhost:3001/",
            "http://foo.localhost/",
            "http://metadata.google.internal/",
            "http://user:pass@example.com/",
            "http://2130706433/",
            "http://0x7f.1/",
            "file:///etc/passwd",
            "ftp://example.com/file",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "",
        ] {
            assert!(
                parse_target(url).is_err(),
                "{url} should be rejected by parse_target"
            );
        }
    }

    #[test]
    fn parse_target_accepts_public_urls() {
        let target = parse_target("http://example.com/a/b?c=1#frag").expect("public url");
        assert_eq!(target.scheme, Scheme::Http);
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 80);
        assert_eq!(target.path, "/a/b?c=1");

        let target = parse_target("https://example.com").expect("https parsed (fetch rejects)");
        assert_eq!(target.scheme, Scheme::Https);
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/");

        let target = parse_target("http://1.1.1.1:8080/x").expect("explicit port");
        assert_eq!(target.port, 8080);
    }

    #[tokio::test]
    async fn http_fetch_rejects_loopback_and_file_scheme() {
        let loopback = http_fetch("http://127.0.0.1:9/", Duration::from_millis(200), 1024)
            .await
            .expect_err("loopback must be blocked");
        assert!(
            matches!(loopback, AdapterError::Blocked { .. }),
            "{loopback:?}"
        );

        let file = http_fetch("file:///etc/passwd", Duration::from_millis(200), 1024)
            .await
            .expect_err("file scheme must be blocked");
        assert!(matches!(file, AdapterError::Blocked { .. }), "{file:?}");

        let https = http_fetch("https://example.com/", Duration::from_millis(200), 1024)
            .await
            .expect_err("https needs TLS");
        assert!(
            matches!(https, AdapterError::Unsupported { .. }),
            "{https:?}"
        );

        let zero_limit = http_fetch("http://example.com/", Duration::from_millis(200), 0)
            .await
            .expect_err("max_bytes must be > 0");
        assert!(
            matches!(zero_limit, AdapterError::Unsupported { .. }),
            "{zero_limit:?}"
        );
    }

    #[tokio::test]
    async fn read_response_enforces_max_bytes_on_content_length() {
        let (mut client, mut server) = duplex(4096);
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n";
        server
            .write_all(response.as_bytes())
            .await
            .expect("write head");
        // 故意不再写正文：content-length 超限应在读正文**之前**就报 Limit。
        let err = read_response(&mut client, 64).await.expect_err("limit");
        assert!(matches!(err, AdapterError::Limit { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn read_response_enforces_max_bytes_streaming() {
        let (mut client, mut server) = duplex(8192);
        tokio::spawn(async move {
            server
                .write_all(b"HTTP/1.1 200 OK\r\n\r\n")
                .await
                .expect("write head");
            for _ in 0..8 {
                if server.write_all(&[b'x'; 512]).await.is_err() {
                    break;
                }
            }
        });
        let err = read_response(&mut client, 1024).await.expect_err("limit");
        assert!(matches!(err, AdapterError::Limit { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn read_response_decodes_chunked_body() {
        let (mut client, mut server) = duplex(4096);
        tokio::spawn(async move {
            server
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                      4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
                )
                .await
                .expect("write chunked");
        });
        let body = read_response(&mut client, 1024)
            .await
            .expect("chunked body");
        assert_eq!(body, "Wikipedia");
    }

    #[tokio::test]
    async fn read_response_rejects_redirects() {
        let (mut client, mut server) = duplex(4096);
        tokio::spawn(async move {
            server
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\n\r\n")
                .await
                .expect("write redirect");
        });
        let err = read_response(&mut client, 1024)
            .await
            .expect_err("redirect");
        assert!(matches!(err, AdapterError::Unsupported { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn timeout_fires_on_stalled_body() {
        let (mut client, mut server) = duplex(4096);
        server
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await
            .expect("write head");
        let result =
            tokio::time::timeout(Duration::from_millis(50), read_response(&mut client, 1024)).await;
        assert!(
            result.is_err(),
            "stalled read must be cancelled by the timeout"
        );
        drop(server);
    }
}
