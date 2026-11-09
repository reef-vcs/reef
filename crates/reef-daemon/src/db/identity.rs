//! Queries on `identity_authorities`, `identities`, `sessions` and
//! `session_keys` (R3.03, ADR 0057). All four are machine-wide, as
//! `remote_tokens` and `evidence_keys` are, and none holds a private key.
//!
//! A session key's `certificate` is authoritative and its other columns
//! index it; reads take every field they report from the verified
//! certificate (`crate::identity::read`). The revocation columns are the
//! daemon's own records: `invalid_from` is never after `revoked_at`, and a
//! later revocation only ever moves `invalid_from` earlier.

use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::{bytes_at, integer_at, text_at};
use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, unique_match};
use crate::model::Timestamp;

/// The record kind prefix lookups name.
const KEY_WHAT: &str = "session key";

/// A revocation, as the daemon records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationRow {
    /// When it was made.
    pub revoked_at: Timestamp,
    /// From when signatures stop counting; never after `revoked_at`.
    pub invalid_from: Timestamp,
    /// Why; `None` for no reason given.
    pub reason: Option<String>,
}

/// One row of `identity_authorities`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityRow {
    /// SHA-256 of `public_key`, lowercase hex.
    pub id: String,
    /// `ed25519`.
    pub algorithm: String,
    /// The DER `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
    /// The host it was made on.
    pub host: String,
    /// When it was made.
    pub created_at: Timestamp,
    /// When it stopped issuing; `None` for the current one.
    pub retired_at: Option<Timestamp>,
    /// Its revocation.
    pub revocation: Option<RevocationRow>,
    /// `reef-daemon`, or `sigstore` for a Fulcio CA a keyless key came
    /// from (R3.42), which never issues.
    pub kind: String,
}

/// One row of `identity_oidc` (R3.42): an exact OIDC issuer and subject an
/// identity accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcMatchRow {
    /// `oidc_` and 16 hex digits.
    pub id: String,
    /// The identity it binds to.
    pub identity_id: String,
    /// The OIDC issuer, exact.
    pub issuer: String,
    /// The certificate's subject (its SAN), exact.
    pub subject: String,
    /// When it was added.
    pub created_at: Timestamp,
    /// Its revocation.
    pub revocation: Option<RevocationRow>,
}

/// One row of `identities`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRow {
    /// `idn_` and 16 hex digits.
    pub id: String,
    /// Unique among all identities, never reused.
    pub name: String,
    /// `person` or `ci`.
    pub kind: String,
    /// For display.
    pub display_name: Option<String>,
    /// For display.
    pub email: Option<String>,
    /// When it was created.
    pub created_at: Timestamp,
    /// Its revocation.
    pub revocation: Option<RevocationRow>,
}

/// One row of `sessions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    /// `ses_` and 16 hex digits.
    pub id: String,
    /// The identity its keys answer to.
    pub identity_id: String,
    /// The actor its keys are declared for.
    pub actor: String,
    /// The first key's issuing agent.
    pub agent: Option<String>,
    /// When its first key was issued.
    pub created_at: Timestamp,
    /// Where its first key was issued: `socket`, or the remote token's id.
    /// Renewal must come through the same one.
    pub channel: String,
    /// Its revocation.
    pub revocation: Option<RevocationRow>,
}

/// One row of `session_keys`. Columns are read leniently, as evidence rows
/// are: a value of the wrong type written behind the daemon reads as zero
/// or empty, since reads take the certificate's values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionKeyRow {
    /// The key's id: SHA-256 of its DER `SubjectPublicKeyInfo`.
    pub key_id: String,
    /// Its session.
    pub session_id: String,
    /// The identity it answers to.
    pub identity_id: String,
    /// The authority that certified it.
    pub authority_id: String,
    /// `human`, `agent` or `executor`.
    pub actor: String,
    /// `none`.
    pub presence: String,
    /// `client` or `daemon`.
    pub custody: String,
    /// The canonical envelope.
    pub certificate: Vec<u8>,
    /// The start of its window.
    pub not_before: Timestamp,
    /// Its end, inclusive.
    pub not_after: Timestamp,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// The issuing request's `reef-agent`.
    pub issued_by_agent: Option<String>,
    /// The issuing request's remote token.
    pub remote_token_id: Option<String>,
    /// Its own revocation.
    pub revocation: Option<RevocationRow>,
    /// `reef-daemon`, or `sigstore` for a keyless key (R3.42), whose
    /// `certificate` is its Sigstore bundle.
    pub issuer_kind: String,
    /// The OIDC match a keyless key was bound through.
    pub oidc_id: Option<String>,
}

/// What stopped [`Db::insert_identity_with_oidc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InTheWay {
    /// An identity with the name, revoked or not.
    Identity(IdentityRow),
    /// A live match with one of the issuers and subjects.
    Match(OidcMatchRow),
}

/// What [`Db::insert_session_key`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
    /// The key (and its new session) is stored.
    Stored,
    /// Nothing: the identity holds its limit of unexpired keys.
    Full,
}

/// Which session keys a listing returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionKeyFilter {
    /// Only this identity's keys, by id.
    pub identity_id: Option<String>,
    /// Only this session's keys.
    pub session_id: Option<String>,
    /// Also keys expired before this time, or revoked (they and their
    /// session); `None` lists only those live at the time given.
    pub live_at: Option<Timestamp>,
    /// Only keys of this issuer kind (`reef-daemon` or `sigstore`).
    pub issuer_kind: Option<String>,
}

/// The records reads of session keys need beside the keys: the sessions
/// and identities their certificates (or columns) name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyRecords {
    /// Sessions, by id.
    pub sessions: HashMap<String, SessionRow>,
    /// Identities, by id.
    pub identities: HashMap<String, IdentityRow>,
}

