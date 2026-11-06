-- R3.02: the artifact store's index. Bytes are files under repos/<id>/artifacts/ (one per
-- sha256); a row with collected_at set is a tombstone: retention removed the bytes while some
-- evidence still names them. evidence_artifacts records which digests each evidence object's
-- payload lists, read from the payload at add. evidence_by_added (0007) already orders evidence
-- by age for retention. Released migrations are never edited.
CREATE TABLE artifacts(repo_id TEXT NOT NULL, sha256 TEXT NOT NULL,
             size INTEGER NOT NULL,
             stored_at INTEGER NOT NULL,       -- when its bytes were last stored
             used_at INTEGER NOT NULL,         -- stored, uploaded again, or named by evidence added
             collected_at INTEGER,             -- NULL while stored
             added_by_agent TEXT, remote_token_id TEXT,
             PRIMARY KEY (repo_id, sha256)) WITHOUT ROWID;
CREATE INDEX artifacts_by_use ON artifacts(repo_id, collected_at, used_at);
CREATE TABLE evidence_artifacts(repo_id TEXT NOT NULL, evidence_id TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             PRIMARY KEY (repo_id, evidence_id, sha256),
             FOREIGN KEY (repo_id, evidence_id) REFERENCES evidence(repo_id, id)) WITHOUT ROWID;
CREATE INDEX evidence_artifacts_by_sha256 ON evidence_artifacts(repo_id, sha256);
