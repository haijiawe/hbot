//! QQ OpenAPI / 网关发现用的最小 HTTP/1.1 客户端（`http://` 与 `https://`）。
//!
//! 为什么不是 [`crate::http`]：那个模块是**给工具层抓网页**用的受限 GET（硬编码
//! `GET`、默认拦截内网/回环、不跟随 3xx）。QQ 这边要 POST + JSON + 自定义
//! `Authorization` 头，而且测试必须能指向 `127.0.0.1` 上的假服务器 —— 两者的安全
//! 口径相反，混用只会让其中一边变松。
//!
//! 但 **TLS 一定复用** [`crate::tls`]：`https://` 与本 crate 的 `wss://` 走同一份
//! rustls `ClientConfig`（公共根 + 调用方追加的根），校验始终开启，没有"跳过校验"
//! 开关。这里不新建第二套 TLS 配置。
//!
//! 刻意不做：连接池、keep-alive、gzip、3xx 跟随、HTTP/2、代理、客户端证书。
//! QQ 的这几个端点都是「一问一答的 JSON」，`Connection: close` 足够。

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::AdapterError;

/// 响应头的字节上限（正常 JSON 响应只有几百字节）。
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// 单次响应的正文上限：QQ 的 OpenAPI 响应都是小 JSON，2 MiB 已经非常宽松。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

const USER_AGENT: &str = "hbot/0.1 (qq-official)";

/// 一次 HTTP 请求的参数。
pub(crate) struct HttpRequest {
    /// 方法（`GET` / `POST`）。
    pub method: &'static str,
    /// 完整 URL（`http://` 或 `https://`）。
    pub url: String,
    /// 额外请求头（`Authorization`、`Content-Type` 等）。
    pub headers: Vec<(String, String)>,
    /// 请求体（`POST` 用）。
    pub body: Option<Vec<u8>>,
    /// 整次请求（连接 + 收完正文）的超时。
    pub timeout: Duration,
    /// 追加信任根（DER）。
    pub extra_roots: Vec<Vec<u8>>,
}

impl HttpRequest {
    /// 一个不带额外头的请求骨架。
    pub(crate) fn new(method: &'static str, url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout,
            extra_roots: Vec::new(),
        }
    }

    /// 追加一个请求头。
    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// 设置 JSON 请求体（同时补 `Content-Type`）。
    pub(crate) fn json_body(mut self, body: &Value) -> Result<Self, AdapterError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|err| AdapterError::protocol(format!("cannot encode json body: {err}")))?;
        self.body = Some(bytes);
        self.headers
            .push(("content-type".to_owned(), "application/json".to_owned()));
        Ok(self)
    }
}

/// 一次 HTTP 响应。
pub(crate) struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 原始正文。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 状态码是不是 2xx。
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 把正文解析成 JSON。
    ///
    /// 空正文当作 `Value::Null`（有些端点成功时回 204/空体）。错误信息里只有
    /// serde 的定位信息，不含正文 —— 正文可能带平台侧字段，不能进错误串。
    pub(crate) fn json(&self) -> Result<Value, AdapterError> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|err| AdapterError::protocol(format!("qq response is not valid json: {err}")))
    }
}

/// 发一次请求并读完响应。
///
/// 非 2xx **不在这里报错**：调用方要按状态码区分「token 过期（401/403，续期后重试
/// 一次）」「限流（429）」「平台故障（5xx）」，过早折叠成错误就分不出来了。
pub(crate) async fn request(req: HttpRequest) -> Result<HttpResponse, AdapterError> {
    let timeout = req.timeout;
    match tokio::time::timeout(timeout, exchange(req)).await {
        Ok(result) => result,
        Err(_) => Err(AdapterError::timeout("qq http request", timeout)),
    }
}

/// 解析后的端点。
struct Target {
    tls: bool,
    host: String,
    port: u16,
    path: String,
    host_header: String,
}

