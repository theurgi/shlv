//! Tag proposals: names outside the canonical list, waiting for the user to decide (task 031).
//!
//! A proposal is a candidate for the list, never a tag on a bookmark. It has an optional
//! description, the source that proposed it first, and the bookmarks it was proposed for, which
//! it collects across runs. Sources: the `[ai] tag` provider, tags typed with
//! `strict_tags = "propose"`, and import ([`crate::ImportOptions::propose_unknown`]).
//!
//! Decisions: [`Store::accept_proposal`] adds the name to the list and tags its bookmarks,
//! [`Store::alias_proposal`] makes it an alias of a tag and tags the bookmarks with that, and
//! [`Store::reject_proposal`] remembers it, so no source proposes it again.
//!
//! Names are normalized first. A name that is a canonical tag, or an alias of one, isn't a
//! proposal: the AI's becomes a suggestion of that tag. A name that is an alias of a tag off the
//! list is proposed under that tag's name.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::choose::NewTag;
use crate::store::{Store, ensure_tag, normalize_tag, now_ms, resolve_tag, touch_many};
use crate::suggest::{self, OfferStats, Offered, Source};
use crate::tag::{
    add_alias, alias_target, check_description, forget_declared, live_tag, merge_into, pinned,
    set_description, tag_name,
};

/// Who proposed a name. Stored as text in `tag_proposal.source` and `tag_proposal_bookmark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposalSource {
    /// The `[ai] tag` provider.
    Llm,
    /// A tag typed in `add` or `edit` with `strict_tags = "propose"`.
    User,
    /// A tag in an imported file.
    Import,
}

impl ProposalSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalSource::Llm => "llm",
            ProposalSource::User => "user",
            ProposalSource::Import => "import",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "llm" => Ok(ProposalSource::Llm),
            "user" => Ok(ProposalSource::User),
            "import" => Ok(ProposalSource::Import),
            _ => Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("proposal source {s:?}"),
                rusqlite::types::Type::Text,
            )),
        }
    }

    /// The `bookmark_tag.source` a bookmark gets when the proposal is accepted: the AI's tags
    /// stay marked as the AI's (so removing one later counts as rejecting it), the rest are the
    /// user's.
    fn tag_source(self) -> &'static str {
        match self {
            ProposalSource::Llm => Source::Llm.as_str(),
            ProposalSource::User | ProposalSource::Import => "user",
        }
    }
}

/// Strict mode (`strict_tags`): whether typed tags must be canonical (or an alias of a canonical
/// tag), and what happens to those that aren't.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StrictTags {
    /// Any tag goes.
    #[default]
    Off,
    /// Tags off the list are an error ([`crate::NonCanonicalTags`]).
    On,
    /// Tags off the list become proposals for it; the bookmark doesn't get them.
    Propose,
}

impl From<bool> for StrictTags {
    fn from(on: bool) -> Self {
        if on { StrictTags::On } else { StrictTags::Off }
    }
}

impl<'de> Deserialize<'de> for StrictTags {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(d) {
            Ok(Raw::Bool(b)) => Ok(b.into()),
            Ok(Raw::Text(s)) if s == "propose" => Ok(StrictTags::Propose),
            _ => Err(serde::de::Error::custom(
                "expected true, false, or \"propose\"",
            )),
        }
    }
}

impl Serialize for StrictTags {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            StrictTags::Off => s.serialize_bool(false),
            StrictTags::On => s.serialize_bool(true),
            StrictTags::Propose => s.serialize_str("propose"),
        }
    }
}

/// Whether a proposal waits for a decision or was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposalState {
    Pending,
    Rejected,
}

/// A proposal, as listed by [`Store::proposals`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub name: String,
    pub description: Option<String>,
    /// Who proposed it first.
    pub source: ProposalSource,
    pub state: ProposalState,
    /// Live bookmarks it was proposed for, newest first (none once rejected).
    pub bookmarks: Vec<i64>,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds: when it was last proposed or decided.
    pub updated_at: i64,
}

/// Typed tags that weren't canonical, with `strict_tags = "propose"` (or an import that proposes
/// them): recorded as proposals, or dropped because the name was rejected. Normalized, in input
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diverted {
    pub proposed: Vec<String>,
    pub rejected: Vec<String>,
}

impl Diverted {
    pub fn is_empty(&self) -> bool {
        self.proposed.is_empty() && self.rejected.is_empty()
    }
}

/// What [`Store::propose_tags`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposeStats {
    /// Names recorded as proposals for the bookmark (new to it), normalized.
    pub proposed: Vec<String>,
    /// Names that turned out to be canonical tags or their aliases, offered as suggestions.
    pub suggested: OfferStats,
    /// Names dropped because they were rejected.
    pub rejected: Vec<String>,
}

