-- R1.16: the index of the snapshot operations the daemon writes, one row per
-- operation, which records the workspace, the reason, and that the snapshot
-- has not left the machine. The columns are the ones in
-- docs/dev-tickets/phase-1/README.md. jj's operation log stays authoritative
-- (ADR 0003): every row can be rebuilt from it, except that the log cannot
-- tell `explicit` from `watcher` or `shutdown` from `final`. `local` is
-- cleared only by R4's publish. `taken_at` is the operation's end time as jj
-- recorded it. Released migrations are never edited.
CREATE TABLE snapshots(op_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL, commit_id TEXT NOT NULL,
          change_id TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('watcher','explicit','final','move','shutdown')),
          local INTEGER NOT NULL DEFAULT 1, taken_at INTEGER NOT NULL);

-- A workspace's snapshots, newest first: `ws inspect` and its counts. `op_id`
-- orders snapshots taken in the same millisecond, so the newest-first query
-- reads the index in order instead of sorting.
CREATE INDEX snapshots_by_workspace ON snapshots(workspace_id, taken_at DESC, op_id);
-- Every workspace's snapshots of one change: what R4's publish clears.
CREATE INDEX snapshots_by_change ON snapshots(repo_id, change_id);