/// 把绝对 URL 拆成 `(scheme, host, port, path)`。
///
/// 只接受 `http://` / `https://`；拒绝 userinfo（`user@host`）、空主机、空 scheme。
/// 支持 `[::1]:8080` 这种 IPv6 字面量（运维配置里写成什么样就按什么样连）。
fn parse_url(url: &str) -> Result<Target, AdapterError> {
    let trimmed = url.trim();
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| {
        AdapterError::protocol("qq endpoint url is missing a scheme (expected http:// or https://)")
    })?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        other => {
            return Err(AdapterError::protocol(format!(
                "qq endpoint uses unsupported scheme `{other}`: only http:// and https:// are allowed"
            )));
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(AdapterError::protocol(
            "qq endpoint url must not contain userinfo",
        ));
    }
    let (host, port) = split_authority(authority, tls)?;
    Ok(Target {
        tls,
        path: path.to_owned(),
        host_header: authority.to_owned(),
        host,
        port,
    })
}

/// 拆 `host[:port]`，支持 `[ipv6]:port`。
fn split_authority(authority: &str, tls: bool) -> Result<(String, u16), AdapterError> {
    let default_port = if tls { 443 } else { 80 };
    if authority.is_empty() {
        return Err(AdapterError::protocol("qq endpoint url has an empty host"));
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(|| {
            AdapterError::protocol("qq endpoint url has an unterminated ipv6 host")
        })?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(text) => parse_port(text)?,
            None if after.is_empty() => default_port,
            None => {
                return Err(AdapterError::protocol(
                    "qq endpoint url has a malformed ipv6 authority",
                ));
            }
        };
        if host.is_empty() {
            return Err(AdapterError::protocol("qq endpoint url has an empty host"));
        }
        return Ok((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, text)) if !host.is_empty() => Ok((host.to_owned(), parse_port(text)?)),
        Some(_) => Err(AdapterError::protocol("qq endpoint url has an empty host")),
        None => Ok((authority.to_owned(), default_port)),
    }
}

/// 解析端口号（0 与非法数字都拒绝）。
fn parse_port(text: &str) -> Result<u16, AdapterError> {
    let port: u16 = text
        .parse()
        .map_err(|_| AdapterError::protocol("qq endpoint url has an invalid port"))?;
    if port == 0 {
        return Err(AdapterError::protocol(
            "qq endpoint url has an invalid port (0)",
        ));
    }
    Ok(port)
}

/// 连接 + 发请求 + 读响应。
async fn exchange(req: HttpRequest) -> Result<HttpResponse, AdapterError> {
    let target = parse_url(&req.url)?;
    let stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|err| AdapterError::transport(format!("qq http connect failed: {err}")))?;
    // 小 JSON 请求不需要 Nagle 攒包。
    let _ = stream.set_nodelay(true);
    let mut stream: Box<dyn IoStream> = if target.tls {
        Box::new(wrap_tls(stream, &target.host, &req.extra_roots).await?)
    } else {
        Box::new(stream)
    };
    let head = build_request_head(&req, &target);
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|err| AdapterError::transport(format!("qq http write failed: {err}")))?;
    if let Some(body) = req.body.as_deref() {
        stream
            .write_all(body)
            .await
            .map_err(|err| AdapterError::transport(format!("qq http write failed: {err}")))?;
    }
    stream
        .flush()
        .await
        .map_err(|err| AdapterError::transport(format!("qq http flush failed: {err}")))?;
    read_response(&mut stream, MAX_BODY_BYTES).await
}

/// 任意可读可写的连接（明文 TCP 或 TLS）。
trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

/// 组装请求头（不含正文）。
fn build_request_head(req: &HttpRequest, target: &Target) -> String {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: application/json\r\nConnection: close\r\n",
        req.method, target.path, target.host_header, USER_AGENT
    );
    for (name, value) in &req.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(body) = req.body.as_deref() {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    head
}

/// （`https://` 时）在已连上的 TCP 流外面套 TLS。
///
/// 与 `crate::http` 用的是**同一份**配置：公共根 + 追加根，校验始终开启。
async fn wrap_tls(
    stream: TcpStream,
    host: &str,
    extra_roots: &[Vec<u8>],
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, AdapterError> {
    let config = crate::tls::client_config(extra_roots)?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
        AdapterError::protocol("qq endpoint host cannot be used as a tls server name")
    })?;
    let connector = tokio_rustls::TlsConnector::from(config);
    connector.connect(server_name, stream).await.map_err(|err| {
        crate::tls::classify_tls_failure("qq https handshake failed", &err)
            .unwrap_or_else(|| AdapterError::tls(format!("qq https handshake failed: {err}")))
    })
}

