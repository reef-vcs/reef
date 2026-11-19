-- R3.43: verified human presence (ADR 0085). passkeys holds each person identity's WebAuthn
-- passkeys, machine-wide like identities: the credential id, the public key as DER SPKI with its
-- COSE algorithm, its fingerprint (SHA-256 of the SPKI, which a policy's [passkeys] pins), the last
-- sign count, and when and through which web UI session it was added, used and removed. A removed
-- row stays, so the record of every add and removal does. A session key a passkey approved names
-- the passkey, the in-memory approval it was issued through, and the web UI session that approved
-- it, and, once it signed its one evidence object, which (spent_repo_id, spent_evidence_id); an
-- evidence row names the web UI session that added it. Approvals and registration
-- challenges are never stored. Times are milliseconds since the Unix epoch. Released migrations are
-- never edited.
CREATE TABLE passkeys(id TEXT PRIMARY KEY,                     -- pk_<16 hex>, never reused
             identity_id TEXT NOT NULL REFERENCES identities(id),
             credential_id BLOB NOT NULL,
             public_key BLOB NOT NULL,                        -- DER SPKI
             algorithm INTEGER NOT NULL,                      -- COSE: -7, -8 or -257
             fingerprint TEXT NOT NULL,                       -- sha256 of public_key, hex
             rp_id TEXT NOT NULL,
             label TEXT NOT NULL,
             aaguid TEXT NOT NULL,                            -- 32 hex
             sign_count INTEGER NOT NULL,
             created_at INTEGER NOT NULL, created_web_session TEXT,
             last_used_at INTEGER,
             removed_at INTEGER, removed_reason TEXT, removed_web_session TEXT);
CREATE UNIQUE INDEX passkeys_live_credential ON passkeys(credential_id) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX passkeys_live_fingerprint ON passkeys(fingerprint) WHERE removed_at IS NULL;
CREATE INDEX passkeys_by_identity ON passkeys(identity_id);
ALTER TABLE session_keys ADD COLUMN passkey_id TEXT REFERENCES passkeys(id);
ALTER TABLE session_keys ADD COLUMN approval_id TEXT;
ALTER TABLE session_keys ADD COLUMN web_session TEXT;
ALTER TABLE session_keys ADD COLUMN spent_repo_id TEXT;
ALTER TABLE session_keys ADD COLUMN spent_evidence_id TEXT;
CREATE INDEX session_keys_by_passkey ON session_keys(passkey_id) WHERE passkey_id IS NOT NULL;
ALTER TABLE evidence ADD COLUMN web_session TEXT;
