-- R3.17: pushing landed history to a git remote (ADR 0077). destination_pushes is one row per
-- (repository, destination) whose policy pushes it (`[landing] push`): the remote and branch,
-- its state (pushed pending retrying diverged refused disabled), the destination's head and the
-- remote branch as last seen, how many landings the remote lacks, the last failure (reason,
-- message, git's redacted stderr, hint, next as JSON), the remote's head when diverged (JSON),
-- the retry schedule and since when it fails, and the last successful push and its operation. landings gains when a push
-- first carried the landed commit, and that push's operation. landing_batches gains the
-- `[history]` table a batch formed under (JSON, as landings.history), so a landing finished at
-- start after a crash records it (#297). Released migrations are never edited.
CREATE TABLE destination_pushes (
    repo_id             TEXT    NOT NULL,
    destination         TEXT    NOT NULL,
    git_remote          TEXT    NOT NULL,
    branch              TEXT    NOT NULL,
    state               TEXT    NOT NULL,
    local_commit        TEXT,
    remote_commit       TEXT,
    unpushed_count      INTEGER NOT NULL DEFAULT 0,
    reason              TEXT,
    message             TEXT,
    git_stderr          TEXT,
    hint                TEXT,
    next                TEXT,
    remote_head         TEXT,
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     INTEGER,
    failing_since       INTEGER,
    pushed_at           INTEGER,
    operation_id        TEXT,
    operation_collected INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (repo_id, destination)
) WITHOUT ROWID;
ALTER TABLE landings ADD COLUMN pushed_at INTEGER;
ALTER TABLE landings ADD COLUMN push_operation_id TEXT;
CREATE INDEX landings_unpushed ON landings(repo_id, destination)
    WHERE outcome = 'landed' AND pushed_at IS NULL;
ALTER TABLE landing_batches ADD COLUMN history TEXT;
