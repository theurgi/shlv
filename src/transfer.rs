//! Import and export: JSON lines (the shape of `ls --json`), Netscape bookmark HTML (see
//! [`crate::netscape`]), and, for import only, plain lists of URLs.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};

use crate::proposal::{Diverted, ProposalSource, StrictTags, divert};
use crate::store::{
    Put, check_url, non_empty, normalize_tag, now_ms, put, reindex_many, styled_name,
};
use crate::suggest::{RuleStats, offer_rules_many};
use crate::tag::NonCanonicalTags;
use crate::{Store, netscape};

/// A bookmark to import. JSON import reads the fields `ls --json` writes; others (such as `id`)
/// are ignored.
#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ImportItem {
    pub url: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub summary: Option<String>,
    /// Raw tag names; normalized on import, empty ones dropped.
    pub tags: Vec<String>,
    /// Unix milliseconds; now if absent.
    pub created_at: Option<i64>,
    /// Unix milliseconds; for a new bookmark only (an updated one gets now). Defaults to
    /// `created_at`'s value, or now.
    pub updated_at: Option<i64>,
    /// Kept for a new bookmark if it's a valid UUID no bookmark in the database has.
    pub uid: Option<String>,
}

impl ImportItem {
    /// Keeps only the URL, the add date, and, with `keep_title`, the title: for moving from
    /// another app without its tags, notes, and summaries (`import --urls-only`).
    pub fn urls_only(&mut self, keep_title: bool) {
        *self = ImportItem {
            url: std::mem::take(&mut self.url),
            title: self.title.take().filter(|_| keep_title),
            created_at: self.created_at,
            ..Default::default()
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Json,
    Html,
    /// One URL per line; see [`parse_urls`].
    Urls,
}

impl Format {
    /// Netscape HTML if the text starts with its doctype (case-insensitively, after any BOM and
    /// whitespace). Otherwise JSON if the first line that isn't blank or a `#` comment starts with
    /// `{`, `[`, or `<` (HTML without the doctype then gets JSON's hint), else a URL list.
    pub fn detect(text: &str) -> Format {
        let text = text.trim_start_matches('\u{feff}');
        let head = text.trim_start();
        if head
            .get(..netscape::DOCTYPE.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(netscape::DOCTYPE))
        {
            return Format::Html;
        }
        match url_lines(text).next() {
            Some((_, line)) if !line.starts_with(['{', '[', '<']) => Format::Urls,
            _ => Format::Json,
        }
    }
}

/// Parses JSON lines: one object per line; blank lines are skipped. A whole-file JSON array of
/// objects is accepted too. An object without a `url` fails, such as another app's export that
/// wraps its bookmarks in one object.
pub fn parse_json(text: &str) -> Result<Vec<ImportItem>> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('[') {
        let items: Vec<ImportItem> = serde_json::from_str(text).context("invalid JSON array")?;
        if let Some(i) = items.iter().position(|item| item.url.is_empty()) {
            bail!("item {}: no \"url\"", i + 1);
        }
        return Ok(items);
    }
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item: ImportItem = serde_json::from_str(line).with_context(|| {
            let hint = if line.trim_start().starts_with('<') {
                " (HTML without the Netscape doctype? try --format html)"
            } else {
                ""
            };
            format!("line {}: not a JSON bookmark{hint}", i + 1)
        })?;
        if item.url.is_empty() {
            bail!("line {}: no \"url\"", i + 1);
        }
        items.push(item);
    }
    Ok(items)
}

/// The lines of a URL list that aren't blank or `#` comments, trimmed, with 1-based numbers.
fn url_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(i, line)| (i + 1, line))
}

/// Parses a URL list: one URL per line, in any form `add` takes. Blank lines and lines starting
/// with `#` are skipped. A line with whitespace inside fails, so another kind of text file isn't
/// read as a list of broken URLs.
pub fn parse_urls(text: &str) -> Result<Vec<ImportItem>> {
    url_lines(text.trim_start_matches('\u{feff}'))
        .map(|(n, line)| {
            if line.contains(char::is_whitespace) {
                let shown: String = line.chars().take(60).collect();
                bail!("line {n}: not a URL: {shown:?}");
            }
            Ok(ImportItem {
                url: line.into(),
                ..Default::default()
            })
        })
        .collect()
}

