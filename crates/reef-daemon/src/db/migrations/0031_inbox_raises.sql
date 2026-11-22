-- R3.39: the inbox's raise ledger (ADR 0093). Each row is an inbox item's latest needs-you-now
-- episode: its key (`proposal:<repo>:<change>` and so on), its reason (the `InboxReason` number) and
-- a cause, which is the commit id for a proposal or a conflict and empty for anything else; another
-- reason or cause replaces the row. seq is the episode's raise: AUTOINCREMENT never reuses the seq
-- of a deleted row (https://sqlite.org/autoinc.html).
CREATE TABLE inbox_raises(seq INTEGER PRIMARY KEY AUTOINCREMENT,
             repo_id TEXT NOT NULL,
             key TEXT NOT NULL,
             reason INTEGER NOT NULL,
             cause TEXT NOT NULL,
             UNIQUE (key));
CREATE INDEX inbox_raises_repo ON inbox_raises(repo_id);
