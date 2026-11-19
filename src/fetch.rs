//! Fetching web pages: one bounded HTTP GET, charset handling, and the page title. AI providers
//! make their own requests.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use ureq::ResponseExt;

use crate::html::{self, Tag, find_ci, tag_end};

/// Sent with every request.
pub const USER_AGENT: &str = concat!(
    "shlv/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/theurgi/shlv)"
);

/// Bounds on one fetch.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// To establish the TCP (and TLS) connection.
    pub connect: Duration,
    /// For the whole request, redirects and body included.
    pub total: Duration,
    pub redirects: u32,
    /// Bytes of body read; the rest is ignored. Titles are near the top.
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            connect: Duration::from_secs(5),
            total: Duration::from_secs(15),
            redirects: 5,
            max_bytes: 2 << 20,
        }
    }
}

/// A fetched HTML page.
#[derive(Debug, Clone)]
pub struct Page {
    /// Where the page ended up, after redirects.
    pub url: String,
    /// The body, decoded to UTF-8.
    pub html: String,
    /// Whether the body was longer than [`Limits::max_bytes`] and cut.
    pub truncated: bool,
}

/// Why a fetch failed.
#[derive(Debug)]
pub enum FetchError {
    /// Only `http` and `https` are fetched.
    Scheme(String),
    Status(u16),
    /// The response wasn't HTML; holds its content type.
    NotHtml(String),
    Timeout,
    TooManyRedirects,
    /// Anything else: DNS, connection, TLS, protocol.
    Network(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FetchError::Scheme(s) => write!(f, "only http and https URLs can be fetched, not {s}:"),
            FetchError::Status(code) => write!(f, "HTTP {code}"),
            FetchError::NotHtml(t) => write!(f, "not an HTML page ({t})"),
            FetchError::Timeout => write!(f, "timed out"),
            FetchError::TooManyRedirects => write!(f, "too many redirects"),
            FetchError::Network(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<ureq::Error> for FetchError {
    fn from(e: ureq::Error) -> Self {
        let msg = match e {
            ureq::Error::StatusCode(code) => return FetchError::Status(code),
            ureq::Error::Timeout(_) => return FetchError::Timeout,
            ureq::Error::TooManyRedirects => return FetchError::TooManyRedirects,
            ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return FetchError::Timeout;
            }
            ureq::Error::Io(e) => e.to_string(),
            ureq::Error::HostNotFound => "host not found".into(),
            ureq::Error::ConnectionFailed => "connection failed".into(),
            e => e.to_string(),
        };
        if msg.contains("UnknownIssuer") {
            return FetchError::Network(format!(
                "{msg}: the server's certificate isn't signed by a trusted CA; if a proxy \
                 re-signs HTTPS, add its CA to the system store or point SSL_CERT_FILE at a \
                 bundle that has it"
            ));
        }
        FetchError::Network(msg)
    }
}

/// Fetches pages with fixed limits. Cheap to clone and safe to share across threads.
#[derive(Clone)]
pub struct Fetcher {
    agent: ureq::Agent,
    limits: Limits,
}

impl Default for Fetcher {
    fn default() -> Self {
        Fetcher::new(Limits::default())
    }
}

impl Fetcher {
    pub fn new(limits: Limits) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(limits.connect))
            .timeout_global(Some(limits.total))
            .max_redirects(limits.redirects)
            .user_agent(USER_AGENT)
            .accept("text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
            .tls_config(tls())
            .build()
            .new_agent();
        Fetcher { agent, limits }
    }

    /// GETs `url` and returns it if it's HTML.
    pub fn get(&self, url: &str) -> Result<Page, FetchError> {
        let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        if !lower.starts_with("http://") && !lower.starts_with("https://") {
            let scheme = url.split(':').next().unwrap_or(url);
            return Err(FetchError::Scheme(scheme.into()));
        }
        let mut resp = self.agent.get(url).call()?;
        let final_url = resp.get_uri().to_string();
        let mime = resp.body().mime_type().map(str::to_ascii_lowercase);
        let charset = resp.body().charset().map(str::to_string);
        if let Some(m) = &mime
            && m != "text/html"
            && m != "application/xhtml+xml"
        {
            return Err(FetchError::NotHtml(m.clone()));
        }
        let mut bytes = Vec::new();
        resp.body_mut()
            .as_reader()
            .take(self.limits.max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| FetchError::from(ureq::Error::from(e)))?;
        let truncated = bytes.len() as u64 > self.limits.max_bytes;
        bytes.truncate(self.limits.max_bytes as usize);
        if mime.is_none() && !looks_like_html(&bytes) {
            return Err(FetchError::NotHtml("no content type".into()));
        }
        Ok(Page {
            url: final_url,
            html: decode_body(&bytes, charset.as_deref()),
            truncated,
        })
    }
}

/// Where CA bundles usually are: the variables curl and Nix honor, then Debian/NixOS/Arch,
/// Fedora/RHEL, openSUSE, and macOS/Alpine/BSD.
const CA_FILES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// TLS settings for every request: the system's CA bundle when there is one, so a proxy's CA
/// that the system trusts works; else the bundled Mozilla roots. Read once per process.
pub(crate) fn tls() -> ureq::tls::TlsConfig {
    static ROOTS: std::sync::OnceLock<ureq::tls::RootCerts> = std::sync::OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let env = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty());
        let candidates = ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE"]
            .iter()
            .filter_map(|v| env(v))
            .chain(CA_FILES.iter().map(|s| s.to_string()));
        candidates
            .filter_map(|p| ca_bundle(Path::new(&p)))
            .next()
            .unwrap_or(ureq::tls::RootCerts::WebPki)
    });
    ureq::tls::TlsConfig::builder()
        .root_certs(roots.clone())
        .build()
}

