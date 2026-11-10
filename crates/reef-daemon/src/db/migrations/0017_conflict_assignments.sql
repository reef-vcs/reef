-- R3.27: resolver assignments (ADR 0068). One open row per (repository, change, kind) for a change
-- that introduces a conflict; the daemon's follower opens, updates and closes rows as the
-- operation log moves, and claims, releases and assignments change them. An event's seq is the
-- repository's, kept in meta (`assignment_seq:<repo>`) so retention never lets it go back. Owners are copied in, so
-- a row reads the same after its provenance session is swept. Rows hold no operation ids: the
-- operations that announce events name the events. Times are milliseconds since the Unix epoch.
-- Released migrations are never edited.
CREATE TABLE conflict_assignments(repo_id TEXT NOT NULL, id TEXT NOT NULL,       -- asg_<16 hex>
             change_id TEXT NOT NULL, kind TEXT NOT NULL,                     -- tree
             commit_id TEXT NOT NULL, conflict_id TEXT NOT NULL, first_conflict_id TEXT NOT NULL,
             introduced INTEGER NOT NULL, files_total INTEGER NOT NULL,
             state TEXT NOT NULL,                     -- assigned queued taken unowned closed
             reason TEXT NOT NULL,                    -- why the state and owner are what they are
             owner_session_id TEXT, owner_workspace_id TEXT, owner_workspace_owner TEXT,
             owner_identity_id TEXT, owner_actor TEXT, owner_agent TEXT, owner_client_session TEXT,
             owner_ended_at INTEGER,                  -- the owner's session ended; grace runs from here
             priority INTEGER NOT NULL DEFAULT 0, queued_at INTEGER,
             claims INTEGER NOT NULL DEFAULT 0, events INTEGER NOT NULL DEFAULT 0,
             opened_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
             conflict_changed_at INTEGER NOT NULL,
             closed_at INTEGER, closed_commit_id TEXT, resolved_session_id TEXT,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE UNIQUE INDEX conflict_assignments_open ON conflict_assignments(repo_id, change_id, kind)
             WHERE closed_at IS NULL;
CREATE INDEX conflict_assignments_queue ON conflict_assignments(repo_id, priority DESC, queued_at, id)
             WHERE state = 'queued';
CREATE INDEX conflict_assignments_by_owner ON conflict_assignments(owner_workspace_id)
             WHERE closed_at IS NULL;
CREATE INDEX conflict_assignments_by_client ON conflict_assignments(owner_client_session)
             WHERE closed_at IS NULL;
CREATE INDEX conflict_assignments_by_change ON conflict_assignments(repo_id, change_id, closed_at);
CREATE INDEX conflict_assignments_closed ON conflict_assignments(repo_id, closed_at)
             WHERE closed_at IS NOT NULL;
CREATE TABLE conflict_assignment_events(repo_id TEXT NOT NULL, id TEXT NOT NULL, -- cae_<16 hex>
             seq INTEGER NOT NULL,                    -- per repository, in commit order
             assignment_id TEXT NOT NULL, change_id TEXT NOT NULL, conflict_id TEXT NOT NULL,
             from_state TEXT, to_state TEXT NOT NULL, reason TEXT NOT NULL,
             owner_session_id TEXT, owner_workspace_id TEXT, owner_workspace_owner TEXT,
             owner_identity_id TEXT, owner_actor TEXT, owner_agent TEXT, owner_client_session TEXT,
             owner_ended_at INTEGER,
             from_workspace_id TEXT,                  -- the owner's workspace before, when it moved
             priority INTEGER NOT NULL, note TEXT,    -- the caller's reason, at most 4 KiB
             by_actor TEXT NOT NULL,                  -- human agent daemon
             by_agent TEXT, by_identity_id TEXT, by_session_id TEXT, by_remote_token TEXT,
             at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE INDEX conflict_assignment_events_by_assignment
             ON conflict_assignment_events(repo_id, assignment_id, seq);
CREATE INDEX conflict_assignment_events_by_seq ON conflict_assignment_events(repo_id, seq);
