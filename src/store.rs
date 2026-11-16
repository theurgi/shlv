//! Bookmark storage: add, get, edit, delete, list, search. Tag commands are in [`crate::tag`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Added {
    pub id: i64,
    /// False if the URL was already bookmarked and that bookmark was updated instead.
    pub created: bool,
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
    strict_tags: bool,
    pub(crate) rules: DomainRules,
    pub(crate) suggest_mode: SuggestMode,
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
            strict_tags: false,
            rules: DomainRules::default(),
            suggest_mode: SuggestMode::default(),
        }
    }

    /// In strict mode, adding a tag that isn't canonical or an alias of a canonical tag fails
    /// with [`NonCanonicalTags`] before anything is written. Off by default.
    pub fn set_strict_tags(&mut self, on: bool) {
        self.strict_tags = on;
    }

    /// Whether strict mode is on; see [`Store::set_strict_tags`].
    pub fn strict_tags(&self) -> bool {
        self.strict_tags
    }

    /// Fails with [`NonCanonicalTags`] if strict mode is on and any of `tags` (normalized) isn't
    /// canonical or an alias of a canonical tag.
    pub(crate) fn check_strict(&self, tags: &[String]) -> Result<()> {
        if !self.strict_tags {
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
    pub fn add(&mut self, new: &NewBookmark) -> Result<Added> {
        let (full, norm) = check_url(&new.url)?;
        let tags = normalize_tags(&new.tags)?;
        self.check_strict(&tags)?;
        self.check_rules()?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let added = put(
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
        let add = normalize_tags(&edit.add_tags)?;
        let remove = normalize_tags(&edit.remove_tags)?;
        self.check_strict(&add)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE bookmark SET updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now],
        )?;
        if n == 0 {
            return Ok(false);
        }
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
            let Some(tag_id) = resolve_tag(&self.conn, &normalize_tag(tag))? else {
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
            Added { id, created: false }
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

pub(crate) fn normalize_tags(tags: &[String]) -> Result<Vec<String>> {
    tags.iter()
        .map(|t| {
            let n = normalize_tag(t);
            if n.is_empty() {
                bail!("empty tag name");
            }
            Ok(n)
        })
        .collect()
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
