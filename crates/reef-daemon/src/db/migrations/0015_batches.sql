-- R3.14: batches (ADR 0067). landing_batches is one row per attempt to land several queued changes
-- for one destination together: testing, bisecting, then finished with an outcome (landed, split,
-- unavailable, reformed, interrupted, busy). landing_batch_members holds its members in chain
-- order, each with its landing, queued commit, the chain's planned tree with it applied, and what
-- became of it. landing_batch_trials holds each tested prefix: its tree, the trial commit carrying
-- it, the executor jobs asked for it, and its result. landings gains the batch a landing is, or
-- was last, in; landing_changes gains the tested tree, its trial commit, the evaluation over the
-- tested tree's evidence, and the change's position in its batch. GC maps operation ids through
-- its rewrite and sets operation_collected for one it dropped; it never deletes a row. Times are
-- milliseconds since the Unix epoch. Released migrations are never edited.
CREATE TABLE landing_batches(repo_id TEXT NOT NULL, id TEXT NOT NULL,     -- bat_<16 hex>
             destination TEXT NOT NULL, base_commit TEXT NOT NULL,
             state TEXT NOT NULL,                        -- testing bisecting finished
             outcome TEXT,                               -- landed split unavailable reformed interrupted busy
             size INTEGER NOT NULL, culprit_position INTEGER,
             landed_count INTEGER NOT NULL DEFAULT 0,
             checks TEXT NOT NULL,                       -- batch proposal
             policy_commit TEXT, policy_sha256 TEXT,
             operation_id TEXT, operation_collected INTEGER NOT NULL DEFAULT 0,
             message TEXT, created_at INTEGER NOT NULL, finished_at INTEGER,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX landing_batches_by_created ON landing_batches(repo_id, created_at, id);
CREATE INDEX landing_batches_open ON landing_batches(repo_id, state) WHERE state != 'finished';
CREATE TABLE landing_batch_members(repo_id TEXT NOT NULL, batch_id TEXT NOT NULL,
             position INTEGER NOT NULL,                  -- from 1
             landing_id TEXT NOT NULL, change_id TEXT NOT NULL, queued_commit TEXT NOT NULL,
             planned_tree TEXT NOT NULL,
             result TEXT,                                -- landed culprit requeued dequeued left unavailable
             PRIMARY KEY (repo_id, batch_id, position)) WITHOUT ROWID;
CREATE INDEX landing_batch_members_by_landing ON landing_batch_members(repo_id, landing_id);
CREATE INDEX landing_batch_members_by_change ON landing_batch_members(repo_id, change_id);
CREATE TABLE landing_batch_trials(repo_id TEXT NOT NULL, batch_id TEXT NOT NULL,
             seq INTEGER NOT NULL,                       -- the order tested, from 0
             prefix INTEGER NOT NULL, tree TEXT NOT NULL,
             commit_id TEXT, operation_id TEXT, operation_collected INTEGER NOT NULL DEFAULT 0,
             result TEXT,                                -- passed failed unavailable cancelled
             job_ids TEXT NOT NULL DEFAULT '[]',         -- JSON list
             needs TEXT,                                 -- JSON: {"needs": [...], "total": n}
             attempts INTEGER NOT NULL DEFAULT 1,
             created_at INTEGER NOT NULL, finished_at INTEGER,
             PRIMARY KEY (repo_id, batch_id, seq)) WITHOUT ROWID;
CREATE INDEX landing_batch_trials_by_tree ON landing_batch_trials(repo_id, tree);
ALTER TABLE landings ADD COLUMN batch_id TEXT;
ALTER TABLE landing_changes ADD COLUMN tested_tree TEXT;
ALTER TABLE landing_changes ADD COLUMN tested_commit TEXT;
ALTER TABLE landing_changes ADD COLUMN tested_evaluation TEXT;
ALTER TABLE landing_changes ADD COLUMN batch_position INTEGER;
