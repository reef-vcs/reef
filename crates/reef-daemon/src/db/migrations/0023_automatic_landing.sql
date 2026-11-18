-- R3.45: automatic landing (ADR 0081). The automatic service keeps each open proposal's status in
-- memory and stores one thing: the evaluation key under which it queued a landing, so a landing
-- that comes back is not queued again until something the gate reads has changed, across restarts.
-- NULL for a landing a client asked for. Released migrations are never edited.
ALTER TABLE landings ADD COLUMN automatic_key TEXT;
