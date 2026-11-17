//! Seeding the tag list from interests (task 034): stored interests, candidates and the
//! decisions made on them, and writing the kept ones to the list.
//!
//! Interests and tags aren't distinct kinds, and nothing constrains one against the other. An
//! interest is stored only when asked (`tag seed --save`) or pinned in config
//! (`[tags] interests`). Which tags were kept under which interest is recorded either way, for
//! tag health; a tag named like an interest (or an alias of one) also counts as under it.

use anyhow::{Result, bail};
use rusqlite::{Transaction, params};

use crate::store::{Store, normalize_tag, now_ms, resolve_tag};
use crate::tag::{
    add_alias, alias_target, forget_declared, live_tag, mark_canonical, merge_into, require_tag,
    set_description,
};

/// A stored interest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Interest {
    pub name: String,
    /// Stored in the database (`tag seed --save`).
    pub saved: bool,
    /// Pinned in config (`[tags] interests`).
    pub pinned: bool,
}

/// An interest as typed, normalized like a tag name (`Home Lab` is `home-lab`); empty if blank.
pub fn normalize_interest(s: &str) -> String {
    normalize_tag(s)
}

/// What a candidate name is, once styled: new, or an existing tag (an alias resolves to its tag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedName {
    New(String),
    Tag { name: String, canonical: bool },
}

impl SeedName {
    pub fn name(&self) -> &str {
        match self {
            SeedName::New(n) | SeedName::Tag { name: n, .. } => n,
        }
    }

    /// Already on the canonical list.
    pub fn listed(&self) -> bool {
        matches!(
            self,
            SeedName::Tag {
                canonical: true,
                ..
            }
        )
    }
}

/// What the user decided for a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Decision {
    /// Not decided (the user stopped early): nothing is written.
    #[default]
    Undecided,
    /// Goes on the list (under its name, which may have been renamed).
    Keep,
    Drop,
    /// Becomes an alias of this tag or kept candidate (or, if it is a tag in use, is merged into
    /// it).
    Merge(String),
    /// Already on the list: only recorded under its interest.
    Listed,
}

/// A suggested tag under an interest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The interest it falls under; empty for none (`tag draft` proposes tags outside the
    /// interests, and redundant ones). Kept candidates are recorded under a non-empty one.
    pub interest: String,
    /// Styled; for an existing tag, its name.
    pub name: String,
    pub description: Option<String>,
    /// The user edited the description: it replaces a tag's current one.
    pub edited: bool,
    pub decision: Decision,
    /// For a tag already on the list that looks redundant (`tag draft`): what it could be merged
    /// into, and why. Keeping or dropping it leaves the tag as it is.
    pub hint: Option<Hint>,
}

/// A suggestion to merge a listed tag into another tag or candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub into: String,
    pub reason: Option<String>,
}

/// What [`Store::apply_seed`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seeded {
    /// Tags put on the list (newly canonical).
    pub kept: Vec<String>,
    /// `(name, into)`: names made aliases of (or merged into) tags.
    pub merged: Vec<(String, String)>,
    /// Descriptions set.
    pub described: usize,
    /// Interests newly stored.
    pub saved: usize,
    /// Decisions that couldn't be carried out, with the reason.
    pub skipped: Vec<String>,
}

impl Store {
    /// Pins interests from config (`[tags] interests`): normalized, deduplicated. Pinned
    /// interests aren't written to the database and can't be forgotten from the command line.
    pub fn set_pinned_interests(&mut self, interests: &[String]) {
        let mut v: Vec<String> = interests
            .iter()
            .map(|i| normalize_interest(i))
            .filter(|i| !i.is_empty())
            .collect();
        v.sort();
        v.dedup();
        self.pinned_interests = v;
    }

