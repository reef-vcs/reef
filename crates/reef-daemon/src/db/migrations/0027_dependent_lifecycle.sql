-- R3.11: the dependent change lifecycle (ADR 0084). A transition the daemon records to carry a
-- proposal across a rewrite of a change below it (cause `dependency`) names that change and what
-- happened to it: amended, landed or superseded. Released migrations are never edited.
ALTER TABLE lifecycle_transitions ADD COLUMN dependency_change_id TEXT;
ALTER TABLE lifecycle_transitions ADD COLUMN dependency_event TEXT;      -- amended landed superseded
