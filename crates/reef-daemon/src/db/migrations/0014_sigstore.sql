-- R3.42: Sigstore keyless session keys (ADR 0066). identity_oidc holds the exact OIDC issuer and
-- subject each operator identity accepts; at most one live match per pair, so a certificate binds
-- to one identity or none. A keyless key is a session_keys row whose issuer_kind is 'sigstore',
-- whose certificate column holds its Sigstore bundle (v0.3 JSON, as registered), whose oidc_id
-- names the match it was bound through, and whose authority_id names the Fulcio CA that issued
-- it, recorded in identity_authorities with kind 'sigstore' and retired_at set, so it never
-- issues. Times are milliseconds since the Unix epoch. Released migrations are never edited.
CREATE TABLE identity_oidc(id TEXT PRIMARY KEY,                -- oidc_<16 hex>, never reused
             identity_id TEXT NOT NULL REFERENCES identities(id),
             issuer TEXT NOT NULL, subject TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             revoked_at INTEGER, invalid_from INTEGER, revoke_reason TEXT);
CREATE UNIQUE INDEX identity_oidc_live ON identity_oidc(issuer, subject) WHERE revoked_at IS NULL;
CREATE INDEX identity_oidc_by_identity ON identity_oidc(identity_id);
ALTER TABLE identity_authorities ADD COLUMN kind TEXT NOT NULL DEFAULT 'reef-daemon';
ALTER TABLE session_keys ADD COLUMN issuer_kind TEXT NOT NULL DEFAULT 'reef-daemon';
ALTER TABLE session_keys ADD COLUMN oidc_id TEXT REFERENCES identity_oidc(id);
