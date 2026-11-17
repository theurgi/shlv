//! Bookmark storage: add, get, edit, delete, list, search. Tag commands are in [`crate::tag`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};

use crate::proposal::{self, Diverted, ProposalSource, StrictTags};
use crate::style::TagStyle;
use crate::suggest::{self, DomainRules, SuggestMode};
use crate::tag::NonCanonicalTags;
use crate::{db, search, url};

/// Page cache for bulk writes, in KiB as `PRAGMA cache_size` takes it (negative); see
/// [`Store::with_bulk_cache`].
const BULK_CACHE_KIB: i64 = -32 * 1024;

/// bm25 column weights, in FTS column order: title, url, note, summary, tags.
const BM25: &str = "bm25(bookmark_fts, 10.0, 2.0, 5.0, 3.0, 5.0)";

const SELECT: &str = "SELECT b.id, b.uid, b.url, b.title, b.note, b.summary, b.created_at, \
     b.updated_at, (SELECT group_concat(t.name, ' ' ORDER BY t.name) FROM bookmark_tag bt \
     JOIN tag t ON t.id = bt.tag_id WHERE bt.bookmark_id = b.id) FROM bookmark b";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Bookmark {
    pub id: i64,
    pub uid: String,
    pub url: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub summary: Option<String>,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
    /// Sorted tag names.
    pub tags: Vec<String>,
}

