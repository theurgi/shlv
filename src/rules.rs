//! Domain rules saved in the database (task 043), alongside the pinned rules from config
//! (`[tags.rules]`): adding and removing them, listing them with what they tagged, and spotting
//! rules worth offering, where the user has tagged several bookmarks from one site the same way.
//!
//! The rules in effect are both sets together ([`Store::rules`]); config rules aren't copied into
//! the database, so removing one from config removes it, as with pinned interests.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::{Transaction, params};
use serde::Serialize;

use crate::store::{Store, now_ms};
use crate::suggest::{DomainRules, PENDING, normalize_host};
use crate::tag::alias_target;
use crate::url;

/// A rule (one host, one tag) as [`Store::rule_list`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleInfo {
    pub host: String,
    pub tag: String,
    /// In config (`[tags.rules]`): can't be removed here.
    pub pinned: bool,
    /// Saved in the database.
    pub saved: bool,
    /// Bookmarks on the host (or a subdomain) that a rule gave the tag or suggested it for.
    pub tagged: usize,
    /// Of those, the ones where the suggestion is still pending.
    pub pending: usize,
}

/// What [`Store::add_rule`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleAdded {
    /// The host, normalized.
    pub host: String,
    /// Tags added to the host's rule.
    pub added: Vec<String>,
    /// Tags the host's rules already gave.
    pub existing: Vec<String>,
}

/// What [`Store::remove_rule`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleRemoved {
    pub host: String,
    pub removed: Vec<String>,
    /// Pending suggestions of the removed rules that no other rule makes, dropped.
    pub dropped: usize,
}

/// A rule worth offering: on `host` and its subdomains, `count` of the `of` bookmarks the user
/// has tagged carry `tag`, and no rule gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleCandidate {
    pub host: String,
    pub tag: String,
    pub count: usize,
    pub of: usize,
}

/// Rules are offered once this many bookmarks of a site share a tag...
pub const OFFER_MIN: usize = 3;
/// ...and that tag is on at least this share of the site's tagged bookmarks.
pub const OFFER_SHARE: f64 = 0.8;

