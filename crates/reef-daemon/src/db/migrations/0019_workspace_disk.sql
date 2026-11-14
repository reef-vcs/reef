-- #272: the daemon-kept disk figure of each active workspace (ADR 0076, docs/disk.md), one row
-- per active workspace that has been walked, written after each walk and deleted when the
-- workspace ends. `largest` and `missing_dependencies` are JSON arrays. The last four columns are #274's
-- (idle eviction), carried from the start so that ticket needs no migration; #272 never writes
-- them. Released migrations are never edited.
CREATE TABLE workspace_disk(repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
       tracked_bytes INTEGER NOT NULL, build_bytes INTEGER NOT NULL, unshared_bytes INTEGER,
       files INTEGER NOT NULL, largest TEXT NOT NULL DEFAULT '[]',
       missing_dependencies TEXT NOT NULL DEFAULT '[]', incomplete INTEGER NOT NULL DEFAULT 0,
       measured_at INTEGER NOT NULL,
       build_output TEXT, evicted_at INTEGER, evicted_bytes INTEGER, evict_reason TEXT,
       PRIMARY KEY (repo_id, workspace_id));