impl Bookmark {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        let tags: Option<String> = r.get(8)?;
        Ok(Bookmark {
            id: r.get(0)?,
            uid: r.get(1)?,
            url: r.get(2)?,
            title: r.get(3)?,
            note: r.get(4)?,
            summary: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            tags: tags
                .map(|t| t.split(' ').map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct NewBookmark {
    pub url: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub tags: Vec<String>,
}

/// Changes to a bookmark. `Some("")` clears a field.
#[derive(Debug, Default, Clone)]
pub struct Edit {
    pub title: Option<String>,
    pub note: Option<String>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub id: i64,
    /// False if the URL was already bookmarked and that bookmark was updated instead.
    pub created: bool,
    /// With `strict_tags = "propose"`: given tags that weren't canonical.
    pub diverted: Diverted,
}

#[derive(Debug, Clone)]
pub struct ListQuery {
    /// Only bookmarks with all of these tags.
    pub tags: Vec<String>,
    pub limit: usize,
}

impl Default for ListQuery {
    fn default() -> Self {
        ListQuery {
            tags: Vec::new(),
            limit: 50,
        }
    }
}

/// Normalizes a tag name: trimmed, lowercase, leading `#` dropped, whitespace runs become `-`.
pub fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    let tag = tag.strip_prefix('#').unwrap_or(tag);
    tag.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn new_uid() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Empty strings mean "no value".
pub(crate) fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub struct Store {
    pub(crate) conn: Connection,
    strict_tags: StrictTags,
    /// The declared list, once [`Store::pin`] has synced it.
    pub(crate) pinned: Option<crate::tag::DeclaredTags>,
    pub(crate) rules: DomainRules,
    pub(crate) suggest_mode: SuggestMode,
    pub(crate) style: TagStyle,
}

impl Store {
    /// Opens the database at `path`, creating it and applying migrations as needed.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Store::new(db::open(path)?))
    }

    /// Opens the database at the default location (see [`db::default_path`]).
    pub fn open_default() -> Result<Self> {
        Self::open(&db::default_path()?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Store::new(db::open_in_memory()?))
    }

    fn new(conn: Connection) -> Self {
        Store {
            conn,
            strict_tags: StrictTags::Off,
            pinned: None,
            rules: DomainRules::default(),
            suggest_mode: SuggestMode::default(),
            style: TagStyle::default(),
        }
    }

    /// In strict mode, adding a tag that isn't canonical or an alias of a canonical tag fails
    /// with [`NonCanonicalTags`] before anything is written ([`StrictTags::On`], or `true`), or
    /// becomes a proposal instead ([`StrictTags::Propose`]). Off by default.
    pub fn set_strict_tags(&mut self, mode: impl Into<StrictTags>) {
        self.strict_tags = mode.into();
    }

    /// Whether strict mode is on, either way; see [`Store::set_strict_tags`].
    pub fn strict_tags(&self) -> bool {
        self.strict_tags != StrictTags::Off
    }

    /// The strict mode; see [`Store::set_strict_tags`].
    pub fn strict_mode(&self) -> StrictTags {
        self.strict_tags
    }

    /// Sets the tag style (`[tags.style]`) that names entering from now on must follow; see
    /// [`crate::style`]. None by default.
    pub fn set_style(&mut self, style: TagStyle) {
        self.style = style;
    }

    pub fn style(&self) -> TagStyle {
        self.style
    }

    /// Typed or given tag names as they will be stored: see [`styled_name`]. Fails on the first
    /// empty name or one the style refuses, before anything is written.
    pub(crate) fn tag_names(&self, tags: &[String]) -> Result<Vec<String>> {
        tags.iter()
            .map(|t| styled_name(&self.conn, &self.style, t))
            .collect()
    }

    /// One name entering as a tag; see [`styled_name`].
    pub fn styled(&self, tag: &str) -> Result<String> {
        styled_name(&self.conn, &self.style, tag)
    }

    /// A tag's new name (`tag rename`, the way to fix an old name): always fixed by the style and
    /// checked, even if it names an existing tag as typed.
    pub(crate) fn new_name(&self, tag: &str) -> Result<String> {
        let fixed = self.style.fix(tag);
        if fixed.is_empty() {
            bail!("empty tag name");
        }
        self.style.check(&fixed).map_err(|why| anyhow!(why))?;
        Ok(fixed)
    }

    /// A new alias: like [`Store::styled`], but only separators are fixed. An alias is a name to
    /// type, not a tag, so it has no length limit.
    pub(crate) fn alias_name(&self, alias: &str) -> Result<String> {
        let base = normalize_tag(alias);
        if base.is_empty() {
            bail!("empty tag name");
        }
        if self.style.is_default() || resolve_tag(&self.conn, &base)?.is_some() {
            return Ok(base);
        }
        let fixed = self.style.fix(&base);
        if fixed.is_empty() {
            bail!("{alias:?} is not a valid tag name");
        }
        Ok(fixed)
    }

    /// The name of an existing tag or alias; see [`lookup_name`]. Fails on an empty name.
    pub(crate) fn lookup(&self, tag: &str) -> Result<String> {
        let n = lookup_name(&self.conn, &self.style, tag)?;
        if n.is_empty() {
            bail!("empty tag name");
        }
        Ok(n)
    }

    /// Names of existing tags or aliases to look up; see [`lookup_name`].
    pub(crate) fn lookup_names(&self, tags: &[String]) -> Result<Vec<String>> {
        tags.iter().map(|t| self.lookup(t)).collect()
    }

    /// Splits typed tags (normalized) into those a bookmark gets and those that become
    /// proposals: with `strict_tags = "propose"`, the ones that aren't canonical. In the other
    /// modes nothing is split off, and strict mode fails as [`Store::check_strict`] does.
    pub(crate) fn split_strict(&self, tags: Vec<String>) -> Result<(Vec<String>, Vec<String>)> {
        if self.strict_tags != StrictTags::Propose {
            self.check_strict(&tags)?;
            return Ok((tags, Vec::new()));
        }
        let bad = self.non_canonical(&tags)?;
        let keep = tags.into_iter().filter(|t| !bad.contains(t)).collect();
        Ok((keep, bad))
    }

    /// Fails with [`NonCanonicalTags`] if strict mode is on and any of `tags` (normalized) isn't
    /// canonical or an alias of a canonical tag.
    pub(crate) fn check_strict(&self, tags: &[String]) -> Result<()> {
        if !self.strict_tags() {
            return Ok(());
        }
        let bad = self.non_canonical(tags)?;
        if !bad.is_empty() {
            return Err(NonCanonicalTags(bad).into());
        }
        Ok(())
    }

    /// Runs `f` with a page cache big enough for one transaction that rewrites a large part of
    /// the database (import, bulk tagging). Those outgrow SQLite's default 2 MiB cache and spill
    /// to the WAL over and over.
    pub(crate) fn with_bulk_cache<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let cache: i64 = self.conn.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
        self.conn
            .pragma_update(None, "cache_size", BULK_CACHE_KIB)?;
        let out = f(self);
        self.conn.pragma_update(None, "cache_size", cache)?;
        out
    }

    /// The underlying connection, for inspection and tests.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Adds a bookmark. If its normalized URL is already bookmarked, updates that bookmark
    /// instead: given title and note replace the old ones, tags are added. Matching domain rules
    /// (see [`Store::set_rules`]) are recorded as suggestions or applied in the same transaction.
    /// With `strict_tags = "propose"`, given tags that aren't canonical become proposals for the
    /// bookmark ([`Added::diverted`]).
    pub fn add(&mut self, new: &NewBookmark) -> Result<Added> {
        let (full, norm) = check_url(&new.url)?;
        let (tags, divert) = self.split_strict(self.tag_names(&new.tags)?)?;
        self.check_rules()?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut added = put(
            &tx,
            &Put {
                url: &full,
                norm: &norm,
                title: non_empty(&new.title),
                note: non_empty(&new.note),
                summary: None,
                tags: &tags,
                uid: None,
                created_at: now,
                updated_at: now,
            },
            now,
        )?;
        proposal::divert(
            &tx,
            &divert,
            ProposalSource::User,
            added.id,
            now,
            &mut added.diverted,
        )?;
        if !self.rules.is_empty() {
            suggest::offer_rules(&tx, &self.rules, self.suggest_mode, added.id, &norm, now)?;
        }
        reindex(&tx, added.id)?;
        tx.commit()?;
        Ok(added)
    }

    /// Returns a live (not deleted) bookmark by id.
    pub fn get(&self, id: i64) -> Result<Option<Bookmark>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SELECT} WHERE b.id = ?1 AND b.deleted_at IS NULL"
        ))?;
        Ok(stmt.query_row([id], Bookmark::from_row).optional()?)
    }

    /// Returns the live bookmark whose normalized URL matches `url`'s.
    pub fn find_by_url(&self, url: &str) -> Result<Option<Bookmark>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SELECT} WHERE b.url_norm = ?1 AND b.deleted_at IS NULL"
        ))?;
        Ok(stmt
            .query_row([url::normalize(url)], Bookmark::from_row)
            .optional()?)
    }

    /// Applies `edit` to a bookmark. Returns false if there's no live bookmark with that id.
    pub fn edit(&mut self, id: i64, edit: &Edit) -> Result<bool> {
        Ok(self.edit_diverting(id, edit)?.is_some())
    }

    /// Like [`Store::edit`], but says which added tags became proposals (with
    /// `strict_tags = "propose"`). Returns `None` if there's no live bookmark with that id.
    pub fn edit_diverting(&mut self, id: i64, edit: &Edit) -> Result<Option<Diverted>> {
        let (add, divert) = self.split_strict(self.tag_names(&edit.add_tags)?)?;
        let remove = self.lookup_names(&edit.remove_tags)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE bookmark SET updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let mut diverted = Diverted::default();
        proposal::divert(&tx, &divert, ProposalSource::User, id, now, &mut diverted)?;
        if let Some(title) = &edit.title {
            tx.execute(
                "UPDATE bookmark SET title = ?2 WHERE id = ?1",
                params![id, non_empty(&Some(title.clone()))],
            )?;
        }
        if let Some(note) = &edit.note {
            tx.execute(
                "UPDATE bookmark SET note = ?2 WHERE id = ?1",
                params![id, non_empty(&Some(note.clone()))],
            )?;
        }
        for tag in &remove {
            if let Some(tag_id) = resolve_tag(&tx, tag)? {
                suggest::reject_applied(&tx, id, tag_id, now)?;
                tx.execute(
                    "DELETE FROM bookmark_tag WHERE bookmark_id = ?1 AND tag_id = ?2",
                    params![id, tag_id],
                )?;
            }
            // Removing a name proposed for the bookmark withdraws it there, and the proposal
            // with it if that was its last bookmark.
            let withdrawn = tx
                .prepare_cached(
                    "DELETE FROM tag_proposal_bookmark WHERE bookmark_id = ?1 \
                     AND proposal_id = (SELECT id FROM tag_proposal WHERE name = ?2)",
                )?
                .execute(params![id, tag])?;
            if withdrawn > 0 {
                tx.prepare_cached(
                    "DELETE FROM tag_proposal WHERE name = ?1 AND state = 'pending' AND NOT \
                     EXISTS (SELECT 1 FROM tag_proposal_bookmark WHERE proposal_id = id)",
                )?
                .execute([tag])?;
            }
        }
        for tag in &add {
            let tag_id = ensure_tag(&tx, tag, now)?;
            tx.execute(
                "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) VALUES (?1, ?2)",
                params![id, tag_id],
            )?;
        }
        reindex(&tx, id)?;
        tx.commit()?;
        Ok(Some(diverted))
    }

    /// Sets (or with `None`, clears) a bookmark's summary in its own short transaction: the
    /// summary, `updated_at`, and its search row. Returns false if there's no live bookmark with
    /// that id.
    pub fn set_summary(&mut self, id: i64, summary: Option<&str>) -> Result<bool> {
        let summary = summary.map(str::trim).filter(|s| !s.is_empty());
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE bookmark SET summary = ?2, updated_at = ?3 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, summary, now_ms()],
        )?;
        if n == 0 {
            return Ok(false);
        }
        reindex(&tx, id)?;
        tx.commit()?;
        Ok(true)
    }

    /// Deletes a bookmark, leaving a tombstone for sync: content fields and tags are cleared and
    /// it leaves the search index. Returns false if there's no live bookmark with that id.
    pub fn delete(&mut self, id: i64) -> Result<bool> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE bookmark SET title = NULL, note = NULL, summary = NULL, deleted_at = ?2, \
             updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now],
        )?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute("DELETE FROM bookmark_tag WHERE bookmark_id = ?1", [id])?;
        tx.execute("DELETE FROM tag_suggestion WHERE bookmark_id = ?1", [id])?;
        tx.execute(
            "DELETE FROM tag_proposal_bookmark WHERE bookmark_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM bookmark_fts WHERE rowid = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// Lists live bookmarks, newest first.
    pub fn list(&self, q: &ListQuery) -> Result<Vec<Bookmark>> {
        let mut found = Vec::new();
        self.list_each(q, |b| {
            found.push(b);
            Ok(())
        })?;
        Ok(found)
    }

    /// Like [`Store::list`], but calls `f` for each bookmark as it's read instead of collecting
    /// them. An error from `f` stops the listing and is returned as is.
    pub fn list_each(
        &self,
        q: &ListQuery,
        mut f: impl FnMut(Bookmark) -> Result<()>,
    ) -> Result<()> {
        let mut sql = format!("{SELECT} WHERE b.deleted_at IS NULL");
        let mut args = Vec::new();
        if !self.tag_filter(&q.tags, &mut sql, &mut args)? {
            return Ok(());
        }
        sql.push_str(" ORDER BY b.created_at DESC, b.id DESC LIMIT ?");
        args.push(Value::Integer(i64::try_from(q.limit).unwrap_or(-1)));
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        while let Some(row) = rows.next()? {
            f(Bookmark::from_row(row)?)?;
        }
        Ok(())
    }

    /// Searches live bookmarks; see [`search`] for the query language. Results are ranked by
    /// relevance, or newest first if the query has only filters.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Bookmark>> {
        let mut found = Vec::new();
        self.search_each(query, limit, |b| {
            found.push(b);
            Ok(())
        })?;
        Ok(found)
    }

    /// Like [`Store::search`], but calls `f` for each bookmark as it's found instead of
    /// collecting them. An error from `f` stops the search and is returned as is.
    pub fn search_each(
        &self,
        query: &str,
        limit: usize,
        mut f: impl FnMut(Bookmark) -> Result<()>,
    ) -> Result<()> {
        let q = search::parse(query);
        let filtered = !q.tags.is_empty() || !q.sites.is_empty();
        let mut args = Vec::new();
        // Rank ids first, then load each row as it's emitted. Loading full rows (with their
        // tags) inside the ranked query would do it for every match, not just the ones returned.
        let mut sql = match &q.fts {
            // The index only holds live bookmarks, so without filters it can rank alone.
            Some(fts) if !filtered => {
                args.push(Value::Text(fts.clone()));
                "SELECT rowid FROM bookmark_fts WHERE bookmark_fts MATCH ?".to_string()
            }
            Some(fts) => {
                args.push(Value::Text(fts.clone()));
                "SELECT b.id FROM bookmark_fts JOIN bookmark b ON b.id = bookmark_fts.rowid \
                 WHERE bookmark_fts MATCH ?"
                    .to_string()
            }
            None => "SELECT b.id FROM bookmark b WHERE b.deleted_at IS NULL".to_string(),
        };
        if !self.tag_filter(&q.tags, &mut sql, &mut args)? {
            return Ok(());
        }
        for site in &q.sites {
            sql.push_str(" AND on_site(b.url_norm, ?)");
            args.push(Value::Text(site.clone()));
        }
        sql.push_str(&match q.fts {
            Some(_) => format!(" ORDER BY {BM25}, bookmark_fts.rowid DESC LIMIT ?"),
            None => " ORDER BY b.created_at DESC, b.id DESC LIMIT ?".to_string(),
        });
        args.push(Value::Integer(i64::try_from(limit).unwrap_or(-1)));

        let invalid = |e: rusqlite::Error| {
            if e.to_string().contains("fts5") {
                anyhow::anyhow!("invalid search query {query:?}: {e}")
            } else {
                e.into()
            }
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(params_from_iter(args)).map_err(invalid)?;
        while let Some(row) = rows.next().map_err(invalid)? {
            let id = row.get(0)?;
            f(self.get(id)?.context("bookmark vanished during search")?)?;
        }
        Ok(())
    }

    /// The largest bookmark id ever used (0 if none), e.g. to size an id column.
    pub fn max_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT coalesce(max(id), 0) FROM bookmark", [], |r| {
                r.get(0)
            })?)
    }

    /// Appends one EXISTS clause per tag. Returns false if a tag doesn't exist, in which case
    /// nothing can match.
    fn tag_filter(&self, tags: &[String], sql: &mut String, args: &mut Vec<Value>) -> Result<bool> {
        for tag in tags {
            let Some(tag_id) =
                resolve_tag(&self.conn, &lookup_name(&self.conn, &self.style, tag)?)?
            else {
                return Ok(false);
            };
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM bookmark_tag bt \
                 WHERE bt.bookmark_id = b.id AND bt.tag_id = ?)",
            );
            args.push(Value::Integer(tag_id));
        }
        Ok(true)
    }
}