/// The result of accepting or aliasing a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    /// The canonical tag the bookmarks got.
    pub tag: String,
    /// Bookmarks newly tagged with it.
    pub tagged: usize,
    /// Whether the proposal's description was set on the tag.
    pub described: bool,
}

/// What [`propose`] did with one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proposed {
    /// Recorded under this name (the input, or the tag an alias resolves to); `new` when the
    /// bookmark wasn't linked to it before.
    Recorded { name: String, new: bool },
    /// The name was rejected before.
    Rejected,
    /// The name is this canonical tag, or an alias of it.
    Canonical(i64),
}

/// Records `name` (normalized, non-empty) as a proposal, for `bookmark` if given. A description
/// fills in one the proposal doesn't have yet.
pub(crate) fn propose(
    tx: &Transaction,
    name: &str,
    description: Option<&str>,
    source: ProposalSource,
    bookmark: Option<i64>,
    now: i64,
) -> Result<Proposed> {
    let mut name = name.to_string();
    if let Some(id) = resolve_tag(tx, &name)? {
        let (tag, canonical): (String, bool) = tx
            .prepare_cached("SELECT name, canonical FROM tag WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if canonical {
            return Ok(Proposed::Canonical(id));
        }
        name = tag;
    }
    let found: Option<(i64, String)> = tx
        .prepare_cached("SELECT id, state FROM tag_proposal WHERE name = ?1")?
        .query_row([&name], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let id = match found {
        Some((_, state)) if state == "rejected" => return Ok(Proposed::Rejected),
        Some((id, _)) => {
            tx.prepare_cached(
                "UPDATE tag_proposal SET description = coalesce(description, ?2), \
                 updated_at = ?3 WHERE id = ?1",
            )?
            .execute(params![id, description, now])?;
            id
        }
        None => tx
            .prepare_cached(
                "INSERT INTO tag_proposal (name, description, source, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
            )?
            .query_row(params![name, description, source.as_str(), now], |r| {
                r.get(0)
            })?,
    };
    let new = match bookmark {
        Some(b) => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO tag_proposal_bookmark (proposal_id, bookmark_id, source) \
                 VALUES (?1, ?2, ?3)",
            )?
            .execute(params![id, b, source.as_str()])?
                > 0
        }
        None => false,
    };
    Ok(Proposed::Recorded { name, new })
}

/// Proposes typed tags that aren't canonical for a bookmark; see [`Diverted`].
pub(crate) fn divert(
    tx: &Transaction,
    tags: &[String],
    source: ProposalSource,
    bookmark: i64,
    now: i64,
    out: &mut Diverted,
) -> Result<()> {
    for t in tags {
        match propose(tx, t, None, source, Some(bookmark), now)? {
            Proposed::Recorded { name, .. } => {
                if !out.proposed.contains(&name) {
                    out.proposed.push(name);
                }
            }
            Proposed::Rejected => {
                if !out.rejected.contains(t) {
                    out.rejected.push(t.clone());
                }
            }
            // Callers pass only names that aren't canonical; if one became canonical meanwhile,
            // the bookmark just gets it.
            Proposed::Canonical(tag_id) => {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) VALUES (?1, ?2)",
                )?
                .execute(params![bookmark, tag_id])?;
            }
        }
    }
    Ok(())
}

