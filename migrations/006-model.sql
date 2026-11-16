-- The trained tag classifier (task 022, feature `model`), stored in the database so everything
-- stays in one file. Derived data: it can always be retrained from the tags. `fingerprint`
-- describes the training data it was built from, so a stale model is detected cheaply.
CREATE TABLE model (
  name        TEXT PRIMARY KEY,
  data        BLOB NOT NULL,
  fingerprint TEXT NOT NULL,
  trained_at  INTEGER NOT NULL
) WITHOUT ROWID;
