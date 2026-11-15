//! Tag management: listing, rename, merge, aliases, and the canonical list.
//!
//! Invariants kept here: a name is never both a live tag and an alias, and aliases point at live
//! tags. Every change to a bookmark's tags bumps that bookmark's `updated_at` and reindexes it in
//! the same transaction; every change to a tag or its aliases bumps the tag's `updated_at`.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::store::{Store, ensure_tag, normalize_tag, now_ms, resolve_tag, touch_many};

/// A live tag, as listed by [`Store::tags`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TagInfo {
    pub name: String,
    pub uid: String,
    /// Live bookmarks with this tag.
    pub count: i64,
    pub canonical: bool,
    /// Sorted alias names.
    pub aliases: Vec<String>,
    /// Unix milliseconds.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TagSort {
    /// By name.
    #[default]
    Name,
    /// Most bookmarks first, then by name.
    Count,
}

/// The result of a rename or merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retagged {
    /// The tag the bookmarks now carry.
    pub tag: String,
    /// Bookmarks whose tags changed.
    pub bookmarks: usize,
}

/// One line of a canonical list file: a tag and its aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalEntry {
    /// 1-based line number in the source text (0 when not parsed from text).
    pub line: usize,
    pub tag: String,
    pub aliases: Vec<String>,
}

/// What [`Store::import_canonical`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Lines read (tags in the file).
    pub tags: usize,
    /// Tags newly marked canonical (created if needed).
    pub marked: usize,
    /// Aliases newly added.
    pub aliases: usize,
}

/// Parses a canonical list: one tag per line followed by its aliases, separated by whitespace
/// (`javascript js ecmascript`). Blank lines are skipped, and a word starting with `#` starts a
/// comment that runs to the end of the line. Names are normalized.
pub fn parse_canonical(text: &str) -> Vec<CanonicalEntry> {
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let mut words = line.split_whitespace().take_while(|w| !w.starts_with('#'));
        let Some(tag) = words.next() else { continue };
        entries.push(CanonicalEntry {
            line: i + 1,
            tag: normalize_tag(tag),
            aliases: words.map(normalize_tag).collect(),
        });
    }
    entries
}

/// Formats tags as a canonical list that [`parse_canonical`] reads back.
pub fn format_canonical(tags: &[TagInfo]) -> String {
    let mut out = String::new();
    for t in tags {
        out.push_str(&t.name);
        for a in &t.aliases {
            out.push(' ');
            out.push_str(a);
        }
        out.push('\n');
    }
    out
}

/// Strict mode rejected these tags (normalized, deduplicated, in input order): they aren't
/// canonical or an alias of a canonical tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonCanonicalTags(pub Vec<String>);

impl std::fmt::Display for NonCanonicalTags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tags = self.0.join(" ");
        write!(
            f,
            "not canonical (strict_tags is on): {tags}\n\
             hint: use a canonical tag (`shlv tag canonical ls`), make these canonical \
             (`shlv tag canonical add {tags}`), or alias them to one (`shlv tag alias`)"
        )
    }
}

impl std::error::Error for NonCanonicalTags {}

