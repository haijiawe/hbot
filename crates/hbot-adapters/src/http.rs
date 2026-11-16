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
//! - `https://` 走 rustls，与 `wss://`（`crate::tls`）共用同一份配置：
//!   **证书校验始终开启**，校验失败报 [`AdapterError::Tls`]，不会退回明文；
//!   私有 CA 用 [`FetchOptions::extra_roots`] 追加信任根，没有"跳过校验"的开关。
//! - 不跟随重定向：3xx 直接报错。跟随会把 SSRF 校验绕过去（跳到内网），
//!   需要的话由调用方决定是否重新抓下一个 URL。
//! - 拒绝 userinfo（`http://user:pass@host/`）、内网/回环/链路本地/保留地址、
//!   `localhost`、`*.localhost`、`*.internal`、纯数字主机（`http://2130706433/`
//!   是十进制 127.0.0.1）、IDN（非 ASCII 主机名）。
//! - 响应头 ≤ 64 KiB；正文 ≤ `max_bytes`（超了 `Limit`）；整体 ≤ `timeout`。
//!
//! 两档返回口径，共用同一段 SSRF 守卫与读响应代码（不是两份实现）：
//!
//! - [`http_fetch`] / [`http_fetch_with`] 只给正文文本：超限报 `Limit`、非 2xx 报错；
//! - [`http_fetch_full`] / [`http_fetch_full_with`] 给 [`FullResponse`]：状态码、响应头、
//!   正文字节都在，超限**截断**并置 `truncated`，4xx/5xx 原样交出（状态码本身就是
//!   调用方要的信息）。两档都不会跟随 3xx。
//!
//! 注意 DNS rebinding 仍可能绕过"解析后校验"：这里校验解析出来的**所有**地址，
//! 并且只连校验过的 `SocketAddr`（不把主机名交给连接层再解析一次）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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
    /// HTTPS over TLS（rustls，证书校验始终开启）。
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

/// 抓取策略。`Default` 就是**生产默认值**，生产路径不要改它。
///
/// 加这个结构而不是给 `http_fetch` 加参数，是为了让"生产默认严格"这件事有类型
/// 层面的位置：`http_fetch` 签名不变、永远按 `Default` 跑；需要放开时的调用点
/// 必须显式构造并写清理由。
#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// 追加信任根（DER 字节），与公共根证书**并存**（不替换）。
    ///
    /// 自签/私有 CA 的 `https://` 部署用；不提供"跳过校验"。
    pub extra_roots: Vec<Vec<u8>>,
    /// 额外放行的字面主机名（大小写不敏感，**精确匹配**，不支持通配/后缀）。
    ///
    /// 生产默认空：`127.0.0.1`、`10/8`、`localhost` 等一律拒绝。这个口子只给
    /// 测试用 —— `tests/tls_sockets.rs` 必须在**回环**上起真实 TLS 监听器，而
    /// 本机没有公网地址可绑，所以选了"可注入精确名单"而不是"放宽生产默认"：
    ///
    /// - 生产默认仍是 `[]` ⇒ 所有检查（[`check_host_name`]、[`is_blocked_ip`]、
    ///   纯数字主机、DNS 后逐地址复核）原样全跑；
    /// - 名字命中名单时，正是对该**这一个**主机名跳过上述四项内网判定（否则回环
    ///   监听器永远连不上，测试就没法证明 TLS 真的通了）。名单按字面比较，
    ///   不做后缀匹配，所以 `evil.127.0.0.1` 之类不会蹭到放行；
    /// - 这是"显式点名一个目标"，不是"关掉 SSRF 校验"的布尔开关：没有通配、
    ///   没有环境变量、生产代码路径里也没有任何地方会填它。
    pub allowed_hosts: Vec<String>,
    /// 是否放开**地址位置**判定（回环 / 内网 / 云元数据）。
    ///
    /// 默认 `false` = 生产严格口径：`localhost`、`127.0.0.1`、`10/8`、`169.254.169.254`
    /// 一律拒绝。置 `true` 时跳过 [`check_host_name`]、字面 IP 的 [`is_blocked_ip`]、
    /// 以及 [`resolve`] 里的逐地址复核。
    ///
    /// ## 为什么会有这个字段
    ///
    /// 2026-11-16 的产品决策（策略 **C**，见 `docs/02-architecture.md` §6.6）：**网络访问
    /// 范围完全由清单的 `permissions.http` 决定**，适配层不再另立一套地址黑白名单。理由：
    /// 用户要接本机 Ollama / 本机 API / 局域网 NAS，而"清单写了 `127.0.0.1:11434` 却连不上"
    /// 是不可解释的行为——两道判据各说一套，最终解释权在第一道。
    ///
    /// 因此**授权判据只有一份**，在 `hbot-code` 的 `permissions::authorize`；这个字段只
    /// 告诉适配层"上游已经判过了，不要再按地址位置二次拒绝"。
    ///
    /// ## 代价（写清，不掩饰）
    ///
    /// 清单写 `*` 时，模型生成的代码可以访问本机所有端口，以及云上的元数据地址
    /// （`169.254.169.254`、`100.100.100.200`、`metadata.google.internal`）——那是提示注入
    /// 拿机器身份凭据的标准路径。这是用户明确选择的取舍，不是遗漏。
    ///
    /// ## 仍然生效的保护（与地址位置无关）
    ///
    /// 只 `http(s)`、拒 userinfo、拒 CR/LF 注入、拒 3xx 跟随、URL/头/正文长度上限、
    /// 超时、以及歧义纯数字主机（`http://2130706433/`）的拒绝。最后一条是**解析歧义**防护
    /// 而非位置判据，故 `allow_private` 不影响它。
    pub allow_private: bool,
    /// 额外请求头，按给定顺序追加在固定头之后。
    ///
    /// 默认空，所以 [`http_fetch`] / [`http_fetch_with`] 的报文与没有这个字段时
    /// 逐字节相同。名字必须是 HTTP token，值里不许有 CR/LF（否则就是请求注入）——
    /// 由 [`build_request`] 再校验一次，**不信任调用方已校验**。
    pub headers: Vec<(String, String)>,
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