fn revocation_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<RevocationRow>> {
    let SqlValue::Integer(revoked_at) = row.get::<_, SqlValue>(idx)? else {
        return Ok(None);
    };
    let invalid_from = match row.get::<_, SqlValue>(idx + 1)? {
        SqlValue::Integer(at) => at.min(revoked_at),
        _ => revoked_at,
    };
    Ok(Some(RevocationRow {
        revoked_at: Timestamp(revoked_at),
        invalid_from: Timestamp(invalid_from),
        reason: text_at(row, idx + 2)?,
    }))
}

const AUTHORITY_COLUMNS: &str = "id, algorithm, public_key, host, created_at, retired_at, revoked_at, invalid_from, revoke_reason, kind";

fn authority_from(row: &Row<'_>) -> rusqlite::Result<AuthorityRow> {
    Ok(AuthorityRow {
        id: text_at(row, 0)?.unwrap_or_default(),
        algorithm: text_at(row, 1)?.unwrap_or_default(),
        public_key: bytes_at(row, 2)?.unwrap_or_default(),
        host: text_at(row, 3)?.unwrap_or_default(),
        created_at: Timestamp(integer_at(row, 4)?),
        retired_at: match row.get::<_, SqlValue>(5)? {
            SqlValue::Null => None,
            _ => Some(Timestamp(integer_at(row, 5)?)),
        },
        revocation: revocation_at(row, 6)?,
        kind: text_at(row, 9)?.unwrap_or_default(),
    })
}

const OIDC_COLUMNS: &str =
    "id, identity_id, issuer, subject, created_at, revoked_at, invalid_from, revoke_reason";

fn oidc_from(row: &Row<'_>) -> rusqlite::Result<OidcMatchRow> {
    Ok(OidcMatchRow {
        id: text_at(row, 0)?.unwrap_or_default(),
        identity_id: text_at(row, 1)?.unwrap_or_default(),
        issuer: text_at(row, 2)?.unwrap_or_default(),
        subject: text_at(row, 3)?.unwrap_or_default(),
        created_at: Timestamp(integer_at(row, 4)?),
        revocation: revocation_at(row, 5)?,
    })
}

const IDENTITY_COLUMNS: &str =
    "id, name, kind, display_name, email, created_at, revoked_at, invalid_from, revoke_reason";

fn identity_from(row: &Row<'_>) -> rusqlite::Result<IdentityRow> {
    Ok(IdentityRow {
        id: text_at(row, 0)?.unwrap_or_default(),
        name: text_at(row, 1)?.unwrap_or_default(),
        kind: text_at(row, 2)?.unwrap_or_default(),
        display_name: text_at(row, 3)?,
        email: text_at(row, 4)?,
        created_at: Timestamp(integer_at(row, 5)?),
        revocation: revocation_at(row, 6)?,
    })
}

const SESSION_COLUMNS: &str =
    "id, identity_id, actor, agent, created_at, revoked_at, invalid_from, revoke_reason, channel";

fn session_from(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: text_at(row, 0)?.unwrap_or_default(),
        identity_id: text_at(row, 1)?.unwrap_or_default(),
        actor: text_at(row, 2)?.unwrap_or_default(),
        agent: text_at(row, 3)?,
        created_at: Timestamp(integer_at(row, 4)?),
        revocation: revocation_at(row, 5)?,
        channel: text_at(row, 8)?.unwrap_or_default(),
    })
}

const KEY_COLUMNS: &str = "k.key_id, k.session_id, k.identity_id, k.authority_id, k.actor, \
     k.presence, k.custody, k.certificate, k.not_before, k.not_after, k.issued_at, \
     k.issued_by_agent, k.remote_token_id, k.revoked_at, k.invalid_from, k.revoke_reason, \
     k.issuer_kind, k.oidc_id";

fn key_from(row: &Row<'_>) -> rusqlite::Result<SessionKeyRow> {
    Ok(SessionKeyRow {
        key_id: text_at(row, 0)?.unwrap_or_default(),
        session_id: text_at(row, 1)?.unwrap_or_default(),
        identity_id: text_at(row, 2)?.unwrap_or_default(),
        authority_id: text_at(row, 3)?.unwrap_or_default(),
        actor: text_at(row, 4)?.unwrap_or_default(),
        presence: text_at(row, 5)?.unwrap_or_default(),
        custody: text_at(row, 6)?.unwrap_or_default(),
        certificate: bytes_at(row, 7)?.unwrap_or_default(),
        not_before: Timestamp(integer_at(row, 8)?),
        not_after: Timestamp(integer_at(row, 9)?),
        issued_at: Timestamp(integer_at(row, 10)?),
        issued_by_agent: text_at(row, 11)?,
        remote_token_id: text_at(row, 12)?,
        revocation: revocation_at(row, 13)?,
        issuer_kind: text_at(row, 16)?.unwrap_or_default(),
        oidc_id: text_at(row, 17)?,
    })
}

/// The revocation `table`'s row `id` ends up with after one at `new`:
/// the first `revoked_at` kept, `invalid_from` the earlier of the two, and
/// the latest reason given.
fn revoke_in(
    tx: &Transaction<'_>,
    table: &str,
    id_column: &str,
    id: &str,
    new: &RevocationRow,
) -> Result<usize, DbError> {
    Ok(tx.execute(
        &format!(
            "UPDATE {table} SET revoked_at = COALESCE(revoked_at, ?2),
                                invalid_from = min(COALESCE(invalid_from, ?3), ?3),
                                revoke_reason = COALESCE(?4, revoke_reason)
             WHERE {id_column} = ?1"
        ),
        params![id, new.revoked_at, new.invalid_from, new.reason],
    )?)
}

/// A session key `k` with what its liveness depends on: its session `s`,
/// its identity `i`, its authority `a` and, for a keyless key, its OIDC
/// match `o`, by its columns.
const LIVE_FROM: &str = "FROM session_keys k
     JOIN sessions s ON s.id = k.session_id
     LEFT JOIN identities i ON i.id = k.identity_id
     LEFT JOIN identity_authorities a ON a.id = k.authority_id
     LEFT JOIN identity_oidc o ON o.id = k.oidc_id";

