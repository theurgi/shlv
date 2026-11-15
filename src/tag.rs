//! Tag management: listing, rename, merge, aliases, and the canonical list.
//!
//! Invariants kept here: a name is never both a live tag and an alias, and aliases point at live
//! tags. Every change to a bookmark's tags bumps that bookmark's `updated_at` and reindexes it in
//! the same transaction; every change to a tag or its aliases bumps the tag's `updated_at`.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::db::{meta_delete, meta_get, meta_set};
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

/// The `meta` key holding [`DeclaredTags::hash`] of the list last mirrored into the database.
const DECLARED_HASH: &str = "canonical_hash";

/// A canonical list declared in config: tag names mapped to their aliases. Built only through
/// validation, so names are normalized, no alias belongs to two tags, and no alias is also a
/// declared tag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "BTreeMap<String, Vec<String>>")]
pub struct DeclaredTags {
    /// Aliases are sorted, so the hash doesn't depend on their order in the file.
    tags: BTreeMap<String, Vec<String>>,
    /// Alias to tag.
    aliases: HashMap<String, String>,
}

impl DeclaredTags {
    /// Validates a declared list. Names must already be normalized: config is a source of truth,
    /// so `"Web Dev"` is an error rather than quietly becoming `web-dev`.
    pub fn new(tags: BTreeMap<String, Vec<String>>) -> Result<Self, String> {
        let check = |name: &str| {
            let n = normalize_tag(name);
            if n.is_empty() {
                Err(format!("{name:?} is not a valid tag name"))
            } else if n != name {
                Err(format!(
                    "{name:?} is not a normalized tag name; write {n:?}"
                ))
            } else {
                Ok(())
            }
        };
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut sorted = BTreeMap::new();
        for (tag, list) in tags {
            check(&tag)?;
            for a in &list {
                check(a)?;
                if a == &tag {
                    return Err(format!("{tag:?} lists itself as an alias"));
                }
                if let Some(other) = aliases.insert(a.clone(), tag.clone()) {
                    return Err(if other == tag {
                        format!("alias {a:?} is listed twice for {tag:?}")
                    } else {
                        format!("alias {a:?} is declared for both {other:?} and {tag:?}")
                    });
                }
            }
            let mut list = list;
            list.sort_unstable();
            sorted.insert(tag, list);
        }
        if let Some((a, tag)) = aliases.iter().find(|(a, _)| sorted.contains_key(*a)) {
            return Err(format!(
                "{a:?} is declared both as a tag and as an alias of {tag:?}"
            ));
        }
        Ok(DeclaredTags {
            tags: sorted,
            aliases,
        })
    }

    /// Declared tags by name, each with its sorted aliases.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.tags.iter().map(|(t, a)| (t.as_str(), a.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.tags.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// Whether `name` (normalized) is a declared tag.
    pub fn is_tag(&self, name: &str) -> bool {
        self.tags.contains_key(name)
    }

    /// The declared tag that `name` (normalized) names or is an alias of.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        match self.tags.get_key_value(name) {
            Some((t, _)) => Some(t),
            None => self.aliases.get(name).map(String::as_str),
        }
    }

    /// A 64-bit FNV-1a hash of the list in canonical-list format, as 16 hex digits. It only
    /// detects changes, so it needs no collision resistance; the leading version tag lets the
    /// format change later at the cost of one resync.
    pub fn hash(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |s: &str| {
            for b in s.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed("v1\n");
        for (tag, aliases) in &self.tags {
            feed(tag);
            for a in aliases {
                feed(" ");
                feed(a);
            }
            feed("\n");
        }
        format!("{h:016x}")
    }
}

impl TryFrom<BTreeMap<String, Vec<String>>> for DeclaredTags {
    type Error = String;
    fn try_from(tags: BTreeMap<String, Vec<String>>) -> Result<Self, String> {
        DeclaredTags::new(tags)
    }
}

impl Serialize for DeclaredTags {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.tags.serialize(s)
    }
}