/// 读完整响应（先头后体，正文按 `Content-Length` / chunked / 读到 EOF 三种口径）。
async fn read_response<S: AsyncRead + Unpin>(
    stream: &mut S,
    max_body: usize,
) -> Result<HttpResponse, AdapterError> {
    let (buffer, head_end) = read_head(stream).await?;
    let (status, headers) = parse_head(&buffer[..head_end])?;
    let mut reader = BodyReader::new(stream, buffer, head_end);
    let body = read_body(&mut reader, &headers, max_body).await?;
    Ok(HttpResponse { status, body })
}

/// 读到 `\r\n\r\n` 为止，返回 `(缓冲区, 头结束位置)`。
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(Vec<u8>, usize), AdapterError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(index) = find_subsequence(&buffer, b"\r\n\r\n") {
            return Ok((buffer, index + 4));
        }
        if buffer.len() >= MAX_HEADER_BYTES {
            return Err(AdapterError::limit(format!(
                "qq http response headers exceed {MAX_HEADER_BYTES} bytes"
            )));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| AdapterError::transport(format!("qq http read failed: {err}")))?;
        if read == 0 {
            return Err(AdapterError::transport(
                "qq http connection closed before the response headers were complete",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// 解析状态行与响应头，返回 `(状态码, 头表)`（头名统一小写）。
fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), AdapterError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| AdapterError::protocol("qq http response headers are not valid utf-8"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| AdapterError::protocol("qq http response is missing a status line"))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(AdapterError::protocol(format!(
            "qq http response has an unsupported status line: {status_line}"
        )));
    }
    let status: u16 = parts
        .next()
        .ok_or_else(|| AdapterError::protocol("qq http response is missing a status code"))?
        .parse()
        .map_err(|_| AdapterError::protocol("qq http response has an invalid status code"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok((status, headers))
}

/// 按头表决定怎么读正文。
async fn read_body<S: AsyncRead + Unpin>(
    reader: &mut BodyReader<'_, S>,
    headers: &[(String, String)],
    max_body: usize,
) -> Result<Vec<u8>, AdapterError> {
    if let Some(length) =
        header_value(headers, "content-length").and_then(|v| v.parse::<usize>().ok())
    {
        if length > max_body {
            return Err(AdapterError::limit(format!(
                "qq http response body declares {length} bytes, above the {max_body} byte limit"
            )));
        }
        return reader.take_exact(length).await;
    }
    if header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return reader.read_chunked(max_body).await;
    }
    reader.read_to_end(max_body).await
}

/// 取第一个匹配的头值。
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 带游标的正文读取器：缓冲区里已经有头之后读进来的字节。
struct BodyReader<'a, S> {
    stream: &'a mut S,
    buffer: Vec<u8>,
    position: usize,
}

impl<'a, S: AsyncRead + Unpin> BodyReader<'a, S> {
    fn new(stream: &'a mut S, buffer: Vec<u8>, position: usize) -> Self {
        Self {
            stream,
            buffer,
            position,
        }
    }

    fn available(&self) -> usize {
        self.buffer.len().saturating_sub(self.position)
    }