/// Parses `text` in `format`; see [`netscape::parse`] for `folders_as_tags`.
pub fn parse(text: &str, format: Format, folders_as_tags: bool) -> Result<Vec<ImportItem>> {
    match format {
        Format::Json => parse_json(text),
        Format::Html => Ok(netscape::parse(text, folders_as_tags)),
        Format::Urls => parse_urls(text),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// Drop tags that aren't canonical or an alias of a canonical tag, instead of failing in
    /// strict mode. Applies with strict mode off too.
    pub drop_non_canonical: bool,
    /// Don't run the domain rules (see [`Store::set_rules`]) on the imported bookmarks.
    pub skip_rules: bool,
    /// Keep tags that aren't canonical as proposals for their bookmarks (source `import`)
    /// instead of tags. Always on with `strict_tags = "propose"`.
    pub propose_unknown: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub added: usize,
    /// Items whose URL was already bookmarked (or appeared earlier in the import).
    pub updated: usize,
    /// Items not imported: their URL and why.
    pub skipped: Vec<(String, String)>,
    /// Tags dropped by [`ImportOptions::drop_non_canonical`], or because they were rejected as
    /// proposals, in first-seen order.
    pub dropped_tags: Vec<String>,
    /// Tags kept as proposals ([`ImportOptions::propose_unknown`]), in first-seen order.
    pub proposed: Vec<String>,
    /// Tags the tag style refused (and dropped), each with the reason, in first-seen order.
    pub refused_tags: Vec<String>,
    /// What the domain rules did; `checked` counts distinct bookmarks.
    pub rules: RuleStats,
}

/// URL schemes that only mean something inside a browser.
fn browser_only(url: &str) -> Option<&'static str> {
    let scheme = url.split_once(':')?.0.trim().to_ascii_lowercase();
    match scheme.as_str() {
        "javascript" => Some("javascript: URL"),
        "place" => Some("place: URL"),
        _ => None,
    }
}

/// Whether `title` is just the bookmark's URL, as exporters write for untitled bookmarks (Karakeep
/// does). Such a title says nothing and would keep `fetch --all` from finding a real one.
fn is_url_of(title: &str, norm: &str) -> bool {
    !title.contains(char::is_whitespace) && crate::url::normalize(title) == norm
}

/// An item that passed checks, ready to write.
struct Ready<'a> {
    item: &'a ImportItem,
    url: String,
    norm: String,
    /// The item's title, unless it's empty or only the URL.
    title: Option<&'a str>,
    tags: Vec<String>,
    /// Tags kept as proposals instead.
    proposed: Vec<String>,
}

