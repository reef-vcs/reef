-- R3.31: reviewing from the web UI (ADR 0086). A decline, a comment, an instruction's confirm or
-- cancel, and a landing asked for through the web listener record the web UI session they came
-- through, beside where a call through the remote listener records its token: 16 hex digits of
-- the digest of the session's token, never the token. NULL for any other call. Released
-- migrations are never edited.
ALTER TABLE lifecycle_transitions ADD COLUMN web_session TEXT;
ALTER TABLE comments ADD COLUMN web_session TEXT;
ALTER TABLE comment_events ADD COLUMN web_session TEXT;
ALTER TABLE landings ADD COLUMN web_session TEXT;