/// The certificates in a PEM bundle, if it has any.
fn ca_bundle(path: &Path) -> Option<ureq::tls::RootCerts> {
    let pem = std::fs::read(path).ok()?;
    let certs: Vec<ureq::tls::Certificate<'static>> = ureq::tls::parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
            _ => None,
        })
        .collect();
    (!certs.is_empty()).then(|| ureq::tls::RootCerts::new_with_certs(&certs))
}
/// Sniffs a body served without a content type.
fn looks_like_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    find_ci(head, "<!doctype html").is_some() || find_ci(head, "<html").is_some()
}

/// Decodes a body to UTF-8. Handles UTF-8 and the Latin-1 family (ISO-8859-1, read as
/// windows-1252 as browsers do), which covers nearly every page; other declared charsets are
/// decoded as lossy UTF-8. An undeclared body that isn't valid UTF-8 is read as windows-1252.
/// The charset comes from the `Content-Type` header, else a `<meta>` in the first 1024 bytes.
pub fn decode_body(bytes: &[u8], header_charset: Option<&str>) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let declared = header_charset
        .map(str::to_string)
        .or_else(|| meta_charset(bytes))
        .map(|c| c.trim().to_ascii_lowercase());
    let latin1 = |c: &str| {
        matches!(
            c,
            "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1" | "windows-1252" | "cp1252"
        )
    };
    match declared.as_deref() {
        Some(c) if latin1(c) => windows_1252(bytes),
        None => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => windows_1252(bytes),
        },
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The charset from `<meta charset=...>` or `<meta http-equiv content="...; charset=...">`.
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let mut rest = &head[..];
    while let Some(i) = find_ci(rest, "<meta") {
        rest = &rest[i..];
        let end = tag_end(rest)?;
        let tag = Tag::parse(&rest[1..end]);
        rest = &rest[end..];
        if let Some(c) = tag.attr("charset") {
            return Some(c);
        }
        if let Some(content) = tag.attr("content")
            && let Some(i) = find_ci(&content, "charset=")
        {
            let c = &content[i + 8..];
            let c = c.trim_matches(|c: char| c == '"' || c == '\'');
            return Some(c.split([';', ' ']).next().unwrap_or(c).to_string());
        }
    }
    None
}

