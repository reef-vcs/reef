-- R3.01: evidence objects (signed in-toto statements in DSSE envelopes), the keys that signed
-- them, and which changes they were added for. `envelope` is the canonical envelope; `id` is its
-- sha256. Every other evidence column is read from the envelope's payload at insert, an index
-- over it. Released migrations are never edited.
CREATE TABLE evidence_keys(id TEXT PRIMARY KEY,            -- sha256 of public_key, lowercase hex
             algorithm TEXT NOT NULL CHECK (algorithm IN ('ed25519')),
             public_key BLOB NOT NULL,                     -- DER SubjectPublicKeyInfo
             first_seen_at INTEGER NOT NULL);
CREATE TABLE evidence(repo_id TEXT NOT NULL, id TEXT NOT NULL,
             envelope BLOB NOT NULL, format INTEGER NOT NULL,   -- 1: urn:reef-vcs:evidence:v1
             tree_id TEXT NOT NULL, kind TEXT NOT NULL,
             result TEXT NOT NULL CHECK (result IN ('pass','fail','error')),
             target TEXT, key_id TEXT NOT NULL REFERENCES evidence_keys(id),
             started_at INTEGER NOT NULL, finished_at INTEGER NOT NULL,
             added_at INTEGER NOT NULL, added_by_agent TEXT, remote_token_id TEXT,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX evidence_by_tree ON evidence(repo_id, tree_id, kind, target);
CREATE INDEX evidence_by_added ON evidence(repo_id, added_at DESC, id);
CREATE TABLE evidence_links(repo_id TEXT NOT NULL, change_id TEXT NOT NULL,
             evidence_id TEXT NOT NULL, commit_id TEXT NOT NULL, linked_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, change_id, evidence_id),
             FOREIGN KEY (repo_id, evidence_id) REFERENCES evidence(repo_id, id)) WITHOUT ROWID;
CREATE INDEX evidence_links_by_evidence ON evidence_links(repo_id, evidence_id);
