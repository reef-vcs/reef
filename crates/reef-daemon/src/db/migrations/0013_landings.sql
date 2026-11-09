-- R3.13: landings (ADR 0064). landings is one row per attempt to land: BLOCKED at the gate,
-- QUEUED while the landing worker has it, then LANDED, DEQUEUED or CANCELLED. landing_changes
-- holds each change of a landing (one until stacks and batches) with the commit it was queued at,
-- the commit that landed, and the policy evaluation as JSON. landing_evidence pins the evidence a
-- landed change's satisfied rules used: GC's retention never deletes it. landing_policies keeps
-- the policy bytes each landing evaluated, by SHA-256. GC maps operation ids through its rewrite
-- and sets operation_collected for one it dropped; it never deletes a row. Times are milliseconds
-- since the Unix epoch. Released migrations are never edited.
CREATE TABLE landings(repo_id TEXT NOT NULL, id TEXT NOT NULL,          -- lnd_<16 hex>
             destination TEXT NOT NULL,
             destination_before TEXT, destination_after TEXT,
             outcome TEXT NOT NULL,                      -- blocked queued landed dequeued cancelled
             reason TEXT, message TEXT,
             policy_commit TEXT, policy_blob TEXT, policy_sha256 TEXT,
             attempts INTEGER NOT NULL DEFAULT 1,
             enqueue_operation_id TEXT, operation_id TEXT,
             operation_collected INTEGER NOT NULL DEFAULT 0,
             agent TEXT, actor TEXT, identity_name TEXT,
             remote_token TEXT, remote_token_name TEXT,
             created_at INTEGER NOT NULL, finished_at INTEGER,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX landings_by_created ON landings(repo_id, created_at, id);
CREATE INDEX landings_open ON landings(repo_id, outcome) WHERE outcome = 'queued';
CREATE TABLE landing_changes(repo_id TEXT NOT NULL, landing_id TEXT NOT NULL, position INTEGER NOT NULL,
             change_id TEXT NOT NULL, queued_commit TEXT NOT NULL, queued_tree TEXT,
             landed_commit TEXT, landed_tree TEXT, rebased INTEGER NOT NULL DEFAULT 0,
             overlap TEXT, conflicts TEXT,                -- JSON: {"paths": [...], "total": n}
             decision TEXT, evaluation TEXT, evaluation_sha256 TEXT,
             PRIMARY KEY (repo_id, landing_id, position)) WITHOUT ROWID;
CREATE INDEX landing_changes_by_change ON landing_changes(repo_id, change_id);
CREATE TABLE landing_evidence(repo_id TEXT NOT NULL, landing_id TEXT NOT NULL,
             change_id TEXT NOT NULL, evidence_id TEXT NOT NULL,
             PRIMARY KEY (repo_id, evidence_id, landing_id)) WITHOUT ROWID;
CREATE TABLE landing_policies(repo_id TEXT NOT NULL, sha256 TEXT NOT NULL, bytes BLOB NOT NULL,
             first_used_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, sha256)) WITHOUT ROWID;