/// windows-1252, the superset of ISO-8859-1 that browsers use for both.
fn windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9f => HIGH[usize::from(b - 0x80)],
            b => char::from(b),
        })
        .collect()
}

/// Longest title kept, in characters.
const MAX_TITLE: usize = 500;

/// The page title: `<title>`, else `<meta property="og:title">`. Entities are decoded and
/// whitespace collapsed; empty titles count as none.
pub fn title(html: &str) -> Option<String> {
    title_element(html)
        .or_else(|| og_title(html))
        .map(|t| match t.char_indices().nth(MAX_TITLE) {
            Some((i, _)) => format!("{}…", &t[..i]),
            None => t,
        })
}

fn title_element(html: &str) -> Option<String> {
    let mut rest = html;
    loop {
        let i = find_ci(rest, "<title")?;
        rest = &rest[i..];
        let end = tag_end(rest)?;
        // `<title>` or `<title attr>`, not `<titles>`.
        if Tag::parse(&rest[1..end]).name.eq_ignore_ascii_case("title") {
            rest = &rest[end + 1..];
            break;
        }
        rest = &rest[end..];
    }
    let close = find_ci(rest, "</title").unwrap_or(rest.len().min(4096));
    let t = html::collapse_ws(&html::decode(&rest[..close]));
    (!t.is_empty()).then_some(t)
}

fn og_title(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(i) = find_ci(rest, "<meta") {
        rest = &rest[i..];
        let end = tag_end(rest)?;
        let tag = Tag::parse(&rest[1..end]);
        rest = &rest[end..];
        let key = tag.attr("property").or_else(|| tag.attr("name"));
        if key.is_some_and(|k| k.eq_ignore_ascii_case("og:title"))
            && let Some(c) = tag.attr("content")
        {
            let t = html::collapse_ws(&html::decode(&c));
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        let t = |h: &str| title(h);
        assert_eq!(
            t("<html><head><TITLE>\n  Rust &amp; Tokio &mdash;\tdocs </TITLE>").as_deref(),
            Some("Rust & Tokio — docs")
        );
        assert_eq!(
            t("<titles>x</titles><title lang=en>Real</title>").as_deref(),
            Some("Real")
        );
        assert_eq!(
            t(r#"<title> </title><meta content="OG &quot;x&quot;" property="og:title">"#)
                .as_deref(),
            Some("OG \"x\"")
        );
        assert_eq!(
            t(r#"<meta name="og:title" content="by name">"#).as_deref(),
            Some("by name")
        );
        assert_eq!(t("<p>no title</p>"), None);
        assert_eq!(t("<title>unclosed").as_deref(), Some("unclosed"));
        let long = format!("<title>{}</title>", "x".repeat(600));
        assert_eq!(t(&long).unwrap().chars().count(), MAX_TITLE + 1);
    }

    #[test]
    fn charsets() {
        assert_eq!(decode_body("café".as_bytes(), None), "café");
        assert_eq!(decode_body(b"caf\xe9", None), "café");
        assert_eq!(
            decode_body(b"caf\xe9 \x93q\x94", Some("ISO-8859-1")),
            "café “q”"
        );
        assert_eq!(decode_body(b"\xef\xbb\xbfBOM", None), "BOM");
        let meta = b"<meta charset=\"windows-1252\"><title>caf\xe9</title>";
        assert_eq!(title(&decode_body(meta, None)).as_deref(), Some("café"));
        let equiv = b"<meta http-equiv=Content-Type content=\"text/html; charset=iso-8859-1\">\xe9";
        assert!(decode_body(equiv, None).ends_with('é'));
        // Declared UTF-8 but broken: lossy, not Latin-1.
        assert_eq!(decode_body(b"caf\xe9", Some("utf-8")), "caf\u{fffd}");
    }

    #[test]
    fn user_agent_names_shlv() {
        assert!(USER_AGENT.starts_with(concat!("shlv/", env!("CARGO_PKG_VERSION"))));
    }
}