impl Store {
    /// Imports bookmarks in one transaction, with `add` semantics for URLs already bookmarked
    /// (see [`Store::add`]; the earlier `created_at` also wins). `javascript:` and `place:` URLs
    /// and invalid ones are skipped and reported.
    ///
    /// Matching domain rules are recorded as suggestions or applied in the same transaction, as
    /// in [`Store::add`], unless `opts` skips them. Tags they apply don't bump `updated_at`: like
    /// the import's own tags, they're part of the write that brings the bookmark in.
    ///
    /// In strict mode, fails with [`NonCanonicalTags`] listing every offending tag, or with
    /// [`NonCanonicalRuleTags`](crate::NonCanonicalRuleTags) for the rules, before anything is
    /// written, unless `opts` drops the tags or skips the rules. Tags that aren't canonical become
    /// proposals for their bookmarks instead with [`ImportOptions::propose_unknown`] or
    /// `strict_tags = "propose"` (see [`crate::proposal`]).
    pub fn import(&mut self, items: &[ImportItem], opts: ImportOptions) -> Result<ImportSummary> {
        let mut summary = ImportSummary::default();
        let mut ready = Vec::with_capacity(items.len());
        for item in items {
            if let Some(why) = browser_only(&item.url) {
                summary.skipped.push((item.url.clone(), why.into()));
                continue;
            }
            match check_url(&item.url) {
                Ok((url, norm)) => {
                    let tags = item
                        .tags
                        .iter()
                        .map(|t| normalize_tag(t))
                        .filter(|t| !t.is_empty())
                        .collect();
                    let title = non_empty(&item.title).filter(|t| !is_url_of(t, &norm));
                    ready.push(Ready {
                        item,
                        title,
                        url,
                        norm,
                        tags,
                        proposed: Vec::new(),
                    });
                }
                Err(_) => summary
                    .skipped
                    .push((item.url.clone(), "invalid URL".into())),
            }
        }

        if !self.style.is_default() {
            self.style_import(&mut ready, &mut summary)?;
        }
        let mut seen = HashSet::new();
        let all: Vec<String> = ready
            .iter()
            .flat_map(|r| &r.tags)
            .filter(|t| seen.insert(t.as_str()))
            .cloned()
            .collect();
        let propose = opts.propose_unknown || self.strict_mode() == StrictTags::Propose;
        if opts.drop_non_canonical || propose {
            let bad: HashSet<String> = self.non_canonical(&all)?.into_iter().collect();
            if !bad.is_empty() {
                if !propose {
                    summary.dropped_tags = all.into_iter().filter(|t| bad.contains(t)).collect();
                }
                for r in &mut ready {
                    let (keep, off) = std::mem::take(&mut r.tags)
                        .into_iter()
                        .partition(|t| !bad.contains(t));
                    r.tags = keep;
                    if propose {
                        r.proposed = off;
                    }
                }
            }
        } else if self.strict_tags() {
            let bad = self.non_canonical(&all)?;
            if !bad.is_empty() {
                return Err(NonCanonicalTags(bad).into());
            }
        }
        let rules = !opts.skip_rules && !self.rules.is_empty();
        if rules {
            self.check_rules()?;
        }

        // At 100k bookmarks on the VM's virtiofs: 13.3 s with the default cache, 5.0 s with the
        // bulk cache (and 4.7 s with 128 MiB).
        self.with_bulk_cache(|s| s.write_import(&ready, rules, &mut summary))?;
        Ok(summary)
    }