/// Completes and checks a URL; returns it and its normalized form.
pub fn check_url(input: &str) -> Result<(String, String)> {
    let full = url::complete(input);
    if full.trim() == "https://" || full.chars().any(char::is_whitespace) {
        bail!("invalid URL: {input:?}");
    }
    let norm = url::normalize(&full);
    Ok((full, norm))
}

/// A bookmark to insert, or to merge into the live one with the same normalized URL; see
/// [`put`].
pub(crate) struct Put<'a> {
    /// Completed and checked (see [`check_url`]).
    pub url: &'a str,
    pub norm: &'a str,
    pub title: Option<&'a str>,
    pub note: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// Normalized.
    pub tags: &'a [String],
    /// Kept for a new bookmark if it's a valid UUID no other bookmark (live or deleted) has;
    /// otherwise a new one is generated.
    pub uid: Option<&'a str>,
    pub created_at: i64,
    /// For a new bookmark; an updated one gets `now`.
    pub updated_at: i64,
}

/// Inserts a bookmark, or, if its normalized URL is already bookmarked, updates that one: given
/// title, note, and summary replace the old ones, tags are added, and the earlier `created_at`
/// wins. Callers check strict mode first and reindex the bookmark after.
pub(crate) fn put(tx: &Transaction, p: &Put, now: i64) -> Result<Added> {
    let existing: Option<i64> = tx
        .prepare_cached("SELECT id FROM bookmark WHERE url_norm = ?1 AND deleted_at IS NULL")?
        .query_row([p.norm], |r| r.get(0))
        .optional()?;
    let added = match existing {
        Some(id) => {
            tx.prepare_cached(
                "UPDATE bookmark SET title = coalesce(?2, title), note = coalesce(?3, note), \
                 summary = coalesce(?4, summary), created_at = min(created_at, ?5), \
                 updated_at = ?6 WHERE id = ?1",
            )?
            .execute(params![id, p.title, p.note, p.summary, p.created_at, now])?;
            Added {
                id,
                created: false,
                diverted: Diverted::default(),
            }
        }
        None => {
            let uid = match p.uid.and_then(|u| uuid::Uuid::parse_str(u).ok()) {
                Some(u) if !uid_taken(tx, &u.to_string())? => u.to_string(),
                _ => new_uid(),
            };
            tx.prepare_cached(
                "INSERT INTO bookmark (uid, url, url_norm, title, note, summary, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                uid,
                p.url,
                p.norm,
                p.title,
                p.note,
                p.summary,
                p.created_at,
                p.updated_at
            ])?;
            Added {
                id: tx.last_insert_rowid(),
                created: true,
                diverted: Diverted::default(),
            }
        }
    };
    let mut link = tx.prepare_cached(
        "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) VALUES (?1, ?2)",
    )?;
    for tag in p.tags {
        link.execute(params![added.id, ensure_tag(tx, tag, now)?])?;
    }
    Ok(added)
}

