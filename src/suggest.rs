//! Tag suggestions, and the first tagging layer that makes them: domain rules from config.
//!
//! Every layer (rules now; the local model and the LLM later) goes through [`offer`]. In confirm
//! mode (the default) an offered tag becomes a pending row in `tag_suggestion`, which
//! [`Store::accept`] moves into `bookmark_tag` with its source and confidence, and
//! [`Store::reject`] marks rejected. In auto mode the tag is applied at once. Either way, a tag
//! the bookmark already has, or one that was rejected for it, is never offered again.
//!
//! Removing a tag that a layer applied (`edit -t -TAG`) counts as rejecting it, so auto mode
//! doesn't put it back on the next run.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::store::{Store, ensure_tag, normalize_tag, now_ms, resolve_tag, touch_many};
use crate::url;

/// Which tagging layer made a suggestion. Stored as text in `tag_suggestion.source` and
/// `bookmark_tag.source` (where user tags are `user`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A domain rule from config.
    Rule,
    /// The local classifier (feature `model`).
    Model,
    /// The configured LLM provider (feature `ai`).
    Llm,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Rule => "rule",
            Source::Model => "model",
            Source::Llm => "llm",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "rule" => Ok(Source::Rule),
            "model" => Ok(Source::Model),
            "llm" => Ok(Source::Llm),
            _ => Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("suggestion source {s:?}"),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

/// Whether suggestions are applied at once or wait for review (`[tags] suggestions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestMode {
    /// Suggestions wait for `shlv suggest accept` or `reject`.
    #[default]
    Confirm,
    /// Suggestions become tags right away.
    Auto,
}

impl SuggestMode {
    pub fn is_default(&self) -> bool {
        *self == SuggestMode::Confirm
    }
}

/// Domain rules from config (`[tags.rules]`): a host, which also matches its subdomains (like
/// `site:`), mapped to tags. Built only through validation, so hosts and tags are normalized,
/// every rule has a tag, and no rule lists a tag twice.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(try_from = "BTreeMap<String, Vec<String>>")]
pub struct DomainRules {
    rules: BTreeMap<String, Vec<String>>,
}

impl DomainRules {
    /// Validates rules. Like the declared canonical list, hosts and tags must already be in
    /// normalized form: `"GitHub.com"` is an error that suggests `"github.com"`.
    pub fn new(rules: BTreeMap<String, Vec<String>>) -> Result<Self, String> {
        let mut out = DomainRules::default();
        for (host, tags) in rules {
            let norm = normalize_host(&host);
            match norm {
                None => return Err(format!("rule {host:?}: not a host name")),
                Some(n) if n != host => {
                    return Err(format!("rule {host:?}: not a normalized host; write {n:?}"));
                }
                Some(_) => {}
            }
            if tags.is_empty() {
                return Err(format!("rule {host:?}: no tags"));
            }
            for (i, tag) in tags.iter().enumerate() {
                let n = normalize_tag(tag);
                if n.is_empty() {
                    return Err(format!("rule {host:?}: {tag:?} is not a valid tag name"));
                }
                if &n != tag {
                    return Err(format!(
                        "rule {host:?}: {tag:?} is not a normalized tag name; write {n:?}"
                    ));
                }
                if tags[..i].contains(tag) {
                    return Err(format!("rule {host:?}: tag {tag:?} is listed twice"));
                }
            }
            out.rules.insert(host, tags);
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Rules by host, each with its tags.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.rules.iter().map(|(h, t)| (h.as_str(), t.as_slice()))
    }

    /// Tags of every rule matching the normalized URL's host, deduplicated: the rule for the host
    /// itself first, then those for its parent domains.
    pub fn matching(&self, url_norm: &str) -> Vec<&str> {
        let mut tags: Vec<&str> = Vec::new();
        let Some(mut host) = url::host(url_norm) else {
            return tags;
        };
        loop {
            if let Some(list) = self.rules.get(host) {
                for t in list {
                    if !tags.contains(&t.as_str()) {
                        tags.push(t);
                    }
                }
            }
            match host.split_once('.') {
                Some((_, parent)) if !parent.is_empty() => host = parent,
                _ => return tags,
            }
        }
    }

    /// Every tag the rules use, deduplicated, in rule order.
    fn tags(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.rules
            .values()
            .flatten()
            .map(String::as_str)
            .filter(|t| seen.insert(*t))
            .collect()
    }
}

impl TryFrom<BTreeMap<String, Vec<String>>> for DomainRules {
    type Error = String;
    fn try_from(rules: BTreeMap<String, Vec<String>>) -> Result<Self, String> {
        DomainRules::new(rules)
    }
}

impl Serialize for DomainRules {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.rules.serialize(s)
    }
}

