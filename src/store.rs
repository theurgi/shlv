//! Bookmark storage: add, get, edit, delete, list, search.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};

use crate::{db, search, url};

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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn new_uid() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Empty strings mean "no value".
fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database at `path`, creating it and applying migrations as needed.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Store {
            conn: db::open(path)?,
        })
    }

    /// Opens the database at the default location (see [`db::default_path`]).
    pub fn open_default() -> Result<Self> {
        Self::open(&db::default_path()?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Store {
            conn: db::open_in_memory()?,
        })
    }

    /// The underlying connection, for inspection and tests.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Adds a bookmark. If its normalized URL is already bookmarked, updates that bookmark
    /// instead: given title and note replace the old ones, tags are added.
    pub fn add(&mut self, new: &NewBookmark) -> Result<Added> {
        let full = url::complete(&new.url);
        if full.trim() == "https://" || full.chars().any(char::is_whitespace) {
            bail!("invalid URL: {:?}", new.url);
        }
        let norm = url::normalize(&full);
        let tags = normalize_tags(&new.tags)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bookmark WHERE url_norm = ?1 AND deleted_at IS NULL",
                [&norm],
                |r| r.get(0),
            )
            .optional()?;
        let added = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE bookmark SET title = coalesce(?2, title), note = coalesce(?3, note), \
                     updated_at = ?4 WHERE id = ?1",
                    params![id, non_empty(&new.title), non_empty(&new.note), now],
                )?;
                Added { id, created: false }
            }
            None => {
                tx.execute(
                    "INSERT INTO bookmark (uid, url, url_norm, title, note, created_at, \
                     updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![
                        new_uid(),
                        full,
                        norm,
                        non_empty(&new.title),
                        non_empty(&new.note),
                        now
                    ],
                )?;
                Added {
                    id: tx.last_insert_rowid(),
                    created: true,
                }
            }
        };
        for tag in &tags {
            let tag_id = ensure_tag(&tx, tag, now)?;
            tx.execute(
                "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) VALUES (?1, ?2)",
                params![added.id, tag_id],
            )?;
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
        tx.execute("DELETE FROM bookmark_fts WHERE rowid = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// Lists live bookmarks, newest first.
    pub fn list(&self, q: &ListQuery) -> Result<Vec<Bookmark>> {
        let mut sql = format!("{SELECT} WHERE b.deleted_at IS NULL");
        let mut args = Vec::new();
        if !self.tag_filter(&q.tags, &mut sql, &mut args)? {
            return Ok(Vec::new());
        }
        sql.push_str(" ORDER BY b.created_at DESC, b.id DESC LIMIT ?");
        args.push(Value::Integer(i64::try_from(q.limit).unwrap_or(-1)));
        self.query(&sql, args)
    }

    /// Searches live bookmarks; see [`search`] for the query language. Results are ranked by
    /// relevance, or newest first if the query has only filters.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Bookmark>> {
        let q = search::parse(query);
        let filtered = !q.tags.is_empty() || !q.sites.is_empty();
        let mut args = Vec::new();
        // First find the page of ids, then load those rows. Loading full rows (with their tags)
        // inside the ranked query would do it for every match, not just the page.
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
            return Ok(Vec::new());
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

        let ids: Vec<i64> = (|| {
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(args), |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()
        })()
        .map_err(|e| {
            if e.to_string().contains("fts5") {
                anyhow::anyhow!("invalid search query {query:?}: {e}")
            } else {
                e.into()
            }
        })?;
        ids.into_iter()
            .map(|id| self.get(id)?.context("bookmark vanished during search"))
            .collect()
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

    fn query(&self, sql: &str, args: Vec<Value>) -> Result<Vec<Bookmark>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params_from_iter(args), Bookmark::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn normalize_tags(tags: &[String]) -> Result<Vec<String>> {
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
fn resolve_tag(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT coalesce((SELECT tag_id FROM tag_alias WHERE alias = ?1), \
         (SELECT id FROM tag WHERE name = ?1 AND deleted_at IS NULL))",
    )?;
    Ok(stmt.query_row([name], |r| r.get(0))?)
}

/// Finds a tag by alias or name, creating (or reviving) it if needed.
fn ensure_tag(tx: &Transaction, name: &str, now: i64) -> Result<i64> {
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

/// Rewrites a bookmark's row in the full-text index.
fn reindex(tx: &Transaction, id: i64) -> Result<()> {
    tx.execute("DELETE FROM bookmark_fts WHERE rowid = ?1", [id])?;
    tx.execute(
        "INSERT INTO bookmark_fts (rowid, title, url, note, summary, tags) \
         SELECT b.id, b.title, b.url, b.note, b.summary, \
         (SELECT group_concat(t.name, ' ') FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
          WHERE bt.bookmark_id = b.id) \
         FROM bookmark b WHERE b.id = ?1 AND b.deleted_at IS NULL",
        [id],
    )?;
    Ok(())
}
