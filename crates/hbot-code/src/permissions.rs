//! Authorization for the only networked host call: `http.get`.
//!
//! `hbot.json` carries `permissions.http: string[]` — the list of hosts a
//! project may reach (`docs/02-architecture.md` §6.1, §6.3). This module is the
//! **single** place that decides whether one URL is covered by that list; the
//! isolate never looks at the manifest itself, it calls [`authorize`].
//!
//! # What the list means
//!
//! One entry per host, compared against the URL's **host** only (lowercase, one
//! trailing `.` removed):
//!
//! | Entry | Matches | Does *not* match |
//! |---|---|---|
//! | `api.example.com` | `api.example.com` on any port | `evil-api.example.com`, `api.example.com.evil.test` |
//! | `*.example.com` | `a.example.com`, `a.b.example.com` | `example.com` (the bare domain is *not* covered), `evilexample.com` |
//! | `api.example.com:8443` | `https://api.example.com:8443/…` | the same host on 8080 |
//! | `*` | everything (warned once, see below) | — |
//!
//! * An entry **without** `:port` accepts **any** port; an entry **with**
//!   `:port` accepts only that port. The URL's port participates in the
//!   comparison, and defaults to 80/443 from the scheme when absent.
//! * `*.example.com` matches **one or more** sub-domain labels, never the bare
//!   domain: `example.com` itself must be listed separately if it is wanted.
//! * Matching is case-insensitive; comparison is exact per host, never a prefix
//!   test — `evil-api.example.com` is not covered by `api.example.com`, and
//!   `api.example.com.evil.test` is not either.
//! * An **empty** list denies everything (fail-closed). A project that wants no
//!   network writes nothing.
//! * An entry that does not parse (spaces, a path, a `?`, userinfo) is ignored:
//!   a typo must not turn into a grant.
//!
//! `*` is honoured because a project may legitimately need it, but it is a
//! manifest-level "no restriction" and therefore logged **once per process**
//! with `tracing::warn!`, so an audit trail exists that the wildcard is live.
//!
//! # Redaction
//!
//! Every failure message and log line carries the URL with its query replaced by
//! `?<redacted>` and any userinfo dropped ([`Target::display`]): a query string
//! is where API tokens end up (`?token=SECRET`), and the isolate's exception
//! text is surfaced to the model and the panel. [`hbot_core::redact::REDACTED`]
//! provides the marker so the wording matches the rest of the runtime.

use std::fmt;

use hbot_core::redact::REDACTED;

/// Machine-readable prefix for refusals: not authorized, wrong scheme, refused
/// credentials, blocked target, or a read-only sandbox.
pub const DENIED_CODE: &str = "HBOT_HTTP_DENIED";
/// Machine-readable prefix for a request that was authorized but did not
/// produce a usable response: transport failure, timeout, oversized body.
pub const FAILED_CODE: &str = "HBOT_HTTP_FAILED";

/// Same cap as the adapters' client (`crates/hbot-adapters/src/http.rs`): a URL
/// longer than this is refused instead of being handed to a resolver.
pub(crate) const MAX_URL_BYTES: usize = 8 * 1024;

/// Port implied by a scheme when the URL does not spell one out.
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;

/// Why a request was refused.
///
/// The `message` is the exact text the handler sees (it is what becomes the
/// JavaScript rejection reason), so it always starts with [`DENIED_CODE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionError {
    message: String,
}

impl PermissionError {
    /// A refusal, with `detail` appended after the machine-readable prefix.
    pub(crate) fn denied(detail: impl fmt::Display) -> Self {
        Self {
            message: format!("{DENIED_CODE}: {detail}"),
        }
    }

    /// The stable code callers and tests match on.
    pub fn code(&self) -> &'static str {
        DENIED_CODE
    }

    /// The full, already-redacted message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PermissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PermissionError {}

/// `http` or `https` — the only two schemes a handler may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