/// What [`Store::sync_declared`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Tags newly marked canonical (created if needed).
    pub marked: usize,
    /// Canonical tags not in the list, now unmarked.
    pub unmarked: usize,
    /// Aliases added or repointed.
    pub aliases_added: usize,
    /// Aliases removed: undeclared aliases of declared tags, and aliases whose name is now a
    /// declared tag.
    pub aliases_removed: usize,
    /// Live tags that the list declares as aliases, folded into their tag: (from, into, bookmarks
    /// retagged).
    pub merged: Vec<(String, String, usize)>,
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
        forget_declared(&tx)?;
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
        let bookmarks = merge_into(&tx, from_id, &from, into_id, now)?;
        let tag = tx.query_row("SELECT name FROM tag WHERE id = ?1", [into_id], |r| {
            r.get(0)
        })?;
        forget_declared(&tx)?;
        tx.commit()?;
        Ok(Retagged { tag, bookmarks })
    }

    /// Makes `alias` resolve to the live tag `tag`. Returns false if it already did. Fails if
    /// `alias` is a live tag or already an alias of another tag.
    pub fn add_alias(&mut self, alias: &str, tag: &str) -> Result<bool> {
        let (alias, tag) = (tag_name(alias)?, tag_name(tag)?);
        let tx = self.conn.transaction()?;
        let tag_id = require_tag(&tx, &tag)?;
        let added = add_alias(&tx, &alias, tag_id, &tag, now_ms())?;
        forget_declared(&tx)?;
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
        forget_declared(&tx)?;
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
        forget_declared(&tx)?;
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
        forget_declared(&tx)?;
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
        forget_declared(&tx)?;
        tx.commit()?;
        Ok(stats)
    }

    /// Makes the database mirror a declared canonical list, unless the list's hash matches the
    /// one stored by the last sync; then it costs one indexed read and returns `None`. Otherwise,
    /// in one transaction:
    ///
    /// - declared tags exist and are canonical (a declared name that was an alias stops being
    ///   one; its old tag keeps its bookmarks);
    /// - declared aliases point at their tags (a declared alias that is a live tag is merged into
    ///   its declared tag, as `tag merge` would);
    /// - canonical tags not in the list are unmarked (their bookmarks keep them);
    /// - aliases of declared tags that aren't in the list are removed; aliases of undeclared tags
    ///   (left by `tag merge`, say) stay.
    pub fn sync_declared(&mut self, declared: &DeclaredTags) -> Result<Option<SyncStats>> {
        let hash = declared.hash();
        if meta_get(&self.conn, DECLARED_HASH)?.as_deref() == Some(hash.as_str()) {
            return Ok(None);
        }
        // Take the write lock up front: a deferred transaction upgrading to a write fails at once
        // with SQLITE_BUSY in WAL mode if another process syncs at the same time.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if meta_get(&tx, DECLARED_HASH)?.as_deref() == Some(hash.as_str()) {
            return Ok(None); // another process just did it
        }
        let now = now_ms();
        let mut stats = SyncStats::default();

        let mut ids = HashMap::with_capacity(declared.len());
        for (tag, _) in declared.iter() {
            if let Some((owner, _)) = alias_target(&tx, tag)? {
                tx.execute("DELETE FROM tag_alias WHERE alias = ?1", [tag])?;
                touch_tag(&tx, owner, now)?;
                stats.aliases_removed += 1;
            }
            stats.marked += usize::from(mark_canonical(&tx, tag, now)?);
            ids.insert(tag, require_tag(&tx, tag)?);
        }

        for (tag, aliases) in declared.iter() {
            let id = ids[tag];
            for a in aliases {
                // Validation keeps declared tags out of `aliases`, so this is never one of them.
                if let Some(from) = live_tag(&tx, a)? {
                    let n = merge_into(&tx, from, a, id, now)?;
                    stats.merged.push((a.clone(), tag.to_string(), n));
                    continue;
                }
                match alias_target(&tx, a)? {
                    Some((owner, _)) if owner == id => continue,
                    Some((owner, _)) => {
                        tx.execute(
                            "UPDATE tag_alias SET tag_id = ?2 WHERE alias = ?1",
                            params![a, id],
                        )?;
                        touch_tag(&tx, owner, now)?;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO tag_alias (alias, tag_id) VALUES (?1, ?2)",
                            params![a, id],
                        )?;
                    }
                }
                touch_tag(&tx, id, now)?;
                stats.aliases_added += 1;
            }
        }

        // Only after every declared alias is in place, so one moving between declared tags is
        // repointed rather than deleted and re-added.
        for (tag, aliases) in declared.iter() {
            let id = ids[tag];
            let keep: HashSet<&str> = aliases.iter().map(String::as_str).collect();
            let stale: Vec<String> = tx
                .prepare_cached("SELECT alias FROM tag_alias WHERE tag_id = ?1")?
                .query_map([id], |r| r.get(0))?
                .filter(|a| {
                    a.as_ref()
                        .map_or(true, |a: &String| !keep.contains(a.as_str()))
                })
                .collect::<rusqlite::Result<_>>()?;
            for a in &stale {
                tx.execute("DELETE FROM tag_alias WHERE alias = ?1", [a])?;
            }
            if !stale.is_empty() {
                touch_tag(&tx, id, now)?;
                stats.aliases_removed += stale.len();
            }
        }

        let canonical: Vec<(i64, String)> = tx
            .prepare("SELECT id, name FROM tag WHERE canonical AND deleted_at IS NULL")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, name) in canonical {
            if !declared.is_tag(&name) {
                tx.execute(
                    "UPDATE tag SET canonical = 0, updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                stats.unmarked += 1;
            }
        }

        meta_set(&tx, DECLARED_HASH, &hash)?;
        tx.commit()?;
        Ok(Some(stats))
    }
}

/// Moves every bookmark from live tag `from_id` (named `from`) to live tag `into_id`, tombstones
/// `from`, leaves `from` as an alias of `into`, and moves `from`'s aliases over. `into` becomes
/// canonical if `from` was. Returns how many bookmarks were retagged.
fn merge_into(tx: &Transaction, from_id: i64, from: &str, into_id: i64, now: i64) -> Result<usize> {
    let bookmarks = tagged(tx, from_id)?;
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
    touch_many(tx, &bookmarks, now)?;
    Ok(bookmarks.len())
}

/// Drops the stored hash of the declared list. Every manual change to tags, aliases, or the
/// canonical marks calls this, so the next run with a declared list re-mirrors it even if the
/// list itself is unchanged (say, after declarative mode was turned off, the database edited, and
/// the same list declared again).
fn forget_declared(tx: &Transaction) -> Result<()> {
    meta_delete(tx, DECLARED_HASH)
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
