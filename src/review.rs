//! Review rounds (task 039): the library side of `shlv review`, which goes through bookmarks in
//! batches and settles each one's tags.
//!
//! - **Queue.** Bookmarks not reviewed yet ([`Store::review_queue`]), or with filters, those with
//!   matching pending suggestions. [`discovery_order`] spreads a batch across domains, untagged
//!   bookmarks first, to find the vocabulary; once the classifier has [`DISCOVERY`] reviewed
//!   bookmarks to learn from, [`least_confident`] picks the ones it's least sure about, to teach
//!   it most.
//! - **Decisions** commit per bookmark, in one transaction ([`Store::commit_review`]): accepted
//!   and rejected suggestions, tags added and removed, new names proposed, and the bookmark
//!   marked reviewed. A tag missing from a reviewed bookmark is a negative example for the
//!   classifier, so the screen asks for complete tagging.
//! - **Progress.** Each batch is a `review_batch` row ([`Store::finish_review_batch`],
//!   [`Store::review_history`]).

use std::collections::HashMap;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::proposal::{self, ProposalSource};
use crate::store::{Store, now_ms, resolve_tag, touch_many};
use crate::suggest::{self, PENDING, Source};
use crate::{Diverted, url};

/// Reviewed bookmarks the classifier needs before batches are ordered by its confidence rather
/// than spread across domains: [`crate::model::FULL_AT`]'s 100, about three batches. Below that
/// its probabilities are mostly the lexical guess (task 038).
pub const DISCOVERY: usize = 100;

/// Which bookmarks to review. With `tag` or `source`, bookmarks with a matching pending
/// suggestion, reviewed or not; otherwise bookmarks not reviewed yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewFilter {
    /// A tag (or alias) suggested for the bookmark.
    pub tag: Option<String>,
    /// The layer that suggested it.
    pub source: Option<Source>,
    /// Only bookmarks whose most confident (matching) suggestion is below this; a suggestion
    /// without a confidence (the LLM's) counts as 0.5, none at all as 0.
    pub below: Option<f64>,
}

impl ReviewFilter {
    pub fn is_empty(&self) -> bool {
        self.tag.is_none() && self.source.is_none() && self.below.is_none()
    }

    /// Whether the queue holds bookmarks with matching suggestions rather than unreviewed ones.
    fn by_suggestion(&self) -> bool {
        self.tag.is_some() || self.source.is_some()
    }
}

/// A bookmark waiting for review.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub id: i64,
    /// Its host, for spreading a batch across domains.
    pub host: String,
    /// It has canonical tags already.
    pub tagged: bool,
    /// Its most confident (matching) pending suggestion: 0.5 without a confidence, 0 without
    /// any.
    pub top: f64,
}

/// What the user decided for one bookmark. Tags are names as stored (or aliases).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// Pending suggestions to accept.
    pub accept: Vec<String>,
    /// Pending suggestions to reject: no layer offers them again, and the classifier learns the
    /// tag doesn't apply.
    pub reject: Vec<String>,
    /// Canonical tags to add.
    pub add: Vec<String>,
    /// Tags to remove; one a layer applied is remembered as rejected.
    pub remove: Vec<String>,
    /// New names, proposed for the list (source `user`).
    pub propose: Vec<String>,
}

/// What [`Store::commit_review`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Committed {
    /// Tags the bookmark gained: accepted suggestions and added tags.
    pub tagged: usize,
    pub rejected: usize,
    pub removed: usize,
    /// Proposed names, and names dropped because they were rejected before.
    pub diverted: Diverted,
}

/// One batch's numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchStats {
    pub reviewed: usize,
    pub skipped: usize,
    /// Names proposed for the list for the first time during the batch.
    pub proposed: usize,
    /// Classifier suggestions on the reviewed bookmarks, and how many were accepted.
    pub model_shown: usize,
    pub model_accepted: usize,
    /// Tags added to the canonical list after the batch.
    pub tags_added: usize,
}

/// A batch in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRecord {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub stats: BatchStats,
}

