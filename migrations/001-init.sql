CREATE TABLE bookmark (
  id          INTEGER PRIMARY KEY,
  uid         TEXT NOT NULL UNIQUE,
  url         TEXT NOT NULL,
  url_norm    TEXT NOT NULL,
  title       TEXT,
  note        TEXT,
  summary     TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  deleted_at  INTEGER
);
CREATE UNIQUE INDEX bookmark_url ON bookmark(url_norm) WHERE deleted_at IS NULL;
-- Not in the plan's first cut: serves `ls` ordering without a full sort.
CREATE INDEX bookmark_created ON bookmark(created_at) WHERE deleted_at IS NULL;

CREATE TABLE tag (
  id         INTEGER PRIMARY KEY,
  uid        TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL UNIQUE,
  canonical  INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
);

CREATE TABLE tag_alias (
  alias  TEXT PRIMARY KEY,
  tag_id INTEGER NOT NULL REFERENCES tag(id) ON DELETE CASCADE
);

CREATE TABLE bookmark_tag (
  bookmark_id INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
  tag_id      INTEGER NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
  source      TEXT NOT NULL DEFAULT 'user',
  confidence  REAL,
  PRIMARY KEY (bookmark_id, tag_id)
);
-- Not in the plan's first cut: serves tag filters.
CREATE INDEX bookmark_tag_tag ON bookmark_tag(tag_id);

CREATE VIRTUAL TABLE bookmark_fts USING fts5(
  title, url, note, summary, tags,
  content='', contentless_delete=1,
  tokenize='unicode61 remove_diacritics 2', prefix='2 3'
);
