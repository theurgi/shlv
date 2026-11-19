//! Bookkeeping for AI Message Batches: which ones were submitted and not yet collected. The
//! batches themselves live with the provider ([`crate::ai`]); this part is plain storage.

use anyhow::Result;
use rusqlite::params;

use crate::store::{Store, now_ms};

/// A submitted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBatch {
    pub id: String,
    /// The `[provider.NAME]` it was sent to.
    pub provider: String,
    pub requests: i64,
    /// Unix milliseconds.
    pub created_at: i64,
}

impl Store {
    /// Records a submitted batch.
    pub fn add_batch(&mut self, id: &str, provider: &str, requests: usize) -> Result<()> {
        self.conn().execute(
            "INSERT INTO ai_batch (id, provider, requests, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, provider, requests as i64, now_ms()],
        )?;
        Ok(())
    }

    /// Batches not collected yet, oldest first.
    pub fn pending_batches(&self) -> Result<Vec<PendingBatch>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, provider, requests, created_at FROM ai_batch \
             WHERE collected_at IS NULL ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingBatch {
                id: r.get(0)?,
                provider: r.get(1)?,
                requests: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Marks a batch collected.
    pub fn finish_batch(&mut self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE ai_batch SET collected_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }
}

/// The `custom_id` for a bookmark's job: `b{id}-{s|t|st}`.
pub fn custom_id(bookmark: i64, summary: bool, tags: bool) -> String {
    let kind = match (summary, tags) {
        (true, true) => "st",
        (true, false) => "s",
        _ => "t",
    };
    format!("b{bookmark}-{kind}")
}

/// Reads a [`custom_id`] back: the bookmark, and whether it asked for a summary and for tags.
pub fn parse_custom_id(s: &str) -> Option<(i64, bool, bool)> {
    let (id, kind) = s.strip_prefix('b')?.split_once('-')?;
    let (summary, tags) = match kind {
        "st" => (true, true),
        "s" => (true, false),
        "t" => (false, true),
        _ => return None,
    };
    Some((id.parse().ok()?, summary, tags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_ids_round_trip() {
        for (s, t) in [(true, true), (true, false), (false, true)] {
            assert_eq!(parse_custom_id(&custom_id(42, s, t)), Some((42, s, t)));
        }
        assert_eq!(parse_custom_id("x42-s"), None);
        assert_eq!(parse_custom_id("b42-q"), None);
    }

    #[test]
    fn pending_batches() {
        let mut s = Store::open_in_memory().unwrap();
        s.add_batch("msgbatch_1", "claude", 3).unwrap();
        s.add_batch("msgbatch_2", "claude", 1).unwrap();
        s.finish_batch("msgbatch_1").unwrap();
        let p = s.pending_batches().unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].id.as_str(), p[0].requests), ("msgbatch_2", 1));
    }
}
