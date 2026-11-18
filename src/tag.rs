//! Tag management: listing, rename, merge, aliases, and the canonical list.
//!
//! The database holds the canonical list. A list declared in config is a pinned base on top of
//! it ([`Store::pin`]): pinned tags always exist and are canonical, with their declared aliases
//! and descriptions, and edits that would undo that fail with [`Pinned`].
//!
//! Invariants kept here: a name is never both a live tag and an alias, and aliases point at live
//! tags. Every change to a bookmark's tags bumps that bookmark's `updated_at` and reindexes it in
//! the same transaction; every change to a tag or its aliases bumps the tag's `updated_at`.

use std::collections::{BTreeMap, HashMap};

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
    /// What belongs under this tag, for the tagging layers.
    pub description: Option<String>,
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
    /// The text after `#`, if any.
    pub description: Option<String>,
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
    /// Descriptions set or changed.
    pub described: usize,
}

/// Parses a canonical list: one tag per line followed by its aliases, separated by whitespace
/// (`javascript js ecmascript`). Blank lines are skipped. A word starting with `#` starts a
/// comment that runs to the end of the line; on a tag's line, that comment is the tag's
/// description (`infra ops # servers, deployment, CI`). Names are normalized.
pub fn parse_canonical(text: &str) -> Vec<CanonicalEntry> {
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // The comment starts at the first word that starts with `#`.
        let mut cut = line.len();
        let mut at = 0;
        for w in line.split_whitespace() {
            let pos = at + line[at..].find(w).unwrap_or(0);
            if w.starts_with('#') {
                cut = pos;
                break;
            }
            at = pos + w.len();
        }
        let (words, comment) = line.split_at(cut);
        let mut words = words.split_whitespace();
        let Some(tag) = words.next() else { continue };
        let description = comment.strip_prefix('#').map(str::trim);
        entries.push(CanonicalEntry {
            line: i + 1,
            tag: normalize_tag(tag),
            aliases: words.map(normalize_tag).collect(),
            description: description.filter(|d| !d.is_empty()).map(str::to_string),
        });
    }
    entries
}

/// Formats tags as a canonical list that [`parse_canonical`] reads back, descriptions included.
pub fn format_canonical(tags: &[TagInfo]) -> String {
    let mut out = String::new();
    for t in tags {
        out.push_str(&t.name);
        for a in &t.aliases {
            out.push(' ');
            out.push_str(a);
        }
        if let Some(d) = &t.description {
            out.push_str(" # ");
            out.push_str(d);
        }
        out.push('\n');
    }
    out
}

/// Formats canonical tags as a `[tags.canonical]` TOML table, the config's shape. Tags in
/// `pinned` get a `# pinned` comment.
pub fn format_canonical_toml(tags: &[TagInfo], pinned: Option<&DeclaredTags>) -> String {
    let key = |s: &str| {
        let bare = !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if bare { s.to_string() } else { toml_string(s) }
    };
    let list = |v: &[String]| {
        let items: Vec<String> = v.iter().map(|a| toml_string(a)).collect();
        format!("[{}]", items.join(", "))
    };
    let mut out = String::from("[tags.canonical]\n");
    for t in tags {
        out.push_str(&key(&t.name));
        out.push_str(" = ");
        match &t.description {
            None => out.push_str(&list(&t.aliases)),
            Some(d) => {
                let (a, d) = (list(&t.aliases), toml_string(d));
                out.push_str(&format!("{{ aliases = {a}, description = {d} }}"));
            }
        }
        if pinned.is_some_and(|p| p.is_tag(&t.name)) {
            out.push_str(" # pinned");
        }
        out.push('\n');
    }
    out
}