    /// 保证缓冲区里至少有 `wanted` 个未读字节。
    async fn fill_to(&mut self, wanted: usize) -> Result<(), AdapterError> {
        let mut chunk = [0u8; 4096];
        while self.available() < wanted {
            if self.position > 0 && self.position >= self.buffer.len() / 2 {
                self.buffer.drain(..self.position);
                self.position = 0;
            }
            let read =
                self.stream.read(&mut chunk).await.map_err(|err| {
                    AdapterError::transport(format!("qq http read failed: {err}"))
                })?;
            if read == 0 {
                return Err(AdapterError::transport(
                    "qq http connection closed before the response body was complete",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    /// 读走 `length` 个字节。
    async fn take_exact(&mut self, length: usize) -> Result<Vec<u8>, AdapterError> {
        self.fill_to(length).await?;
        let end = self.position + length;
        let bytes = self.buffer[self.position..end].to_vec();
        self.position = end;
        Ok(bytes)
    }

    /// 读一行（`\r\n` 结尾，返回不含 `\r\n` 的内容）。
    async fn read_line(&mut self, max: usize) -> Result<Vec<u8>, AdapterError> {
        loop {
            if let Some(index) = find_subsequence(&self.buffer[self.position..], b"\r\n") {
                let end = self.position + index;
                let line = self.buffer[self.position..end].to_vec();
                self.position = end + 2;
                return Ok(line);
            }
            if self.available() >= max {
                return Err(AdapterError::limit(format!(
                    "qq http chunk header exceeds {max} bytes"
                )));
            }
            self.fill_to(self.available() + 1).await?;
        }
    }

    /// 读到 EOF（上限 `max`）。
    async fn read_to_end(&mut self, max: usize) -> Result<Vec<u8>, AdapterError> {
        let mut chunk = [0u8; 4096];
        loop {
            let read =
                self.stream.read(&mut chunk).await.map_err(|err| {
                    AdapterError::transport(format!("qq http read failed: {err}"))
                })?;
            if read == 0 {
                break;
            }
            if self.available() + read > max {
                return Err(AdapterError::limit(format!(
                    "qq http response body exceeds the {max} byte limit"
                )));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
        let bytes = self.buffer[self.position..].to_vec();
        self.position = self.buffer.len();
        Ok(bytes)
    }

    /// 解 chunked 正文。
    async fn read_chunked(&mut self, max: usize) -> Result<Vec<u8>, AdapterError> {
        let mut body = Vec::new();
        loop {
            let line = self.read_line(1024).await?;
            let text = std::str::from_utf8(&line)
                .map_err(|_| AdapterError::protocol("qq http chunk size is not valid utf-8"))?;
            let size_text = text.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_text, 16).map_err(|_| {
                AdapterError::protocol("qq http chunk size is not a valid hex number")
            })?;
            if size == 0 {
                // 末尾可能还有 trailer，读到空行为止（忽略内容）。
                loop {
                    let trailer = self.read_line(1024).await?;
                    if trailer.is_empty() {
                        break;
                    }
                }
                break;
            }
            if body.len() + size > max {
                return Err(AdapterError::limit(format!(
                    "qq http response body exceeds the {max} byte limit"
                )));
            }
            body.extend_from_slice(&self.take_exact(size).await?);
            // 吃掉 chunk 末尾的 CRLF。
            let _ = self.take_exact(2).await?;
        }
        Ok(body)
    }
}

/// 在字节串里找子串（响应头/分块边界用，数据量都很小）。
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

    #[test]
    fn parse_url_splits_host_port_and_path() {
        let target = parse_url("https://api.sgroup.qq.com/gateway").expect("valid url");
        assert!(target.tls);
        assert_eq!(target.host, "api.sgroup.qq.com");
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/gateway");
        assert_eq!(target.host_header, "api.sgroup.qq.com");

        let target = parse_url("http://127.0.0.1:8080").expect("valid url");
        assert!(!target.tls);
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/");

        let target = parse_url("http://[::1]:9000/x").expect("valid ipv6 url");
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 9000);
        assert_eq!(target.path, "/x");
    }

    #[test]
    fn parse_url_rejects_dangerous_forms() {
        for url in [
            "ftp://example.invalid/",
            "api.sgroup.qq.com/gateway",
            "https://user:pass@example.invalid/",
            "https://:443/",
            "http://127.0.0.1:0/",
        ] {
            assert!(
                parse_url(url).is_err(),
                "expected `{url}` to be rejected as an endpoint"
            );
        }
    }

    #[test]
    fn build_request_head_has_host_and_content_length() {
        let target = parse_url("http://127.0.0.1:8080/v2/groups/g/messages").expect("valid url");
        let request = HttpRequest::new("POST", "http://127.0.0.1:8080/v2", Duration::from_secs(1))
            .header("authorization", "QQBot x")
            .json_body(&serde_json::json!({"content": "hi"}))
            .expect("json body");
        let head = build_request_head(&request, &target);
        assert!(head.starts_with("POST /v2/groups/g/messages HTTP/1.1\r\n"));
        assert!(head.contains("Host: 127.0.0.1:8080\r\n"));
        assert!(head.contains("authorization: QQBot x\r\n"));
        assert!(head.contains("content-length") || head.contains("Content-Length: 16\r\n"));
    }

    #[test]
    fn parse_head_reads_status_and_lowercases_header_names() {
        let (status, headers) =
            parse_head(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 3\r\nX-B: y\r\n\r\n")
                .expect("valid head");
        assert_eq!(status, 429);
        assert_eq!(header_value(&headers, "content-length"), Some("3"));
        assert_eq!(header_value(&headers, "x-b"), Some("y"));
    }
}