/// Whether any bookmark, deleted ones included, has this uid.
fn uid_taken(tx: &Transaction, uid: &str) -> Result<bool> {
    Ok(tx
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM bookmark WHERE uid = ?1)")?
        .query_row([uid], |r| r.get(0))?)
}

/// The name a tag entering from outside (typed, imported, proposed) stands for: a name that
/// already resolves to a tag or alias as is, else normalized and fixed by `style` and checked
/// against it. Fails for an empty name and one the style refuses.
pub(crate) fn styled_name(conn: &Connection, style: &TagStyle, raw: &str) -> Result<String> {
    let base = normalize_tag(raw);
    if base.is_empty() {
        bail!("empty tag name");
    }
    if style.is_default() || resolve_tag(conn, &base)?.is_some() {
        return Ok(base);
    }
    let fixed = style.fix(&base);
    if fixed.is_empty() {
        bail!("{raw:?} is not a valid tag name");
    }
    if fixed != base && resolve_tag(conn, &fixed)?.is_some() {
        return Ok(fixed);
    }
    style.check(&fixed).map_err(|why| anyhow!(why))?;
    Ok(fixed)
}

/// The name to look up for `raw`, which names an existing tag or alias: normalized, or if that
/// resolves to nothing, fixed by `style` (so `Home Lab` finds `home_lab`). Never fails; a name
/// that resolves to nothing either way comes back normalized.
pub(crate) fn lookup_name(conn: &Connection, style: &TagStyle, raw: &str) -> Result<String> {
    let base = normalize_tag(raw);
    if style.is_default() || resolve_tag(conn, &base)?.is_some() {
        return Ok(base);
    }
    let fixed = style.fix(&base);
    if !fixed.is_empty() && resolve_tag(conn, &fixed)?.is_some() {
        return Ok(fixed);
    }
    Ok(base)
}