impl Store {
    /// Reads the saved rules again (another process may have changed them).
    pub fn reload_rules(&mut self) -> Result<()> {
        let mut saved = DomainRules::default();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT host, tag FROM rule ORDER BY host, created_at, tag")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (host, tag): (String, String) = (r.get(0)?, r.get(1)?);
            saved.insert(&host, &tag);
        }
        drop(rows);
        drop(stmt);
        self.saved_rules = saved;
        self.rules = self.pinned_rules.union(&self.saved_rules);
        Ok(())
    }

    /// The rules saved in the database.
    pub fn saved_rules(&self) -> &DomainRules {
        &self.saved_rules
    }

    /// A rule's host as typed (`GitHub.com`, `https://github.com/x`), normalized.
    pub fn rule_host(&self, input: &str) -> Result<String> {
        normalize_host(input.trim()).ok_or_else(|| anyhow!("{input:?} is not a host name"))
    }

    /// Saves a rule: bookmarks on `host` or its subdomains get `tags` suggested. Tags are
    /// normalized and styled as anywhere a name enters, and aliases resolve to their tags; in
    /// strict mode they must be canonical. Tags the host's rules already give are left alone.
    /// Doesn't run the rule on existing bookmarks: see [`Store::apply_rules`].
    pub fn add_rule(&mut self, host: &str, tags: &[String]) -> Result<RuleAdded> {
        let host = self.rule_host(host)?;
        if tags.is_empty() {
            bail!("a rule needs a tag");
        }
        let mut names = Vec::new();
        for t in self.tag_names(tags)? {
            let t = match alias_target(&self.conn, &t)? {
                Some((_, tag)) => tag,
                None => t,
            };
            if !names.contains(&t) {
                names.push(t);
            }
        }
        self.check_strict(&names)?;
        let mut out = RuleAdded {
            host: host.clone(),
            ..Default::default()
        };
        let now = now_ms();
        let tx = self.conn.transaction()?;
        for t in names {
            if self.rules.contains(&host, &t) {
                out.existing.push(t);
                continue;
            }
            tx.execute(
                "INSERT INTO rule (host, tag, created_at) VALUES (?1, ?2, ?3)",
                params![host, t, now],
            )?;
            tx.execute(
                "DELETE FROM rule_declined WHERE host = ?1 AND tag = ?2",
                params![host, t],
            )?;
            out.added.push(t);
        }
        tx.commit()?;
        self.reload_rules()?;
        Ok(out)
    }

    /// Removes saved rules for `host`: the given tags (names or aliases), or all of them if
    /// `tags` is empty. Fails before changing anything if one is pinned in config, or if a given
    /// tag has no rule. Pending suggestions the removed rules made, and no remaining rule would
    /// make, are dropped.
    pub fn remove_rule(&mut self, host: &str, tags: &[String]) -> Result<RuleRemoved> {
        let host = self.rule_host(host)?;
        let mut names = Vec::new();
        for t in self.lookup_names(tags)? {
            let t = match alias_target(&self.conn, &t)? {
                Some((_, tag)) => tag,
                None => t,
            };
            names.push(t);
        }
        if names.is_empty() {
            names = self.saved_rules.tags_of(&host).to_vec();
            if names.is_empty() {
                let pinned = self.pinned_rules.tags_of(&host);
                if pinned.is_empty() {
                    bail!("no rule for {host}");
                }
                return Err(crate::tag::pinned_by(
                    format!("cannot remove the rule for {host} ({})", hashed(pinned)),
                    "[tags.rules]",
                ));
            }
        }
        for t in &names {
            if self.pinned_rules.contains(&host, t) {
                return Err(crate::tag::pinned_by(
                    format!("cannot remove rule {host} → {t}"),
                    "[tags.rules]",
                ));
            }
            if !self.saved_rules.contains(&host, t) {
                bail!("no rule gives {host} the tag {t}");
            }
        }
        let tx = self.conn.transaction()?;
        for t in &names {
            tx.execute(
                "DELETE FROM rule WHERE host = ?1 AND tag = ?2",
                params![host, t],
            )?;
        }
        let mut remaining = self.saved_rules.clone();
        remaining.remove(&host, &names);
        let remaining = self.pinned_rules.union(&remaining);
        let dropped = drop_orphaned(&tx, &host, &names, &remaining)?;
        tx.commit()?;
        self.reload_rules()?;
        Ok(RuleRemoved {
            host,
            removed: names,
            dropped,
        })
    }

    /// Every rule in effect, by host then tag, with what it tagged.
    pub fn rule_list(&self) -> Result<Vec<RuleInfo>> {
        let mut list: Vec<RuleInfo> = Vec::new();
        for (host, tags) in self.rules.iter() {
            for tag in tags {
                list.push(RuleInfo {
                    host: host.to_string(),
                    tag: tag.clone(),
                    pinned: self.pinned_rules.contains(host, tag),
                    saved: self.saved_rules.contains(host, tag),
                    tagged: 0,
                    pending: 0,
                });
            }
        }
        list.sort_by(|a, b| (&a.host, &a.tag).cmp(&(&b.host, &b.tag)));
        if list.is_empty() {
            return Ok(list);
        }
        let index: HashMap<(String, String), usize> = list
            .iter()
            .enumerate()
            .map(|(i, r)| ((r.host.clone(), r.tag.clone()), i))
            .collect();
        // Bookmarks with a tag or a pending suggestion from a rule, credited to every rule
        // whose host and tag match.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT b.url_norm, t.name, 0 FROM bookmark_tag bt \
             JOIN bookmark b ON b.id = bt.bookmark_id JOIN tag t ON t.id = bt.tag_id \
             WHERE bt.source = 'rule' AND b.deleted_at IS NULL \
             UNION ALL SELECT b.url_norm, t.name, 1 FROM tag_suggestion s \
             JOIN bookmark b ON b.id = s.bookmark_id JOIN tag t ON t.id = s.tag_id \
             WHERE s.source = 'rule' AND b.deleted_at IS NULL AND {PENDING}"
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (norm, tag, pending): (String, String, bool) = (r.get(0)?, r.get(1)?, r.get(2)?);
            let Some(host) = url::host(&norm) else {
                continue;
            };
            for d in domains(host) {
                if let Some(&i) = index.get(&(d.to_string(), tag.clone())) {
                    list[i].tagged += 1;
                    list[i].pending += usize::from(pending);
                }
            }
        }
        Ok(list)
    }

    /// Rules worth offering: sites where at least `min` of the bookmarks the user has tagged
    /// share a tag, which is on at least `share` of them, that no rule gives and that wasn't
    /// declined ([`Store::decline_rule`]). A site is a host or a parent domain that bookmarks
    /// on two or more of its hosts share (so `bandcamp.com` for `a.bandcamp.com` and
    /// `b.bandcamp.com`); a parent covers its subdomains, so they aren't offered as well. In
    /// strict mode, only canonical tags. Most bookmarks first.
    pub fn rule_candidates(&self, min: usize, share: f64) -> Result<Vec<RuleCandidate>> {
        // Hosts and parent domains, interned: a 100k library has far fewer of them than tag rows.
        let mut names: Vec<String> = Vec::new();
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut intern = |s: &str| -> u32 {
            if let Some(&i) = ids.get(s) {
                return i;
            }
            names.push(s.to_string());
            ids.insert(s.to_string(), names.len() as u32 - 1);
            names.len() as u32 - 1
        };
        /// A tag's bookmarks on a site, and whether they come from the site itself and from
        /// more than one host.
        struct Seen {
            count: usize,
            host: u32,
            exact: bool,
            multi: bool,
        }
        /// A site's bookmarks tagged (not by rules), the last one counted, and its tags.
        #[derive(Default)]
        struct Site {
            tagged: usize,
            last: i64,
            tags: HashMap<i64, Seen>,
        }
        let mut sites: HashMap<u32, Site> = HashMap::new();
        // By bookmark, so each site counts a bookmark once.
        let mut stmt = self.conn.prepare(
            "SELECT b.id, b.url_norm, bt.tag_id FROM bookmark_tag bt \
             JOIN bookmark b ON b.id = bt.bookmark_id \
             WHERE bt.source <> 'rule' AND b.deleted_at IS NULL ORDER BY bt.bookmark_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (id, tag): (i64, i64) = (r.get(0)?, r.get(2)?);
            let norm = r.get_ref(1)?.as_str()?;
            let Some(host) = url::host(norm) else {
                continue;
            };
            let h = intern(host);
            for (i, d) in domains(host).enumerate() {
                let site = sites.entry(if i == 0 { h } else { intern(d) }).or_default();
                if site.last != id {
                    site.last = id;
                    site.tagged += 1;
                }
                let e = site.tags.entry(tag).or_insert(Seen {
                    count: 0,
                    host: h,
                    exact: false,
                    multi: false,
                });
                e.count += 1;
                e.exact |= i == 0;
                e.multi |= e.host != h;
            }
        }
        drop(rows);
        let tag_names: HashMap<i64, String> = self
            .conn
            .prepare("SELECT id, name FROM tag")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let declined: HashSet<(String, String)> = self
            .conn
            .prepare("SELECT host, tag FROM rule_declined")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut found: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
        for (site, s) in &sites {
            let site = &names[*site as usize];
            for (tag, seen) in &s.tags {
                if seen.count < min
                    || (seen.count as f64) < share * s.tagged as f64
                    || !(seen.exact || seen.multi)
                {
                    continue;
                }
                let Some(tag) = tag_names.get(tag) else {
                    continue;
                };
                let key = (site.clone(), tag.clone());
                if self
                    .rules
                    .matching(&format!("https://{site}/"))
                    .contains(&tag.as_str())
                    || declined.contains(&key)
                {
                    continue;
                }
                found.insert(key, (seen.count, s.tagged));
            }
        }
        // A site's offer covers its subdomains'.
        let covered: Vec<(String, String)> = found
            .keys()
            .filter(|(site, tag)| {
                domains(site)
                    .skip(1)
                    .any(|p| found.contains_key(&(p.to_string(), tag.clone())))
            })
            .cloned()
            .collect();
        for k in covered {
            found.remove(&k);
        }
        let mut out: Vec<RuleCandidate> = found
            .into_iter()
            .map(|((host, tag), (count, of))| RuleCandidate {
                host,
                tag,
                count,
                of,
            })
            .collect();
        if self.strict_tags() {
            let tags: Vec<String> = out.iter().map(|c| c.tag.clone()).collect();
            let bad = self.non_canonical(&tags)?;
            out.retain(|c| !bad.contains(&c.tag));
        }
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.host.cmp(&b.host)));
        Ok(out)
    }

    /// Remembers that the user doesn't want a rule giving `host` the tag `tag`, so
    /// [`Store::rule_candidates`] stops offering it.
    pub fn decline_rule(&mut self, host: &str, tag: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO rule_declined (host, tag) VALUES (?1, ?2)",
            params![host, tag],
        )?;
        Ok(())
    }

    /// Runs the rules on the live bookmarks of `host` and its subdomains; see
    /// [`Store::apply_rules`].
    pub fn apply_rules_on(&mut self, host: &str) -> Result<crate::RuleStats> {
        let ids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, url_norm FROM bookmark WHERE deleted_at IS NULL")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut v = Vec::new();
            for r in rows {
                let (id, norm) = r?;
                if url::on_site(&norm, host) {
                    v.push(id);
                }
            }
            v
        };
        if ids.is_empty() {
            return Ok(crate::RuleStats::default());
        }
        self.apply_rules(Some(&ids))
    }
}

