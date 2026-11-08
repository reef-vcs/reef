-- R3.09: change lifecycle states (ADR 0063). lifecycle_transitions is every transition a change
-- went through, recorded pending before its operation is written and committed with the
-- operation's id after it (seq NULL while pending); GC maps operation_id through its rewrite and
-- sets operation_collected for one it dropped, and never deletes a row. lifecycle is each recorded
-- change's current state: its latest committed transition, plus who made its latest proposal. A
-- change with no row is a draft. Times are milliseconds since the Unix epoch. Released
-- migrations are never edited.
CREATE TABLE lifecycle_transitions(repo_id TEXT NOT NULL, id TEXT NOT NULL,   -- trn_<16 hex>
             seq INTEGER,                                     -- per repository, in commit order
             status TEXT NOT NULL,                            -- pending committed
             change_id TEXT NOT NULL, commit_id TEXT NOT NULL,
             from_state TEXT NOT NULL, to_state TEXT NOT NULL,
             cause TEXT NOT NULL,                             -- request undo observed
             round INTEGER NOT NULL, reason TEXT, superseded_by TEXT, destination TEXT,
             undoes_operation_id TEXT, undoes_transition_id TEXT,
             operation_id TEXT, operation_collected INTEGER NOT NULL DEFAULT 0,
             agent TEXT, actor TEXT NOT NULL,                 -- human agent daemon
             identity_id TEXT, identity_name TEXT, session_id TEXT,
             remote_token TEXT, remote_token_name TEXT,
             created_at INTEGER NOT NULL, recorded_at INTEGER,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX lifecycle_transitions_by_change ON lifecycle_transitions(repo_id, change_id, seq);
CREATE INDEX lifecycle_transitions_by_seq ON lifecycle_transitions(repo_id, seq);
CREATE INDEX lifecycle_transitions_by_operation ON lifecycle_transitions(repo_id, operation_id);
CREATE INDEX lifecycle_transitions_pending ON lifecycle_transitions(status) WHERE status = 'pending';
CREATE TABLE lifecycle(repo_id TEXT NOT NULL, change_id TEXT NOT NULL,
             state TEXT NOT NULL, round INTEGER NOT NULL, commit_id TEXT NOT NULL,
             destination TEXT, superseded_by TEXT, transition_id TEXT NOT NULL,
             proposed_by_agent TEXT, proposed_by_token TEXT,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, change_id)) WITHOUT ROWID;
CREATE INDEX lifecycle_by_state ON lifecycle(repo_id, state, updated_at);
CREATE INDEX lifecycle_by_updated ON lifecycle(repo_id, updated_at);