/// Finds a live tag by alias or name.
pub(crate) fn resolve_tag(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT coalesce((SELECT tag_id FROM tag_alias WHERE alias = ?1), \
         (SELECT id FROM tag WHERE name = ?1 AND deleted_at IS NULL))",
    )?;
    Ok(stmt.query_row([name], |r| r.get(0))?)
}

/// Finds a tag by alias or name, creating (or reviving) it if needed.
pub(crate) fn ensure_tag(tx: &Transaction, name: &str, now: i64) -> Result<i64> {
    if let Some(id) = resolve_tag(tx, name)? {
        return Ok(id);
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO tag (uid, name, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT (name) DO UPDATE SET deleted_at = NULL, updated_at = ?3 RETURNING id",
    )?;
    stmt.query_row(params![new_uid(), name, now], |r| r.get(0))
        .with_context(|| format!("cannot create tag {name:?}"))
}

/// Inserts full-text rows for live bookmarks; callers append a filter on `b.id`.
const FTS_INSERT: &str = "INSERT INTO bookmark_fts (rowid, title, url, note, summary, tags) \
     SELECT b.id, b.title, b.url, b.note, b.summary, \
     (SELECT group_concat(t.name, ' ') FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
      WHERE bt.bookmark_id = b.id) \
     FROM bookmark b WHERE b.deleted_at IS NULL";

