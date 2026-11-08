-- R3.05: executor jobs (checks requested on a tree, leased to one executor at a time) and the
-- evidence each job accepted. A claim token is kept only as its blake3 digest. JSON arrays are
-- sorted and deduplicated. kind and target in job_evidence are index columns read from the
-- verified payload at acceptance, for the completeness check; seq orders a job's evidence as it
-- was accepted, and a re-sent object keeps its first row. Released migrations are never edited.
CREATE TABLE jobs(repo_id TEXT NOT NULL, id TEXT NOT NULL,          -- job_<16 hex>
             protocol INTEGER NOT NULL,                             -- the protocol version it needs
             tree_id TEXT NOT NULL, commit_id TEXT, change_id TEXT,
             kinds TEXT NOT NULL, targets TEXT NOT NULL, requirements TEXT NOT NULL,   -- JSON arrays
             spec_digest TEXT NOT NULL,                             -- sha256 of the canonical spec
             wall_time_ms INTEGER NOT NULL, cpu_time_ms INTEGER, priority INTEGER NOT NULL,
             state TEXT NOT NULL,                                   -- queued running completed cancelled expired
             outcome TEXT, end_reason TEXT, message TEXT,
             attempt INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER NOT NULL,
             submitted_at INTEGER NOT NULL, queue_expires_at INTEGER NOT NULL,
             requested_by_agent TEXT, requested_by_token TEXT,
             claim_digest BLOB, claimed_at INTEGER, lease_ms INTEGER, lease_expires_at INTEGER,
             deadline_at INTEGER,
             executor_name TEXT, executor_version TEXT, executor_labels TEXT, executor_kinds TEXT,
             executor_protocol INTEGER, executor_agent TEXT, executor_token TEXT,
             executor_identity_id TEXT,
             progress TEXT,                                         -- JSON: phase, done, total, unit
             usage_wall_ms INTEGER, usage_cpu_ms INTEGER, elapsed_ms INTEGER NOT NULL DEFAULT 0,
             finished_at INTEGER, cancelled_by_agent TEXT, cancel_reason TEXT,
             PRIMARY KEY (repo_id, id)) WITHOUT ROWID;
CREATE UNIQUE INDEX jobs_by_id ON jobs(id);
CREATE INDEX jobs_open ON jobs(state, priority DESC, submitted_at, id) WHERE state IN ('queued','running');
CREATE INDEX jobs_open_by_spec ON jobs(repo_id, spec_digest) WHERE state IN ('queued','running');
CREATE INDEX jobs_by_submitted ON jobs(repo_id, submitted_at DESC, id);
CREATE INDEX jobs_by_finished ON jobs(finished_at) WHERE finished_at IS NOT NULL;
CREATE TABLE job_evidence(repo_id TEXT NOT NULL, job_id TEXT NOT NULL, evidence_id TEXT NOT NULL,
             seq INTEGER NOT NULL, attempt INTEGER NOT NULL, kind TEXT NOT NULL, target TEXT,
             created INTEGER NOT NULL, fresh INTEGER NOT NULL, accepted_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, job_id, evidence_id)) WITHOUT ROWID;
CREATE INDEX job_evidence_by_evidence ON job_evidence(repo_id, evidence_id);
CREATE INDEX job_evidence_by_seq ON job_evidence(repo_id, job_id, seq);
