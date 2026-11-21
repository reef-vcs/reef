-- R3.33: the triage inbox (ADR 0088). inbox_marks holds where each person last looked at each
-- repository's landings: seq is a committed lifecycle_transitions.seq of the repository, and the
-- digest counts the landings recorded after it. A mark only moves forward, and never past the
-- repository's newest committed transition (Db::advance_inbox_mark). seen_at is when it last moved,
-- in milliseconds since the Unix epoch. Rows are per repository and deleted with it. Released
-- migrations are never edited.
CREATE TABLE inbox_marks(identity_id TEXT NOT NULL REFERENCES identities(id),
             repo_id TEXT NOT NULL,
             seq INTEGER NOT NULL,
             seen_at INTEGER NOT NULL,
             PRIMARY KEY (identity_id, repo_id)) WITHOUT ROWID;
