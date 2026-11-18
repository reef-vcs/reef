-- R3.28: resolution tiers and attempt caps (ADR 0082). Each assignment keeps the tier and attempt
-- cap trunk()'s reef-policy.toml gives its conflict as last read, how many resolver attempts
-- started, how many a person granted past the cap, a person's go-ahead, and (closed resolved)
-- whether landing the change needs a person's sign-off (read through conflict_assignments_by_change).
-- Times are milliseconds since the Unix epoch. Released migrations are never edited.
ALTER TABLE conflict_assignments ADD COLUMN tier TEXT;                 -- land approve ask; NULL until looked up
ALTER TABLE conflict_assignments ADD COLUMN attempt_cap INTEGER;       -- the policy's attempts
ALTER TABLE conflict_assignments ADD COLUMN tier_rule INTEGER;         -- the [[conflicts.rule]] index; NULL for the defaults
ALTER TABLE conflict_assignments ADD COLUMN tier_conflict_id TEXT;     -- the conflict id it was looked up for
ALTER TABLE conflict_assignments ADD COLUMN tier_policy_commit TEXT;   -- trunk()'s commit read
ALTER TABLE conflict_assignments ADD COLUMN tier_policy_sha256 TEXT;   -- NULL: no policy, the defaults
ALTER TABLE conflict_assignments ADD COLUMN tier_error TEXT;           -- why the tier is ask by failure
ALTER TABLE conflict_assignments ADD COLUMN tier_at INTEGER;
ALTER TABLE conflict_assignments ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conflict_assignments ADD COLUMN granted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conflict_assignments ADD COLUMN go_ahead_identity_id TEXT;
ALTER TABLE conflict_assignments ADD COLUMN go_ahead_at INTEGER;
ALTER TABLE conflict_assignments ADD COLUMN approval INTEGER NOT NULL DEFAULT 0;  -- closed resolved: landing needs a person's sign-off
UPDATE conflict_assignments SET attempts = claims + (CASE WHEN EXISTS (
         SELECT 1 FROM conflict_assignment_events e WHERE e.repo_id = conflict_assignments.repo_id
           AND e.assignment_id = conflict_assignments.id AND e.reason = 'default') THEN 1 ELSE 0 END);