impl Store {
    /// Bookmarks to review; see [`ReviewFilter`]. Unordered: see [`discovery_order`] and
    /// [`least_confident`]. Fails if the filter's tag doesn't exist.
    pub fn review_queue(&self, filter: &ReviewFilter) -> Result<Vec<Queued>> {
        let tag_id = match &filter.tag {
            Some(t) => {
                let name = self.lookup(t)?;
                match resolve_tag(&self.conn, &name)? {
                    Some(id) => Some(id),
                    None => bail!("no tag {name:?}"),
                }
            }
            None => None,
        };
        // Without a filter, the confidence isn't needed, and `IN` lists are built once rather
        // than probed per bookmark: 58 against 109 ms at 100k bookmarks.
        let sql = if filter.is_empty() {
            "SELECT b.id, b.url_norm, b.id IN (SELECT bt.bookmark_id FROM bookmark_tag bt \
               JOIN tag u ON u.id = bt.tag_id WHERE u.canonical), NULL \
             FROM bookmark b WHERE b.deleted_at IS NULL \
             AND b.id NOT IN (SELECT bookmark_id FROM review_bookmark)"
                .to_string()
        } else {
            format!(
                "SELECT id, url_norm, tagged, top FROM (\
                   SELECT b.id, b.url_norm, \
                     EXISTS (SELECT 1 FROM bookmark_tag bt JOIN tag u ON u.id = bt.tag_id \
                             WHERE bt.bookmark_id = b.id AND u.canonical) AS tagged, \
                     (SELECT max(coalesce(s.confidence, 0.5)) FROM tag_suggestion s \
                      WHERE s.bookmark_id = b.id AND {PENDING} \
                      AND (?1 IS NULL OR s.tag_id = ?1) AND (?2 IS NULL OR s.source = ?2)) AS top, \
                     EXISTS (SELECT 1 FROM review_bookmark r WHERE r.bookmark_id = b.id) AS done \
                   FROM bookmark b WHERE b.deleted_at IS NULL) \
                 WHERE CASE WHEN ?3 THEN top IS NOT NULL ELSE NOT done END \
                 AND (?4 IS NULL OR coalesce(top, 0) < ?4)"
            )
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let params = if filter.is_empty() {
            params![]
        } else {
            params![
                tag_id,
                filter.source.map(Source::as_str),
                filter.by_suggestion(),
                filter.below
            ]
        };
        let rows = stmt.query_map(params, |r| {
            let norm: String = r.get(1)?;
            Ok(Queued {
                id: r.get(0)?,
                host: url::host(&norm).unwrap_or_default().to_string(),
                tagged: r.get(2)?,
                top: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many bookmarks have been reviewed.
    pub fn reviewed_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM review_bookmark r JOIN bookmark b ON b.id = r.bookmark_id \
             WHERE b.deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Reviewed bookmarks per host, for [`discovery_order`].
    pub fn reviewed_hosts(&self) -> Result<HashMap<String, usize>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT b.url_norm FROM review_bookmark r JOIN bookmark b ON b.id = r.bookmark_id \
             WHERE b.deleted_at IS NULL",
        )?;
        let mut hosts = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let norm: String = r.get(0)?;
            *hosts
                .entry(url::host(&norm).unwrap_or_default().to_string())
                .or_insert(0) += 1;
        }
        Ok(hosts)
    }

    /// Bookmarks a tag new to the list should be checked against: reviewed ones, and ones with
    /// canonical tags (the classifier's examples either way). Newest first.
    pub fn reviewed_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT b.id FROM bookmark b WHERE b.deleted_at IS NULL AND (\
               EXISTS (SELECT 1 FROM review_bookmark r WHERE r.bookmark_id = b.id) \
               OR EXISTS (SELECT 1 FROM bookmark_tag bt JOIN tag t ON t.id = bt.tag_id \
                          WHERE bt.bookmark_id = b.id AND t.canonical)) \
             ORDER BY b.id DESC",
        )?;
        let ids = stmt.query_map([], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a bookmark has been reviewed.
    pub fn is_reviewed(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM review_bookmark WHERE bookmark_id = ?1")?
            .exists([id])?)
    }

    /// Starts a batch; returns its id and start time (ms), for [`Store::proposals_since`].
    pub fn start_review_batch(&mut self) -> Result<(i64, i64)> {
        let now = now_ms();
        let id = self.conn.query_row(
            "INSERT INTO review_batch (started_at) VALUES (?1) RETURNING id",
            [now],
            |r| r.get(0),
        )?;
        Ok((id, now))
    }

    /// Records a batch's numbers and when it finished.
    pub fn finish_review_batch(&mut self, batch: i64, s: &BatchStats) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE review_batch SET finished_at = ?2, reviewed = ?3, skipped = ?4, \
             proposed = ?5, model_shown = ?6, model_accepted = ?7, tags_added = ?8 \
             WHERE id = ?1",
            params![
                batch,
                now_ms(),
                s.reviewed as i64,
                s.skipped as i64,
                s.proposed as i64,
                s.model_shown as i64,
                s.model_accepted as i64,
                s.tags_added as i64
            ],
        )?;
        if n == 0 {
            bail!("no review batch {batch}");
        }
        Ok(())
    }