impl Scheme {
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => HTTP_PORT,
            Self::Https => HTTPS_PORT,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// A parsed `http(s)` URL, reduced to the parts authorization needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) scheme: Scheme,
    /// Lowercase, trailing `.` removed.
    pub(crate) host: String,
    /// Explicit port, or the scheme's default.
    pub(crate) port: u16,
    /// Path including the leading `/` (empty when the URL has none).
    pub(crate) path: String,
    /// Whether the URL carried a `?query` (its content is never kept).
    pub(crate) had_query: bool,
}

impl Target {
    /// `scheme://host[:port]path` with the query redacted.
    ///
    /// This is the only form of a URL allowed into an error message or a log
    /// line: credentials and query values must not survive into the model's
    /// context.
    pub(crate) fn display(&self) -> String {
        let mut out = String::with_capacity(self.host.len() + self.path.len() + 24);
        out.push_str(self.scheme.as_str());
        out.push_str("://");
        out.push_str(&self.host);
        if self.port != self.scheme.default_port() {
            out.push(':');
            out.push_str(&self.port.to_string());
        }
        out.push_str(&self.path);
        if self.had_query {
            out.push('?');
            out.push_str(REDACTED);
        }
        out
    }
}

/// Decide whether `url` is covered by `allowed` (`hbot.json` `permissions.http`).
///
/// This is the one and only authorization judgement for the network host API;
/// [`crate::http`] calls it before touching a transport.
///
/// # Errors
///
/// [`PermissionError`] with code [`DENIED_CODE`] when the list is empty, the URL
/// is not an absolute `http(s)` URL, it carries credentials, the scheme is
/// neither `http` nor `https`, or no entry covers the host/port.
pub fn authorize(allowed: &[String], url: &str) -> Result<(), PermissionError> {
    authorize_target(allowed, url).map(|_| ())
}

/// [`authorize`], also returning the parsed [`Target`] so the caller does not
/// parse the same URL twice (and so the redacted display form is available for
/// the request's later failure messages).
///
/// # Errors
///
/// As [`authorize`].
pub(crate) fn authorize_target(allowed: &[String], url: &str) -> Result<Target, PermissionError> {
    let target = parse_target(url)?;
    if allowed.is_empty() {
        return Err(PermissionError::denied(format!(
            "{} is not reachable: permissions.http is empty (default deny)",
            target.display()
        )));
    }

    for entry in allowed {
        match parse_rule(entry) {
            // A `*` entry is a manifest-level "no restriction"; make it visible
            // once per process rather than on every request.
            Some(Rule::Any) => {
                warn_wildcard_once();
                return Ok(target);
            }
            Some(rule) if rule_matches(&rule, &target) => return Ok(target),
            Some(_) => {}
            None => {
                tracing::debug!(
                    entry = %entry,
                    "ignoring an unparseable permissions.http entry"
                );
            }
        }
    }

    Err(PermissionError::denied(format!(
        "{} is not in permissions.http",
        target.display()
    )))
}

/// Parse one `http(s)` URL without a URL crate (this crate depends on
/// `hbot-core` only).
///
/// Deliberately strict: anything ambiguous (whitespace, a missing host, an
/// unparseable port, percent-encoded host, userinfo) is a refusal rather than a
/// best-effort interpretation, because a lenient parser is how a check and the
/// client end up looking at different hosts.
pub(crate) fn parse_target(url: &str) -> Result<Target, PermissionError> {
    if url.len() > MAX_URL_BYTES {
        return Err(PermissionError::denied(format!(
            "url is longer than {MAX_URL_BYTES} bytes"
        )));
    }
    if url.is_empty() {
        return Err(PermissionError::denied("url is empty"));
    }
    if url.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return Err(PermissionError::denied(
            "url contains whitespace or control characters",
        ));
    }

    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| PermissionError::denied("url must be absolute, e.g. https://host/path"))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        // `file:`, `data:` and `ftp:` are refused here rather than by the
        // transport: they are not network reads, they are local file reads.
        other => {
            return Err(PermissionError::denied(format!(
                "scheme `{other}` is not allowed; only http and https are"
            )));
        }
    };

    // The fragment never reaches the wire; dropping it also keeps `#` out of the
    // path that ends up in a message.
    let rest = rest.split('#').next().unwrap_or_default();
    let (rest, had_query) = match rest.split_once('?') {
        Some((head, _query)) => (head, true),
        None => (rest, false),
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(PermissionError::denied("url has no host"));
    }
    if authority.contains('@') {
        // The message never repeats the userinfo: it is a credential, and the
        // point of refusing is to keep it out of logs and model context.
        return Err(PermissionError::denied(
            "url carries credentials (userinfo before @); use headers instead",
        ));
    }

    let (host, port) = split_host_port(authority)
        .ok_or_else(|| PermissionError::denied("url has an unparseable host or port"))?;
    let host = normalise_host(&host)
        .ok_or_else(|| PermissionError::denied("url has an unparseable host"))?;

    Ok(Target {
        scheme,
        host,
        port: port.unwrap_or_else(|| scheme.default_port()),
        path: path.to_owned(),
        had_query,
    })
}