    /// Applies the tag style to the items' (normalized) tags: fixed where they can be, dropped
    /// and reported where they can't. Names of existing tags and aliases are kept as they are.
    fn style_import(&self, ready: &mut [Ready], summary: &mut ImportSummary) -> Result<()> {
        let mut styled: HashMap<String, Option<String>> = HashMap::new();
        for r in ready.iter_mut() {
            let mut tags = Vec::with_capacity(r.tags.len());
            for t in std::mem::take(&mut r.tags) {
                if !styled.contains_key(&t) {
                    let s = match styled_name(&self.conn, &self.style, &t) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            let why = e.to_string();
                            if !summary.refused_tags.contains(&why) {
                                summary.refused_tags.push(why);
                            }
                            None
                        }
                    };
                    styled.insert(t.clone(), s);
                }
                if let Some(s) = &styled[&t]
                    && !tags.contains(s)
                {
                    tags.push(s.clone());
                }
            }
            r.tags = tags;
        }
        Ok(())
    }

    fn write_import(
        &mut self,
        ready: &[Ready],
        rules: bool,
        summary: &mut ImportSummary,
    ) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(ready.len());
        let mut diverted = Diverted::default();
        for r in ready {
            let created_at = r.item.created_at.unwrap_or(now);
            let added = put(
                &tx,
                &Put {
                    url: &r.url,
                    norm: &r.norm,
                    title: r.title,
                    note: non_empty(&r.item.note),
                    summary: non_empty(&r.item.summary),
                    tags: &r.tags,
                    uid: r.item.uid.as_deref(),
                    created_at,
                    updated_at: r.item.updated_at.unwrap_or(created_at).max(created_at),
                },
                now,
            )
            .with_context(|| format!("cannot import {}", r.url))?;
            ids.push(added.id);
            divert(
                &tx,
                &r.proposed,
                ProposalSource::Import,
                added.id,
                now,
                &mut diverted,
            )?;
            if added.created {
                summary.added += 1;
            } else {
                summary.updated += 1;
            }
        }
        summary.proposed = diverted.proposed;
        summary.dropped_tags.extend(diverted.rejected);
        if rules {
            // Before reindexing, so applied tags are searchable; the reindex covers every id.
            let mut seen = HashSet::new();
            let targets = ids
                .iter()
                .zip(ready)
                .filter(|(id, _)| seen.insert(**id))
                .map(|(id, r)| (*id, r.norm.as_str()));
            (summary.rules, _) =
                offer_rules_many(&tx, &self.rules, self.suggest_mode, targets, now)?;
        }
        // Set-based, once: per-row reindexing made a 100k import 2.5× slower, and a re-import
        // (every row an update) 8× slower.
        reindex_many(&tx, &ids)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_only_keeps_url_date_and_maybe_title() {
        let full = ImportItem {
            url: "https://a.example/".into(),
            title: Some("A".into()),
            note: Some("n".into()),
            summary: Some("s".into()),
            tags: vec!["x".into()],
            created_at: Some(1),
            updated_at: Some(2),
            uid: Some("u".into()),
        };
        let mut item = full.clone();
        item.urls_only(true);
        assert_eq!(
            item,
            ImportItem {
                url: full.url.clone(),
                title: full.title.clone(),
                created_at: Some(1),
                ..Default::default()
            }
        );
        item.urls_only(false);
        assert_eq!(item.title, None);
    }

    #[test]
    fn json_needs_urls() {
        let items = parse_json("{\"url\": \"https://a.example/\"}\n\n{\"url\": \"b\"}").unwrap();
        assert_eq!(items.len(), 2);
        // Another app's export in one object.
        let err = parse_json("{\"bookmarks\": [{\"content\": {}}]}").unwrap_err();
        assert_eq!(err.to_string(), "line 1: no \"url\"");
        let err = parse_json("[{\"url\": \"a\"}, {\"title\": \"t\"}]").unwrap_err();
        assert_eq!(err.to_string(), "item 2: no \"url\"");
    }

    #[test]
    fn detect_formats() {
        let html = "\u{feff}\n<!doctype NETSCAPE-Bookmark-file-1>\n";
        assert_eq!(Format::detect(html), Format::Html);
        for json in [
            "{\"url\": \"a\"}",
            " [",
            "<DL><p>",
            "# list\n\n{\"url\": \"a\"}",
            "",
        ] {
            assert_eq!(Format::detect(json), Format::Json, "{json:?}");
        }
        for urls in [
            "https://a.example/",
            "\u{feff}# saved\r\n\r\nlwn.net\r\n",
            "a b",
        ] {
            assert_eq!(Format::detect(urls), Format::Urls, "{urls:?}");
        }
    }

    #[test]
    fn url_lists() {
        let text =
            "\u{feff}# from Karakeep\r\n\r\n  https://a.example/x  \r\n#https://no/\r\nlwn.net\n";
        let urls: Vec<_> = parse_urls(text)
            .unwrap()
            .into_iter()
            .map(|i| i.url)
            .collect();
        assert_eq!(urls, ["https://a.example/x", "lwn.net"]);
        let err = parse_urls("a.example\n\nhttps://b.example/ Some title\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "line 3: not a URL: \"https://b.example/ Some title\""
        );
        assert!(parse_urls("# only comments\n").unwrap().is_empty());
    }

    #[test]
    fn titles_that_are_the_url() {
        let norm = crate::url::normalize("https://lwn.net/");
        assert!(is_url_of("https://lwn.net/", &norm));
        assert!(is_url_of("lwn.net", &norm));
        assert!(!is_url_of(
            "LWN.net",
            &crate::url::normalize("https://lwn.net/a")
        ));
        assert!(!is_url_of("lwn net", &norm));
    }
}