/// 一次完整响应：状态行 + 响应头 + 正文字节。
///
/// 只有 [`http_fetch_full`] / [`http_fetch_full_with`] 返回它。与 [`http_fetch`] 的
/// 三处差别都在这里能看出来：给了状态码、给了响应头、超限时是**截断**而不是报错。
///
/// 3xx 永远不会出现在 `status` 里：不跟随重定向是本模块的硬约束，先一步报
/// [`AdapterError::Unsupported`]（调用方要跟就自己重新走一次守卫）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullResponse {
    /// 状态码（2xx/4xx/5xx 原样；3xx 已被拒绝，见上）。
    pub status: u16,
    /// 响应头：名字已小写、值已 trim，保持服务端顺序（重复头各占一行）。
    pub headers: Vec<(String, String)>,
    /// 正文字节。**不做**字符集转换：调用方要文本就自己 `from_utf8_lossy`。
    pub body: Vec<u8>,
    /// 正文是否被 `max_bytes` 截断（为 `true` 时 `body.len() == max_bytes`）。
    pub truncated: bool,
}

/// 读响应的两种口径，决定"超限"和"非 2xx"怎么处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadMode {
    /// [`http_fetch`] 的老口径：超限报 [`AdapterError::Limit`]、非 2xx 报错、只交正文。
    /// 这是既有契约，行为逐字不变。
    Strict,
    /// [`http_fetch_full`] 的口径：超限截断并标 `truncated`、非 2xx 原样返回。
    Full,
}

/// 正文 + 截断标记。两个口径共用同一个返回值，差别只在 `truncated` 可不可能为真。
struct Body {
    bytes: Vec<u8>,
    truncated: bool,
}

/// 解析并校验 URL 的静态部分（不做 DNS）。
///
/// 生产路径永远走这个：等同于 `allowed_hosts` 为空、位置判定全跑。
pub fn parse_target(url: &str) -> Result<HttpTarget, AdapterError> {
    parse_target_with(url, &[], false)
}