/// Split `host[:port]` / `[v6][:port]` without touching the case of either part.
fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail {
            "" => None,
            tail => Some(parse_port(tail.strip_prefix(':')?)?),
        };
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            Some((host.to_owned(), Some(parse_port(port)?)))
        }
        _ => Some((authority.to_owned(), None)),
    }
}

fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match text.parse::<u16>() {
        Ok(0) | Err(_) => None,
        Ok(port) => Some(port),
    }
}

/// Lowercase a host, drop trailing dots, and refuse anything that cannot be a
/// host name or address.
fn normalise_host(host: &str) -> Option<String> {
    let lowered = host.to_ascii_lowercase();
    let trimmed = lowered.trim_end_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    let valid = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'));
    valid.then(|| trimmed.to_owned())
}

/// One parsed `permissions.http` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// `*`: every host, every port. Warned once per process.
    Any,
    /// `host` or `host:port`.
    Exact { host: String, port: Option<u16> },
    /// `*.suffix` or `*.suffix:port`; the bare suffix is not covered.
    Subdomain { suffix: String, port: Option<u16> },
}

/// Parse one entry; `None` means "ignore it" (never "allow it").
fn parse_rule(entry: &str) -> Option<Rule> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if entry == "*" {
        return Some(Rule::Any);
    }
    // A path, query, fragment or userinfo in an allow-list entry is a mistake;
    // ignoring it keeps the mistake from widening the grant.
    if entry.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }

    let (host, port) = if !entry.starts_with('[') && entry.matches(':').count() > 1 {
        // A bare IPv6 literal (`::1`, `fe80::1`) has no port: splitting on the
        // last colon would read `::1` as host `::` port `1`. Bracketed form
        // (`[::1]:8080`) still goes through the splitter.
        (entry.to_owned(), None)
    } else {
        split_host_port(entry)?
    };
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }

    if let Some(suffix) = host.strip_prefix("*.") {
        // Exactly one exponent is allowed, and it must not be bare `*.`.
        if suffix.is_empty() || suffix.contains('*') {
            return None;
        }
        let suffix = normalise_host(suffix)?;
        return Some(Rule::Subdomain { suffix, port });
    }
    if host.contains('*') {
        return None;
    }
    let host = normalise_host(host)?;
    Some(Rule::Exact { host, port })
}

fn rule_matches(rule: &Rule, target: &Target) -> bool {
    match rule {
        Rule::Any => true,
        Rule::Exact { host, port } => target.host == *host && port_matches(*port, target.port),
        Rule::Subdomain { suffix, port } => {
            // `ends_with` alone would let `evilexample.com` match
            // `*.example.com`; the byte just before the suffix must be a dot,
            // and the host must be strictly longer than `.suffix` so the bare
            // domain does not match.
            let bytes = target.host.as_bytes();
            let boundary = bytes.len().checked_sub(suffix.len() + 1);
            match boundary {
                Some(index) => {
                    bytes[index] == b'.'
                        && target.host[index + 1..] == *suffix
                        && port_matches(*port, target.port)
                }
                None => false,
            }
        }
    }
}

