-- Small key-value settings the database keeps about itself, such as the hash of the canonical tag
-- list last mirrored from config.
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;