    /// Finished batches that reviewed something, oldest first.
    pub fn review_history(&self) -> Result<Vec<BatchRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, started_at, finished_at, reviewed, skipped, proposed, model_shown, \
             model_accepted, tags_added FROM review_batch \
             WHERE finished_at IS NOT NULL AND reviewed > 0 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let n = |i: usize| r.get::<_, i64>(i).map(|v| v as usize);
            Ok(BatchRecord {
                id: r.get(0)?,
                started_at: r.get(1)?,
                finished_at: r.get(2)?,
                stats: BatchStats {
                    reviewed: n(3)?,
                    skipped: n(4)?,
                    proposed: n(5)?,
                    model_shown: n(6)?,
                    model_accepted: n(7)?,
                    tags_added: n(8)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Names proposed for the first time since `since` (ms), still pending.
    pub fn proposals_since(&self, since: i64) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM tag_proposal WHERE state = 'pending' AND created_at >= ?1",
            [since],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Commits the decision for one bookmark in one transaction and marks it reviewed in
    /// `batch`. Fails, changing nothing, if the bookmark is gone, a tag to accept or reject
    /// isn't pending for it, or a tag to add isn't canonical.
    pub fn commit_review(&mut self, batch: i64, id: i64, d: &Decision) -> Result<Committed> {
        let lookup = |s: &Self, names: &[String]| -> Result<Vec<i64>> {
            names
                .iter()
                .map(|t| {
                    let n = s.lookup(t)?;
                    resolve_tag(&s.conn, &n)?.ok_or_else(|| anyhow::anyhow!("no tag {n:?}"))
                })
                .collect()
        };
        let accept = lookup(self, &d.accept)?;
        let reject = lookup(self, &d.reject)?;
        let remove = lookup(self, &d.remove)?;
        let add = lookup(self, &d.add)?;
        let propose = self.tag_names(&d.propose)?;
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let live: bool = tx
            .prepare_cached("SELECT 1 FROM bookmark WHERE id = ?1 AND deleted_at IS NULL")?
            .exists([id])?;
        if !live {
            bail!("no bookmark with id {id}");
        }
        for (&tag_id, name) in add.iter().zip(&d.add) {
            let canonical: bool = tx
                .prepare_cached("SELECT canonical FROM tag WHERE id = ?1")?
                .query_row([tag_id], |r| r.get(0))?;
            if !canonical {
                bail!("{name:?} isn't on the canonical list");
            }
        }
        let pending = |tag_id: i64| -> Result<bool> {
            Ok(tx
                .prepare_cached(&format!(
                    "SELECT 1 FROM tag_suggestion s WHERE s.bookmark_id = ?1 AND s.tag_id = ?2 \
                     AND {PENDING}"
                ))?
                .exists(params![id, tag_id])?)
        };
        let mut c = Committed::default();
        for (&tag_id, name) in accept.iter().zip(&d.accept) {
            if !pending(tag_id)? {
                bail!("cannot accept {name:?}: not suggested for bookmark {id}");
            }
            c.tagged +=
                suggest::accept_rows(&tx, "bookmark_id = ?1 AND tag_id = ?2", params![id, tag_id])?;
        }
        for (&tag_id, name) in reject.iter().zip(&d.reject) {
            if !pending(tag_id)? {
                bail!("cannot reject {name:?}: not suggested for bookmark {id}");
            }
            c.rejected += tx
                .prepare_cached(
                    "UPDATE tag_suggestion SET state = 'rejected', updated_at = ?3 \
                     WHERE bookmark_id = ?1 AND tag_id = ?2",
                )?
                .execute(params![id, tag_id, now])?;
        }
        for &tag_id in &remove {
            suggest::reject_applied(&tx, id, tag_id, now)?;
            c.removed += tx
                .prepare_cached("DELETE FROM bookmark_tag WHERE bookmark_id = ?1 AND tag_id = ?2")?
                .execute(params![id, tag_id])?;
        }
        for &tag_id in &add {
            // A tag typed while it was also suggested takes the suggestion, keeping its source.
            let n =
                suggest::accept_rows(&tx, "bookmark_id = ?1 AND tag_id = ?2", params![id, tag_id])?;
            c.tagged += n;
            if n == 0 {
                c.tagged += tx
                    .prepare_cached(
                        "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) VALUES (?1, ?2)",
                    )?
                    .execute(params![id, tag_id])?;
            }
        }
        // Names that turned out canonical are tagged by `divert`; the CLI resolves them first.
        let new: Vec<String> = {
            let mut v = Vec::new();
            for name in propose {
                match resolve_tag(&tx, &name)? {
                    Some(tag_id) if is_canonical(&tx, tag_id)? => {
                        c.tagged += tx
                            .prepare_cached(
                                "INSERT OR IGNORE INTO bookmark_tag (bookmark_id, tag_id) \
                                 VALUES (?1, ?2)",
                            )?
                            .execute(params![id, tag_id])?;
                    }
                    _ => v.push(name),
                }
            }
            v
        };
        proposal::divert(&tx, &new, ProposalSource::User, id, now, &mut c.diverted)?;
        tx.prepare_cached(
            "INSERT OR REPLACE INTO review_bookmark (bookmark_id, batch_id, reviewed_at) \
             VALUES (?1, ?2, ?3)",
        )?
        .execute(params![id, batch, now])?;
        if c.tagged > 0 || c.removed > 0 {
            touch_many(&tx, &[id], now)?;
        }
        tx.commit()?;
        Ok(c)
    }
}

fn is_canonical(conn: &rusqlite::Connection, tag_id: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT canonical FROM tag WHERE id = ?1")?
        .query_row([tag_id], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// Up to `n` bookmarks spread across domains: untagged bookmarks before tagged ones, and within
/// each, round-robin over hosts, newest first per host. Hosts nothing has been reviewed from yet
/// come first, then hosts with more bookmarks waiting. `reviewed` counts reviewed bookmarks per
/// host ([`Store::reviewed_hosts`]).
pub fn discovery_order(queue: &[Queued], reviewed: &HashMap<String, usize>, n: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(n.min(queue.len()));
    for tagged in [false, true] {
        let mut hosts: HashMap<&str, Vec<i64>> = HashMap::new();
        for q in queue.iter().filter(|q| q.tagged == tagged) {
            hosts.entry(&q.host).or_default().push(q.id);
        }
        let mut hosts: Vec<(&str, Vec<i64>)> = hosts.into_iter().collect();
        for (_, ids) in &mut hosts {
            ids.sort_unstable_by(|a, b| b.cmp(a));
        }
        hosts.sort_by(|(ha, a), (hb, b)| {
            let seen = |h: &str| reviewed.get(h).copied().unwrap_or(0) > 0;
            seen(ha)
                .cmp(&seen(hb))
                .then(b.len().cmp(&a.len()))
                .then(ha.cmp(hb))
        });
        let rounds = hosts.iter().map(|(_, ids)| ids.len()).max().unwrap_or(0);
        for round in 0..rounds {
            for (_, ids) in &hosts {
                if out.len() == n {
                    return out;
                }
                if let Some(&id) = ids.get(round) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Up to `n` ids, least confident first: `scores` are (id, confidence in 0..1), ties broken by
/// newest first.
pub fn least_confident(mut scores: Vec<(i64, f64)>, n: usize) -> Vec<i64> {
    scores.sort_by(|(ia, a), (ib, b)| a.total_cmp(b).then(ib.cmp(ia)));
    scores.into_iter().take(n).map(|(id, _)| id).collect()
}

/// Up to `k` of `ids`, picked at random with `seed` (a partial Fisher-Yates shuffle).
pub fn sample(ids: &[i64], k: usize, seed: u64) -> Vec<i64> {
    let mut v = ids.to_vec();
    let k = k.min(v.len());
    let mut x = seed | 1;
    for i in 0..k {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = i + (x % (v.len() - i) as u64) as usize;
        v.swap(i, j);
    }
    v.truncate(k);
    v
}

/// How sure the classifier is about a bookmark, from its probabilities: 1 when every tag is
/// clearly in or out, 0 when one sits at 0.5. The tag it's least sure about decides.
pub fn confidence(probabilities: impl IntoIterator<Item = f64>) -> f64 {
    probabilities
        .into_iter()
        .map(|p| (2.0 * p - 1.0).abs())
        .fold(1.0, f64::min)
}

impl Store {
    /// The classifier's [`confidence`] for each of `ids` (live bookmarks; others are left out).
    pub fn review_confidence(
        &self,
        ids: &[i64],
        classifier: &crate::model::Classifier,
    ) -> Result<Vec<(i64, f64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT url, title, note, summary FROM bookmark WHERE id = ?1 AND deleted_at IS NULL",
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let item = stmt
                .query_row([id], |r| {
                    Ok(crate::choose::Item {
                        url: r.get(0)?,
                        title: r.get(1)?,
                        note: r.get(2)?,
                        summary: r.get(3)?,
                        text: None,
                    })
                })
                .optional()?;
            if let Some(item) = item {
                let p = classifier.probabilities(&item);
                out.push((id, confidence(p.into_iter().map(|(_, p)| p))));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: i64, host: &str, tagged: bool) -> Queued {
        Queued {
            id,
            host: host.into(),
            tagged,
            top: 0.0,
        }
    }

    #[test]
    fn spreads_across_domains() {
        let queue = [
            q(1, "a.com", false),
            q(2, "a.com", false),
            q(3, "a.com", false),
            q(4, "b.com", false),
            q(5, "b.com", false),
            q(6, "c.com", false),
            q(7, "d.com", true),
            q(8, "a.com", true),
        ];
        let none = HashMap::new();
        // Untagged first, one per host per round (biggest host first, newest first per host),
        // then the tagged ones.
        assert_eq!(discovery_order(&queue, &none, 10), [3, 5, 6, 2, 4, 1, 8, 7]);
        assert_eq!(discovery_order(&queue, &none, 4), [3, 5, 6, 2]);
        // Hosts already reviewed from wait.
        let seen = HashMap::from([("a.com".to_string(), 2)]);
        assert_eq!(discovery_order(&queue, &seen, 4), [5, 6, 3, 4]);
    }

    #[test]
    fn orders_by_confidence() {
        assert_eq!(confidence([0.5, 0.01]), 0.0);
        assert!((confidence([0.9, 0.05]) - 0.8).abs() < 1e-9);
        assert_eq!(confidence([]), 1.0);
        assert_eq!(
            least_confident(vec![(1, 0.9), (2, 0.1), (3, 0.5), (4, 0.1)], 3),
            [4, 2, 3]
        );
    }

    #[test]
    fn samples() {
        let ids: Vec<i64> = (1..=100).collect();
        let s = sample(&ids, 10, 42);
        assert_eq!(s.len(), 10);
        let mut d = s.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), 10);
        assert_eq!(s, sample(&ids, 10, 42));
        assert_eq!(sample(&ids[..3], 10, 1).len(), 3);
    }
}
