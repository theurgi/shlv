//! Parses the search language: FTS5 syntax plus `#tag`, `tag:`, and `site:` shorthands.

/// FTS5 column names, usable as `title:word` filters.
const COLUMNS: &[&str] = &["title", "url", "note", "summary", "tags"];

#[derive(Debug, Default, PartialEq)]
pub struct Query {
    /// FTS5 MATCH expression, if any full-text terms remain after shorthands.
    pub fts: Option<String>,
    /// Tag names (normalized, aliases not yet resolved).
    pub tags: Vec<String>,
    /// Hosts; each matches itself and its subdomains.
    pub sites: Vec<String>,
}

/// Splits on whitespace, keeping `"quoted phrases"` (FTS5 escapes `"` as `""`) and treating
/// parentheses as their own tokens.
fn lex(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            '(' | ')' => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
                tokens.push(c.to_string());
            }
            '"' => {
                cur.push('"');
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            cur.push_str("\"\"");
                        }
                        Some('"') | None => break,
                        Some(c) => cur.push(c),
                    }
                }
                cur.push('"');
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// True for FTS5 barewords: letters, digits, `_`, and any non-ASCII character.
fn is_bareword(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii())
}

/// Turns a single term into FTS5 syntax with prefix matching.
fn term(t: &str) -> String {
    if t.starts_with('"') {
        // Explicit phrase: exact, as the user wrote it.
        return t.to_string();
    }
    if let Some(stem) = t.strip_suffix('*')
        && is_bareword(stem)
    {
        return t.to_string();
    }
    if is_bareword(t) {
        return format!("{t}*");
    }
    // Anything with punctuation (`rust-lang`, `c++`) becomes a quoted phrase.
    format!("\"{}\"*", t.replace('"', "\"\""))
}

pub fn parse(input: &str) -> Query {
    let mut q = Query::default();
    let mut fts = Vec::new();
    for tok in lex(input) {
        let lower = tok.to_lowercase();
        if let Some(tag) = tok.strip_prefix('#').filter(|t| !t.is_empty()) {
            q.tags.push(crate::store::normalize_tag(tag));
        } else if let Some(tag) = lower.strip_prefix("tag:").filter(|t| !t.is_empty()) {
            q.tags.push(crate::store::normalize_tag(tag));
        } else if let Some(site) = lower.strip_prefix("site:").filter(|t| !t.is_empty()) {
            let site = site.split_once("://").map_or(site, |(_, s)| s);
            let site = site.split('/').next().unwrap_or(site);
            q.sites.push(site.trim_end_matches('.').to_string());
        } else if matches!(tok.as_str(), "AND" | "OR" | "NOT" | "(" | ")") {
            fts.push(tok);
        } else if let Some((col, rest)) = tok.split_once(':')
            && COLUMNS.contains(&col.to_lowercase().as_str())
            && !rest.is_empty()
        {
            fts.push(format!("{}:{}", col.to_lowercase(), term(rest)));
        } else {
            fts.push(term(&tok));
        }
    }
    q.fts = (!fts.is_empty()).then(|| fts.join(" "));
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts(s: &str) -> Option<String> {
        parse(s).fts
    }

    #[test]
    fn bare_words_get_prefix_matching() {
        assert_eq!(fts("ru lang"), Some("ru* lang*".into()));
        assert_eq!(fts("café"), Some("café*".into()));
    }

    #[test]
    fn punctuation_becomes_phrase() {
        assert_eq!(fts("rust-lang"), Some("\"rust-lang\"*".into()));
        assert_eq!(fts("c++"), Some("\"c++\"*".into()));
    }

    #[test]
    fn phrases_operators_and_columns_pass_through() {
        assert_eq!(
            fts("\"exact phrase\" OR (a NOT b)"),
            Some("\"exact phrase\" OR ( a* NOT b* )".into())
        );
        assert_eq!(fts("Title:rust"), Some("title:rust*".into()));
        assert_eq!(fts("rust*"), Some("rust*".into()));
    }

    #[test]
    fn shorthands() {
        let q = parse("#Rust tag:web-dev site:Example.com async");
        assert_eq!(q.tags, vec!["rust", "web-dev"]);
        assert_eq!(q.sites, vec!["example.com"]);
        assert_eq!(q.fts, Some("async*".into()));
    }

    #[test]
    fn site_accepts_a_pasted_url() {
        assert_eq!(parse("site:https://docs.rs/foo").sites, vec!["docs.rs"]);
    }

    #[test]
    fn filters_only() {
        let q = parse("#rust");
        assert_eq!(q.fts, None);
        assert_eq!(q.tags, vec!["rust"]);
    }

    #[test]
    fn lone_hash_is_a_term() {
        assert_eq!(fts("#"), Some("\"#\"*".into()));
    }
}