/// That none of `k`, `s`, `i`, `a` and `o` is revoked.
const LIVE: &str = "k.revoked_at IS NULL AND s.revoked_at IS NULL \
     AND i.revoked_at IS NULL AND a.revoked_at IS NULL AND o.revoked_at IS NULL";

/// `?, ?, …`: `n` placeholders.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

impl Db {
    /// Every authority, newest first.
    pub fn authorities(&self) -> Result<Vec<AuthorityRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {AUTHORITY_COLUMNS} FROM identity_authorities ORDER BY created_at DESC, id"
            ))?;
            stmt.query_map([], authority_from)?.collect()
        })
    }

    /// Makes `new` the current authority in one transaction: every other
    /// current one retired at `new.created_at`, and `revoke`, if given,
    /// applied to the authority it names.
    pub fn replace_authority(
        &self,
        new: &AuthorityRow,
        revoke: Option<(&str, &RevocationRow)>,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                "UPDATE identity_authorities SET retired_at = ?1
                 WHERE retired_at IS NULL AND kind = 'reef-daemon'",
                [new.created_at],
            )?;
            if let Some((id, revocation)) = revoke {
                revoke_in(tx, "identity_authorities", "id", id, revocation)?;
            }
            tx.execute(
                &format!(
                    "INSERT INTO identity_authorities ({AUTHORITY_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, NULL, 'reef-daemon')"
                ),
                params![
                    new.id,
                    new.algorithm,
                    new.public_key,
                    new.host,
                    new.created_at
                ],
            )?;
            Ok(())
        })
    }

    /// Retires every current authority at `at`: a restore does, so the
    /// restored daemon issues from a new one (R3.03).
    pub fn retire_authorities(&self, at: Timestamp) -> Result<usize, DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE identity_authorities SET retired_at = ?1
                 WHERE retired_at IS NULL AND kind = 'reef-daemon'",
                [at],
            )
        })
    }

    /// Every identity, newest first.
    pub fn identities(&self) -> Result<Vec<IdentityRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {IDENTITY_COLUMNS} FROM identities ORDER BY created_at DESC, id"
            ))?;
            stmt.query_map([], identity_from)?.collect()
        })
    }

    /// The identity `text` names: by id when it starts with `idn_`, which
    /// no name may (`reef_evidence::session::RESERVED_PREFIXES`), else by
    /// name. One rule, so no text can name two identities.
    pub fn identity(&self, text: &str) -> Result<Option<IdentityRow>, DbError> {
        let column = if text.starts_with("idn_") {
            "id"
        } else {
            "name"
        };
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {IDENTITY_COLUMNS} FROM identities WHERE {column} = ?1"),
                [text],
                identity_from,
            )
            .optional()
        })
    }

    /// Inserts `row` unless an identity already has its name, which is
    /// returned instead.
    pub fn insert_identity(&self, row: &IdentityRow) -> Result<Option<IdentityRow>, DbError> {
        self.in_transaction(|tx| {
            let existing = tx
                .query_row(
                    &format!("SELECT {IDENTITY_COLUMNS} FROM identities WHERE name = ?1"),
                    [&row.name],
                    identity_from,
                )
                .optional()?;
            if existing.is_some() {
                return Ok(existing);
            }
            tx.execute(
                &format!(
                    "INSERT INTO identities ({IDENTITY_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL)"
                ),
                params![
                    row.id,
                    row.name,
                    row.kind,
                    row.display_name,
                    row.email,
                    row.created_at
                ],
            )?;
            Ok(None)
        })
    }

    /// Inserts `row` and its OIDC `matches` in one transaction, unless an
    /// identity has its name or a live match one of the matches' issuer and
    /// subject: then nothing is inserted, and the one in the way is returned.
    pub fn insert_identity_with_oidc(
        &self,
        row: &IdentityRow,
        matches: &[OidcMatchRow],
    ) -> Result<Option<InTheWay>, DbError> {
        self.in_transaction(|tx| {
            if let Some(existing) = tx
                .query_row(
                    &format!("SELECT {IDENTITY_COLUMNS} FROM identities WHERE name = ?1"),
                    [&row.name],
                    identity_from,
                )
                .optional()?
            {
                return Ok(Some(InTheWay::Identity(existing)));
            }
            for matched in matches {
                if let Some(existing) = tx
                    .query_row(
                        &format!(
                            "SELECT {OIDC_COLUMNS} FROM identity_oidc
                             WHERE issuer = ?1 AND subject = ?2 AND revoked_at IS NULL"
                        ),
                        params![matched.issuer, matched.subject],
                        oidc_from,
                    )
                    .optional()?
                {
                    return Ok(Some(InTheWay::Match(existing)));
                }
            }
            tx.execute(
                &format!(
                    "INSERT INTO identities ({IDENTITY_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL)"
                ),
                params![
                    row.id,
                    row.name,
                    row.kind,
                    row.display_name,
                    row.email,
                    row.created_at
                ],
            )?;
            for matched in matches {
                tx.execute(
                    &format!(
                        "INSERT INTO identity_oidc ({OIDC_COLUMNS})
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)"
                    ),
                    params![
                        matched.id,
                        row.id,
                        matched.issuer,
                        matched.subject,
                        matched.created_at
                    ],
                )?;
            }
            Ok(None)
        })
    }

    /// Records `revocation` on identity `id`, keeping an earlier
    /// `invalid_from`; the identity as it is after.
    pub fn revoke_identity(
        &self,
        id: &str,
        revocation: &RevocationRow,
    ) -> Result<Option<IdentityRow>, DbError> {
        self.in_transaction(|tx| {
            revoke_in(tx, "identities", "id", id, revocation)?;
            Ok(tx
                .query_row(
                    &format!("SELECT {IDENTITY_COLUMNS} FROM identities WHERE id = ?1"),
                    [id],
                    identity_from,
                )
                .optional()?)
        })
    }

    /// Per identity id, how many of its keys are live at `now`: not expired,
    /// and neither it, its session, its identity nor its authority revoked.
    pub fn live_session_key_counts(&self, now: Timestamp) -> Result<HashMap<String, u32>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT k.identity_id, count(*) {LIVE_FROM} WHERE k.not_after >= ?1 AND {LIVE}
                     GROUP BY k.identity_id"
            ))?;
            stmt.query_map([now], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    u32::try_from(row.get::<_, i64>(1)?).unwrap_or(u32::MAX),
                ))
            })?
            .collect()
        })
    }

    /// The session `id`.
    pub fn session(&self, id: &str) -> Result<Option<SessionRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [id],
                session_from,
            )
            .optional()
        })
    }

    /// Whether a session or session key has the id; for new ids.
    pub fn session_id_taken(&self, id: &str) -> Result<bool, DbError> {
        self.with(|conn| {
            Ok(conn
                .query_row("SELECT 1 FROM sessions WHERE id = ?1", [id], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }

    /// Whether an identity has the id; for new ids.
    pub fn identity_id_taken(&self, id: &str) -> Result<bool, DbError> {
        self.with(|conn| {
            Ok(conn
                .query_row("SELECT 1 FROM identities WHERE id = ?1", [id], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }

    /// Whether `key_id` is certified (`Some(true)`), already signed evidence
    /// as a bare key (`Some(false)`), or is new (`None`).
    pub fn key_state(&self, key_id: &str) -> Result<Option<bool>, DbError> {
        self.with(|conn| {
            let certified = conn
                .query_row(
                    "SELECT 1 FROM session_keys WHERE key_id = ?1",
                    [key_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if certified {
                return Ok(Some(true));
            }
            let used = conn
                .query_row(
                    "SELECT 1 FROM evidence_keys WHERE id = ?1",
                    [key_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            Ok(used.then_some(false))
        })
    }

    /// How many of `identity_id`'s keys have not expired at `now`.
    pub fn unexpired_session_keys(
        &self,
        identity_id: &str,
        now: Timestamp,
    ) -> Result<usize, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT count(*) FROM session_keys WHERE identity_id = ?1 AND not_after >= ?2",
                params![identity_id, now],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        })
    }

    /// Stores `key`, and `session` first when it is new, in one
    /// transaction, unless its identity already holds `limit` keys that have
    /// not expired at `key.issued_at` (counted in the same transaction).
    /// `Err(DbError::Conflict)` when the key is already there: two requests
    /// raced with one key.
    pub fn insert_session_key(
        &self,
        session: Option<&SessionRow>,
        key: &SessionKeyRow,
        limit: usize,
    ) -> Result<Inserted, DbError> {
        self.in_transaction(|tx| {
            let held: i64 = tx.query_row(
                "SELECT count(*) FROM session_keys WHERE identity_id = ?1 AND not_after >= ?2",
                params![key.identity_id, key.issued_at],
                |row| row.get(0),
            )?;
            if usize::try_from(held).unwrap_or(usize::MAX) >= limit {
                return Ok(Inserted::Full);
            }
            if let Some(session) = session {
                tx.execute(
                    &format!(
                        "INSERT INTO sessions ({SESSION_COLUMNS})
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6)"
                    ),
                    params![
                        session.id,
                        session.identity_id,
                        session.actor,
                        session.agent,
                        session.created_at,
                        session.channel
                    ],
                )?;
            }
            let inserted = tx.execute(
                "INSERT INTO session_keys(key_id, session_id, identity_id, authority_id, actor,
                                          presence, custody, certificate, not_before, not_after,
                                          issued_at, issued_by_agent, remote_token_id,
                                          issuer_kind, oidc_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(key_id) DO NOTHING",
                params![
                    key.key_id,
                    key.session_id,
                    key.identity_id,
                    key.authority_id,
                    key.actor,
                    key.presence,
                    key.custody,
                    key.certificate,
                    key.not_before,
                    key.not_after,
                    key.issued_at,
                    key.issued_by_agent,
                    key.remote_token_id,
                    key.issuer_kind,
                    key.oidc_id,
                ],
            )?;
            if inserted == 0 {
                return Err(DbError::Conflict {
                    what: KEY_WHAT,
                    id: key.key_id.clone(),
                    reason: "it is already certified",
                });
            }
            Ok(Inserted::Stored)
        })
    }

    /// Records `revocation` on authority `id`, keeping an earlier
    /// `invalid_from`; the authority as it is after.
    pub fn revoke_authority(
        &self,
        id: &str,
        revocation: &RevocationRow,
    ) -> Result<Option<AuthorityRow>, DbError> {
        self.in_transaction(|tx| {
            revoke_in(tx, "identity_authorities", "id", id, revocation)?;
            Ok(tx
                .query_row(
                    &format!("SELECT {AUTHORITY_COLUMNS} FROM identity_authorities WHERE id = ?1"),
                    [id],
                    authority_from,
                )
                .optional()?)
        })
    }

    /// The session key whose id starts with `prefix` (4 to 64 hex
    /// digits). [`DbError::InvalidPrefix`], [`DbError::NotFound`] or
    /// [`DbError::Ambiguous`] otherwise.
    pub fn session_key_by_prefix(&self, prefix: &str) -> Result<SessionKeyRow, DbError> {
        let pattern = format!(
            "{}*",
            super::normalize_prefix_up_to(KEY_WHAT, prefix, reef_evidence::keys::KEY_ID_LEN)?
        );
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM session_keys k WHERE k.key_id GLOB ?1
                 ORDER BY k.key_id LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map([pattern], key_from)?.collect()
        })?;
        unique_match(KEY_WHAT, prefix, matches, |key| key.key_id.clone())
    }

    /// The session keys with these ids; ids not certified are absent.
    pub fn session_keys_by_id(&self, ids: &[&str]) -> Result<Vec<SessionKeyRow>, DbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM session_keys k WHERE k.key_id IN ({})",
                placeholders(ids.len())
            ))?;
            stmt.query_map(params_from_iter(ids), key_from)?.collect()
        })
    }

    /// The keys of session `id`, newest issued first.
    pub fn keys_of_session(&self, id: &str) -> Result<Vec<SessionKeyRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM session_keys k WHERE k.session_id = ?1
                 ORDER BY k.issued_at DESC, k.key_id"
            ))?;
            stmt.query_map([id], key_from)?.collect()
        })
    }

    /// At most `limit` session keys matching `filter`, newest issued first
    /// (then by key id), after `cursor` (the `(issued_at, key_id)` of the
    /// last one of the previous page).
    pub fn list_session_keys(
        &self,
        filter: &SessionKeyFilter,
        cursor: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<SessionKeyRow>, DbError> {
        let mut sql = format!("SELECT {KEY_COLUMNS} {LIVE_FROM} WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(identity) = &filter.identity_id {
            sql.push_str(" AND k.identity_id = ?");
            values.push(SqlValue::Text(identity.clone()));
        }
        if let Some(session) = &filter.session_id {
            sql.push_str(" AND k.session_id = ?");
            values.push(SqlValue::Text(session.clone()));
        }
        if let Some(kind) = &filter.issuer_kind {
            sql.push_str(" AND k.issuer_kind = ?");
            values.push(SqlValue::Text(kind.clone()));
        }
        if let Some(now) = filter.live_at {
            sql.push_str(" AND k.not_after >= ? AND ");
            sql.push_str(LIVE);
            values.push(SqlValue::Integer(now.0));
        }
        if let Some((at, id)) = cursor {
            sql.push_str(" AND (k.issued_at < ? OR (k.issued_at = ? AND k.key_id > ?))");
            values.extend([
                SqlValue::Integer(at.0),
                SqlValue::Integer(at.0),
                SqlValue::Text(id.to_owned()),
            ]);
        }
        sql.push_str(" ORDER BY k.issued_at DESC, k.key_id LIMIT ?");
        values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), key_from)?
                .collect()
        })
    }

    /// Records `revocation` on key `key_id`, keeping an earlier
    /// `invalid_from`.
    pub fn revoke_session_key(
        &self,
        key_id: &str,
        revocation: &RevocationRow,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            revoke_in(tx, "session_keys", "key_id", key_id, revocation).map(drop)
        })
    }

    /// Records `revocation` on session `id`, keeping an earlier
    /// `invalid_from`.
    pub fn revoke_session(&self, id: &str, revocation: &RevocationRow) -> Result<(), DbError> {
        self.in_transaction(|tx| revoke_in(tx, "sessions", "id", id, revocation).map(drop))
    }

    /// The sessions and identities with these ids, for reading certified
    /// keys.
    pub fn key_records(
        &self,
        session_ids: &[&str],
        identity_ids: &[&str],
    ) -> Result<KeyRecords, DbError> {
        let sessions = if session_ids.is_empty() {
            Vec::new()
        } else {
            self.with(|conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE id IN ({})",
                    placeholders(session_ids.len())
                ))?;
                stmt.query_map(params_from_iter(session_ids), session_from)?
                    .collect()
            })?
        };
        let identities = if identity_ids.is_empty() {
            Vec::new()
        } else {
            self.with(|conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {IDENTITY_COLUMNS} FROM identities WHERE id IN ({})",
                    placeholders(identity_ids.len())
                ))?;
                stmt.query_map(params_from_iter(identity_ids), identity_from)?
                    .collect()
            })?
        };
        Ok(KeyRecords {
            sessions: sessions
                .into_iter()
                .map(|row: SessionRow| (row.id.clone(), row))
                .collect(),
            identities: identities
                .into_iter()
                .map(|row: IdentityRow| (row.id.clone(), row))
                .collect(),
        })
    }

    /// GC's sweep of session keys (R3.03): keys whose `not_after` is before
    /// `before` that no evidence in any repository names (`pinned`, an SQL
    /// condition on the key `k`, from `crate::identity::retention`), then
    /// the sessions left with no key. With `dry_run`, only counts. Returns
    /// the keys and sessions removed.
    pub fn sweep_session_keys(
        &self,
        before: Timestamp,
        pinned: &str,
        dry_run: bool,
    ) -> Result<(u32, u32), DbError> {
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        self.in_transaction(|tx| {
            let doomed = format!("FROM session_keys k WHERE k.not_after < ?1 AND NOT ({pinned})");
            if dry_run {
                let keys: i64 =
                    tx.query_row(&format!("SELECT count(*) {doomed}"), [before], |row| {
                        row.get(0)
                    })?;
                let sessions: i64 = tx.query_row(
                    &format!(
                        "SELECT count(*) FROM sessions s
                         WHERE NOT EXISTS (SELECT 1 FROM session_keys k
                                           WHERE k.session_id = s.id
                                             AND NOT (k.not_after < ?1 AND NOT ({pinned})))"
                    ),
                    [before],
                    |row| row.get(0),
                )?;
                return Ok((
                    count(usize::try_from(keys).unwrap_or_default()),
                    count(usize::try_from(sessions).unwrap_or_default()),
                ));
            }
            let keys = tx.execute(
                &format!("DELETE FROM session_keys WHERE key_id IN (SELECT k.key_id {doomed})"),
                [before],
            )?;
            let sessions = tx.execute(
                "DELETE FROM sessions
                 WHERE NOT EXISTS (SELECT 1 FROM session_keys k WHERE k.session_id = sessions.id)",
                [],
            )?;
            Ok((count(keys), count(sessions)))
        })
    }

    /// Sets a session key's window, for tests that age it.
    #[doc(hidden)]
    pub fn set_session_key_window(
        &self,
        key_id: &str,
        not_before: Timestamp,
        not_after: Timestamp,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE session_keys SET not_before = ?2, not_after = ?3 WHERE key_id = ?1",
                params![key_id, not_before, not_after],
            )
            .map(drop)
        })
    }

    /// Records a Fulcio CA a keyless key came from as an authority of kind
    /// `sigstore`, retired from the start so it is never current; nothing
    /// when it is recorded already. The row as it is after.
    pub fn record_sigstore_authority(&self, row: &AuthorityRow) -> Result<AuthorityRow, DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                &format!(
                    "INSERT INTO identity_authorities ({AUTHORITY_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL, NULL, NULL, 'sigstore')
                     ON CONFLICT(id) DO NOTHING"
                ),
                params![
                    row.id,
                    row.algorithm,
                    row.public_key,
                    row.host,
                    row.created_at
                ],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {AUTHORITY_COLUMNS} FROM identity_authorities WHERE id = ?1"),
                [&row.id],
                authority_from,
            )?)
        })
    }

    /// Inserts `row` unless a live match has its issuer and subject, which
    /// is returned instead.
    pub fn insert_oidc_match(&self, row: &OidcMatchRow) -> Result<Option<OidcMatchRow>, DbError> {
        self.in_transaction(|tx| {
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT {OIDC_COLUMNS} FROM identity_oidc
                         WHERE issuer = ?1 AND subject = ?2 AND revoked_at IS NULL"
                    ),
                    params![row.issuer, row.subject],
                    oidc_from,
                )
                .optional()?;
            if existing.is_some() {
                return Ok(existing);
            }
            tx.execute(
                &format!(
                    "INSERT INTO identity_oidc ({OIDC_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)"
                ),
                params![
                    row.id,
                    row.identity_id,
                    row.issuer,
                    row.subject,
                    row.created_at
                ],
            )?;
            Ok(None)
        })
    }

    /// The live match of `issuer` and `subject`, if any.
    pub fn live_oidc_match(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<OidcMatchRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {OIDC_COLUMNS} FROM identity_oidc
                     WHERE issuer = ?1 AND subject = ?2 AND revoked_at IS NULL"
                ),
                params![issuer, subject],
                oidc_from,
            )
            .optional()
        })
    }

    /// The match `id`.
    pub fn oidc_match(&self, id: &str) -> Result<Option<OidcMatchRow>, DbError> {
        Ok(self.oidc_matches_by_id(&[id])?.remove(id))
    }

    /// The matches with these ids, by id.
    pub fn oidc_matches_by_id(
        &self,
        ids: &[&str],
    ) -> Result<HashMap<String, OidcMatchRow>, DbError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {OIDC_COLUMNS} FROM identity_oidc WHERE id IN ({})",
                placeholders(ids.len())
            ))?;
            stmt.query_map(params_from_iter(ids), oidc_from)?
                .map(|row| row.map(|row| (row.id.clone(), row)))
                .collect()
        })
    }

    /// Every match of these identities, oldest first, revoked ones included.
    pub fn oidc_matches_of(&self, identity_ids: &[&str]) -> Result<Vec<OidcMatchRow>, DbError> {
        if identity_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {OIDC_COLUMNS} FROM identity_oidc WHERE identity_id IN ({})
                 ORDER BY created_at, id",
                placeholders(identity_ids.len())
            ))?;
            stmt.query_map(params_from_iter(identity_ids), oidc_from)?
                .collect()
        })
    }

    /// Records `revocation` on match `id`, keeping an earlier
    /// `invalid_from`; the match as it is after.
    pub fn revoke_oidc_match(
        &self,
        id: &str,
        revocation: &RevocationRow,
    ) -> Result<Option<OidcMatchRow>, DbError> {
        self.in_transaction(|tx| {
            revoke_in(tx, "identity_oidc", "id", id, revocation)?;
            Ok(tx
                .query_row(
                    &format!("SELECT {OIDC_COLUMNS} FROM identity_oidc WHERE id = ?1"),
                    [id],
                    oidc_from,
                )
                .optional()?)
        })
    }

    /// The ids of up to `limit` keys bound through match `id`, newest
    /// issued first.
    pub fn keys_of_oidc_match(&self, id: &str, limit: usize) -> Result<Vec<String>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key_id FROM session_keys WHERE oidc_id = ?1
                 ORDER BY issued_at DESC, key_id LIMIT ?2",
            )?;
            stmt.query_map(
                params![id, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| row.get::<_, String>(0),
            )?
            .collect()
        })
    }

    /// Whether a match has the id; for new ids.
    pub fn oidc_id_taken(&self, id: &str) -> Result<bool, DbError> {
        Ok(self.oidc_match(id)?.is_some())
    }

    /// How many rows the four identity tables have:
    /// `(authorities, identities, sessions, session_keys)`.
    pub fn identity_counts(&self) -> Result<(u64, u64, u64, u64), DbError> {
        self.with(|conn| {
            let count = |table: &str| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|n| u64::try_from(n).unwrap_or_default())
            };
            Ok((
                count("identity_authorities")?,
                count("identities")?,
                count("sessions")?,
                count("session_keys")?,
            ))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn identity(id: &str, name: &str, at: i64) -> IdentityRow {
        IdentityRow {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: "person".into(),
            display_name: None,
            email: None,
            created_at: Timestamp(at),
            revocation: None,
        }
    }

    fn authority(id: &str, at: i64) -> AuthorityRow {
        AuthorityRow {
            id: id.to_owned(),
            algorithm: "ed25519".into(),
            public_key: vec![1, 2, 3],
            host: "host".into(),
            created_at: Timestamp(at),
            retired_at: None,
            revocation: None,
            kind: "reef-daemon".into(),
        }
    }

    fn session(id: &str, identity: &str) -> SessionRow {
        SessionRow {
            id: id.to_owned(),
            identity_id: identity.to_owned(),
            actor: "agent".into(),
            agent: None,
            created_at: Timestamp(10),
            channel: "socket".into(),
            revocation: None,
        }
    }

    fn key(id: &str, session: &str, identity: &str, issued: i64, not_after: i64) -> SessionKeyRow {
        SessionKeyRow {
            key_id: id.to_owned(),
            session_id: session.to_owned(),
            identity_id: identity.to_owned(),
            authority_id: "a1".into(),
            actor: "agent".into(),
            presence: "none".into(),
            custody: "client".into(),
            certificate: b"{}".to_vec(),
            not_before: Timestamp(issued),
            not_after: Timestamp(not_after),
            issued_at: Timestamp(issued),
            issued_by_agent: None,
            remote_token_id: None,
            revocation: None,
            issuer_kind: "reef-daemon".into(),
            oidc_id: None,
        }
    }

    fn revocation(at: i64, from: i64) -> RevocationRow {
        RevocationRow {
            revoked_at: Timestamp(at),
            invalid_from: Timestamp(from),
            reason: Some("why".into()),
        }
    }

    #[test]
    fn identities_are_named_once_and_revoked_earlier_only() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(
            db.insert_identity(&identity("idn_1", "alice", 1)).unwrap(),
            None
        );
        let again = db.insert_identity(&identity("idn_2", "alice", 2)).unwrap();
        assert_eq!(again.unwrap().id, "idn_1");
        assert_eq!(db.identity("alice").unwrap().unwrap().id, "idn_1");
        assert_eq!(db.identity("idn_1").unwrap().unwrap().name, "alice");
        // One rule per text: an id never matches a name, nor a name an id.
        assert_eq!(db.identity("idn_2").unwrap(), None);
        let revoked = db
            .revoke_identity("idn_1", &revocation(100, 90))
            .unwrap()
            .unwrap();
        assert_eq!(revoked.revocation, Some(revocation(100, 90)));
        // Later never moves it; earlier does, and the first revoked_at stays.
        let later = db
            .revoke_identity("idn_1", &revocation(200, 150))
            .unwrap()
            .unwrap();
        assert_eq!(
            later.revocation.as_ref().unwrap().invalid_from,
            Timestamp(90)
        );
        assert_eq!(
            later.revocation.as_ref().unwrap().revoked_at,
            Timestamp(100)
        );
        let earlier = db
            .revoke_identity("idn_1", &revocation(300, 5))
            .unwrap()
            .unwrap();
        assert_eq!(earlier.revocation.unwrap().invalid_from, Timestamp(5));
    }

    #[test]
    fn keys_page_filter_and_sweep() {
        let db = Db::open_in_memory().unwrap();
        db.insert_identity(&identity("idn_1", "alice", 1)).unwrap();
        db.replace_authority(&authority("a1", 1), None).unwrap();
        db.insert_session_key(
            Some(&session("s1", "idn_1")),
            &key("k1", "s1", "idn_1", 10, 20),
            100,
        )
        .unwrap();
        db.insert_session_key(None, &key("k2", "s1", "idn_1", 30, 1_000), 100)
            .unwrap();
        db.insert_session_key(
            Some(&session("s2", "idn_1")),
            &key("k3", "s2", "idn_1", 30, 40),
            100,
        )
        .unwrap();
        let error = db
            .insert_session_key(None, &key("k3", "s2", "idn_1", 30, 40), 100)
            .unwrap_err();
        assert!(matches!(error, DbError::Conflict { .. }), "{error:?}");
        let all = SessionKeyFilter::default();
        let ids = |rows: Vec<SessionKeyRow>| rows.into_iter().map(|k| k.key_id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.list_session_keys(&all, None, 10).unwrap()),
            ["k2", "k3", "k1"]
        );
        assert_eq!(
            ids(db
                .list_session_keys(&all, Some((Timestamp(30), "k2")), 10)
                .unwrap()),
            ["k3", "k1"]
        );
        let live = SessionKeyFilter {
            live_at: Some(Timestamp(35)),
            ..SessionKeyFilter::default()
        };
        assert_eq!(
            ids(db.list_session_keys(&live, None, 10).unwrap()),
            ["k2", "k3"]
        );
        db.revoke_session("s2", &revocation(36, 36)).unwrap();
        assert_eq!(ids(db.list_session_keys(&live, None, 10).unwrap()), ["k2"]);
        assert_eq!(db.key_state("k1").unwrap(), Some(true));
        assert_eq!(db.key_state("nope").unwrap(), None);
        assert_eq!(
            db.unexpired_session_keys("idn_1", Timestamp(25)).unwrap(),
            2
        );
        // The sweep: k1 and k3 are past; nothing pins them.
        assert_eq!(
            db.sweep_session_keys(Timestamp(50), "0", true).unwrap(),
            (2, 1)
        );
        assert_eq!(db.identity_counts().unwrap(), (1, 1, 2, 3));
        assert_eq!(
            db.sweep_session_keys(Timestamp(50), "0", false).unwrap(),
            (2, 1)
        );
        assert_eq!(db.identity_counts().unwrap(), (1, 1, 1, 1));
        assert!(db.session("s2").unwrap().is_none());
    }

    // Review finding 5: a key is live only while its identity and its
    // authority are not revoked either.
    #[test]
    fn live_keys_need_a_live_identity_and_authority() {
        let db = Db::open_in_memory().unwrap();
        db.insert_identity(&identity("idn_1", "alice", 1)).unwrap();
        db.insert_identity(&identity("idn_2", "bob", 2)).unwrap();
        db.replace_authority(&authority("a1", 1), None).unwrap();
        db.insert_session_key(
            Some(&session("s1", "idn_1")),
            &key("k1", "s1", "idn_1", 10, 100),
            100,
        )
        .unwrap();
        db.insert_session_key(
            Some(&session("s2", "idn_2")),
            &key("k2", "s2", "idn_2", 11, 100),
            100,
        )
        .unwrap();
        let live = SessionKeyFilter {
            live_at: Some(Timestamp(20)),
            ..SessionKeyFilter::default()
        };
        let ids = |db: &Db| {
            db.list_session_keys(&live, None, 10)
                .unwrap()
                .into_iter()
                .map(|k| k.key_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&db), ["k2", "k1"]);
        db.revoke_identity("idn_1", &revocation(30, 30)).unwrap();
        assert_eq!(ids(&db), ["k2"]);
        assert_eq!(
            db.live_session_key_counts(Timestamp(20)).unwrap(),
            [("idn_2".to_owned(), 1)].into()
        );
        db.revoke_authority("a1", &revocation(31, 31)).unwrap();
        assert_eq!(ids(&db).len(), 0);
        assert!(
            db.live_session_key_counts(Timestamp(20))
                .unwrap()
                .is_empty()
        );
    }

    // Review finding 12: the per-identity cap is counted in the insert's
    // own transaction.
    #[test]
    fn the_cap_is_counted_in_the_insert() {
        let db = Db::open_in_memory().unwrap();
        db.insert_identity(&identity("idn_1", "alice", 1)).unwrap();
        db.replace_authority(&authority("a1", 1), None).unwrap();
        let first = db
            .insert_session_key(
                Some(&session("s1", "idn_1")),
                &key("k1", "s1", "idn_1", 10, 100),
                1,
            )
            .unwrap();
        assert_eq!(first, Inserted::Stored);
        let second = db
            .insert_session_key(
                Some(&session("s2", "idn_1")),
                &key("k2", "s2", "idn_1", 10, 100),
                1,
            )
            .unwrap();
        assert_eq!(second, Inserted::Full);
        assert!(
            db.session("s2").unwrap().is_none(),
            "nothing of it is stored"
        );
        // An expired key does not count.
        let later = key("k3", "s1", "idn_1", 200, 300);
        assert_eq!(
            db.insert_session_key(None, &later, 1).unwrap(),
            Inserted::Stored
        );
    }

    #[test]
    fn authorities_retire_on_replace() {
        let db = Db::open_in_memory().unwrap();
        db.replace_authority(&authority("a1", 1), None).unwrap();
        db.replace_authority(&authority("a2", 2), Some(("a1", &revocation(2, 1))))
            .unwrap();
        let rows = db.authorities().unwrap();
        assert_eq!(rows[0].id, "a2");
        assert_eq!(rows[0].retired_at, None);
        assert_eq!(rows[1].retired_at, Some(Timestamp(2)));
        assert_eq!(rows[1].revocation, Some(revocation(2, 1)));
        assert_eq!(db.retire_authorities(Timestamp(3)).unwrap(), 1);
        assert!(
            db.authorities()
                .unwrap()
                .iter()
                .all(|a| a.retired_at.is_some())
        );
    }

    fn oidc(id: &str, identity: &str, subject: &str) -> OidcMatchRow {
        OidcMatchRow {
            id: id.to_owned(),
            identity_id: identity.to_owned(),
            issuer: "https://issuer.example".into(),
            subject: subject.to_owned(),
            created_at: Timestamp(5),
            revocation: None,
        }
    }

    /// R3.42 test 5: OIDC matches are unique among live ones, revocation
    /// frees the pair, a create with a taken match inserts nothing, and a
    /// Fulcio CA is an authority that never becomes current.
    #[test]
    fn oidc_matches_and_sigstore_authorities() {
        let db = Db::open_in_memory().unwrap();
        db.insert_identity(&identity("idn_1", "ci-main", 1))
            .unwrap();
        assert_eq!(
            db.insert_oidc_match(&oidc("oidc_1", "idn_1", "s")).unwrap(),
            None
        );
        let taken = db.insert_oidc_match(&oidc("oidc_2", "idn_1", "s")).unwrap();
        assert_eq!(taken.map(|row| row.id), Some("oidc_1".to_owned()));
        assert_eq!(
            db.live_oidc_match("https://issuer.example", "s")
                .unwrap()
                .map(|row| row.id),
            Some("oidc_1".to_owned())
        );
        // A create whose match is taken inserts neither.
        let blocked = db
            .insert_identity_with_oidc(
                &identity("idn_2", "ci-other", 2),
                &[oidc("oidc_3", "", "s")],
            )
            .unwrap();
        assert!(matches!(blocked, Some(InTheWay::Match(row)) if row.id == "oidc_1"));
        assert!(db.identity("ci-other").unwrap().is_none());
        // Revoked, the pair is free; the earlier invalid_from is kept.
        db.revoke_oidc_match("oidc_1", &revocation(20, 10)).unwrap();
        let revoked = db
            .revoke_oidc_match("oidc_1", &revocation(30, 25))
            .unwrap()
            .unwrap();
        assert_eq!(revoked.revocation.unwrap().invalid_from, Timestamp(10));
        assert_eq!(
            db.live_oidc_match("https://issuer.example", "s").unwrap(),
            None
        );
        assert_eq!(
            db.insert_identity_with_oidc(
                &identity("idn_2", "ci-other", 2),
                &[oidc("oidc_3", "", "s")],
            )
            .unwrap(),
            None
        );
        let of = db.oidc_matches_of(&["idn_1", "idn_2"]).unwrap();
        assert_eq!(of.len(), 2);
        assert_eq!(of[1].identity_id, "idn_2");
        // A Fulcio CA: recorded once, retired from the start, never current.
        db.replace_authority(&authority("a1", 1), None).unwrap();
        let ca = AuthorityRow {
            kind: "sigstore".into(),
            ..authority("ca1", 3)
        };
        let recorded = db.record_sigstore_authority(&ca).unwrap();
        assert_eq!(recorded.kind, "sigstore");
        assert_eq!(recorded.retired_at, Some(Timestamp(3)));
        db.record_sigstore_authority(&authority("ca1", 9)).unwrap();
        db.replace_authority(&authority("a2", 4), None).unwrap();
        let rows = db.authorities().unwrap();
        let current: Vec<&str> = rows
            .iter()
            .filter(|row| row.retired_at.is_none())
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(current, ["a2"]);
        let ca = rows.iter().find(|row| row.id == "ca1").unwrap();
        assert_eq!(
            (ca.created_at, ca.retired_at),
            (Timestamp(3), Some(Timestamp(3)))
        );
        db.retire_authorities(Timestamp(50)).unwrap();
        let ca = db
            .authorities()
            .unwrap()
            .into_iter()
            .find(|row| row.id == "ca1")
            .unwrap();
        assert_eq!(ca.retired_at, Some(Timestamp(3)));
    }
}