/// Formats canonical tags as the home-manager module's `settings.tags.canonical` attrset. Tags in
/// `pinned` get a `# pinned` comment.
pub fn format_canonical_nix(tags: &[TagInfo], pinned: Option<&DeclaredTags>) -> String {
    const KEYWORDS: &[&str] = &[
        "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
    ];
    let key = |s: &str| {
        let mut bytes = s.bytes();
        let bare = bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b"_'-".contains(&b))
            && !KEYWORDS.contains(&s);
        if bare { s.to_string() } else { nix_string(s) }
    };
    let list = |v: &[String]| {
        let mut s = String::from("[ ");
        for a in v {
            s.push_str(&nix_string(a));
            s.push(' ');
        }
        s.push(']');
        s
    };
    let mut out = String::from("programs.shlv.settings.tags.canonical = {\n");
    for t in tags {
        out.push_str("  ");
        out.push_str(&key(&t.name));
        out.push_str(" = ");
        match &t.description {
            None => out.push_str(&list(&t.aliases)),
            Some(d) => {
                let (a, d) = (list(&t.aliases), nix_string(d));
                out.push_str(&format!("{{ aliases = {a}; description = {d}; }}"));
            }
        }
        out.push(';');
        if pinned.is_some_and(|p| p.is_tag(&t.name)) {
            out.push_str(" # pinned");
        }
        out.push('\n');
    }
    out.push_str("};\n");
    out
}

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A Nix double-quoted string; `${` is escaped so nothing is interpolated.
fn nix_string(s: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `meta` key holding [`DeclaredTags::hash`] of the list last synced into the database.
const DECLARED_HASH: &str = "canonical_hash";

/// Longest tag description, in characters.
pub const MAX_DESCRIPTION: usize = 200;

/// Checks a tag description: one line of at most [`MAX_DESCRIPTION`] characters. Returns it
/// trimmed; blank means none.
pub fn check_description(text: &str) -> Result<Option<String>, String> {
    let t = text.trim();
    if t.contains(['\n', '\r']) {
        return Err("a tag description must be one line".into());
    }
    let n = t.chars().count();
    if n > MAX_DESCRIPTION {
        return Err(format!(
            "a tag description can be at most {MAX_DESCRIPTION} characters; this one has {n}"
        ));
    }
    Ok((!t.is_empty()).then(|| t.to_string()))
}

/// One declared tag: its aliases and an optional description. In config it is either a list of
/// aliases (`rust = ["rs"]`) or a table (`infra = { aliases = ["ops"], description = "..." }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredEntry {
    pub aliases: Vec<String>,
    pub description: Option<String>,
}

/// The table form; unknown keys are errors.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryTable {
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

impl<'de> Deserialize<'de> for DeclaredEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::value::{MapAccessDeserializer, SeqAccessDeserializer};

        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = DeclaredEntry;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of aliases, or a table with `aliases` and `description`")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                a: A,
            ) -> Result<Self::Value, A::Error> {
                Ok(DeclaredEntry {
                    aliases: Vec::deserialize(SeqAccessDeserializer::new(a))?,
                    description: None,
                })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                a: A,
            ) -> Result<Self::Value, A::Error> {
                let t = EntryTable::deserialize(MapAccessDeserializer::new(a))?;
                let description = match t.description {
                    Some(d) => check_description(&d).map_err(serde::de::Error::custom)?,
                    None => None,
                };
                Ok(DeclaredEntry {
                    aliases: t.aliases,
                    description,
                })
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for DeclaredEntry {
    /// The list form when there is no description, so `config show` prints what was written.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match &self.description {
            None => self.aliases.serialize(s),
            Some(d) => {
                let mut t = s.serialize_struct("DeclaredEntry", 2)?;
                t.serialize_field("aliases", &self.aliases)?;
                t.serialize_field("description", d)?;
                t.end()
            }
        }
    }
}

/// A canonical list declared in config: tag names mapped to their aliases and descriptions. Built
/// only through validation, so names are normalized, no alias belongs to two tags, and no alias is
/// also a declared tag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "BTreeMap<String, DeclaredEntry>")]
pub struct DeclaredTags {
    /// Aliases are sorted, so the hash doesn't depend on their order in the file.
    tags: BTreeMap<String, DeclaredEntry>,
    /// Alias to tag.
    aliases: HashMap<String, String>,
}