/// A host as rules and `site:` see it: what `github.com`, `GitHub.com.`, or
/// `https://github.com/x` normalize to.
fn normalize_host(input: &str) -> Option<String> {
    if input.trim().is_empty() || input.chars().any(char::is_whitespace) {
        return None;
    }
    let norm = url::normalize(input);
    let host = url::host(&norm)?;
    // A port or user info isn't part of a host, and ports never match anyway.
    let authority = norm.split_once("://")?.1.split('/').next()?;
    (authority == host && !host.starts_with('.')).then(|| host.to_string())
}

/// A pending suggestion, as listed by [`Store::pending_each`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    /// The bookmark's id.
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub tag: String,
    pub source: Source,
    pub confidence: Option<f64>,
}

/// A pending suggestion of a tag being back-filled, as [`Store::backfill_candidates`] ranks them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackfillCandidate {
    #[serde(flatten)]
    pub suggestion: Suggestion,
    /// The bookmark has canonical tags: the classifier trains on it, as a negative example of
    /// this tag until it gets it.
    pub reviewed: bool,
}

/// Strict mode is on and domain rules use tags that aren't canonical: (host, tag) pairs, in rule
/// order. Returned before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonCanonicalRuleTags(pub Vec<(String, String)>);

impl std::fmt::Display for NonCanonicalRuleTags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list: Vec<String> = self.0.iter().map(|(h, t)| format!("{t} ({h})")).collect();
        write!(
            f,
            "[tags.rules] uses tags that aren't canonical (strict_tags is on): {}",
            list.join(", ")
        )
    }
}

impl std::error::Error for NonCanonicalRuleTags {}

/// What [`offer`] did with a suggested tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offered {
    /// Recorded as pending (confirm mode).
    Pending,
    /// Applied as a tag (auto mode).
    Applied,
    /// Nothing: the bookmark has the tag, the tag was rejected for it, or it's already pending.
    Skipped,
}

/// What [`Store::offer_choices`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OfferStats {
    /// New pending suggestions (confirm mode).
    pub pending: usize,
    /// Tags applied (auto mode).
    pub applied: usize,
}

/// What [`Store::apply_rules`], or the rules in [`Store::import`], did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleStats {
    /// Bookmarks checked.
    pub checked: usize,
    /// Bookmarks at least one rule matched.
    pub matched: usize,
    /// New pending suggestions (confirm mode).
    pub pending: usize,
    /// Tags applied (auto mode).
    pub applied: usize,
    /// Bookmarks that got new tags (auto mode).
    pub tagged: usize,
}

