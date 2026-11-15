-- Tag suggestions from the tagging layers (domain rules now; the local model and the LLM later),
-- one row per bookmark and tag. A pending row waits for `shlv suggest accept` or `reject`;
-- accepting moves it into bookmark_tag with the same source and confidence. A rejected row stays
-- so that no layer offers that tag for that bookmark again.
CREATE TABLE tag_suggestion (
  bookmark_id INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
  tag_id      INTEGER NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
  source      TEXT NOT NULL CHECK (source IN ('rule', 'model', 'llm')),
  confidence  REAL,
  state       TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'rejected')),
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (bookmark_id, tag_id)
) WITHOUT ROWID;
-- Serves tag merges, which move suggestions along with bookmarks.
CREATE INDEX tag_suggestion_tag ON tag_suggestion(tag_id);
