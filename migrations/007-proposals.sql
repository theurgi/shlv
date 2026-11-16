-- Tag proposals (task 031): names outside the canonical list, waiting for the user to add them to
-- the list, alias them to a tag, or reject them. A proposal is never a tag on a bookmark; it
-- collects the bookmarks it was proposed for, which get the tag when it is accepted or aliased.
-- Accepted and aliased proposals are deleted (the name is then a tag or an alias). Rejected ones
-- stay, without bookmarks, so no source proposes the name again.
CREATE TABLE tag_proposal (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT,
  -- Who proposed it first.
  source      TEXT NOT NULL CHECK (source IN ('llm', 'user', 'import')),
  state       TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'rejected')),
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE tag_proposal_bookmark (
  proposal_id INTEGER NOT NULL REFERENCES tag_proposal(id) ON DELETE CASCADE,
  bookmark_id INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
  -- Who proposed it for this bookmark; an accepted proposal tags it with this source.
  source      TEXT NOT NULL CHECK (source IN ('llm', 'user', 'import')),
  PRIMARY KEY (proposal_id, bookmark_id)
) WITHOUT ROWID;
-- Serves deleting a bookmark and listing a bookmark's proposals.
CREATE INDEX tag_proposal_bookmark_bookmark ON tag_proposal_bookmark(bookmark_id);