/// Offers tag `tag_id` for bookmark `bookmark_id`; see [`Offered`]. In auto mode the caller
/// reindexes the bookmark (and bumps its `updated_at`) afterward if anything was applied. A tag
/// offered again while pending keeps the higher confidence and that one's source.
pub(crate) fn offer(
    tx: &Transaction,
    bookmark_id: i64,
    tag_id: i64,
    source: Source,
    confidence: Option<f64>,
    mode: SuggestMode,
    now: i64,
) -> Result<Offered> {
    let (tagged, state): (bool, Option<String>) = tx
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM bookmark_tag WHERE bookmark_id = ?1 AND tag_id = ?2), \
             (SELECT state FROM tag_suggestion WHERE bookmark_id = ?1 AND tag_id = ?2)",
        )?
        .query_row(params![bookmark_id, tag_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if tagged || state.as_deref() == Some("rejected") {
        return Ok(Offered::Skipped);
    }
    let pending = state.is_some();
    match mode {
        SuggestMode::Confirm if pending => {
            tx.prepare_cached(
                "UPDATE tag_suggestion SET source = ?3, confidence = ?4, updated_at = ?5 \
                 WHERE bookmark_id = ?1 AND tag_id = ?2 AND coalesce(confidence, 0) < ?4",
            )?
            .execute(params![
                bookmark_id,
                tag_id,
                source.as_str(),
                confidence,
                now
            ])?;
            Ok(Offered::Skipped)
        }
        SuggestMode::Confirm => {
            tx.prepare_cached(
                "INSERT INTO tag_suggestion (bookmark_id, tag_id, source, confidence, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                bookmark_id,
                tag_id,
                source.as_str(),
                confidence,
                now
            ])?;
            Ok(Offered::Pending)
        }
        SuggestMode::Auto => {
            if pending {
                tx.prepare_cached(
                    "DELETE FROM tag_suggestion WHERE bookmark_id = ?1 AND tag_id = ?2",
                )?
                .execute(params![bookmark_id, tag_id])?;
            }
            tx.prepare_cached(
                "INSERT INTO bookmark_tag (bookmark_id, tag_id, source, confidence) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![bookmark_id, tag_id, source.as_str(), confidence])?;
            Ok(Offered::Applied)
        }
    }
}

/// Offers the tags of every rule matching `url_norm` for a bookmark. Returns how many were
/// applied or recorded.
pub(crate) fn offer_rules(
    tx: &Transaction,
    rules: &DomainRules,
    mode: SuggestMode,
    bookmark_id: i64,
    url_norm: &str,
    now: i64,
) -> Result<usize> {
    let mut n = 0;
    for tag in rules.matching(url_norm) {
        let tag_id = ensure_tag(tx, tag, now)?;
        if offer(tx, bookmark_id, tag_id, Source::Rule, Some(1.0), mode, now)? != Offered::Skipped {
            n += 1;
        }
    }
    Ok(n)
}

/// Runs [`offer_rules`] over `(bookmark id, url_norm)` pairs. Returns the stats and the ids that
/// got new tags (auto mode), which the caller reindexes.
pub(crate) fn offer_rules_many<'a>(
    tx: &Transaction,
    rules: &DomainRules,
    mode: SuggestMode,
    targets: impl IntoIterator<Item = (i64, &'a str)>,
    now: i64,
) -> Result<(RuleStats, Vec<i64>)> {
    let mut stats = RuleStats::default();
    let mut tagged = Vec::new();
    for (id, norm) in targets {
        stats.checked += 1;
        if rules.matching(norm).is_empty() {
            continue;
        }
        stats.matched += 1;
        let n = offer_rules(tx, rules, mode, id, norm, now)?;
        match mode {
            SuggestMode::Confirm => stats.pending += n,
            SuggestMode::Auto => {
                stats.applied += n;
                if n > 0 {
                    tagged.push(id);
                }
            }
        }
    }
    stats.tagged = tagged.len();
    Ok((stats, tagged))
}

/// Before a tag a layer applied is removed from a bookmark, remembers it as rejected so it isn't
/// offered again. User tags (`source = 'user'`) aren't remembered.
pub(crate) fn reject_applied(
    tx: &Transaction,
    bookmark_id: i64,
    tag_id: i64,
    now: i64,
) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO tag_suggestion (bookmark_id, tag_id, source, confidence, state, updated_at) \
         SELECT bookmark_id, tag_id, source, confidence, 'rejected', ?3 FROM bookmark_tag \
         WHERE bookmark_id = ?1 AND tag_id = ?2 AND source <> 'user' \
         ON CONFLICT DO UPDATE SET state = 'rejected', updated_at = ?3",
    )?
    .execute(params![bookmark_id, tag_id, now])?;
    Ok(())
}

/// Pending rows that are still news: the bookmark doesn't carry the tag yet (a user may have
/// added it by hand since).
const PENDING: &str = "s.state = 'pending' AND NOT EXISTS (SELECT 1 FROM bookmark_tag bt \
     WHERE bt.bookmark_id = s.bookmark_id AND bt.tag_id = s.tag_id)";

