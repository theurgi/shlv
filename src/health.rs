//! Tag health (task 040): measurements that help keep the canonical list small while still
//! describing the bookmarks. Advisory only: nothing here writes to the database.
//!
//! One pass over `bookmark_tag` gives per-tag counts and co-occurring pairs (redundant and too
//! broad tags, coverage); one query gives the bookmarks the list doesn't describe (gaps), which
//! are grouped by host and by shared title words. Style checks run on tag names alone.

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use crate::choose::Item;
use crate::store::Store;

/// Thresholds for [`Store::health`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthOptions {
    /// A pair is redundant when each tag is on at least this share of the other's bookmarks.
    pub redundant: f64,
    /// ...and they are together on at least this many bookmarks.
    pub min_together: usize,
    /// A tag is too broad on at least this share of all bookmarks.
    pub broad: f64,
    /// A canonical tag is rare on at most this many bookmarks.
    pub rare: usize,
    /// Smallest gap group reported.
    pub min_group: usize,
    /// Most gap groups reported per kind (host, word).
    pub groups: usize,
}

impl Default for HealthOptions {
    fn default() -> Self {
        HealthOptions {
            redundant: 0.8,
            min_together: 3,
            broad: 0.3,
            rare: 2,
            min_group: 3,
            groups: 10,
        }
    }
}

/// The report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Health {
    pub coverage: Coverage,
    pub redundant: Vec<Redundant>,
    pub broad: Vec<Broad>,
    pub rare: Vec<Rare>,
    pub gaps: Gaps,
    pub style: Vec<StyleIssue>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Coverage {
    /// Live bookmarks.
    pub bookmarks: usize,
    /// With at least one tag.
    pub tagged: usize,
    /// With at least one canonical tag.
    pub canonical_tagged: usize,
    /// Live tags, and how many of them are canonical.
    pub tags: usize,
    pub canonical: usize,
    /// Mean tags per tagged bookmark.
    pub tags_per_tagged: f64,
    /// Bookmarks by number of tags: index 0..=4, the last meaning 4 or more.
    pub histogram: [usize; 5],
}

/// Two tags that nearly always come together: a merge candidate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Redundant {
    /// The tag on fewer bookmarks (merge it into `b`).
    pub a: String,
    pub b: String,
    pub a_count: usize,
    pub b_count: usize,
    /// Bookmarks with both.
    pub together: usize,
    /// together / bookmarks with either.
    pub jaccard: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Broad {
    pub tag: String,
    pub count: usize,
    /// Of all bookmarks.
    pub share: f64,
}

/// A canonical tag on few bookmarks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rare {
    pub tag: String,
    pub count: usize,
    /// When the tag was created (Unix milliseconds, from its UUIDv7 uid), if known.
    pub created_at: Option<i64>,
    /// Declared in config: remove it there, not with `tag canonical rm`.
    pub pinned: bool,
}

/// Bookmarks the list doesn't describe: without a canonical tag (without any tag when there is
/// no canonical list).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Gaps {
    pub bookmarks: usize,
    /// Of those, how many the classifier could tag (a tag at or above the offer threshold);
    /// `None` when it didn't run. The groups leave these out.
    pub placeable: Option<usize>,
    pub by_host: Vec<Group>,
    pub by_word: Vec<Group>,
}

/// Gap bookmarks sharing a host or a title word.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    pub key: String,
    pub ids: Vec<i64>,
    /// A few titles (or URLs, for untitled bookmarks).
    pub examples: Vec<String>,
    /// The tag (or alias's tag) named like `key`, for word groups: the bookmarks may only need
    /// tagging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing: Option<String>,
    /// A tag name for the group, from the AI provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named: Option<GroupName>,
}

/// What the provider said fits a gap group.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupName {
    pub name: String,
    pub description: Option<String>,
    /// Already a canonical tag (the group only needs tagging).
    pub listed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StyleKind {
    /// Breaks `[tags.style]`.
    Style,
    /// Singular and plural both present.
    Plural,
    /// A compound of other tags.
    Compound,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StyleIssue {
    pub tag: String,
    pub kind: StyleKind,
    /// The other tags involved (the singular; the compound's parts).
    pub related: Vec<String>,
    pub message: String,
}

struct TagRow {
    name: String,
    canonical: bool,
    uid: String,
}

const STOP: &[&str] = &[
    "the", "and", "for", "with", "you", "your", "from", "how", "what", "why", "when", "who",
    "this", "that", "are", "was", "has", "have", "not", "but", "all", "can", "use", "using",
    "about", "into", "our", "its", "will", "more", "one", "out", "get", "new", "via", "www", "com",
    "org", "net", "http", "https", "html", "htm", "php", "index",
];

/// Lowercase title words worth grouping by: 3+ characters, not a number, not a stop word.
fn words(s: &str) -> impl Iterator<Item = Cow<'_, str>> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 && !w.chars().all(|c| c.is_ascii_digit()))
        .map(|w| {
            if w.chars().any(char::is_uppercase) {
                Cow::Owned(w.to_lowercase())
            } else {
                Cow::Borrowed(w)
            }
        })
        .filter(|w| !STOP.contains(&w.as_ref()))
}