/// 同 [`parse_target`]，但接受一份显式放行名单（见 [`FetchOptions::allowed_hosts`]）
/// 与位置判定开关（见 [`FetchOptions::allow_private`]）。
fn parse_target_with(
    url: &str,
    allowed_hosts: &[String],
    allow_private: bool,
) -> Result<HttpTarget, AdapterError> {
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
    let host_allowed = is_host_allowed(&host, allowed_hosts);
    // 位置判定（回环/内网/云元数据）在两种情况下不做：显式点名了这一个主机，
    // 或上游已按清单授权（`allow_private`）。
    let location_checked = !host_allowed && !allow_private;
    if location_checked {
        check_host_name(&host)?;

        // 字面 IP 先查一遍，省掉 DNS；域名等解析后再查全部地址。
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(AdapterError::blocked(format!(
                    "refusing to fetch a private/loopback address: {ip}"
                )));
            }
        }
    }
    // 歧义纯数字主机（`http://2130706433/`、`http://0x7f.1/`）与"是不是私网"无关：
    // 各平台解析规则不一致，**始终**拒绝。显式点名的主机维持旧行为（跳过）。
    if !host_allowed && host.parse::<IpAddr>().is_err() && is_numeric_host(&host) {
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

/// 主机名是否在显式放行名单里（**精确匹配**，大小写不敏感，不做后缀/通配）。
fn is_host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts
        .iter()
        .any(|allowed| allowed.trim().eq_ignore_ascii_case(host))
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
    http_fetch_with(url, timeout, max_bytes, &FetchOptions::default()).await
}

/// 同 [`http_fetch`]，可带 TLS 信任根与放行名单（见 [`FetchOptions`]）。
///
/// 生产调用点应当继续用 [`http_fetch`]：它等于 `FetchOptions::default()`，
/// 也就是校验最严的那一档。
pub async fn http_fetch_with(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    options: &FetchOptions,
) -> Result<String, AdapterError> {
    let response = fetch_with_mode(url, timeout, max_bytes, options, ReadMode::Strict).await?;
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

/// 抓取 URL 并返回**完整**响应（状态码 + 响应头 + 正文字节）。
///
/// 守卫与 [`http_fetch`] 是**同一段代码**（静态检查 → DNS 后逐地址复核 → 只连校验
/// 过的 `SocketAddr`），唯一的差别是读响应的口径（见 [`FullResponse`]）：
/// 正文超过 `max_bytes` 时截断并置 `truncated`，4xx/5xx 算响应。
pub async fn http_fetch_full(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<FullResponse, AdapterError> {
    http_fetch_full_with(url, timeout, max_bytes, &FetchOptions::default()).await
}

/// 同 [`http_fetch_full`]，可带 TLS 信任根、放行名单与额外请求头（见 [`FetchOptions`]）。
pub async fn http_fetch_full_with(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    options: &FetchOptions,
) -> Result<FullResponse, AdapterError> {
    fetch_with_mode(url, timeout, max_bytes, options, ReadMode::Full).await
}

/// 两档返回口径共用的入口：URL/超时/上限的校验只能有一份。
async fn fetch_with_mode(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    options: &FetchOptions,
    mode: ReadMode,
) -> Result<FullResponse, AdapterError> {
    if max_bytes == 0 {
        return Err(AdapterError::unsupported(
            "max_bytes must be greater than 0",
        ));
    }
    let target = parse_target_with(url, &options.allowed_hosts, options.allow_private)?;
    if timeout.is_zero() {
        return Err(AdapterError::timeout("http fetch", timeout));
    }
    match tokio::time::timeout(timeout, fetch_inner(&target, max_bytes, options, mode)).await {
        Ok(result) => result,
        Err(_) => Err(AdapterError::timeout("http fetch", timeout)),
    }
}

async fn fetch_inner(
    target: &HttpTarget,
    max_bytes: usize,
    options: &FetchOptions,
    mode: ReadMode,
) -> Result<FullResponse, AdapterError> {
    // 先组装请求（含额外的头校验）：参数错就不该去解析 DNS、更不该建连接。
    let request = build_request(target, &options.headers)?;
    let addresses = resolve(
        &target.host,
        target.port,
        &options.allowed_hosts,
        options.allow_private,
    )
    .await?;
    let stream = connect_any(&addresses).await?;
    // 明文与 TLS 只在"拿到哪个流"这一步不同，之后的写请求/读响应完全共用：
    // 两种 scheme 的协议行为必须一致，否则 https 会悄悄多出/少掉一些限制。
    match target.scheme {
        Scheme::Http => exchange(stream, &request, max_bytes, mode).await,
        Scheme::Https => {
            let stream = wrap_tls(stream, &target.host, &options.extra_roots).await?;
            exchange(stream, &request, max_bytes, mode).await
        }
    }
}

/// 组装请求报文：固定的五行 + 调用方额外头 + 空行。
///
/// 额外头在**这里**再校验一次（不是只靠调用方）：这是公开 API，CR/LF 注入不能
/// 靠"上游已经查过"的假设。
fn build_request(
    target: &HttpTarget,
    extra_headers: &[(String, String)],
) -> Result<String, AdapterError> {
    let host_header = if target.port == target.scheme.default_port() {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        target.path
    );
    for (name, value) in extra_headers {
        if !is_valid_header_name(name) || !is_valid_header_value(value) {
            return Err(AdapterError::unsupported(format!(
                "invalid request header `{name}`"
            )));
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    Ok(request)
}

/// 头名字必须是 RFC 9110 的 token，且不许为空。
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// 头的值：不许有 CR/LF/NUL（否则就是请求注入），其余字节原样透传。
fn is_valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte != b'\r' && byte != b'\n' && byte != 0)
}

/// 写请求 + 读响应。泛型是为了让明文 `TcpStream` 与 TLS 流走**同一段**代码。
async fn exchange<S>(
    mut stream: S,
    request: &str,
    max_bytes: usize,
    mode: ReadMode,
) -> Result<FullResponse, AdapterError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| AdapterError::transport(format!("http write failed: {err}")))?;
    stream
        .flush()
        .await
        .map_err(|err| AdapterError::transport(format!("http flush failed: {err}")))?;
    read_response(&mut stream, max_bytes, mode).await
}