/// An entry without a port accepts any port; with a port, only that port.
fn port_matches(allowed: Option<u16>, port: u16) -> bool {
    allowed.is_none_or(|allowed| allowed == port)
}

/// Warn exactly once per process that a `*` entry is in force.
fn warn_wildcard_once() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "hbot.json permissions.http contains \"*\": every host is reachable from this bot"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_owned()).collect()
    }

    fn denied(entries: &[&str], url: &str) -> PermissionError {
        authorize(&allowed(entries), url).expect_err("url must be refused")
    }

    #[test]
    fn an_empty_list_denies_everything() {
        let error = denied(&[], "https://api.example.com/x");
        assert_eq!(error.code(), DENIED_CODE);
        assert!(error.message().starts_with(DENIED_CODE));
        assert!(error.message().contains("default deny"));
    }

    #[test]
    fn an_exact_host_matches_only_that_host() {
        assert!(authorize(&allowed(&["api.example.com"]), "https://api.example.com/v1").is_ok());
        // A prefix test would let this through; an exact comparison must not.
        assert!(
            authorize(
                &allowed(&["api.example.com"]),
                "https://evil-api.example.com/v1"
            )
            .is_err()
        );
        // ... and the same in the other direction.
        assert!(
            authorize(
                &allowed(&["evil-api.example.com"]),
                "https://api.example.com/v1"
            )
            .is_err()
        );
        assert!(
            authorize(
                &allowed(&["api.example.com"]),
                "https://api.example.com.evil.test/"
            )
            .is_err()
        );
        // The comparison is on the host, never on the path or query.
        assert!(
            authorize(
                &allowed(&["api.example.com"]),
                "https://api.example.com.evil/"
            )
            .is_err()
        );
    }

    #[test]
    fn a_wildcard_covers_one_or_more_labels_but_not_the_bare_domain() {
        let list = allowed(&["*.example.com"]);
        assert!(authorize(&list, "https://a.example.com/").is_ok());
        assert!(authorize(&list, "https://a.b.example.com/").is_ok());
        assert!(authorize(&list, "https://deep.a.b.example.com/").is_ok());
        assert!(authorize(&list, "https://example.com/").is_err());
        assert!(authorize(&list, "https://evilexample.com/").is_err());
        assert!(authorize(&list, "https://example.com.evil.test/").is_err());
        // The list entry's own case is irrelevant.
        assert!(authorize(&allowed(&["*.Example.COM"]), "https://a.example.com/").is_ok());
    }

    #[test]
    fn an_entry_port_pins_the_port_and_its_absence_accepts_any() {
        let pinned = allowed(&["api.example.com:8080"]);
        assert!(authorize(&pinned, "http://api.example.com:8080/").is_ok());
        assert!(authorize(&pinned, "http://api.example.com:9090/").is_err());
        // Absent port on the URL means the scheme default, not "any".
        assert!(authorize(&pinned, "http://api.example.com/").is_err());

        let any_port = allowed(&["api.example.com"]);
        assert!(authorize(&any_port, "http://api.example.com:1/").is_ok());
        assert!(authorize(&any_port, "https://api.example.com:65535/").is_ok());
        assert!(authorize(&any_port, "http://api.example.com/").is_ok());
        assert!(authorize(&any_port, "http://api.example.com:0/").is_err());

        assert!(
            authorize(
                &allowed(&["api.example.com:8443"]),
                "https://api.example.com:8443/"
            )
            .is_ok()
        );
        assert!(
            authorize(
                &allowed(&["*.example.com:8443"]),
                "https://a.example.com:8443/"
            )
            .is_ok()
        );
        assert!(authorize(&allowed(&["*.example.com:8443"]), "https://a.example.com/").is_err());
    }

    #[test]
    fn only_http_and_https_are_accepted() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "data:text/plain,hi",
            "gopher://example.com/",
            "javascript:alert(1)",
        ] {
            let error = denied(&["*"], url);
            assert_eq!(error.code(), DENIED_CODE, "url {url}");
        }
    }

    #[test]
    fn credentials_in_the_url_are_refused() {
        let error = denied(&["*"], "https://user:pass@api.example.com/x");
        assert!(error.message().contains("credentials"));
        assert!(!error.message().contains("pass"));
    }

    #[test]
    fn trailing_dots_and_case_do_not_change_the_host() {
        assert!(
            authorize(
                &allowed(&["api.example.com"]),
                "https://API.Example.COM./v1"
            )
            .is_ok()
        );
        assert!(
            authorize(
                &allowed(&["api.example.com."]),
                "https://api.example.com/v1"
            )
            .is_ok()
        );
    }

    #[test]
    fn the_wildcard_entry_allows_everything() {
        assert!(authorize(&allowed(&["*"]), "https://anywhere.test:4443/x").is_ok());
        assert!(authorize(&allowed(&["*"]), "http://127.0.0.1:1/").is_ok());
    }

    #[test]
    fn unparseable_entries_grant_nothing() {
        for entry in [
            "",
            "  ",
            "api.example.com/x",
            "api.example.com?x=1",
            "a@b.test",
            "api.example.com:0",
            "*.*.example.com",
        ] {
            let error = denied(&[entry], "https://api.example.com/");
            assert_eq!(error.code(), DENIED_CODE, "entry {entry:?}");
        }
    }

    #[test]
    fn messages_redact_the_query_and_the_fragment() {
        // The host is *not* allowed, so the denial has to name the target —
        // and that name must carry `?<redacted>` rather than the query.
        let error = denied(
            &["other.example.com"],
            "https://api.example.com/v1?token=SECRET#frag",
        );
        assert!(!error.message().contains("SECRET"), "{}", error.message());
        assert!(!error.message().contains("frag"), "{}", error.message());
        assert!(
            error.message().contains("?<redacted>"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("api.example.com/v1"),
            "{}",
            error.message()
        );

        // A refused scheme must not echo the path either.
        let error = denied(&[], "httpx://api.example.com/?token=SECRET");
        assert!(!error.message().contains("SECRET"));
    }

    #[test]
    fn an_unparseable_url_never_echoes_its_text() {
        let error = denied(&[], "http:///nohost?token=SECRET");
        assert!(!error.message().contains("SECRET"), "{}", error.message());
        assert!(!error.message().contains("nohost?token"));
    }

    #[test]
    fn display_keeps_only_the_safe_parts() {
        let target = parse_target("https://api.example.com:8443/v1?token=SECRET").expect("parses");
        assert_eq!(
            target.display(),
            "https://api.example.com:8443/v1?<redacted>"
        );
        assert_eq!(target.port, 8443);
        assert!(target.had_query);

        let plain = parse_target("http://api.example.com").expect("parses");
        assert_eq!(plain.display(), "http://api.example.com");
        assert_eq!(plain.port, HTTP_PORT);
        assert!(!plain.had_query);
    }

    #[test]
    fn ipv4_and_ipv6_literals_are_compared_as_hosts() {
        assert!(authorize(&allowed(&["127.0.0.1"]), "http://127.0.0.1:3000/").is_ok());
        assert!(authorize(&allowed(&["127.0.0.1"]), "http://127.0.0.2/").is_err());
        assert!(authorize(&allowed(&["[::1]"]), "http://[::1]:3000/").is_ok());
        assert!(authorize(&allowed(&["[::1]:3000"]), "http://[::1]:3001/").is_err());
        assert!(authorize(&allowed(&["::1"]), "http://[::1]/").is_ok());
    }

    #[test]
    fn overlong_urls_are_refused_before_parsing() {
        let url = format!("https://api.example.com/{}", "a".repeat(MAX_URL_BYTES));
        let error = denied(&["api.example.com"], &url);
        assert!(error.message().contains("longer than"));
    }

    #[test]
    fn whitespace_and_control_characters_are_refused() {
        for url in [
            "https://api.example.com/a b",
            "https://api.example.com/\n",
            " https://api.example.com/",
        ] {
            assert_eq!(
                denied(&["api.example.com"], url).code(),
                DENIED_CODE,
                "url {url:?}"
            );
        }
    }
}
