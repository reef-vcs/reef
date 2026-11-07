-- R3.04: provenance records (signed in-toto statements about the commits the daemon records for a
-- workspace, in DSSE envelopes) and each workspace's provenance session with its declared context.
-- `envelope` is the canonical envelope; `id` is its sha256. Every other provenance column is read
-- from the payload at insert, an index over it. No instruction text and no private key is stored.
-- Released migrations are never edited.
CREATE TABLE provenance(repo_id TEXT NOT NULL, id TEXT NOT NULL,
             envelope BLOB NOT NULL, format INTEGER NOT NULL,        -- 1: urn:reef-vcs:provenance:v1
             commit_id TEXT NOT NULL, change_id TEXT NOT NULL, tree_id TEXT,
             workspace_id TEXT NOT NULL, session_id TEXT NOT NULL, sequence INTEGER NOT NULL,
             kind TEXT NOT NULL, key_id TEXT NOT NULL REFERENCES evidence_keys(id),
             agent TEXT, model TEXT, harness TEXT, task TEXT, instruction_sha256 TEXT,
             client_session TEXT, context_digest TEXT NOT NULL,   -- sha256 of the canonical context
             recorded_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE UNIQUE INDEX provenance_by_commit ON provenance(repo_id, commit_id);
CREATE INDEX provenance_by_change ON provenance(repo_id, change_id, recorded_at);
CREATE INDEX provenance_by_session ON provenance(session_id, sequence);
CREATE INDEX provenance_by_recorded ON provenance(repo_id, recorded_at DESC, id);
CREATE INDEX provenance_by_key ON provenance(key_id);
CREATE TABLE provenance_sessions(session_id TEXT PRIMARY KEY,          -- ses_<16 hex>; in `sessions` once certified
             repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
             identity_id TEXT, actor TEXT NOT NULL, agent TEXT,
             channel TEXT NOT NULL,                                -- 'socket', or the creating token's id
             model TEXT, harness TEXT, task TEXT, instruction_sha256 TEXT, client_session TEXT,
             declared_at INTEGER, declared_by_agent TEXT,
             next_sequence INTEGER NOT NULL DEFAULT 1, missing_records INTEGER NOT NULL DEFAULT 0,
             problem TEXT,                                         -- why records are unbound, if they are
             started_at INTEGER NOT NULL, ended_at INTEGER);
CREATE UNIQUE INDEX provenance_sessions_live ON provenance_sessions(workspace_id) WHERE ended_at IS NULL;
CREATE INDEX provenance_sessions_by_repo ON provenance_sessions(repo_id, started_at);