/// `https://` 的 TLS 握手。
///
/// 校验策略与 `wss://`（`crate::tls`）**是同一份 `ClientConfig`**：服务端证书
/// 必须能链到公共根证书或调用方显式追加的根。失败分两类，都不是静默降级：
///
/// - 证书/主机名不匹配 → [`AdapterError::Tls`]（`crate::tls::classify_tls_failure`
///   能把 tokio-rustls 包在 `io::Error` 里的 `rustls::Error` 取出来）；
/// - 其它 IO 原因（对端 RST 等）→ `Transport`。
async fn wrap_tls(
    stream: TcpStream,
    host: &str,
    extra_roots: &[Vec<u8>],
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, AdapterError> {
    let config = crate::tls::client_config(extra_roots)?;
    // `ServerName` 支持 DNS 名与 IP 字面量（IP 走 IP SAN 校验）。
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| AdapterError::tls(format!("`{host}` is not a valid TLS server name")))?;
    tokio_rustls::TlsConnector::from(config)
        .connect(server_name, stream)
        .await
        .map_err(
            |err| match crate::tls::classify_tls_failure("https handshake failed", &err) {
                Some(tls) => tls,
                None => AdapterError::transport(format!("https handshake failed: {err}")),
            },
        )
}

/// 解析主机名并**校验所有**地址；任何一个落在内网就整体拒绝。
async fn resolve(
    host: &str,
    port: u16,
    allowed_hosts: &[String],
    allow_private: bool,
) -> Result<Vec<SocketAddr>, AdapterError> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| AdapterError::transport(format!("dns lookup failed for `{host}`: {err}")))?
        .collect();
    if addresses.is_empty() {
        return Err(AdapterError::transport(format!(
            "dns lookup returned no address for `{host}`"
        )));
    }
    // 名单命中的主机名、以及上游已按清单授权（`allow_private`）时跳过逐地址复核。
    if !is_host_allowed(host, allowed_hosts) && !allow_private {
        if let Some(blocked) = addresses.iter().find(|addr| is_blocked_ip(addr.ip())) {
            return Err(AdapterError::blocked(format!(
                "host `{host}` resolves to a private/loopback address ({})",
                blocked.ip()
            )));
        }
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
async fn read_response<S>(
    stream: &mut S,
    max_bytes: usize,
    mode: ReadMode,
) -> Result<FullResponse, AdapterError>
where
    S: AsyncRead + Unpin,
{
    let mut buffer: Vec<u8> = Vec::with_capacity(8192);
    loop {
        if let Some(head_end) = find_subsequence(&buffer, b"\r\n\r\n") {
            let (status, headers) = parse_head(&buffer[..head_end])?;
            let body_head = buffer[head_end + 4..].to_vec();
            let body =
                finish_response(stream, status, &headers, body_head, max_bytes, mode).await?;
            return Ok(FullResponse {
                status,
                headers,
                body: body.bytes,
                truncated: body.truncated,
            });
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
    mode: ReadMode,
) -> Result<Body, AdapterError>
where
    S: AsyncRead + Unpin,
{
    if (300..400).contains(&status) {
        let location = header(headers, "location").unwrap_or("");
        return Err(AdapterError::unsupported(format!(
            "http redirect ({status}) is not followed (target: {location:?})"
        )));
    }
    if !(200..300).contains(&status) && mode == ReadMode::Strict {
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

    if chunked {
        read_chunked(stream, body_head, max_bytes, mode).await
    } else if let Some(length) = content_length {
        read_fixed(stream, body_head, length, max_bytes, mode).await
    } else {
        read_until_eof(stream, body_head, max_bytes, mode).await
    }
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
///
/// `Strict` 口径在**读正文之前**就按 `Content-Length` 判超限（既有契约）；
/// `Full` 口径只读到 `max_bytes` 就停手，并标 `truncated`。
async fn read_fixed<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    length: usize,
    max_bytes: usize,
    mode: ReadMode,
) -> Result<Body, AdapterError>
where
    S: AsyncRead + Unpin,
{
    let truncated = length > max_bytes;
    let keep = if truncated {
        match mode {
            ReadMode::Strict => {
                return Err(AdapterError::limit(format!(
                    "http body declares {length} bytes, exceeding max_bytes {max_bytes}"
                )));
            }
            ReadMode::Full => max_bytes,
        }
    } else {
        length
    };
    if body.len() > keep {
        body.truncate(keep);
    }
    while body.len() < keep {
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
        let remaining = keep - body.len();
        body.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(Body {
        bytes: body,
        truncated,
    })
}

/// 读到 EOF（HTTP/1.0 风格或 `Connection: close`）。
async fn read_until_eof<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    max_bytes: usize,
    mode: ReadMode,
) -> Result<Body, AdapterError>
where
    S: AsyncRead + Unpin,
{
    if body.len() > max_bytes {
        match mode {
            ReadMode::Strict => {
                return Err(AdapterError::limit(format!(
                    "http body exceeds max_bytes {max_bytes}"
                )));
            }
            ReadMode::Full => {
                body.truncate(max_bytes);
                return Ok(Body {
                    bytes: body,
                    truncated: true,
                });
            }
        }
    }
    loop {
        let mut chunk = [0u8; 8192];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| AdapterError::transport(format!("http read failed: {err}")))?;
        if read == 0 {
            return Ok(Body {
                bytes: body,
                truncated: false,
            });
        }
        let room = max_bytes.saturating_sub(body.len());
        if read > room {
            match mode {
                ReadMode::Strict => {
                    return Err(AdapterError::limit(format!(
                        "http body exceeds max_bytes {max_bytes}"
                    )));
                }
                ReadMode::Full => {
                    body.extend_from_slice(&chunk[..room]);
                    return Ok(Body {
                        bytes: body,
                        truncated: true,
                    });
                }
            }
        }
        body.extend_from_slice(&chunk[..read]);
    }
}

/// chunked 正文（`Transfer-Encoding: chunked`）。
async fn read_chunked<S>(
    stream: &mut S,
    mut buffer: Vec<u8>,
    max_bytes: usize,
    mode: ReadMode,
) -> Result<Body, AdapterError>
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
            return Ok(Body {
                bytes: body,
                truncated: false,
            });
        }
        if body.len() + size > max_bytes {
            match mode {
                ReadMode::Strict => {
                    return Err(AdapterError::limit(format!(
                        "http body exceeds max_bytes {max_bytes}"
                    )));
                }
                ReadMode::Full => {
                    // 只取这一块里装得下的前 `keep` 字节，剩下的不读也不留。
                    let keep = max_bytes - body.len();
                    while buffer.len() < cursor + keep {
                        if !read_more(stream, &mut buffer).await? {
                            return Err(AdapterError::transport(
                                "connection closed inside a chunk",
                            ));
                        }
                    }
                    body.extend_from_slice(&buffer[cursor..cursor + keep]);
                    return Ok(Body {
                        bytes: body,
                        truncated: true,
                    });
                }
            }
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

    #[test]
    fn location_guards_stay_on_by_default() {
        // 生产默认口径没变：没有 `allow_private` 时私网/回环/云元数据照样拒。
        for url in [
            "http://127.0.0.1:11434/api",
            "http://10.1.2.3/",
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:8080/",
            "http://metadata.google.internal/",
        ] {
            let error = parse_target_with(url, &[], false)
                .err()
                .unwrap_or_else(|| panic!("{url} must stay blocked by default"));
            assert!(
                matches!(error, AdapterError::Blocked { .. }),
                "{url} → {error:?}"
            );
        }
    }

    #[test]
    fn allow_private_lets_authorized_private_targets_through() {
        // 策略 C：地址位置不再拦，清单判据在上游（hbot-code）。
        for (url, host, port) in [
            ("http://127.0.0.1:11434/api/chat", "127.0.0.1", 11434u16),
            ("http://10.1.2.3/x", "10.1.2.3", 80),
            (
                "http://169.254.169.254/latest/meta-data/",
                "169.254.169.254",
                80,
            ),
            ("http://localhost:8080/", "localhost", 8080),
            (
                "http://metadata.google.internal/",
                "metadata.google.internal",
                80,
            ),
            ("http://[::1]:9000/x", "::1", 9000),
        ] {
            let target = parse_target_with(url, &[], true)
                .unwrap_or_else(|error| panic!("{url} must be allowed: {error:?}"));
            assert_eq!(target.host, host, "{url}");
            assert_eq!(target.port, port, "{url}");
        }
    }

    #[test]
    fn allow_private_does_not_weaken_protocol_guards() {
        // 放开的是"地址在哪"，不是"请求合法不合法"。
        for url in [
            "file:///etc/passwd",
            "http://user:pass@127.0.0.1/",
            "http://2130706433/",
            "http://0x7f.1/",
        ] {
            let error = parse_target_with(url, &[], true)
                .err()
                .unwrap_or_else(|| panic!("{url} must stay rejected under policy C"));
            assert!(
                matches!(error, AdapterError::Blocked { .. }),
                "{url} → {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn allow_private_really_connects_to_a_loopback_listener() {
        // 端到端：这条路径同时走过 `parse_target_with` 与 `resolve` 的逐地址复核。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("local addr");
        let serving = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nprivate")
                .await
                .expect("write response");
            socket.flush().await.expect("flush");
        });

        let options = FetchOptions {
            allow_private: true,
            ..FetchOptions::default()
        };
        let response = http_fetch_full_with(
            &format!("http://{address}/x"),
            Duration::from_secs(5),
            1024,
            &options,
        )
        .await
        .expect("loopback fetch under policy C");

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"private");
        serving.await.expect("server task");
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

        // `https://` 不再报"缺 TLS 后端"：它现在真的会去握手。这里只断言
        // 静态守卫仍然先把它拦住（不联网，也不依赖 TLS 实现细节）。
        let https_loopback = http_fetch("https://127.0.0.1:9/", Duration::from_millis(200), 1024)
            .await
            .expect_err("https loopback must still be blocked");
        assert!(
            matches!(https_loopback, AdapterError::Blocked { .. }),
            "{https_loopback:?}"
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
        let err = read_response(&mut client, 64, ReadMode::Strict)
            .await
            .expect_err("limit");
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
        let err = read_response(&mut client, 1024, ReadMode::Strict)
            .await
            .expect_err("limit");
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
        let response = read_response(&mut client, 1024, ReadMode::Strict)
            .await
            .expect("chunked body");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"Wikipedia");
        assert!(!response.truncated);
    }

    /// 同一个 chunked 响应在 `Full` 口径下：截断到 `max_bytes` 并标 `truncated`，
    /// 不报错。这是给 `http.get` 用的那一档。
    #[tokio::test]
    async fn read_response_full_truncates_chunked_body() {
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
        let response = read_response(&mut client, 6, ReadMode::Full)
            .await
            .expect("truncated chunked body");
        assert_eq!(response.body, b"Wikipe");
        assert!(response.truncated);
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
        let err = read_response(&mut client, 1024, ReadMode::Strict)
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
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            read_response(&mut client, 1024, ReadMode::Strict),
        )
        .await;
        assert!(
            result.is_err(),
            "stalled read must be cancelled by the timeout"
        );
        drop(server);
    }
}
