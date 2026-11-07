-- R3.03: operator identities, the daemon's issuing authorities, sessions, and the session keys
-- issued for them, with the certificate (a canonical DSSE envelope) each key was issued.
-- Machine-wide, as remote_tokens and evidence_keys are. No private key is stored here or
-- anywhere in the database. Enumerations are checked in Rust, not by CHECK, so that later
-- values (presence, issuer kinds) need no table rebuild. Released migrations are never edited.
CREATE TABLE identity_authorities(id TEXT PRIMARY KEY,         -- key id: sha256 of public_key
             algorithm TEXT NOT NULL, public_key BLOB NOT NULL, -- DER SPKI
             host TEXT NOT NULL, created_at INTEGER NOT NULL,
             retired_at INTEGER,                              -- stopped issuing
             revoked_at INTEGER, invalid_from INTEGER, revoke_reason TEXT);
CREATE TABLE identities(id TEXT PRIMARY KEY,                   -- idn_<16 hex>, never reused
             name TEXT NOT NULL UNIQUE,                       -- never reused, even once revoked
             kind TEXT NOT NULL, display_name TEXT, email TEXT,
             created_at INTEGER NOT NULL,
             revoked_at INTEGER, invalid_from INTEGER, revoke_reason TEXT);
CREATE TABLE sessions(id TEXT PRIMARY KEY,                     -- ses_<16 hex>
             identity_id TEXT NOT NULL REFERENCES identities(id),
             actor TEXT NOT NULL, agent TEXT, created_at INTEGER NOT NULL,
             channel TEXT NOT NULL,                           -- 'socket', or the remote token's id

             revoked_at INTEGER, invalid_from INTEGER, revoke_reason TEXT);
CREATE TABLE session_keys(key_id TEXT PRIMARY KEY,             -- the evidence key id
             session_id TEXT NOT NULL REFERENCES sessions(id),
             identity_id TEXT NOT NULL REFERENCES identities(id),
             authority_id TEXT NOT NULL REFERENCES identity_authorities(id),
             actor TEXT NOT NULL, presence TEXT NOT NULL, custody TEXT NOT NULL,
             certificate BLOB NOT NULL,                       -- canonical envelope
             not_before INTEGER NOT NULL, not_after INTEGER NOT NULL,
             issued_at INTEGER NOT NULL, issued_by_agent TEXT, remote_token_id TEXT,
             revoked_at INTEGER, invalid_from INTEGER, revoke_reason TEXT);
CREATE INDEX session_keys_by_identity ON session_keys(identity_id, not_after);
CREATE INDEX session_keys_by_session ON session_keys(session_id);
CREATE INDEX session_keys_by_expiry ON session_keys(not_after);
ALTER TABLE remote_tokens ADD COLUMN identity_id TEXT REFERENCES identities(id);
