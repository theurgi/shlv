-- Interests (task 034): broad topics the user names to seed the tag list. Interests and tags
-- aren't distinct kinds and nothing constrains one against the other: an interest needn't be a
-- tag, and a tag needn't sit under an interest.
--
-- Interests stored with `shlv tag seed --save`. Config can pin more (`[tags] interests`); those
-- aren't copied here.
CREATE TABLE interest (
  name       TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL
) WITHOUT ROWID;

-- Which tags were kept under which interest when seeding, for tag health. `interest` is a name,
-- not a reference: the interest may be pinned in config, or not stored at all.
CREATE TABLE interest_tag (
  interest TEXT NOT NULL,
  tag_id   INTEGER NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
  PRIMARY KEY (interest, tag_id)
) WITHOUT ROWID;
-- Serves moving links when a tag is merged away.
CREATE INDEX interest_tag_tag ON interest_tag(tag_id);
