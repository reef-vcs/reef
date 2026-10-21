-- R1.20: shared dependency stores, one row per (store name, lockfile hash) the daemon has built
-- or is building, under `<state>/stores/<name>/<lockfile_hash>/`. Stores are per daemon, not per
-- repository: `name` and `lockfile_hash` are the lookup key, and `repo_id` records the repository
-- that first built the store. A row is inserted when a build starts (`sealed = 0`, `built_at`
-- NULL) and sealed when it succeeds; a failed build deletes it. The columns are the ones in
-- docs/dev-tickets/phase-1/README.md. Released migrations are never edited.
CREATE TABLE stores(repo_id TEXT NOT NULL, name TEXT NOT NULL, lockfile_hash TEXT NOT NULL, path TEXT NOT NULL,
       built_at INTEGER, sealed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (repo_id, name, lockfile_hash));

-- Creates and builds look a store up by its key, whichever repository built it.
CREATE INDEX stores_by_key ON stores(name, lockfile_hash);