impl Store {
    /// Offers a choose-tags layer's choices for one bookmark, in one short transaction, through
    /// the same path as domain rules: confirm mode records them as pending, auto mode applies
    /// them; tags the bookmark has or that were rejected for it are skipped. Choices must name
    /// canonical tags (as [`crate::choose::Canonical`] returns them); others are ignored.
    pub fn offer_choices(
        &mut self,
        bookmark: i64,
        source: Source,
        choices: &[crate::choose::Choice],
    ) -> Result<OfferStats> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut stats = OfferStats::default();
        for c in choices {
            let Some(tag_id) = resolve_tag(&tx, &c.tag)? else {
                continue;
            };
            match offer(
                &tx,
                bookmark,
                tag_id,
                source,
                c.confidence,
                self.suggest_mode,
                now,
            )? {
                Offered::Pending => stats.pending += 1,
                Offered::Applied => stats.applied += 1,
                Offered::Skipped => {}
            }
        }
        if stats.applied > 0 {
            touch_many(&tx, &[bookmark], now)?;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Tags a layer shouldn't offer for `bookmark`: the ones it has and the ones rejected for it.
    pub fn excluded_tags(&self, bookmark: i64) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.name FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
             WHERE bt.bookmark_id = ?1 \
             UNION SELECT t.name FROM tag_suggestion s JOIN tag t ON t.id = s.tag_id \
             WHERE s.bookmark_id = ?1 AND s.state = 'rejected'",
        )?;
        let names = stmt.query_map([bookmark], |r| r.get(0))?;
        Ok(names.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether `source` has suggested anything for `bookmark` before: a suggestion pending or
    /// rejected, a tag it suggested that was accepted, or (for `llm`) a name it proposed.
    pub fn suggested_by(&self, bookmark: i64, source: Source) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM tag_suggestion WHERE bookmark_id = ?1 \
                 AND source = ?2) OR EXISTS (SELECT 1 FROM bookmark_tag WHERE bookmark_id = ?1 \
                 AND source = ?2) OR EXISTS (SELECT 1 FROM tag_proposal_bookmark \
                 WHERE bookmark_id = ?1 AND source = ?2)",
            )?
            .query_row(params![bookmark, source.as_str()], |r| r.get(0))?)
    }

    /// Domain rules applied by [`Store::add`] and [`Store::apply_rules`]. None by default.
    pub fn set_rules(&mut self, rules: DomainRules) {
        self.rules = rules;
    }

    /// Whether suggestions are applied at once or wait for review. Confirm by default.
    pub fn set_suggest_mode(&mut self, mode: SuggestMode) {
        self.suggest_mode = mode;
    }

    pub fn suggest_mode(&self) -> SuggestMode {
        self.suggest_mode
    }

    pub fn rules(&self) -> &DomainRules {
        &self.rules
    }

    /// In strict mode, fails unless every rule tag is canonical or an alias of a canonical tag.
    /// Checks all rules, not just the ones a URL matches, so a bad rule fails every time.
    pub fn check_rules(&self) -> Result<()> {
        if !self.strict_tags() || self.rules.is_empty() {
            return Ok(());
        }
        let tags: Vec<String> = self.rules.tags().into_iter().map(String::from).collect();
        let bad = self.non_canonical(&tags)?;
        if bad.is_empty() {
            return Ok(());
        }
        let list = self
            .rules
            .iter()
            .flat_map(|(host, tags)| {
                tags.iter()
                    .filter(|t| bad.contains(t))
                    .map(move |t| (host.to_string(), t.clone()))
            })
            .collect();
        Err(NonCanonicalRuleTags(list).into())
    }

    /// Runs the domain rules on bookmarks: the given ids, or every live bookmark if `ids` is
    /// `None`, in one transaction. Fails if an id isn't a live bookmark.
    pub fn apply_rules(&mut self, ids: Option<&[i64]>) -> Result<RuleStats> {
        self.check_rules()?;
        if self.rules.is_empty() {
            return Ok(RuleStats::default());
        }
        self.with_bulk_cache(|s| s.run_rules(ids))
    }

    fn run_rules(&mut self, ids: Option<&[i64]>) -> Result<RuleStats> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let targets: Vec<(i64, String)> = match ids {
            Some(ids) => {
                let mut stmt = tx.prepare_cached(
                    "SELECT url_norm FROM bookmark WHERE id = ?1 AND deleted_at IS NULL",
                )?;
                let mut v = Vec::with_capacity(ids.len());
                for &id in ids {
                    match stmt.query_row([id], |r| r.get(0)).optional()? {
                        Some(norm) => v.push((id, norm)),
                        None => bail!("no bookmark with id {id}"),
                    }
                }
                v
            }
            None => tx
                .prepare("SELECT id, url_norm FROM bookmark WHERE deleted_at IS NULL")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?,
        };
        let (stats, tagged) = offer_rules_many(
            &tx,
            &self.rules,
            self.suggest_mode,
            targets.iter().map(|(id, norm)| (*id, norm.as_str())),
            now,
        )?;
        touch_many(&tx, &tagged, now)?;
        tx.commit()?;
        Ok(stats)
    }

    /// Calls `f` for each pending suggestion, newest bookmarks first, then by tag name. With
    /// `bookmark`, only that bookmark's.
    pub fn pending_each(
        &self,
        bookmark: Option<i64>,
        mut f: impl FnMut(Suggestion) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT s.bookmark_id, b.url, b.title, t.name, s.source, s.confidence \
             FROM tag_suggestion s JOIN bookmark b ON b.id = s.bookmark_id \
             JOIN tag t ON t.id = s.tag_id \
             WHERE {PENDING} AND (?1 IS NULL OR s.bookmark_id = ?1) \
             ORDER BY s.bookmark_id DESC, t.name"
        ))?;
        let mut rows = stmt.query([bookmark])?;
        while let Some(r) = rows.next()? {
            let source: String = r.get(4)?;
            f(Suggestion {
                id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
                tag: r.get(3)?,
                source: Source::parse(&source)?,
                confidence: r.get(5)?,
            })?;
        }
        Ok(())
    }

    /// Pending suggestions; see [`Store::pending_each`].
    pub fn pending(&self, bookmark: Option<i64>) -> Result<Vec<Suggestion>> {
        let mut v = Vec::new();
        self.pending_each(bookmark, |s| {
            v.push(s);
            Ok(())
        })?;
        Ok(v)
    }

    /// Accepts a bookmark's pending suggestions: the given tags (names or aliases), or all of
    /// them if `tags` is empty. They become tags that keep their source and confidence. Returns
    /// the accepted tag names, sorted. Fails if a given tag isn't pending for the bookmark, or
    /// if none is; in strict mode, fails with [`crate::NonCanonicalTags`] if a tag has stopped
    /// being canonical since it was suggested.
    pub fn accept(&mut self, bookmark: i64, tags: &[String]) -> Result<Vec<String>> {
        let picked = self.pick(bookmark, tags, "accept")?;
        let names: Vec<String> = picked.iter().map(|(_, n)| n.clone()).collect();
        self.check_strict(&names)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        for (tag_id, _) in &picked {
            accept_rows(
                &tx,
                "bookmark_id = ?1 AND tag_id = ?2",
                params![bookmark, tag_id],
            )?;
        }
        touch_many(&tx, &[bookmark], now)?;
        tx.commit()?;
        Ok(names)
    }

    /// Accepts every pending suggestion, in one transaction. Returns (bookmarks retagged, tags
    /// applied). In strict mode, fails with [`crate::NonCanonicalTags`] before writing anything
    /// if a suggested tag has stopped being canonical.
    pub fn accept_all(&mut self) -> Result<(usize, usize)> {
        let names: Vec<String> = self
            .conn
            .prepare(&format!(
                "SELECT DISTINCT t.name FROM tag_suggestion s JOIN tag t ON t.id = s.tag_id \
                 WHERE {PENDING} ORDER BY t.name"
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.check_strict(&names)?;
        self.with_bulk_cache(Self::accept_pending)
    }

    fn accept_pending(&mut self) -> Result<(usize, usize)> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let ids: Vec<i64> = tx
            .prepare(&format!(
                "SELECT DISTINCT s.bookmark_id FROM tag_suggestion s WHERE {PENDING}"
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let n = accept_rows(&tx, "1", [])?;
        touch_many(&tx, &ids, now)?;
        tx.commit()?;
        Ok((ids.len(), n))
    }

    /// Rejects a bookmark's pending suggestions: the given tags, or all of them if `tags` is
    /// empty. No layer offers them for this bookmark again. Returns the rejected tag names,
    /// sorted.
    pub fn reject(&mut self, bookmark: i64, tags: &[String]) -> Result<Vec<String>> {
        let picked = self.pick(bookmark, tags, "reject")?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        for (tag_id, _) in &picked {
            tx.prepare_cached(
                "UPDATE tag_suggestion SET state = 'rejected', updated_at = ?3 \
                 WHERE bookmark_id = ?1 AND tag_id = ?2",
            )?
            .execute(params![bookmark, tag_id, now])?;
        }
        tx.commit()?;
        Ok(picked.into_iter().map(|(_, n)| n).collect())
    }

    /// The canonical tag `name` (a tag or an alias) stands for, for back-filling it. Fails if
    /// it's unknown or not on the canonical list: tags are only back-filled once they're on it.
    pub fn backfill_tag(&self, name: &str) -> Result<String> {
        let looked = self.lookup(name)?;
        let row: Option<(String, bool)> = match resolve_tag(&self.conn, &looked)? {
            Some(id) => Some(self.conn.query_row(
                "SELECT name, canonical FROM tag WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?),
            None => None,
        };
        match row {
            Some((tag, true)) => Ok(tag),
            Some((tag, false)) => bail!(
                "{tag:?} isn't on the canonical list; add it first: shlv tag canonical add {tag}"
            ),
            None => bail!("no tag {looked:?}"),
        }
    }

    /// Back-fill candidates for canonical tag `tag`: its pending suggestions (on the given
    /// bookmarks, or all), reviewed bookmarks first since they're the classifier's examples and
    /// count against the tag until they get it, then by confidence. A suggestion without one
    /// (the LLM's) ranks as 0.5.
    pub fn backfill_candidates(
        &self,
        tag: &str,
        ids: Option<&[i64]>,
    ) -> Result<Vec<BackfillCandidate>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT s.bookmark_id, b.url, b.title, t.name, s.source, s.confidence, \
             EXISTS (SELECT 1 FROM bookmark_tag bt JOIN tag u ON u.id = bt.tag_id \
                     WHERE bt.bookmark_id = s.bookmark_id AND u.canonical) AS reviewed \
             FROM tag_suggestion s JOIN bookmark b ON b.id = s.bookmark_id \
             JOIN tag t ON t.id = s.tag_id \
             WHERE {PENDING} AND t.name = ?1 AND b.deleted_at IS NULL \
             ORDER BY reviewed DESC, coalesce(s.confidence, 0.5) DESC, s.bookmark_id DESC"
        ))?;
        let wanted: Option<HashSet<i64>> = ids.map(|i| i.iter().copied().collect());
        let mut rows = stmt.query([tag])?;
        let mut v = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if wanted.as_ref().is_some_and(|w| !w.contains(&id)) {
                continue;
            }
            let source: String = r.get(4)?;
            v.push(BackfillCandidate {
                suggestion: Suggestion {
                    id,
                    url: r.get(1)?,
                    title: r.get(2)?,
                    tag: r.get(3)?,
                    source: Source::parse(&source)?,
                    confidence: r.get(5)?,
                },
                reviewed: r.get(6)?,
            });
        }
        Ok(v)
    }

    /// Reviewed bookmarks (with canonical tags) that `tag` hasn't been offered for: not tagged
    /// with it, and no suggestion of it pending or rejected. Newest first. With an LLM, these are
    /// the ones worth asking about beyond the classifier's candidates.
    pub fn backfill_unoffered(&self, tag: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT b.id FROM bookmark b WHERE b.deleted_at IS NULL \
             AND EXISTS (SELECT 1 FROM bookmark_tag bt JOIN tag u ON u.id = bt.tag_id \
                         WHERE bt.bookmark_id = b.id AND u.canonical) \
             AND NOT EXISTS (SELECT 1 FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
                             WHERE bt.bookmark_id = b.id AND t.name = ?1) \
             AND NOT EXISTS (SELECT 1 FROM tag_suggestion s JOIN tag t ON t.id = s.tag_id \
                             WHERE s.bookmark_id = b.id AND t.name = ?1) \
             ORDER BY b.id DESC",
        )?;
        let ids = stmt.query_map([tag], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// The pending suggestions `accept`/`reject` act on: (tag id, name), sorted by name.
    fn pick(&self, bookmark: i64, tags: &[String], verb: &str) -> Result<Vec<(i64, String)>> {
        let pending = pending_tags(&self.conn, bookmark)?;
        if pending.is_empty() {
            bail!("no pending suggestions for bookmark {bookmark}");
        }
        if tags.is_empty() {
            return Ok(pending);
        }
        let mut picked: Vec<(i64, String)> = Vec::new();
        for t in tags {
            let name = self.lookup(t)?;
            let found = resolve_tag(&self.conn, &name)?
                .and_then(|id| pending.iter().find(|(p, _)| *p == id));
            match found {
                Some(p) if !picked.contains(p) => picked.push(p.clone()),
                Some(_) => {}
                None => {
                    let list: Vec<&str> = pending.iter().map(|(_, n)| n.as_str()).collect();
                    bail!(
                        "cannot {verb} {name:?}: not suggested for bookmark {bookmark} \
                         (pending: {})",
                        list.join(" ")
                    );
                }
            }
        }
        picked.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(picked)
    }
}

/// A bookmark's pending suggestions: (tag id, name), sorted by name.
fn pending_tags(conn: &Connection, bookmark: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT t.id, t.name FROM tag_suggestion s JOIN tag t ON t.id = s.tag_id \
         WHERE {PENDING} AND s.bookmark_id = ?1 ORDER BY t.name"
    ))?;
    let rows = stmt.query_map([bookmark], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Moves pending suggestions matching `filter` (on `tag_suggestion`) into `bookmark_tag`, keeping
/// source and confidence. Returns how many tags were applied; rows for tags a bookmark already
/// has are dropped without counting.
fn accept_rows(
    tx: &Transaction,
    filter: &str,
    args: impl rusqlite::Params + Clone,
) -> Result<usize> {
    let n = tx.execute(
        &format!(
            "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id, source, confidence) \
             SELECT bookmark_id, tag_id, source, confidence FROM tag_suggestion \
             WHERE state = 'pending' AND {filter}"
        ),
        args.clone(),
    )?;
    tx.execute(
        &format!("DELETE FROM tag_suggestion WHERE state = 'pending' AND {filter}"),
        args,
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &[&str])]) -> Result<DomainRules, String> {
        DomainRules::new(
            pairs
                .iter()
                .map(|(h, t)| (h.to_string(), t.iter().map(|t| t.to_string()).collect()))
                .collect(),
        )
    }

    #[test]
    fn matches_host_and_subdomains() {
        let r = rules(&[
            ("github.com", &["code"]),
            ("gist.github.com", &["snippet", "code"]),
            ("docs.rs", &["rust", "docs"]),
        ])
        .unwrap();
        let m = |u: &str| r.matching(&url::normalize(u));
        assert_eq!(m("https://github.com/x"), ["code"]);
        assert_eq!(m("https://GitHub.com:443/x"), ["code"]);
        assert_eq!(m("https://www.github.com/x"), ["code"]);
        assert_eq!(m("https://gist.github.com/x"), ["snippet", "code"]);
        assert_eq!(m("https://a.gist.github.com/x"), ["snippet", "code"]);
        assert_eq!(m("https://docs.rs/serde"), ["rust", "docs"]);
        assert!(m("https://notgithub.com/").is_empty());
        assert!(m("https://github.com.evil.io/").is_empty());
        assert!(m("https://example.com/github.com").is_empty());
        assert!(m("mailto:me@github.com").is_empty());
        assert!(m("https://user@example.com/").is_empty());
    }

    #[test]
    fn validates_rules() {
        let err = |pairs: &[(&str, &[&str])]| rules(pairs).unwrap_err();
        assert_eq!(
            err(&[("GitHub.com", &["code"])]),
            "rule \"GitHub.com\": not a normalized host; write \"github.com\""
        );
        assert_eq!(
            err(&[("https://github.com/", &["code"])]),
            "rule \"https://github.com/\": not a normalized host; write \"github.com\""
        );
        assert_eq!(
            err(&[("github.com.", &["code"])]),
            "rule \"github.com.\": not a normalized host; write \"github.com\""
        );
        for bad in [
            "",
            "git hub.com",
            "github.com:8080",
            "u@github.com",
            ".github.com",
        ] {
            assert_eq!(
                err(&[(bad, &["code"])]),
                format!("rule {bad:?}: not a host name")
            );
        }
        assert_eq!(err(&[("github.com", &[])]), "rule \"github.com\": no tags");
        assert_eq!(
            err(&[("github.com", &["Code"])]),
            "rule \"github.com\": \"Code\" is not a normalized tag name; write \"code\""
        );
        assert_eq!(
            err(&[("github.com", &["code", "code"])]),
            "rule \"github.com\": tag \"code\" is listed twice"
        );
        assert!(rules(&[("localhost", &["local"]), ("[::1]", &["local"])]).is_ok());
    }
}
