-- R3.16: the landed history projection (ADR 0075). landings.history is the `[history]` table of
-- the policy a landing was judged by, as JSON ({"message": "summary", "trailers": ["change",
-- "landing"]}), set when it lands; NULL before it lands and for landings before R3.16.
-- landing_changes.summarized is 1 when the change landed with its description rewritten as a
-- summary. Released migrations are never edited.
ALTER TABLE landings ADD COLUMN history TEXT;
ALTER TABLE landing_changes ADD COLUMN summarized INTEGER NOT NULL DEFAULT 0;
