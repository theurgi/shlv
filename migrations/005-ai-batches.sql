-- Message Batches submitted by `summarize --batch` and `suggest --batch` (task 024), so any later
-- run can collect them. Each request's custom_id names its bookmark and job (`b42-st`).
CREATE TABLE ai_batch (
  id           TEXT PRIMARY KEY,
  provider     TEXT NOT NULL,
  requests     INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  collected_at INTEGER
) WITHOUT ROWID;
