//! Small, tolerant HTML helpers shared by the Netscape importer and page fetching: entity
//! decoding and a tag tokenizer. Not a parser; just enough for loose real-world markup.

/// Decodes HTML entities: numeric references and the named ones that show up in titles and text
/// (the five XML ones, spaces, dashes, quotes, symbols, common Latin letters). Unknown names are
/// kept as written.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let entity = rest[1..]
            .find(';')
            .filter(|&n| n <= 10)
            .map(|n| &rest[1..n + 1]);
        let ch = entity.and_then(|e| match e.strip_prefix('#') {
            Some(num) => {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => num.parse().ok()?,
                };
                // NUL and surrogates aren't characters; browsers show U+FFFD.
                Some(
                    char::from_u32(code)
                        .filter(|&c| c != '\0')
                        .unwrap_or('\u{fffd}'),
                )
            }
            None => named(e),
        });
        match (ch, entity) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named(e: &str) -> Option<char> {
    Some(match e {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "shy" => '\u{ad}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "laquo" => '«',
        "raquo" => '»',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "rarr" => '→',
        "larr" => '←',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "aacute" => 'á',
        "eacute" => 'é',
        "iacute" => 'í',
        "oacute" => 'ó',
        "uacute" => 'ú',
        "agrave" => 'à',
        "egrave" => 'è',
        "ecirc" => 'ê',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        "Eacute" => 'É',
        _ => return None,
    })
}

/// Collapses whitespace runs (including non-breaking spaces) to single spaces and trims.
pub fn collapse_ws(s: &str) -> String {
    s.split(|c: char| c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The index of the `>` that ends the tag starting at `s[0] == '<'`, skipping quoted values.
pub fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// A start or end tag: its name and raw attributes.
pub struct Tag<'a> {
    pub name: &'a str,
    pub closing: bool,
    pub attrs: &'a str,
}

impl<'a> Tag<'a> {
    /// Parses the inside of `<...>`.
    pub fn parse(inner: &'a str) -> Self {
        let inner = inner.trim_start();
        let (closing, inner) = match inner.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, inner),
        };
        let split = inner
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(inner.len());
        Tag {
            name: &inner[..split],
            closing,
            attrs: &inner[split..],
        }
    }

    /// The raw (still escaped) value of attribute `name`, matched case-insensitively. A bare
    /// attribute has the value "".
    pub fn attr(&self, name: &str) -> Option<String> {
        let mut s = self.attrs;
        loop {
            s = s.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
            if s.is_empty() {
                return None;
            }
            let key_end = s
                .find(|c: char| c.is_ascii_whitespace() || c == '=')
                .unwrap_or(s.len());
            let key = &s[..key_end];
            s = s[key_end..].trim_start();
            let value = match s.strip_prefix('=') {
                Some(v) => {
                    let v = v.trim_start();
                    match v.chars().next() {
                        Some(q @ ('"' | '\'')) => {
                            let close = v[1..].find(q).map_or(v.len(), |i| i + 1);
                            s = v.get(close + 1..).unwrap_or("");
                            &v[1..close]
                        }
                        _ => {
                            let end = v.find(|c: char| c.is_ascii_whitespace()).unwrap_or(v.len());
                            s = &v[end..];
                            &v[..end]
                        }
                    }
                }
                None => "",
            };
            if key.eq_ignore_ascii_case(name) {
                return Some(value.to_string());
            }
        }
    }
}

/// Finds `needle` (ASCII) in `hay`, ignoring ASCII case. Returns its byte offset.
pub fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x263A; &#9731;"),
            "a & b <c> \"d\" 'e' ☺ ☃"
        );
        assert_eq!(decode("AT&T &bogus; & &#xZZ; &"), "AT&T &bogus; & &#xZZ; &");
        assert_eq!(
            decode("Caf&eacute; &mdash; &ldquo;x&rdquo;&hellip; &#0; &#xD800;"),
            "Café — “x”… \u{fffd} \u{fffd}"
        );
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(collapse_ws("  a\n\t b\u{a0}\u{a0}c  "), "a b c");
        assert_eq!(collapse_ws(" \n "), "");
    }

    #[test]
    fn parses_attributes() {
        let t = Tag::parse(r#"A HREF="x?a=1&amp;b=2" add_date=12 TAGS='a,b' FLAG"#);
        assert_eq!(t.name, "A");
        assert_eq!(t.attr("href").as_deref(), Some("x?a=1&amp;b=2"));
        assert_eq!(t.attr("ADD_DATE").as_deref(), Some("12"));
        assert_eq!(t.attr("tags").as_deref(), Some("a,b"));
        assert_eq!(t.attr("flag").as_deref(), Some(""));
        assert_eq!(t.attr("icon"), None);
        let t = Tag::parse("/DL");
        assert!(t.closing);
        assert_eq!(t.name, "DL");
    }

    #[test]
    fn finds_ignoring_case() {
        assert_eq!(find_ci("a <TiTlE>", "<title"), Some(2));
        assert_eq!(find_ci("é<title", "<title"), Some(2));
        assert_eq!(find_ci("abc", "x"), None);
    }
}
