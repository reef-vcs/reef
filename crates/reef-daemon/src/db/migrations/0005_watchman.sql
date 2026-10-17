-- R1.14: which watcher triggers each workspace's snapshots, and the tracked file count that
-- `watcher.backend = "auto"` selects by. A workspace's watcher is fixed at create, because jj
-- reads its working copy's `fsmonitor` settings when the working copy is loaded; rows from before
-- this migration were watched by notify. `tracked_files` is NULL until counted, at registration
-- or at the next daemon start. Released migrations are never edited.
ALTER TABLE workspaces ADD COLUMN watcher TEXT NOT NULL DEFAULT 'notify' CHECK (watcher IN ('notify','watchman'));
ALTER TABLE repositories ADD COLUMN tracked_files INTEGER;
