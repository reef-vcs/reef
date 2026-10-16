-- R1.01: repositories, workspaces, leases, reclaims, and meta. The column
-- definitions are the ones in docs/dev-tickets/phase-1/README.md, verbatim.
-- Timestamps are INTEGER unix milliseconds UTC; ids are TEXT. Released
-- migrations are never edited; a schema change is a new numbered file.

-- workspaces is created first and names leases(id) before that table exists:
-- SQLite resolves foreign keys when a statement runs, not when a table is
-- created. A workspace and its first lease are inserted with lease_id NULL,
-- then the lease, then the update, in one transaction.
CREATE TABLE repositories(id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, jj_repo_path TEXT NOT NULL,
             colocated INTEGER NOT NULL, registered_at INTEGER NOT NULL);
CREATE TABLE workspaces(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL REFERENCES repositories(id),
           owner TEXT NOT NULL, purpose TEXT NOT NULL DEFAULT '',
           change_id TEXT NOT NULL, commit_id TEXT NOT NULL, base_tree_id TEXT NOT NULL,
           backend TEXT NOT NULL, path TEXT NOT NULL UNIQUE, state_dir TEXT NOT NULL,
           lease_id TEXT REFERENCES leases(id),
           state TEXT NOT NULL CHECK (state IN ('active','releasing','released','reclaimed')),
           divergent INTEGER NOT NULL DEFAULT 0, attention TEXT,
           created_at INTEGER NOT NULL, last_snapshot_at INTEGER, last_snapshot_op TEXT);
CREATE TABLE leases(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
       owner TEXT NOT NULL, issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
       last_heartbeat_at INTEGER NOT NULL, length_ms INTEGER NOT NULL, grace_ms INTEGER NOT NULL,
       ended_at INTEGER, end_reason TEXT CHECK (end_reason IN ('released','expired','restored')));   -- a move keeps the lease
CREATE TABLE reclaims(id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
         reason TEXT NOT NULL CHECK (reason IN ('released','expired','gc','restore')),
         final_op_id TEXT, snapshot_taken INTEGER NOT NULL, freed_bytes INTEGER, at INTEGER NOT NULL, detail TEXT);
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- daemon_instance_id, created_at, last_backup_at, rematerialize_pending (R1.18)

-- Listing a repository's workspaces by state (ws list, the lease manager, GC).
CREATE INDEX workspaces_repo_state ON workspaces(repo_id, state);
-- The lease tick's expiry scan, over unended leases only.
CREATE INDEX leases_unended_expires_at ON leases(expires_at) WHERE ended_at IS NULL;
-- The reclaim log, newest first, per repository.
CREATE INDEX reclaims_repo_at ON reclaims(repo_id, at);
