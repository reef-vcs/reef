-- R3.33: the triage inbox (ADR 0088). inbox_marks holds where each person last looked at each
-- repository's landings: seq is a committed lifecycle_transitions.seq of the repository, and the
-- digest counts the landings recorded after it. A mark only moves forward, and never past the
-- repository's newest committed transition (Db::advance_inbox_mark). seen_at is when it last moved,
-- in milliseconds since the Unix epoch, and web_session the web UI session that moved it then, as
-- R3.31's web writes record theirs (0029_web_audit): 16 hex digits of the digest of the session's
-- token, NULL for a move through the socket or the remote listener. Rows are per repository and
-- deleted with it. Released migrations are never edited.
CREATE TABLE inbox_marks(identity_id TEXT NOT NULL REFERENCES identities(id),
             repo_id TEXT NOT NULL,
             seq INTEGER NOT NULL,
             seen_at INTEGER NOT NULL,
             web_session TEXT,
             PRIMARY KEY (identity_id, repo_id)) WITHOUT ROWID;
