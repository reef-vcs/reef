-- R3.12: comments on changes, and instructions to the session that made them (ADR 0069).
-- comments is each comment, written pending before its operation and committed with the
-- operation's id after it; an instruction's state, estimate, target and claim are its columns
-- (NULL for a note), updated as each event commits. comment_events is every write: the creation
-- and each move of an instruction, pending then committed in the same order, with what the move
-- set. GC maps operation_id through its rewrite and sets operation_collected for one it dropped,
-- and never deletes a row. Times are milliseconds since the Unix epoch. Released migrations are
-- never edited.
CREATE TABLE comments(repo_id TEXT NOT NULL, id TEXT NOT NULL,          -- cmt_<16 hex>
             status TEXT NOT NULL,                                     -- pending committed
             change_id TEXT NOT NULL, round INTEGER NOT NULL, commit_id TEXT NOT NULL,
             path TEXT, side TEXT, start_line INTEGER, end_line INTEGER, reply_to TEXT,
             kind TEXT NOT NULL,                                       -- note instruction
             text TEXT NOT NULL, text_sha256 TEXT NOT NULL,
             agent TEXT, actor TEXT NOT NULL, identity_id TEXT, identity_name TEXT,
             session_id TEXT, remote_token TEXT, remote_token_name TEXT,
             created_at INTEGER NOT NULL,
             operation_id TEXT, operation_collected INTEGER NOT NULL DEFAULT 0, recorded_at INTEGER,
             state TEXT, cause TEXT, estimate TEXT, estimate_digest TEXT,
             target_source TEXT, target_session_id TEXT, target_client_session TEXT,
             target_harness TEXT, target_agent TEXT, target_workspace_id TEXT,
             claimed_client_session TEXT, claimed_workspace_id TEXT, claimed_agent TEXT,
             claimed_token TEXT, channel TEXT, taken INTEGER NOT NULL DEFAULT 0, summary TEXT,
             confirmed_at INTEGER, delivered_at INTEGER, started_at INTEGER, finished_at INTEGER,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX comments_by_change ON comments(repo_id, change_id, created_at);
CREATE INDEX comments_by_created ON comments(repo_id, created_at, id);
CREATE INDEX comments_open ON comments(repo_id, state, updated_at)
             WHERE state IN ('awaiting_confirmation', 'pending', 'delivered', 'running');
CREATE INDEX comments_by_target ON comments(target_client_session, state);
CREATE TABLE comment_events(repo_id TEXT NOT NULL, id TEXT NOT NULL,   -- cev_<16 hex>
             seq INTEGER,                                              -- per repository, in commit order
             status TEXT NOT NULL,                                     -- pending committed
             comment_id TEXT NOT NULL, change_id TEXT NOT NULL,
             event TEXT NOT NULL,                     -- comment instruct confirm claim start finish cancel release
             from_state TEXT, to_state TEXT,
             cause TEXT NOT NULL,                                      -- request reproposed change_final expired
             transition_id TEXT,                                       -- reproposed: the transition
             agent TEXT, actor TEXT NOT NULL, identity_id TEXT, remote_token TEXT, summary TEXT,
             claimed_client_session TEXT, claimed_workspace_id TEXT, claimed_agent TEXT,
             claimed_token TEXT, channel TEXT, taken INTEGER NOT NULL DEFAULT 0,
             operation_id TEXT, operation_collected INTEGER NOT NULL DEFAULT 0,
             created_at INTEGER NOT NULL, recorded_at INTEGER,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX comment_events_by_comment ON comment_events(repo_id, comment_id, seq);
CREATE INDEX comment_events_pending ON comment_events(status) WHERE status = 'pending';
CREATE INDEX comment_events_by_seq ON comment_events(repo_id, seq);
CREATE INDEX jobs_by_tree ON jobs(repo_id, tree_id);
