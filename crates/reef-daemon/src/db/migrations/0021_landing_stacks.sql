-- R3.10: stacks (ADR 0078). A change lands with its stack, the ancestors not yet on its
-- destination, which are jj's parent edges and stored nowhere. landings gains `stack`: the landing
-- id of the stack's head in the LandChange request that made the row, so the request's landings
-- read together, and `stack_position`, its place in that stack from 0 at the bottom; both NULL for
-- a change landed alone. They record, and decide nothing. Released migrations are never edited.
ALTER TABLE landings ADD COLUMN stack TEXT;
ALTER TABLE landings ADD COLUMN stack_position INTEGER;
CREATE INDEX landings_by_stack ON landings(repo_id, stack) WHERE stack IS NOT NULL;