type Groups = HashMap<String, Vec<i64>>;

/// Adds bookmark `id` to group `key`, allocating the key only for a new group.
fn push(g: &mut Groups, key: &str, id: i64) {
    match g.get_mut(key) {
        Some(ids) => ids.push(id),
        None => {
            g.insert(key.to_string(), vec![id]);
        }
    }
}

/// Creation time of a UUIDv7, in Unix milliseconds.
fn uid_time(uid: &str) -> Option<i64> {
    let (secs, nanos) = uuid::Uuid::parse_str(uid).ok()?.get_timestamp()?.to_unix();
    Some(secs as i64 * 1000 + i64::from(nanos / 1_000_000))
}

/// Singular forms `name` might be the plural of.
fn singulars(name: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(s) = name.strip_suffix("ies") {
        v.push(format!("{s}y"));
    }
    if let Some(s) = name.strip_suffix("es") {
        v.push(s.to_string());
    }
    if let Some(s) = name.strip_suffix('s')
        && !s.ends_with('s')
    {
        v.push(s.to_string());
    }
    v.retain(|s| s.chars().count() >= 2);
    v
}

impl Store {
    /// Measures the tag list. `placeable`, when given, says whether the classifier would offer a
    /// tag for a bookmark; gap bookmarks it can place are counted but not grouped.
    pub fn health(
        &self,
        opts: &HealthOptions,
        placeable: Option<&dyn Fn(&Item) -> bool>,
    ) -> Result<Health> {
        let conn = &self.conn;
        let mut tags: HashMap<i64, TagRow> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT id, name, canonical, uid FROM tag WHERE deleted_at IS NULL")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                tags.insert(
                    r.get(0)?,
                    TagRow {
                        name: r.get(1)?,
                        canonical: r.get(2)?,
                        uid: r.get(3)?,
                    },
                );
            }
        }
        let bookmarks: usize = conn.query_row(
            "SELECT count(*) FROM bookmark WHERE deleted_at IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize;

        // One pass over the tag links, grouped by bookmark: counts, pairs, coverage.
        let mut count: HashMap<i64, usize> = HashMap::new();
        let mut pairs: HashMap<(i64, i64), usize> = HashMap::new();
        let mut cov = Coverage {
            bookmarks,
            tags: tags.len(),
            canonical: tags.values().filter(|t| t.canonical).count(),
            ..Coverage::default()
        };
        let mut links = 0usize;
        {
            let mut current: Option<i64> = None;
            let mut set: Vec<i64> = Vec::new();
            let mut flush = |set: &mut Vec<i64>, cov: &mut Coverage| {
                if set.is_empty() {
                    return;
                }
                set.sort_unstable();
                cov.tagged += 1;
                cov.histogram[set.len().min(4)] += 1;
                if set.iter().any(|t| tags[t].canonical) {
                    cov.canonical_tagged += 1;
                }
                for (i, &a) in set.iter().enumerate() {
                    *count.entry(a).or_default() += 1;
                    for &b in &set[i + 1..] {
                        *pairs.entry((a, b)).or_default() += 1;
                    }
                }
                links += set.len();
                set.clear();
            };
            let mut stmt =
                conn.prepare("SELECT bookmark_id, tag_id FROM bookmark_tag ORDER BY bookmark_id")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let (b, t): (i64, i64) = (r.get(0)?, r.get(1)?);
                if !tags.contains_key(&t) {
                    continue;
                }
                if current != Some(b) {
                    flush(&mut set, &mut cov);
                    current = Some(b);
                }
                set.push(t);
            }
            flush(&mut set, &mut cov);
        }
        cov.histogram[0] = bookmarks.saturating_sub(cov.tagged);
        cov.tags_per_tagged = if cov.tagged == 0 {
            0.0
        } else {
            links as f64 / cov.tagged as f64
        };
        let n = |t: &i64| count.get(t).copied().unwrap_or(0);
        let name = |t: &i64| tags[t].name.clone();

        let mut redundant: Vec<Redundant> = pairs
            .iter()
            .filter(|&(&(a, b), &k)| {
                k >= opts.min_together
                    && k as f64 >= opts.redundant * n(&a) as f64
                    && k as f64 >= opts.redundant * n(&b) as f64
            })
            .map(|(&(a, b), &k)| {
                // `a` merges into `b`: the one on more bookmarks stays, then the canonical one,
                // then the name that sorts first.
                let keep = |t: &i64| {
                    let t2 = &tags[t];
                    (n(t), t2.canonical, std::cmp::Reverse(t2.name.as_str()))
                };
                let (a, b) = if keep(&a) > keep(&b) { (b, a) } else { (a, b) };
                Redundant {
                    a: name(&a),
                    b: name(&b),
                    a_count: n(&a),
                    b_count: n(&b),
                    together: k,
                    jaccard: k as f64 / (n(&a) + n(&b) - k) as f64,
                }
            })
            .collect();
        redundant.sort_by(|x, y| {
            y.jaccard
                .total_cmp(&x.jaccard)
                .then(y.together.cmp(&x.together))
                .then_with(|| x.a.cmp(&y.a))
        });

        let mut broad: Vec<Broad> = if bookmarks == 0 {
            Vec::new()
        } else {
            count
                .iter()
                .map(|(t, &c)| Broad {
                    tag: name(t),
                    count: c,
                    share: c as f64 / bookmarks as f64,
                })
                .filter(|b| b.share >= opts.broad)
                .collect()
        };
        broad.sort_by(|x, y| y.count.cmp(&x.count).then_with(|| x.tag.cmp(&y.tag)));

        let pinned = self.pinned();
        let mut rare: Vec<Rare> = tags
            .iter()
            .filter(|(id, t)| t.canonical && n(id) <= opts.rare)
            .map(|(id, t)| Rare {
                tag: t.name.clone(),
                count: n(id),
                created_at: uid_time(&t.uid),
                pinned: pinned.is_some_and(|p| p.is_tag(&t.name)),
            })
            .collect();
        rare.sort_by(|x, y| {
            x.count
                .cmp(&y.count)
                .then(x.created_at.cmp(&y.created_at))
                .then_with(|| x.tag.cmp(&y.tag))
        });

        let known = self.known_names(tags.values().map(|t| t.name.as_str()))?;
        let gaps = self.gaps(opts, cov.canonical > 0, placeable, &known)?;
        let style = self.style_issues(tags.values().map(|t| t.name.as_str()), &known);
        Ok(Health {
            coverage: cov,
            redundant,
            broad,
            rare,
            gaps,
            style,
        })
    }

    fn gaps(
        &self,
        opts: &HealthOptions,
        has_canonical: bool,
        placeable: Option<&dyn Fn(&Item) -> bool>,
        known: &HashMap<String, String>,
    ) -> Result<Gaps> {
        // Links only ever point at live tags (merges move them, nothing else deletes a tag), so
        // without a canonical list the tag table needn't be joined.
        let tagged = if has_canonical {
            "SELECT 1 FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
             WHERE bt.bookmark_id = b.id AND t.canonical AND t.deleted_at IS NULL"
        } else {
            "SELECT 1 FROM bookmark_tag bt WHERE bt.bookmark_id = b.id"
        };
        // The classifier needs the whole bookmark; grouping only the host and title.
        let more = if placeable.is_some() {
            ", b.url, b.note, b.summary"
        } else {
            ""
        };
        let sql = format!(
            "SELECT b.id, b.url_norm, b.title{more} FROM bookmark b \
             WHERE b.deleted_at IS NULL AND NOT EXISTS ({tagged}) ORDER BY b.id"
        );
        let mut gaps = Gaps {
            placeable: placeable.map(|_| 0),
            ..Gaps::default()
        };
        // Per group key, its bookmarks (in id order).
        let (mut hosts, mut words_of): (Groups, Groups) = Default::default();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            gaps.bookmarks += 1;
            let id: i64 = r.get(0)?;
            let url_norm = r.get_ref(1)?.as_str()?;
            let title = r.get_ref(2)?.as_str_or_null()?;
            if let Some(place) = placeable {
                let item = Item {
                    url: r.get(3)?,
                    title: title.map(str::to_string),
                    note: r.get(4)?,
                    summary: r.get(5)?,
                    text: None,
                };
                if place(&item) {
                    *gaps.placeable.as_mut().expect("set above") += 1;
                    continue;
                }
            }
            if let Some(h) = crate::url::host(url_norm) {
                push(&mut hosts, h.strip_prefix("www.").unwrap_or(h), id);
            }
            let text = title.unwrap_or_else(|| url_norm.split_once("://").map_or("", |(_, r)| r));
            let mut seen: Vec<Cow<str>> = Vec::new();
            for w in words(text) {
                if !seen.contains(&w) {
                    push(&mut words_of, &w, id);
                    seen.push(w);
                }
            }
        }
        drop(rows);
        let mut example = self
            .conn
            .prepare("SELECT coalesce(title, url) FROM bookmark WHERE id = ?1")?;
        let mut top = |g: Groups| -> Result<Vec<Group>> {
            let mut v: Vec<(String, Vec<i64>)> = g
                .into_iter()
                .filter(|(_, ids)| ids.len() >= opts.min_group)
                .collect();
            v.sort_by(|(ka, a), (kb, b)| b.len().cmp(&a.len()).then_with(|| ka.cmp(kb)));
            v.truncate(opts.groups);
            v.into_iter()
                .map(|(key, ids)| {
                    let examples = ids
                        .iter()
                        .take(3)
                        .map(|id| example.query_row([id], |r| r.get(0)))
                        .collect::<rusqlite::Result<_>>()?;
                    Ok(Group {
                        existing: None,
                        named: None,
                        key,
                        ids,
                        examples,
                    })
                })
                .collect()
        };
        gaps.by_host = top(hosts)?;
        gaps.by_word = top(words_of)?;
        for g in &mut gaps.by_word {
            g.existing = known.get(&g.key).cloned();
        }
        Ok(gaps)
    }

    /// Every tag name and alias, to the tag it names.
    fn known_names<'a>(
        &self,
        names: impl Iterator<Item = &'a str>,
    ) -> Result<HashMap<String, String>> {
        let mut known: HashMap<String, String> =
            names.map(|n| (n.to_string(), n.to_string())).collect();
        let mut stmt = self.conn.prepare(
            "SELECT a.alias, t.name FROM tag_alias a JOIN tag t ON t.id = a.tag_id \
             WHERE t.deleted_at IS NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            known.entry(r.get(0)?).or_insert(r.get(1)?);
        }
        Ok(known)
    }

    /// `[tags.style]` violations, singular/plural pairs, and compounds of other tags.
    fn style_issues<'a>(
        &self,
        names: impl Iterator<Item = &'a str>,
        known: &HashMap<String, String>,
    ) -> Vec<StyleIssue> {
        let mut names: Vec<&str> = names.collect();
        names.sort_unstable();
        let style = self.style();
        let mut issues = Vec::new();
        for &name in &names {
            if let Some(message) = style.violation(name) {
                issues.push(StyleIssue {
                    tag: name.to_string(),
                    kind: StyleKind::Style,
                    related: Vec::new(),
                    message,
                });
            }
            if let Some(single) = singulars(name)
                .into_iter()
                .find(|s| names.binary_search(&s.as_str()).is_ok())
            {
                issues.push(StyleIssue {
                    tag: name.to_string(),
                    message: format!("{name} and {single} are both tags; keep one"),
                    kind: StyleKind::Plural,
                    related: vec![single],
                });
            }
            let parts: Vec<&str> = name.split(['-', '_']).filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2 {
                let tags: Option<Vec<String>> =
                    parts.iter().map(|p| known.get(*p).cloned()).collect();
                if let Some(tags) = tags {
                    issues.push(StyleIssue {
                        tag: name.to_string(),
                        message: format!(
                            "{name} combines {}; tag bookmarks with each instead",
                            tags.join(" and ")
                        ),
                        kind: StyleKind::Compound,
                        related: tags,
                    });
                }
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_singulars() {
        assert_eq!(singulars("plugins"), ["plugin"]);
        assert!(singulars("libraries").contains(&"library".to_string()));
        assert!(singulars("boxes").contains(&"box".to_string()));
        assert!(singulars("css").is_empty());
        assert!(singulars("rust").is_empty());
    }

    #[test]
    fn reads_uuid_v7_times() {
        let t = crate::store::now_ms();
        let uid = uuid::Uuid::now_v7().to_string();
        assert!((uid_time(&uid).unwrap() - t).abs() < 1000);
        assert_eq!(uid_time("nope"), None);
    }

    #[test]
    fn title_words() {
        let w: Vec<_> = words("How to use NixOS with 2024 flakes: the guide").collect();
        assert_eq!(w, ["nixos", "flakes", "guide"]);
    }
}