impl Store {
    /// Of `tags` (already normalized), those that aren't canonical or an alias of a canonical
    /// tag, deduplicated, in input order.
    pub fn non_canonical(&self, tags: &[String]) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM tag t WHERE t.canonical AND t.deleted_at IS NULL \
             AND (t.name = ?1 OR t.id = (SELECT tag_id FROM tag_alias WHERE alias = ?1)))",
        )?;
        let mut bad: Vec<String> = Vec::new();
        for t in tags {
            if !bad.contains(t) && !stmt.query_row([t], |r| r.get::<_, bool>(0))? {
                bad.push(t.clone());
            }
        }
        Ok(bad)
    }

    /// Lists live tags with their bookmark counts and aliases.
    pub fn tags(&self, sort: TagSort) -> Result<Vec<TagInfo>> {
        self.query_tags("", sort)
    }

    /// Lists canonical tags by name, with their aliases.
    pub fn canonical_tags(&self) -> Result<Vec<TagInfo>> {
        self.query_tags(" AND t.canonical", TagSort::Name)
    }

    fn query_tags(&self, filter: &str, sort: TagSort) -> Result<Vec<TagInfo>> {
        let order = match sort {
            TagSort::Name => "t.name",
            TagSort::Count => "5 DESC, t.name",
        };
        let sql = format!(
            "SELECT t.name, t.uid, t.canonical, t.updated_at, \
             (SELECT count(*) FROM bookmark_tag bt WHERE bt.tag_id = t.id), \
             (SELECT group_concat(a.alias, ' ' ORDER BY a.alias) FROM tag_alias a \
              WHERE a.tag_id = t.id) \
             FROM tag t WHERE t.deleted_at IS NULL{filter} ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([], |r| {
            let aliases: Option<String> = r.get(5)?;
            Ok(TagInfo {
                name: r.get(0)?,
                uid: r.get(1)?,
                canonical: r.get(2)?,
                updated_at: r.get(3)?,
                count: r.get(4)?,
                aliases: aliases
                    .map(|a| a.split(' ').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Renames a tag in place, keeping its row and `uid`. Fails if `new` is already a tag (merge
    /// instead) or an alias of another tag. If `new` is an alias of this tag, the alias goes away.
    pub fn rename_tag(&mut self, old: &str, new: &str) -> Result<Retagged> {
        let (old, new) = (tag_name(old)?, tag_name(new)?);
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let id = require_tag(&tx, &old)?;
        if old == new {
            bail!("tag {old:?} is already named that");
        }
        if live_tag(&tx, &new)?.is_some() {
            bail!(
                "tag {new:?} already exists; to combine the two, use `shlv tag merge {old} {new}`"
            );
        }
        match alias_target(&tx, &new)? {
            Some((target, _)) if target == id => {
                tx.execute("DELETE FROM tag_alias WHERE alias = ?1", [&new])?;
            }
            Some((_, name)) => bail!(
                "{new:?} is an alias of {name:?}; remove it first with `shlv tag alias --rm {new}`"
            ),
            None => {}
        }
        free_name(&tx, &new)?;
        tx.execute(
            "UPDATE tag SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, new, now],
        )?;
        let bookmarks = tagged(&tx, id)?;
        touch_many(&tx, &bookmarks, now)?;
        tx.commit()?;
        Ok(Retagged {
            tag: new,
            bookmarks: bookmarks.len(),
        })
    }

    /// Moves every bookmark from tag `from` to tag `into` (a name or alias), tombstones `from`,
    /// and leaves `from` as an alias of `into`. Aliases of `from` move to `into`, and `into`
    /// becomes canonical if `from` was.
    pub fn merge_tags(&mut self, from: &str, into: &str) -> Result<Retagged> {
        let (from, into) = (tag_name(from)?, tag_name(into)?);
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let from_id = require_tag(&tx, &from)?;
        let Some(into_id) = resolve_tag(&tx, &into)? else {
            bail!("no tag {into:?}; to give {from:?} a new name, use `shlv tag rename`");
        };
        if from_id == into_id {
            bail!("{from:?} and {into:?} are the same tag");
        }
        let bookmarks = tagged(&tx, from_id)?;
        // Bookmarks that already have `into` keep that row (and its source); the rest move.
        tx.execute(
            "UPDATE OR IGNORE bookmark_tag SET tag_id = ?2 WHERE tag_id = ?1",
            params![from_id, into_id],
        )?;
        tx.execute("DELETE FROM bookmark_tag WHERE tag_id = ?1", [from_id])?;
        tx.execute(
            "UPDATE tag_alias SET tag_id = ?2 WHERE tag_id = ?1",
            params![from_id, into_id],
        )?;
        tx.execute(
            "INSERT INTO tag_alias (alias, tag_id) VALUES (?1, ?2)",
            params![from, into_id],
        )?;
        tx.execute(
            "UPDATE tag SET updated_at = ?3, \
             canonical = canonical OR (SELECT canonical FROM tag WHERE id = ?2) WHERE id = ?1",
            params![into_id, from_id, now],
        )?;
        tx.execute(
            "UPDATE tag SET deleted_at = ?2, updated_at = ?2, canonical = 0 WHERE id = ?1",
            params![from_id, now],
        )?;
        touch_many(&tx, &bookmarks, now)?;
        let tag = tx.query_row("SELECT name FROM tag WHERE id = ?1", [into_id], |r| {
            r.get(0)
        })?;
        tx.commit()?;
        Ok(Retagged {
            tag,
            bookmarks: bookmarks.len(),
        })
    }

    /// Makes `alias` resolve to the live tag `tag`. Returns false if it already did. Fails if
    /// `alias` is a live tag or already an alias of another tag.
    pub fn add_alias(&mut self, alias: &str, tag: &str) -> Result<bool> {
        let (alias, tag) = (tag_name(alias)?, tag_name(tag)?);
        let tx = self.conn.transaction()?;
        let tag_id = require_tag(&tx, &tag)?;
        let added = add_alias(&tx, &alias, tag_id, &tag, now_ms())?;
        tx.commit()?;
        Ok(added)
    }

    /// Removes an alias. Returns the name of the tag it pointed at, or `None` if there was no
    /// such alias.
    pub fn remove_alias(&mut self, alias: &str) -> Result<Option<String>> {
        let alias = tag_name(alias)?;
        let tx = self.conn.transaction()?;
        let Some((tag_id, tag)) = alias_target(&tx, &alias)? else {
            return Ok(None);
        };
        tx.execute("DELETE FROM tag_alias WHERE alias = ?1", [&alias])?;
        touch_tag(&tx, tag_id, now_ms())?;
        tx.commit()?;
        Ok(Some(tag))
    }

    /// Marks tags canonical, creating them if needed. Returns how many changed.
    pub fn add_canonical(&mut self, tags: &[String]) -> Result<usize> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for t in tags {
            changed += usize::from(mark_canonical(&tx, &tag_name(t)?, now)?);
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Unmarks canonical tags; the tags and their bookmarks stay. Returns how many changed.
    pub fn remove_canonical(&mut self, tags: &[String]) -> Result<usize> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for t in tags {
            let id = require_tag(&tx, &tag_name(t)?)?;
            changed += tx.execute(
                "UPDATE tag SET canonical = 0, updated_at = ?2 WHERE id = ?1 AND canonical",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Adds a canonical list (see [`parse_canonical`]): marks each tag canonical, creating it if
    /// needed, and adds its aliases. Nothing is removed. All or nothing: on an error (such as an
    /// alias that is a live tag) nothing changes.
    pub fn import_canonical(&mut self, entries: &[CanonicalEntry]) -> Result<ImportStats> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut stats = ImportStats::default();
        for e in entries {
            let at = || format!("line {}", e.line);
            stats.tags += 1;
            stats.marked += usize::from(mark_canonical(&tx, &e.tag, now).with_context(at)?);
            let tag_id = require_tag(&tx, &e.tag)?;
            for a in &e.aliases {
                stats.aliases +=
                    usize::from(add_alias(&tx, a, tag_id, &e.tag, now).with_context(at)?);
            }
        }
        tx.commit()?;
        Ok(stats)
    }
}

/// Normalizes a single tag name, rejecting empty ones.
fn tag_name(name: &str) -> Result<String> {
    let n = normalize_tag(name);
    if n.is_empty() {
        bail!("empty tag name");
    }
    Ok(n)
}

/// A live tag's id by exact name; aliases aren't followed.
fn live_tag(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM tag WHERE name = ?1 AND deleted_at IS NULL")?
        .query_row([name], |r| r.get(0))
        .optional()?)
}

/// The tag an alias points at: (id, name).
fn alias_target(conn: &Connection, alias: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT t.id, t.name FROM tag_alias a JOIN tag t ON t.id = a.tag_id WHERE a.alias = ?1",
        )?
        .query_row([alias], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// A live tag's id by exact name, with an error that says when the name is an alias.
fn require_tag(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = live_tag(conn, name)? {
        return Ok(id);
    }
    match alias_target(conn, name)? {
        Some((_, tag)) => bail!("{name:?} is an alias of {tag:?}, not a tag"),
        None => bail!("no tag {name:?}"),
    }
}

/// Marks a tag canonical, creating or reviving it. Returns false if it already was.
fn mark_canonical(tx: &Transaction, name: &str, now: i64) -> Result<bool> {
    if let Some((_, tag)) = alias_target(tx, name)? {
        bail!("{name:?} is an alias of {tag:?}; mark {tag:?} canonical instead");
    }
    let id = ensure_tag(tx, name, now)?;
    Ok(tx.execute(
        "UPDATE tag SET canonical = 1, updated_at = ?2 WHERE id = ?1 AND NOT canonical",
        params![id, now],
    )? > 0)
}

/// Points `alias` at the live tag `tag_id` (named `tag`). Returns false if it already did.
fn add_alias(tx: &Transaction, alias: &str, tag_id: i64, tag: &str, now: i64) -> Result<bool> {
    if alias == tag {
        bail!("{alias:?} can't be an alias of itself");
    }
    if live_tag(tx, alias)?.is_some() {
        bail!("{alias:?} is a tag; to fold it into {tag:?}, use `shlv tag merge {alias} {tag}`");
    }
    match alias_target(tx, alias)? {
        Some((id, _)) if id == tag_id => return Ok(false),
        Some((_, other)) => bail!(
            "{alias:?} is already an alias of {other:?}; remove it first with \
             `shlv tag alias --rm {alias}`"
        ),
        None => {}
    }
    tx.execute(
        "INSERT INTO tag_alias (alias, tag_id) VALUES (?1, ?2)",
        params![alias, tag_id],
    )?;
    touch_tag(tx, tag_id, now)?;
    Ok(true)
}

fn touch_tag(tx: &Transaction, id: i64, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE tag SET updated_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Ids of the bookmarks carrying a tag.
fn tagged(tx: &Transaction, tag_id: i64) -> Result<Vec<i64>> {
    let mut stmt = tx.prepare_cached("SELECT bookmark_id FROM bookmark_tag WHERE tag_id = ?1")?;
    let ids = stmt.query_map([tag_id], |r| r.get(0))?;
    Ok(ids.collect::<rusqlite::Result<_>>()?)
}

/// Frees `name` for a live tag by renaming the tombstone holding it, if any. The tombstone keeps
/// its `uid`, which is what sync matches on; normalized names never contain spaces, so the new
/// name can't collide.
fn free_name(tx: &Transaction, name: &str) -> Result<()> {
    tx.execute(
        "UPDATE tag SET name = uid || ' (deleted)' WHERE name = ?1 AND deleted_at IS NOT NULL",
        [name],
    )?;
    Ok(())
}
