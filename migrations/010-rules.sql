-- Domain rules saved in the database (task 043): `shlv rule add`, the TUI's Rules view, and rules
-- offered in review. Rules in config (`[tags.rules]`) aren't copied here: they are pinned, and the
-- rules in effect are both sets together, as with interests.
--
-- A rule names its tag (as config rules do), so the tag is created only when the rule first
-- matches; renaming or merging the tag rewrites the name.
CREATE TABLE rule (
  host       TEXT NOT NULL,
  tag        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (host, tag)
) WITHOUT ROWID;

CREATE INDEX rule_tag ON rule (tag);

-- Rules the user turned down when review offered them, so they aren't offered again.
CREATE TABLE rule_declined (
  host       TEXT NOT NULL,
  tag        TEXT NOT NULL,
  PRIMARY KEY (host, tag)
) WITHOUT ROWID;