/// Rewrites a bookmark's row in the full-text index.
pub(crate) fn reindex(tx: &Transaction, id: i64) -> Result<()> {
    tx.prepare_cached("DELETE FROM bookmark_fts WHERE rowid = ?1")?
        .execute([id])?;
    tx.prepare_cached(&format!("{FTS_INSERT} AND b.id = ?1"))?
        .execute([id])?;
    Ok(())
}

/// Bumps `updated_at` and rewrites the full-text rows of many bookmarks at once, after their
/// tags changed. Set-based statements are about 3× faster than [`reindex`] in a loop (1.1 s
/// against 3.2 s for 47k bookmarks).
pub(crate) fn touch_many(tx: &Transaction, ids: &[i64], now: i64) -> Result<()> {
    fill_touched(tx, ids)?;
    tx.execute(
        "UPDATE bookmark SET updated_at = ?1 WHERE id IN (SELECT id FROM touched)",
        [now],
    )?;
    reindex_touched(tx)
}

/// Rewrites the full-text rows of many bookmarks at once; see [`touch_many`].
pub(crate) fn reindex_many(tx: &Transaction, ids: &[i64]) -> Result<()> {
    fill_touched(tx, ids)?;
    reindex_touched(tx)
}

/// Fills the temporary `touched` table with `ids`.
fn fill_touched(tx: &Transaction, ids: &[i64]) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS touched (id INTEGER PRIMARY KEY); DELETE FROM touched;",
    )?;
    let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO touched (id) VALUES (?1)")?;
    for &id in ids {
        insert.execute([id])?;
    }
    Ok(())
}

/// Rewrites the full-text rows of the bookmarks in `touched`, then empties it.
fn reindex_touched(tx: &Transaction) -> Result<()> {
    tx.execute(
        "DELETE FROM bookmark_fts WHERE rowid IN (SELECT id FROM touched)",
        [],
    )?;
    tx.execute(
        &format!("{FTS_INSERT} AND b.id IN (SELECT id FROM touched)"),
        [],
    )?;
    tx.execute("DELETE FROM touched", [])?;
    Ok(())
}
