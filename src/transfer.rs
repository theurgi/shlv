//! Import and export: JSON lines (the shape of `ls --json`) and Netscape bookmark HTML (see
//! [`crate::netscape`]).

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::store::{Put, check_url, non_empty, normalize_tag, now_ms, put, reindex_many};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Json,
    Html,
}

impl Format {
    /// Netscape HTML if the text starts with its doctype (case-insensitively, after any BOM and
    /// whitespace), else JSON.
    pub fn detect(text: &str) -> Format {
        let head = text.trim_start_matches('\u{feff}').trim_start();
        match head.get(..netscape::DOCTYPE.len()) {
            Some(h) if h.eq_ignore_ascii_case(netscape::DOCTYPE) => Format::Html,
            _ => Format::Json,
        }
    }
}

/// Parses JSON lines: one object per line; blank lines are skipped. A whole-file JSON array of
/// objects is accepted too.
pub fn parse_json(text: &str) -> Result<Vec<ImportItem>> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text).context("invalid JSON array");
    }
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item = serde_json::from_str(line).with_context(|| {
            let hint = if line.trim_start().starts_with('<') {
                " (HTML without the Netscape doctype? try --format html)"
            } else {
                ""
            };
            format!("line {}: not a JSON bookmark{hint}", i + 1)
        })?;
        items.push(item);
    }
    Ok(items)
}

/// Parses `text` in `format`; see [`netscape::parse`] for `folders_as_tags`.
pub fn parse(text: &str, format: Format, folders_as_tags: bool) -> Result<Vec<ImportItem>> {
    match format {
        Format::Json => parse_json(text),
        Format::Html => Ok(netscape::parse(text, folders_as_tags)),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// Drop tags that aren't canonical or an alias of a canonical tag, instead of failing in
    /// strict mode. Applies with strict mode off too.
    pub drop_non_canonical: bool,
    /// Don't run the domain rules (see [`Store::set_rules`]) on the imported bookmarks.
    pub skip_rules: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub added: usize,
    /// Items whose URL was already bookmarked (or appeared earlier in the import).
    pub updated: usize,
    /// Items not imported: their URL and why.
    pub skipped: Vec<(String, String)>,
    /// Tags dropped by [`ImportOptions::drop_non_canonical`], in first-seen order.
    pub dropped_tags: Vec<String>,
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

/// An item that passed checks, ready to write.
struct Ready<'a> {
    item: &'a ImportItem,
    url: String,
    norm: String,
    tags: Vec<String>,
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
    /// [`NonCanonicalRuleTags`](crate::NonCanonicalRuleTags) for the rules, before anything is written, unless `opts` drops
    /// the tags or skips the rules.
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
                    ready.push(Ready {
                        item,
                        url,
                        norm,
                        tags,
                    });
                }
                Err(_) => summary
                    .skipped
                    .push((item.url.clone(), "invalid URL".into())),
            }
        }

        let mut seen = HashSet::new();
        let all: Vec<String> = ready
            .iter()
            .flat_map(|r| &r.tags)
            .filter(|t| seen.insert(t.as_str()))
            .cloned()
            .collect();
        if opts.drop_non_canonical {
            let bad: HashSet<String> = self.non_canonical(&all)?.into_iter().collect();
            if !bad.is_empty() {
                summary.dropped_tags = all.into_iter().filter(|t| bad.contains(t)).collect();
                for r in &mut ready {
                    r.tags.retain(|t| !bad.contains(t));
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

    fn write_import(
        &mut self,
        ready: &[Ready],
        rules: bool,
        summary: &mut ImportSummary,
    ) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(ready.len());
        for r in ready {
            let created_at = r.item.created_at.unwrap_or(now);
            let added = put(
                &tx,
                &Put {
                    url: &r.url,
                    norm: &r.norm,
                    title: non_empty(&r.item.title),
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
            if added.created {
                summary.added += 1;
            } else {
                summary.updated += 1;
            }
        }
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