    /// Stored interests, saved and pinned, by name.
    pub fn interests(&self) -> Result<Vec<Interest>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name FROM interest ORDER BY name")?;
        let saved: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut all: Vec<Interest> = saved
            .into_iter()
            .map(|name| Interest {
                pinned: self.pinned_interests.contains(&name),
                saved: true,
                name,
            })
            .collect();
        for p in &self.pinned_interests {
            if !all.iter().any(|i| &i.name == p) {
                all.push(Interest {
                    name: p.clone(),
                    saved: false,
                    pinned: true,
                });
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    /// Stores interests (normalized). Returns how many were new.
    pub fn save_interests(&mut self, names: &[String]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = save_interests(&tx, names, now_ms())?;
        tx.commit()?;
        Ok(n)
    }

    /// Forgets saved interests; the tags kept under them stay, and so do the links, so saving
    /// an interest again brings them back. Returns how many were removed. Interests pinned in
    /// config can't be forgotten here.
    pub fn forget_interests(&mut self, names: &[String]) -> Result<usize> {
        let names: Vec<String> = names.iter().map(|n| normalize_interest(n)).collect();
        if let Some(p) = names.iter().find(|n| self.pinned_interests.contains(n)) {
            return Err(crate::tag::pinned(format!(
                "cannot forget interest {p}: it is pinned in config ([tags] interests)"
            )));
        }
        let tx = self.conn.transaction()?;
        let mut n = 0;
        for name in &names {
            n += tx.execute("DELETE FROM interest WHERE name = ?1", [name])?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// A candidate's name as it would enter the list: styled (see [`Store::styled`]), then
    /// resolved to an existing tag if it names one or an alias of one.
    pub fn seed_name(&self, raw: &str) -> Result<SeedName> {
        let name = self.styled(raw)?;
        let Some(id) = resolve_tag(&self.conn, &name)? else {
            return Ok(SeedName::New(name));
        };
        let (name, canonical) =
            self.conn
                .query_row("SELECT name, canonical FROM tag WHERE id = ?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        Ok(SeedName::Tag { name, canonical })
    }

    /// Writes the decisions, in one transaction: kept candidates go on the list with their
    /// descriptions (a tag's existing description is replaced only by an edited one), merged ones
    /// become aliases of their target (a tag in use is merged into it, as `tag merge` does), and
    /// every tag kept, merged into, or already listed is recorded under its interest. Then the
    /// interests in `save` are stored.
    ///
    /// A decision that can't be carried out (a merge into a candidate that wasn't kept, a pinned
    /// tag merged away) is skipped with the reason; everything else still applies.
    pub fn apply_seed(&mut self, candidates: &[Candidate], save: &[String]) -> Result<Seeded> {
        let now = now_ms();
        let pinned = self.pinned.as_ref();
        let forget = pinned.is_none();
        let tx = self.conn.transaction()?;
        let mut s = Seeded::default();

        // Keeps first, so merges can point at kept candidates.
        for c in candidates {
            match &c.decision {
                Decision::Keep => {
                    let name = match alias_target(&tx, &c.name)? {
                        Some((_, tag)) => tag,
                        None => c.name.clone(),
                    };
                    if mark_canonical(&tx, &name, now)? {
                        s.kept.push(name.clone());
                    }
                    let id = require_tag(&tx, &name)?;
                    if let Some(d) = &c.description
                        && describe_seeded(&tx, pinned, id, &name, d, c.edited, now)?
                    {
                        s.described += 1;
                    }
                    link(&tx, &c.interest, id)?;
                }
                Decision::Listed => {
                    if let Some(id) = resolve_tag(&tx, &c.name)? {
                        link(&tx, &c.interest, id)?;
                    }
                }
                Decision::Undecided | Decision::Drop | Decision::Merge(_) => {}
            }
        }
        for c in candidates {
            let Decision::Merge(into) = &c.decision else {
                continue;
            };
            let Some(into_id) = resolve_tag(&tx, into)? else {
                s.skipped.push(format!(
                    "{}: not merged: {into} isn't a tag (it wasn't kept)",
                    c.name
                ));
                continue;
            };
            let into_name: String =
                tx.query_row("SELECT name FROM tag WHERE id = ?1", [into_id], |r| {
                    r.get(0)
                })?;
            if let Some(from_id) = live_tag(&tx, &c.name)? {
                if from_id == into_id {
                    continue;
                }
                if pinned.is_some_and(|p| p.is_tag(&c.name)) {
                    s.skipped.push(format!(
                        "{}: not merged: it is pinned in config ([tags.canonical])",
                        c.name
                    ));
                    continue;
                }
                merge_into(&tx, from_id, &c.name, into_id, now)?;
            } else {
                match alias_target(&tx, &c.name)? {
                    Some((id, _)) if id == into_id => {}
                    Some((_, other)) => {
                        s.skipped.push(format!(
                            "{}: not merged: it is already an alias of {other}",
                            c.name
                        ));
                        continue;
                    }
                    None => {
                        add_alias(&tx, &c.name, into_id, &into_name, now)?;
                    }
                }
            }
            s.merged.push((c.name.clone(), into_name));
            link(&tx, &c.interest, into_id)?;
        }
        s.saved = save_interests(&tx, save, now)?;
        forget_declared(&tx, forget)?;
        tx.commit()?;
        Ok(s)
    }
}

/// Sets a seeded tag's description: always when `edited`, else only if it has none. A
/// description declared in config for a pinned tag stays.
fn describe_seeded(
    tx: &Transaction,
    pinned: Option<&crate::tag::DeclaredTags>,
    id: i64,
    name: &str,
    text: &str,
    edited: bool,
    now: i64,
) -> Result<bool> {
    if pinned.is_some_and(|p| p.description(name).is_some()) {
        return Ok(false);
    }
    let Some(text) = crate::tag::check_description(text).map_err(anyhow::Error::msg)? else {
        return Ok(false);
    };
    if !edited {
        let has: bool = tx.query_row(
            "SELECT description IS NOT NULL FROM tag WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if has {
            return Ok(false);
        }
    }
    set_description(tx, id, Some(&text), now)
}

fn save_interests(tx: &Transaction, names: &[String], now: i64) -> Result<usize> {
    let mut n = 0;
    for name in names {
        let name = normalize_interest(name);
        if name.is_empty() {
            bail!("empty interest");
        }
        n += tx.execute(
            "INSERT OR IGNORE INTO interest (name, created_at) VALUES (?1, ?2)",
            params![name, now],
        )?;
    }
    Ok(n)
}

fn link(tx: &Transaction, interest: &str, tag_id: i64) -> Result<()> {
    if interest.is_empty() {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO interest_tag (interest, tag_id) VALUES (?1, ?2)",
        params![interest, tag_id],
    )?;
    Ok(())
}
