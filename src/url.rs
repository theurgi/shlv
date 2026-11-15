//! URL normalization for deduplication.

/// Query parameters dropped during normalization (besides any `utm_*`).
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "mc_cid", "mc_eid",
    "igshid", "_ga", "_gl", "_hsenc", "_hsmi", "mkt_tok",
];

/// Splits off a URL scheme (`https` in `https://...`), if there is one.
fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next()?.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

/// Completes user input into a full URL: `example.com/x` becomes `https://example.com/x`.
///
/// Input with `://` or a scheme that has no authority (`mailto:`, `about:`, ...) is kept.
pub fn complete(input: &str) -> String {
    let input = input.trim();
    let has_scheme = input.contains("://")
        || split_scheme(input).is_some_and(|(s, _)| {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "mailto" | "about" | "data" | "javascript" | "urn" | "magnet" | "tel" | "news"
            )
        });
    if has_scheme {
        input.to_string()
    } else {
        format!("https://{input}")
    }
}

fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        "ftp" => Some("21"),
        _ => None,
    }
}

fn is_tracking(param: &str) -> bool {
    let key = param.split_once('=').map_or(param, |(k, _)| k);
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_") || TRACKING_PARAMS.contains(&key.as_str())
}

/// Normalizes a URL for deduplication.
///
/// Lowercases the scheme and host, drops default ports, the fragment, and tracking params, and
/// sorts the remaining params by key (keeping the order of repeated keys). An empty path becomes
/// `/`. Input without a scheme is completed first (see [`complete`]).
pub fn normalize(input: &str) -> String {
    let url = complete(input);
    let url = url.split_once('#').map_or(url.as_str(), |(u, _)| u);
    let (scheme, rest) = split_scheme(url).unwrap_or(("https", url));
    let scheme = scheme.to_ascii_lowercase();

    let Some(rest) = rest.strip_prefix("//") else {
        // No authority (mailto:, urn:, ...): nothing else to normalize.
        return format!("{scheme}:{rest}");
    };

    let (before_query, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match before_query.find('/') {
        Some(i) => before_query.split_at(i),
        None => (before_query, ""),
    };

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    // The port follows the last ':' unless that ':' is inside an IPv6 literal.
    let (host, port) = match hostport.rfind(':') {
        Some(i) if !hostport[i..].contains(']') => (&hostport[..i], Some(&hostport[i + 1..])),
        _ => (hostport, None),
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    let port = port.filter(|p| !p.is_empty() && Some(*p) != default_port(&scheme));

    let mut out = format!("{scheme}://");
    if let Some(u) = userinfo {
        out.push_str(u);
        out.push('@');
    }
    out.push_str(host);
    if let Some(p) = port {
        out.push(':');
        out.push_str(p);
    }
    out.push_str(if path.is_empty() { "/" } else { path });

    if let Some(query) = query {
        let mut params: Vec<&str> = query
            .split('&')
            .filter(|p| !p.is_empty() && !is_tracking(p))
            .collect();
        params.sort_by_key(|p| p.split_once('=').map_or(*p, |(k, _)| k));
        if !params.is_empty() {
            out.push('?');
            out.push_str(&params.join("&"));
        }
    }
    out
}

/// Returns the lowercase host of a normalized URL, if it has one.
pub fn host(url_norm: &str) -> Option<&str> {
    let rest = url_norm.split_once("://")?.1;
    let authority = rest.split(['/', '?']).next()?;
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match hostport.rfind(':') {
        Some(i) if !hostport[i..].contains(']') => &hostport[..i],
        _ => hostport,
    };
    (!host.is_empty()).then_some(host)
}

/// True if the normalized URL's host is `site` or a subdomain of it.
pub fn on_site(url_norm: &str, site: &str) -> bool {
    host(url_norm).is_some_and(|h| {
        h == site || (h.ends_with(site) && h[..h.len() - site.len()].ends_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_scheme_and_host_not_path() {
        assert_eq!(
            normalize("HTTPS://Example.COM/Some/Path"),
            "https://example.com/Some/Path"
        );
    }

    #[test]
    fn drops_default_ports_only() {
        assert_eq!(
            normalize("https://example.com:443/a"),
            "https://example.com/a"
        );
        assert_eq!(normalize("http://example.com:80/a"), "http://example.com/a");
        assert_eq!(
            normalize("http://example.com:443/a"),
            "http://example.com:443/a"
        );
        assert_eq!(
            normalize("https://example.com:8443/"),
            "https://example.com:8443/"
        );
    }

    #[test]
    fn drops_fragment() {
        assert_eq!(
            normalize("https://example.com/a#section"),
            "https://example.com/a"
        );
        assert_eq!(
            normalize("https://example.com/a?x=1#y"),
            "https://example.com/a?x=1"
        );
    }

    #[test]
    fn drops_tracking_params() {
        assert_eq!(
            normalize("https://example.com/?utm_source=x&id=3&UTM_Medium=y&fbclid=z"),
            "https://example.com/?id=3"
        );
        assert_eq!(
            normalize("https://example.com/?utm_source=x"),
            "https://example.com/"
        );
    }

    #[test]
    fn sorts_params_by_key_keeping_repeats_in_order() {
        assert_eq!(
            normalize("https://example.com/?b=2&a=1&c=3"),
            "https://example.com/?a=1&b=2&c=3"
        );
        assert_eq!(
            normalize("https://example.com/?z=1&a=2&z=0"),
            "https://example.com/?a=2&z=1&z=0"
        );
        assert_eq!(
            normalize("https://example.com/?b&&a"),
            "https://example.com/?a&b"
        );
    }

    #[test]
    fn empty_path_becomes_slash() {
        assert_eq!(normalize("https://example.com"), "https://example.com/");
        assert_eq!(
            normalize("https://example.com?q=1"),
            "https://example.com/?q=1"
        );
        assert_eq!(normalize("https://example.com."), "https://example.com/");
    }

    #[test]
    fn completes_missing_scheme() {
        assert_eq!(complete("example.com/x"), "https://example.com/x");
        assert_eq!(complete("localhost:8080"), "https://localhost:8080");
        assert_eq!(complete("http://example.com"), "http://example.com");
        assert_eq!(complete("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(normalize("Example.com"), "https://example.com/");
    }

    #[test]
    fn keeps_userinfo_and_ipv6() {
        assert_eq!(
            normalize("https://User:Pw@Example.com:443/"),
            "https://User:Pw@example.com/"
        );
        assert_eq!(normalize("http://[::1]:80/x"), "http://[::1]/x");
        assert_eq!(normalize("http://[::1]:8080/x"), "http://[::1]:8080/x");
        assert_eq!(normalize("http://[::1]/x"), "http://[::1]/x");
    }

    #[test]
    fn non_hierarchical_urls() {
        assert_eq!(normalize("MAILTO:a@b.c#x"), "mailto:a@b.c");
    }

    #[test]
    fn extracts_host() {
        assert_eq!(host("https://example.com/a"), Some("example.com"));
        assert_eq!(host("https://u@example.com:8080/"), Some("example.com"));
        assert_eq!(host("http://[::1]:8080/"), Some("[::1]"));
        assert_eq!(host("mailto:a@b.c"), None);
    }

    #[test]
    fn matches_site_and_subdomains() {
        assert!(on_site("https://example.com/", "example.com"));
        assert!(on_site("https://docs.example.com/x", "example.com"));
        assert!(!on_site("https://notexample.com/", "example.com"));
        assert!(!on_site("https://evil.com/x.example.com/", "example.com"));
    }
}
