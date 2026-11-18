-- R3.32: help requests from agents (ADR 0083). A help request is a comment of kind
-- help_request; its state, what it asked, the asking client session, the conflict it names and
-- its first answer are columns of comments (NULL for other kinds), updated as each event commits.
-- comment_events records each help move's states. Times are milliseconds since the Unix epoch.
-- Released migrations are never edited.
ALTER TABLE comments ADD COLUMN help_state TEXT;               -- open answered withdrawn
ALTER TABLE comments ADD COLUMN help_reason TEXT;              -- decision conflict blocked unclear other
ALTER TABLE comments ADD COLUMN help_tried TEXT;
ALTER TABLE comments ADD COLUMN help_question TEXT;
ALTER TABLE comments ADD COLUMN help_client_session TEXT;
ALTER TABLE comments ADD COLUMN help_conflict_id TEXT;
ALTER TABLE comments ADD COLUMN help_assignment_id TEXT;
ALTER TABLE comments ADD COLUMN help_answer_id TEXT;           -- the first answer
ALTER TABLE comments ADD COLUMN help_cause TEXT;               -- request change_final expired answer_cancelled
ALTER TABLE comments ADD COLUMN help_answered_at INTEGER;
ALTER TABLE comments ADD COLUMN help_closed_at INTEGER;
ALTER TABLE comment_events ADD COLUMN help_from TEXT;
ALTER TABLE comment_events ADD COLUMN help_to TEXT;
ALTER TABLE comment_events ADD COLUMN help_answer_id TEXT;     -- answer: the instruction that answered
CREATE INDEX comments_help_open ON comments(repo_id, created_at) WHERE help_state = 'open';
CREATE INDEX comments_help_by_change ON comments(repo_id, change_id, help_state)
             WHERE help_state IS NOT NULL;