impl DeclaredTags {
    /// Validates a declared list. Names must already be normalized: config is a source of truth,
    /// so `"Web Dev"` is an error rather than quietly becoming `web-dev`.
    pub fn new(tags: BTreeMap<String, DeclaredEntry>) -> Result<Self, String> {
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
        for (tag, mut entry) in tags {
            check(&tag)?;
            for a in &entry.aliases {
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
            if let Some(d) = &entry.description {
                entry.description = check_description(d).map_err(|e| format!("{tag:?}: {e}"))?;
            }
            entry.aliases.sort_unstable();
            sorted.insert(tag, entry);
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
        self.tags
            .iter()
            .map(|(t, e)| (t.as_str(), e.aliases.as_slice()))
    }

    /// A declared tag's description.
    pub fn description(&self, tag: &str) -> Option<&str> {
        self.tags.get(tag)?.description.as_deref()
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

    /// The declared tag that `name` (normalized) is a declared alias of.
    pub fn alias_of(&self, name: &str) -> Option<&str> {
        self.aliases.get(name).map(String::as_str)
    }

    /// The declared tag that `name` (normalized) names or is an alias of.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        match self.tags.get_key_value(name) {
            Some((t, _)) => Some(t),
            None => self.aliases.get(name).map(String::as_str),
        }
    }

    /// A 64-bit FNV-1a hash of the list in canonical-list format, descriptions included, as 16
    /// hex digits. It only detects changes, so it needs no collision resistance; the leading
    /// version tag lets the format change at the cost of one resync (v2 added descriptions).
    pub fn hash(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |s: &str| {
            for b in s.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed("v2\n");
        for (tag, entry) in &self.tags {
            feed(tag);
            for a in &entry.aliases {
                feed(" ");
                feed(a);
            }
            if let Some(d) = &entry.description {
                feed(" # ");
                feed(d);
            }
            feed("\n");
        }
        format!("{h:016x}")
    }
}

impl TryFrom<BTreeMap<String, DeclaredEntry>> for DeclaredTags {
    type Error = String;
    fn try_from(tags: BTreeMap<String, DeclaredEntry>) -> Result<Self, String> {
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
    /// Aliases added or repointed.
    pub aliases_added: usize,
    /// Aliases removed because their name is now a declared tag.
    pub aliases_removed: usize,
    /// Declared tags whose description was set or changed.
    pub described: usize,
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

/// An edit refused because config pins what it would change: a tag, alias, or description of
/// the declared list (see [`Store::pin`]), an interest, or a domain rule. The message says what
/// was refused and the config key that pins it; callers add where the config is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned(pub String, pub &'static str);

impl std::fmt::Display for Pinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: it is pinned by {} in the config", self.0, self.1)
    }
}

impl std::error::Error for Pinned {}

pub(crate) fn pinned(what: String) -> anyhow::Error {
    pinned_by(what, "[tags.canonical]")
}

pub(crate) fn pinned_by(what: String, key: &'static str) -> anyhow::Error {
    Pinned(what, key).into()
}

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
              WHERE a.tag_id = t.id), t.description \
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
                description: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Renames a tag in place, keeping its row and `uid`. Fails if `new` is already a tag (merge
    /// instead) or an alias of another tag. If `new` is an alias of this tag, the alias goes away.
    pub fn rename_tag(&mut self, old: &str, new: &str) -> Result<Retagged> {
        let (old, new) = (self.lookup(old)?, self.new_name(new)?);
        if self.pinned.as_ref().is_some_and(|p| p.is_tag(&old)) {
            return Err(pinned(format!("cannot rename pinned tag {old}")));
        }
        let now = now_ms();
        let forget = self.pinned.is_none();
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
        crate::rules::rename_rule_tag(&tx, &old, &new)?;
        tx.execute(
            "UPDATE tag SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, new, now],
        )?;
        let bookmarks = tagged(&tx, id)?;
        touch_many(&tx, &bookmarks, now)?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        self.reload_rules()?;
        Ok(Retagged {
            tag: new,
            bookmarks: bookmarks.len(),
        })
    }

    /// Moves every bookmark from tag `from` to tag `into` (a name or alias), tombstones `from`,
    /// and leaves `from` as an alias of `into`. Aliases of `from` move to `into`, and `into`
    /// becomes canonical if `from` was. A pinned tag can't be merged away, but other tags can be
    /// merged into it.
    pub fn merge_tags(&mut self, from: &str, into: &str) -> Result<Retagged> {
        let (from, into) = (self.lookup(from)?, self.lookup(into)?);
        if self.pinned.as_ref().is_some_and(|p| p.is_tag(&from)) {
            return Err(pinned(format!("cannot merge away pinned tag {from}")));
        }
        let now = now_ms();
        let forget = self.pinned.is_none();
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
        forget_declared(&tx, forget)?;
        tx.commit()?;
        self.reload_rules()?;
        Ok(Retagged { tag, bookmarks })
    }

    /// Makes `alias` resolve to the live tag `tag`. Returns false if it already did. Fails if
    /// `alias` is a live tag or already an alias of another tag.
    pub fn add_alias(&mut self, alias: &str, tag: &str) -> Result<bool> {
        let (alias, tag) = (self.alias_name(alias)?, self.lookup(tag)?);
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let tag_id = require_tag(&tx, &tag)?;
        let added = add_alias(&tx, &alias, tag_id, &tag, now_ms())?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(added)
    }

    /// Removes an alias. Returns the name of the tag it pointed at, or `None` if there was no
    /// such alias.
    pub fn remove_alias(&mut self, alias: &str) -> Result<Option<String>> {
        let alias = self.lookup(alias)?;
        if let Some(tag) = self.pinned.as_ref().and_then(|p| p.alias_of(&alias)) {
            return Err(pinned(format!(
                "cannot remove alias {alias} of pinned tag {tag}"
            )));
        }
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let Some((tag_id, tag)) = alias_target(&tx, &alias)? else {
            return Ok(None);
        };
        tx.execute("DELETE FROM tag_alias WHERE alias = ?1", [&alias])?;
        touch_tag(&tx, tag_id, now_ms())?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(Some(tag))
    }

    /// Marks tags canonical, creating them if needed. Returns how many changed.
    pub fn add_canonical(&mut self, tags: &[String]) -> Result<usize> {
        let tags = self.tag_names(tags)?;
        let now = now_ms();
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for t in &tags {
            changed += usize::from(mark_canonical(&tx, t, now)?);
        }
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Unmarks canonical tags; the tags and their bookmarks stay. Returns how many changed.
    /// Pinned tags can't be unmarked.
    pub fn remove_canonical(&mut self, tags: &[String]) -> Result<usize> {
        let tags = self.lookup_names(tags)?;
        if let Some(t) = tags
            .iter()
            .find(|t| self.pinned.as_ref().is_some_and(|p| p.is_tag(t)))
        {
            return Err(pinned(format!("cannot unmark pinned tag {t}")));
        }
        let now = now_ms();
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for t in &tags {
            let id = require_tag(&tx, t)?;
            changed += tx.execute(
                "UPDATE tag SET canonical = 0, updated_at = ?2 WHERE id = ?1 AND canonical",
                params![id, now],
            )?;
        }
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Sets (or with blank `text`, clears) the description of live tag `tag`. Returns the tag's
    /// name and whether anything changed. A description declared for a pinned tag can't be
    /// changed; a pinned tag declared without one can be described here.
    pub fn describe_tag(&mut self, tag: &str, text: &str) -> Result<(String, bool)> {
        let name = self.lookup(tag)?;
        let text = check_description(text).map_err(anyhow::Error::msg)?;
        self.check_pinned_description(&name, text.as_deref())?;
        let tx = self.conn.transaction()?;
        let id = require_tag(&tx, &name)?;
        let changed = set_description(&tx, id, text.as_deref(), now_ms())?;
        tx.commit()?;
        Ok((name, changed))
    }

    /// Adds a canonical list (see [`parse_canonical`]): marks each tag canonical, creating it if
    /// needed, and adds its aliases and descriptions. Nothing is removed: a line without a
    /// description keeps the tag's current one. All or nothing: on an error (such as an alias that
    /// is a live tag, or a description that differs from a pinned one) nothing changes.
    pub fn import_canonical(&mut self, entries: &[CanonicalEntry]) -> Result<ImportStats> {
        // Names are styled first: new tags must follow the style, and existing ones keep their
        // names.
        let styled = entries
            .iter()
            .map(|e| {
                let at = || format!("line {}", e.line);
                Ok(CanonicalEntry {
                    tag: self.styled(&e.tag).with_context(at)?,
                    aliases: (e.aliases.iter())
                        .map(|a| self.alias_name(a))
                        .collect::<Result<_>>()
                        .with_context(at)?,
                    ..e.clone()
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let entries = &styled;
        for e in entries {
            if let Some(d) = &e.description {
                let d = check_description(d).map_err(anyhow::Error::msg);
                d.and_then(|d| self.check_pinned_description(&e.tag, d.as_deref()))
                    .with_context(|| format!("line {}", e.line))?;
            }
        }
        let now = now_ms();
        let forget = self.pinned.is_none();
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
            if let Some(d) = &e.description {
                let d = check_description(d).map_err(|m| anyhow::anyhow!("{}: {m}", at()))?;
                stats.described += usize::from(set_description(&tx, tag_id, d.as_deref(), now)?);
            }
        }
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(stats)
    }

    /// Syncs a declared list into the database (see [`Store::sync_declared`]) and pins it: until
    /// the store is dropped, edits that would undo the declared list fail with [`Pinned`].
    /// Renaming, merging away, or unmarking a pinned tag, removing a declared alias, and changing
    /// a declared description are refused. Everything else works as without a declared list:
    /// other tags can be added, aliased, described, and merged (into pinned tags, too), and
    /// pinned tags can get more aliases.
    pub fn pin(&mut self, declared: DeclaredTags) -> Result<Option<SyncStats>> {
        let stats = self.sync_declared(&declared)?;
        self.pinned = Some(declared);
        Ok(stats)
    }

    /// The declared list pinned by [`Store::pin`], if any.
    pub fn pinned(&self) -> Option<&DeclaredTags> {
        self.pinned.as_ref()
    }

    /// Fails with [`Pinned`] if `tag` is pinned with a declared description other than `text`.
    fn check_pinned_description(&self, tag: &str, text: Option<&str>) -> Result<()> {
        match self.pinned.as_ref().and_then(|p| p.description(tag)) {
            Some(d) if Some(d) != text => Err(pinned(format!(
                "cannot change the description of pinned tag {tag}"
            ))),
            _ => Ok(()),
        }
    }

    /// Adds a declared canonical list to the database, unless the list's hash matches the one
    /// stored by the last sync; then it costs one indexed read and returns `None`. Otherwise, in
    /// one transaction:
    ///
    /// - declared tags exist and are canonical (a declared name that was an alias stops being
    ///   one; its old tag keeps its bookmarks);
    /// - declared aliases point at their tags (a declared alias that is a live tag is merged into
    ///   its declared tag, as `tag merge` would);
    /// - declared descriptions are set.
    ///
    /// Nothing else is removed: other canonical tags, other aliases (of declared tags, too), and
    /// descriptions of tags declared without one stay. So a tag, alias, or description dropped
    /// from the list stays in the database, no longer pinned.
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
            let id = require_tag(&tx, tag)?;
            if let Some(d) = declared.description(tag) {
                stats.described += usize::from(set_description(&tx, id, Some(d), now)?);
            }
            ids.insert(tag, id);
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

        meta_set(&tx, DECLARED_HASH, &hash)?;
        tx.commit()?;
        self.reload_rules()?;
        Ok(Some(stats))
    }
}

/// Moves every bookmark from live tag `from_id` (named `from`) to live tag `into_id`, tombstones
/// `from`, leaves `from` as an alias of `into`, and moves `from`'s aliases over. `into` becomes
/// canonical if `from` was. Returns how many bookmarks were retagged.
pub(crate) fn merge_into(
    tx: &Transaction,
    from_id: i64,
    from: &str,
    into_id: i64,
    now: i64,
) -> Result<usize> {
    let bookmarks = tagged(tx, from_id)?;
    // Bookmarks that already have `into` keep that row (and its source); the rest move.
    tx.execute(
        "UPDATE OR IGNORE bookmark_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from_id, into_id],
    )?;
    tx.execute("DELETE FROM bookmark_tag WHERE tag_id = ?1", [from_id])?;
    // Suggestions follow the bookmarks. Where the bookmark already has a row for `into`, that
    // row wins, so a rejection of `into` outlasts a pending `from`.
    tx.execute(
        "UPDATE OR IGNORE tag_suggestion SET tag_id = ?2 WHERE tag_id = ?1",
        params![from_id, into_id],
    )?;
    tx.execute("DELETE FROM tag_suggestion WHERE tag_id = ?1", [from_id])?;
    // So do the interests it was kept under.
    tx.execute(
        "UPDATE OR IGNORE interest_tag SET tag_id = ?2 WHERE tag_id = ?1",
        params![from_id, into_id],
    )?;
    tx.execute("DELETE FROM interest_tag WHERE tag_id = ?1", [from_id])?;
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
    let into: String = tx.query_row("SELECT name FROM tag WHERE id = ?1", [into_id], |r| {
        r.get(0)
    })?;
    crate::rules::rename_rule_tag(tx, from, &into)?;
    touch_many(tx, &bookmarks, now)?;
    Ok(bookmarks.len())
}

/// Drops the stored hash of the declared list when `forget` is set. Every manual change to tags,
/// aliases, or the canonical marks made without a pinned list calls this, so the next run with a
/// declared list syncs it even if the list itself is unchanged (say, after the list was removed
/// from config, a declared tag unmarked, and the same list declared again). With a pinned list,
/// edits that would undo it are refused, so the stored hash stays valid.
pub(crate) fn forget_declared(tx: &Transaction, forget: bool) -> Result<()> {
    if forget {
        meta_delete(tx, DECLARED_HASH)?;
    }
    Ok(())
}

/// A live tag's id by exact name; aliases aren't followed.
pub(crate) fn live_tag(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM tag WHERE name = ?1 AND deleted_at IS NULL")?
        .query_row([name], |r| r.get(0))
        .optional()?)
}

/// The tag an alias points at: (id, name).
pub(crate) fn alias_target(conn: &Connection, alias: &str) -> Result<Option<(i64, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT t.id, t.name FROM tag_alias a JOIN tag t ON t.id = a.tag_id WHERE a.alias = ?1",
        )?
        .query_row([alias], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// A live tag's id by exact name, with an error that says when the name is an alias.
pub(crate) fn require_tag(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = live_tag(conn, name)? {
        return Ok(id);
    }
    match alias_target(conn, name)? {
        Some((_, tag)) => bail!("{name:?} is an alias of {tag:?}, not a tag"),
        None => bail!("no tag {name:?}"),
    }
}

/// Marks a tag canonical, creating or reviving it. Returns false if it already was.
pub(crate) fn mark_canonical(tx: &Transaction, name: &str, now: i64) -> Result<bool> {
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
pub(crate) fn add_alias(
    tx: &Transaction,
    alias: &str,
    tag_id: i64,
    tag: &str,
    now: i64,
) -> Result<bool> {
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

/// Sets a tag's description; returns whether it changed.
pub(crate) fn set_description(
    tx: &Transaction,
    id: i64,
    text: Option<&str>,
    now: i64,
) -> Result<bool> {
    let n = tx.execute(
        "UPDATE tag SET description = ?2, updated_at = ?3 WHERE id = ?1 \
         AND description IS NOT ?2",
        params![id, text, now],
    )?;
    Ok(n > 0)
}
