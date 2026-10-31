-- R2.08: tokens for the remote listener, and which token created a workspace. Only a token's
-- blake3 digest is stored: its secret is shown once, at create or rotate. Revoked and expired
-- rows stay, as the record of which id had which name. Released migrations are never edited.
CREATE TABLE remote_tokens (
  id               TEXT PRIMARY KEY,          -- 16 lowercase hex, never reused
  name             TEXT NOT NULL,
  scope            TEXT NOT NULL CHECK (scope IN ('read', 'write')),
  digest           BLOB NOT NULL,             -- blake3(secret), 32 bytes
  previous_digest  BLOB,                      -- the secret before the last rotation
  previous_until   INTEGER,                   -- unix ms; previous_digest is valid until then
  created_at       INTEGER NOT NULL,
  expires_at       INTEGER,                   -- NULL: never
  rotated_at       INTEGER,
  revoked_at       INTEGER,
  last_used_at     INTEGER,
  last_used_from   TEXT
);
CREATE INDEX remote_tokens_name ON remote_tokens (name);
ALTER TABLE workspaces ADD COLUMN remote_token_id TEXT;