/// A proposal by exact (normalized) name: (id, description, state).
fn find(conn: &Connection, name: &str) -> Result<Option<(i64, Option<String>, String)>> {
    Ok(conn
        .prepare_cached("SELECT id, description, state FROM tag_proposal WHERE name = ?1")?
        .query_row([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?)
}

impl Store {
    /// Records a layer's proposed names for one bookmark, in one short transaction. Names are
    /// normalized (empty ones skipped); a rejected name is dropped. A name that is a canonical
    /// tag or an alias of one is offered as a suggestion of that tag instead (like
    /// [`Store::offer_choices`], source `llm`), or for other sources added as a tag.
    pub fn propose_tags(
        &mut self,
        bookmark: i64,
        source: ProposalSource,
        tags: &[NewTag],
    ) -> Result<ProposeStats> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut stats = ProposeStats::default();
        let mut touched = false;
        for t in tags {
            let name = normalize_tag(&t.name);
            if name.is_empty() {
                continue;
            }
            let description = t
                .description
                .as_deref()
                .and_then(|d| check_description(d).ok().flatten());
            match propose(
                &tx,
                &name,
                description.as_deref(),
                source,
                Some(bookmark),
                now,
            )? {
                Proposed::Recorded { name, new } => {
                    if new && !stats.proposed.contains(&name) {
                        stats.proposed.push(name);
                    }
                }
                Proposed::Rejected => stats.rejected.push(name),
                Proposed::Canonical(tag_id) if source == ProposalSource::Llm => {
                    match suggest::offer(
                        &tx,
                        bookmark,
                        tag_id,
                        Source::Llm,
                        None,
                        self.suggest_mode,
                        now,
                    )? {
                        Offered::Pending => stats.suggested.pending += 1,
                        Offered::Applied => {
                            stats.suggested.applied += 1;
                            touched = true;
                        }
                        Offered::Skipped => {}
                    }
                }
                Proposed::Canonical(tag_id) => {
                    let n = tx
                        .prepare_cached(
                            "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) \
                             VALUES (?1, ?2)",
                        )?
                        .execute(params![bookmark, tag_id])?;
                    stats.suggested.applied += n;
                    touched |= n > 0;
                }
            }
        }
        if touched {
            touch_many(&tx, &[bookmark], now)?;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Proposals in `state`, those on the most bookmarks first, then by name.
    pub fn proposals(&self, state: ProposalState) -> Result<Vec<Proposal>> {
        let state = match state {
            ProposalState::Pending => "pending",
            ProposalState::Rejected => "rejected",
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.name, p.description, p.source, p.state, p.created_at, p.updated_at, \
             (SELECT group_concat(bookmark_id, ' ' ORDER BY bookmark_id DESC) \
              FROM tag_proposal_bookmark WHERE proposal_id = p.id) \
             FROM tag_proposal p WHERE p.state = ?1 \
             ORDER BY (SELECT count(*) FROM tag_proposal_bookmark WHERE proposal_id = p.id) DESC, \
             p.name",
        )?;
        let rows = stmt.query_map([state], |r| {
            let source: String = r.get(2)?;
            let state: String = r.get(3)?;
            let ids: Option<String> = r.get(6)?;
            Ok(Proposal {
                name: r.get(0)?,
                description: r.get(1)?,
                source: ProposalSource::parse(&source)?,
                state: if state == "rejected" {
                    ProposalState::Rejected
                } else {
                    ProposalState::Pending
                },
                bookmarks: ids
                    .map(|s| s.split(' ').filter_map(|i| i.parse().ok()).collect())
                    .unwrap_or_default(),
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Names pending as proposals for `bookmark`, sorted.
    pub fn proposals_for(&self, bookmark: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.name FROM tag_proposal_bookmark pb \
             JOIN tag_proposal p ON p.id = pb.proposal_id \
             WHERE pb.bookmark_id = ?1 AND p.state = 'pending' ORDER BY p.name",
        )?;
        let names = stmt.query_map([bookmark], |r| r.get(0))?;
        Ok(names.collect::<rusqlite::Result<_>>()?)
    }

    /// Accepts a proposal: the name becomes a canonical tag (created if needed; if it has
    /// meanwhile become an alias, its tag is used), the proposal's description goes on the tag
    /// if the tag has none, and the bookmarks it was proposed for get the tag. Works with a
    /// declared list too: the tag goes on the list in the database. A rejected name can be
    /// accepted; that undoes the rejection.
    pub fn accept_proposal(&mut self, name: &str) -> Result<Decided> {
        let name = tag_name(name)?;
        let now = now_ms();
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let Some((id, description, _)) = find(&tx, &name)? else {
            bail!("no proposal {name:?} (see `shlv tag proposals`)");
        };
        let tag_id = match resolve_tag(&tx, &name)? {
            Some(t) => t,
            None => ensure_tag(&tx, &name, now)?,
        };
        tx.execute(
            "UPDATE tag SET canonical = 1, updated_at = ?2 WHERE id = ?1 AND NOT canonical",
            params![tag_id, now],
        )?;
        let (tag, current): (String, Option<String>) = tx.query_row(
            "SELECT name, description FROM tag WHERE id = ?1",
            [tag_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // A pinned tag with a declared description has one, so this never overrides config.
        let described = match (&description, current) {
            (Some(d), None) => set_description(&tx, tag_id, Some(d), now)?,
            _ => false,
        };
        let tagged = tag_bookmarks(&tx, id, tag_id, now)?;
        tx.execute("DELETE FROM tag_proposal WHERE id = ?1", [id])?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(Decided {
            tag,
            tagged,
            described,
        })
    }

    /// Aliases a proposal to live tag `tag` (a name or alias): the name resolves to that tag
    /// from now on, and the bookmarks it was proposed for get it. If the name is also a tag off
    /// the list, that tag is merged into `tag`, as `tag merge` would. In strict mode, `tag` must
    /// be canonical.
    pub fn alias_proposal(&mut self, name: &str, tag: &str) -> Result<Decided> {
        let (name, tag) = (tag_name(name)?, tag_name(tag)?);
        let now = now_ms();
        let strict = self.strict_tags();
        let pinned_name = self.pinned.as_ref().is_some_and(|p| p.is_tag(&name));
        let forget = self.pinned.is_none();
        let tx = self.conn.transaction()?;
        let Some((id, _, _)) = find(&tx, &name)? else {
            bail!("no proposal {name:?} (see `shlv tag proposals`)");
        };
        let Some(tag_id) = resolve_tag(&tx, &tag)? else {
            bail!("no tag {tag:?}; to add {name:?} to the list, accept it");
        };
        let (target, canonical): (String, bool) = tx.query_row(
            "SELECT name, canonical FROM tag WHERE id = ?1",
            [tag_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if strict && !canonical {
            bail!(
                "{target:?} isn't canonical (strict_tags is on); alias {name:?} to a canonical tag"
            );
        }
        if let Some(from) = live_tag(&tx, &name)? {
            if from == tag_id {
                bail!("{name:?} is {target:?}; accept it instead");
            }
            if pinned_name {
                return Err(pinned(format!("cannot merge away pinned tag {name}")));
            }
            let is_canonical: bool =
                tx.query_row("SELECT canonical FROM tag WHERE id = ?1", [from], |r| {
                    r.get(0)
                })?;
            if is_canonical {
                bail!(
                    "{name:?} is a canonical tag now; to fold it into {target:?}, use \
                     `shlv tag merge {name} {target}`"
                );
            }
            merge_into(&tx, from, &name, tag_id, now)?;
        } else if let Some((owner, other)) = alias_target(&tx, &name)? {
            if owner != tag_id {
                bail!(
                    "{name:?} is already an alias of {other:?}; remove it first with \
                     `shlv tag alias --rm {name}`"
                );
            }
        } else {
            add_alias(&tx, &name, tag_id, &target, now)?;
        }
        let tagged = tag_bookmarks(&tx, id, tag_id, now)?;
        tx.execute("DELETE FROM tag_proposal WHERE id = ?1", [id])?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(Decided {
            tag: target,
            tagged,
            described: false,
        })
    }

    /// Rejects a name: no source proposes it again. It needn't have been proposed (rejecting
    /// ahead of time works). Returns how many bookmarks it was proposed for; they don't get it.
    /// A name that is a canonical tag or an alias of one can't be rejected.
    pub fn reject_proposal(&mut self, name: &str) -> Result<usize> {
        let name = tag_name(name)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        if let Some(t) = resolve_tag(&tx, &name)? {
            let (tag, canonical): (String, bool) =
                tx.query_row("SELECT name, canonical FROM tag WHERE id = ?1", [t], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            if canonical {
                bail!(
                    "{name:?} is on the list (tag {tag:?}); to drop it, use `shlv tag canonical rm`"
                );
            }
        }
        let bookmarks = match find(&tx, &name)? {
            Some((id, _, _)) => {
                tx.execute(
                    "UPDATE tag_proposal SET state = 'rejected', updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                tx.execute(
                    "DELETE FROM tag_proposal_bookmark WHERE proposal_id = ?1",
                    [id],
                )?
            }
            None => {
                tx.execute(
                    "INSERT INTO tag_proposal (name, source, state, created_at, updated_at) \
                     VALUES (?1, 'user', 'rejected', ?2, ?2)",
                    params![name, now],
                )?;
                0
            }
        };
        tx.commit()?;
        Ok(bookmarks)
    }
}

/// Tags the live bookmarks linked to proposal `id` with `tag_id`, each with the source that
/// proposed it there, skipping bookmarks where a layer's suggestion of that tag was rejected.
/// Returns how many got it.
fn tag_bookmarks(tx: &Transaction, id: i64, tag_id: i64, now: i64) -> Result<usize> {
    let links: Vec<(i64, String)> = tx
        .prepare_cached(
            "SELECT pb.bookmark_id, pb.source FROM tag_proposal_bookmark pb \
             JOIN bookmark b ON b.id = pb.bookmark_id AND b.deleted_at IS NULL \
             WHERE pb.proposal_id = ?1 AND NOT EXISTS (SELECT 1 FROM tag_suggestion s \
             WHERE s.bookmark_id = pb.bookmark_id AND s.tag_id = ?2 AND s.state = 'rejected')",
        )?
        .query_map(params![id, tag_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id, source) VALUES (?1, ?2, ?3)",
    )?;
    let mut tagged = Vec::new();
    for (b, source) in links {
        let source = ProposalSource::parse(&source)?.tag_source();
        if insert.execute(params![b, tag_id, source])? > 0 {
            tagged.push(b);
        }
    }
    touch_many(tx, &tagged, now)?;
    Ok(tagged.len())
}
