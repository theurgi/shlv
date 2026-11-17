-- Review rounds (task 039): `shlv review` goes through bookmarks in batches.
--
-- One row per batch, for the progress history: how many bookmarks were reviewed and skipped, how
-- many new names were proposed, and how many of the classifier's suggestions were shown and
-- accepted. Proposals tapering off and acceptance rising say the tag list has settled.
CREATE TABLE review_batch (
  id             INTEGER PRIMARY KEY,
  started_at     INTEGER NOT NULL,
  finished_at    INTEGER,
  reviewed       INTEGER NOT NULL DEFAULT 0,
  skipped        INTEGER NOT NULL DEFAULT 0,
  proposed       INTEGER NOT NULL DEFAULT 0,
  model_shown    INTEGER NOT NULL DEFAULT 0,
  model_accepted INTEGER NOT NULL DEFAULT 0,
  -- Tags added to the canonical list on the proposals screen after the batch.
  tags_added     INTEGER NOT NULL DEFAULT 0
);

-- Bookmarks reviewed, so later batches skip them. A reviewed bookmark's missing tags are negative
-- examples for the classifier; one reviewed with no tags at all is still done.
CREATE TABLE review_bookmark (
  bookmark_id INTEGER PRIMARY KEY REFERENCES bookmark(id) ON DELETE CASCADE,
  batch_id    INTEGER REFERENCES review_batch(id) ON DELETE SET NULL,
  reviewed_at INTEGER NOT NULL
);
