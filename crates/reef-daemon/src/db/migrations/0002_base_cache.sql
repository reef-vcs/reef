-- R1.02: the base cache, one row per materialized tree per repository. The
-- columns are the ones in docs/dev-tickets/phase-1/README.md plus `files`,
-- the number of files the tree holds (the README was amended to match).
-- A row is inserted only after bases/<tree_id>/ and bases/<tree_id>.state/
-- are complete; a directory without a row is incomplete and swept at daemon
-- start. Released migrations are never edited.
CREATE TABLE base_cache(repo_id TEXT NOT NULL, tree_id TEXT NOT NULL, path TEXT NOT NULL, bytes INTEGER NOT NULL,
           files INTEGER NOT NULL,
           created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL, PRIMARY KEY (repo_id, tree_id));

-- Eviction order for GC (R1.17): least recently used first, per repository.
CREATE INDEX base_cache_repo_last_used_at ON base_cache(repo_id, last_used_at);
