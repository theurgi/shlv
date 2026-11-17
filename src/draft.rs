//! Drafting the tag list from a sample of bookmarks (task 035): choosing a sample that spreads
//! across sites, and merging the drafts of several requests into one.
//!
//! The provider call itself is in [`crate::ai`] (feature `ai`); this part needs no network.

use std::collections::HashMap;

use anyhow::Result;

use crate::store::{Bookmark, Store};

/// A bookmark's site: its lowercase host without a leading `www.`; empty if it has none.
pub fn site(url: &str) -> String {
    let host = crate::url::host(url).unwrap_or("").to_ascii_lowercase();
    match host.strip_prefix("www.") {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => host,
    }
}

/// A sample of the library.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// In the order they should be sent: interleaved across sites, so each request of a chunked
    /// draft sees a spread too.
    pub bookmarks: Vec<Bookmark>,
    /// Bookmarks in the library.
    pub total: usize,
    /// Sites in the library.
    pub total_sites: usize,
}

impl Sample {
    /// Sites in the sample.
    pub fn sites(&self) -> usize {
        let mut s: Vec<String> = self.bookmarks.iter().map(|b| site(&b.url)).collect();
        s.sort();
        s.dedup();
        s.len()
    }
}

impl Store {
    /// Up to `n` bookmarks spread across sites (see [`spread`]). Only ids and URLs are read to
    /// choose them: 60 ms at 100k bookmarks rather than 300 for whole ones.
    pub fn draft_sample(&self, n: usize) -> Result<Sample> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, url FROM bookmark WHERE deleted_at IS NULL")?;
        let all: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let total = all.len();
        let (ids, total_sites) = spread(all, n);
        let mut bookmarks = Vec::with_capacity(ids.len());
        for id in ids {
            bookmarks.extend(self.get(id)?);
        }
        Ok(Sample {
            bookmarks,
            total,
            total_sites,
        })
    }
}

/// Up to `n` of `bookmarks` (`(id, url)`), spread across sites: one from every site in turn
/// (sites with more bookmarks first), then a second from each that has one, and so on. Which
/// bookmark of a site comes next is fixed by a hash of its id, so the sample is the same from run
/// to run but not biased to the newest or oldest. Returns the ids chosen and how many sites there
/// are.
///
/// The whole library is still interleaved when it is smaller than `n`, for chunked drafts.
pub fn spread(bookmarks: Vec<(i64, String)>, n: usize) -> (Vec<i64>, usize) {
    let mut groups: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, url) in bookmarks {
        groups.entry(site(&url)).or_default().push(id);
    }
    let mut groups: Vec<(String, Vec<i64>)> = groups.into_iter().collect();
    for (_, g) in &mut groups {
        // Popped from the end.
        g.sort_by_key(|&id| std::cmp::Reverse((mix(id as u64), id)));
    }
    // Larger sites first; among equals, a hash of the name rather than the alphabet.
    groups.sort_by(|(a, ga), (b, gb)| {
        gb.len()
            .cmp(&ga.len())
            .then_with(|| fnv(a).cmp(&fnv(b)))
            .then_with(|| a.cmp(b))
    });
    let mut out = Vec::with_capacity(n.min(groups.iter().map(|(_, g)| g.len()).sum()));
    while out.len() < n {
        let mut took = false;
        for (_, g) in &mut groups {
            if out.len() == n {
                break;
            }
            if let Some(id) = g.pop() {
                out.push(id);
                took = true;
            }
        }
        if !took {
            break;
        }
    }
    (out, groups.len())
}

/// A 64-bit mix (splitmix64's finalizer).
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// A tag the provider drafted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftTag {
    /// Styled.
    pub name: String,
    pub description: Option<String>,
    /// One of the stored interests it falls under, if any.
    pub interest: Option<String>,
}

/// A listed tag the provider found redundant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redundant {
    /// On the list.
    pub tag: String,
    /// A listed tag or a drafted one.
    pub into: String,
    pub reason: Option<String>,
}

/// One checked draft reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drafted {
    pub tags: Vec<DraftTag>,
    pub redundant: Vec<Redundant>,
    /// Names the style refused, with the reason.
    pub refused: Vec<String>,
}

/// Several drafts merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Merged {
    /// With how many drafts proposed each; most proposed first, then in the order first
    /// proposed.
    pub tags: Vec<(DraftTag, usize)>,
    pub redundant: Vec<Redundant>,
    /// Drafted tags left out by `max`.
    pub cut: usize,
}

/// Merges drafts (one per request of a chunked draft): tags by name, keeping the first
/// description and interest given, ranked by how many drafts proposed them and cut to `max`;
/// redundant tags by tag, the first suggestion winning.
pub fn merge(drafts: &[Drafted], max: usize) -> Merged {
    let mut tags: Vec<(DraftTag, usize)> = Vec::new();
    let mut redundant: Vec<Redundant> = Vec::new();
    for d in drafts {
        for t in &d.tags {
            match tags.iter_mut().find(|(o, _)| o.name == t.name) {
                Some((o, n)) => {
                    *n += 1;
                    if o.description.is_none() {
                        o.description.clone_from(&t.description);
                    }
                    if o.interest.is_none() {
                        o.interest.clone_from(&t.interest);
                    }
                }
                None => tags.push((t.clone(), 1)),
            }
        }
        for r in &d.redundant {
            if !redundant.iter().any(|o| o.tag == r.tag) {
                redundant.push(r.clone());
            }
        }
    }
    // Stable: ties stay in the order first proposed.
    tags.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let cut = tags.len().saturating_sub(max);
    tags.truncate(max);
    Merged {
        tags,
        redundant,
        cut,
    }
}