/// `host`, then each parent domain that still has a dot: `a.b.example.com`, `b.example.com`,
/// `example.com`.
fn domains(host: &str) -> impl Iterator<Item = &str> {
    let mut next = Some(host);
    std::iter::from_fn(move || {
        let h = next?;
        next = h
            .split_once('.')
            .map(|(_, p)| p)
            .filter(|p| p.contains('.'));
        Some(h)
    })
}

/// Deletes the pending rule suggestions of `tags` on `host`'s bookmarks that `remaining` rules
/// wouldn't make. Returns how many.
fn drop_orphaned(
    tx: &Transaction,
    host: &str,
    tags: &[String],
    remaining: &DomainRules,
) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "SELECT s.bookmark_id, s.tag_id, b.url_norm, t.name FROM tag_suggestion s \
         JOIN bookmark b ON b.id = s.bookmark_id JOIN tag t ON t.id = s.tag_id \
         WHERE s.source = 'rule' AND {PENDING}"
    ))?;
    let rows: Vec<(i64, i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for (bookmark, tag_id, norm, tag) in rows {
        if tags.contains(&tag)
            && url::on_site(&norm, host)
            && !remaining.matching(&norm).contains(&tag.as_str())
        {
            n += tx.execute(
                "DELETE FROM tag_suggestion WHERE bookmark_id = ?1 AND tag_id = ?2",
                params![bookmark, tag_id],
            )?;
        }
    }
    Ok(n)
}

/// Points saved rules naming tag `from` at `into` instead (a rename or merge).
pub(crate) fn rename_rule_tag(tx: &Transaction, from: &str, into: &str) -> Result<()> {
    tx.execute(
        "UPDATE OR IGNORE rule SET tag = ?2 WHERE tag = ?1",
        params![from, into],
    )?;
    tx.execute("DELETE FROM rule WHERE tag = ?1", [from])?;
    Ok(())
}

fn hashed(tags: &[String]) -> String {
    tags.iter()
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ")
}
