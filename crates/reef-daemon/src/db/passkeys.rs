//! Queries on `passkeys` (R3.43, ADR 0085): each person identity's
//! `WebAuthn` passkeys, machine-wide like `identities`. A removed passkey's row stays,
//! with when, why and through which web UI session it was removed; the
//! credential id and the fingerprint are unique among live rows only.

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, params, params_from_iter};

use super::evidence::{bytes_at, integer_at, text_at};
use super::identity::revoke_in;
use super::{Db, DbError, RevocationRow};
use crate::model::Timestamp;

/// One row of `passkeys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasskeyRow {
    /// `pk_` and 16 hex digits.
    pub id: String,
    /// The person identity it verifies.
    pub identity_id: String,
    /// The `WebAuthn` credential id.
    pub credential_id: Vec<u8>,
    /// Its DER `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
    /// Its COSE algorithm.
    pub algorithm: i64,
    /// SHA-256 of `public_key`, lowercase hex.
    pub fingerprint: String,
    /// The RP ID it was registered at.
    pub rp_id: String,
    /// The label the person gave it.
    pub label: String,
    /// The authenticator's AAGUID, 32 hex digits.
    pub aaguid: String,
    /// The last sign count an accepted ceremony reported.
    pub sign_count: u32,
    /// When it was added.
    pub created_at: Timestamp,
    /// The web UI session it was added through.
    pub created_web_session: Option<String>,
    /// When an accepted assertion last used it.
    pub last_used_at: Option<Timestamp>,
    /// When it was removed.
    pub removed_at: Option<Timestamp>,
    /// Why: `removed`, with the remover's reason after a colon when given,
    /// or `reset`, likewise.
    pub removed_reason: Option<String>,
    /// The web UI session that removed it.
    pub removed_web_session: Option<String>,
}

/// What [`Db::insert_passkey`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasskeyInserted {
    /// It is stored.
    Stored,
    /// Nothing: the identity holds its limit of live passkeys.
    Full,
    /// Nothing: a live passkey has the credential id or the fingerprint.
    Exists(String),
}

const COLUMNS: &str = "id, identity_id, credential_id, public_key, algorithm, fingerprint, \
     rp_id, label, aaguid, sign_count, created_at, created_web_session, last_used_at, \
     removed_at, removed_reason, removed_web_session";

fn optional_time(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Timestamp>> {
    Ok(match row.get::<_, SqlValue>(idx)? {
        SqlValue::Integer(at) => Some(Timestamp(at)),
        _ => None,
    })
}

fn passkey_from(row: &Row<'_>) -> rusqlite::Result<PasskeyRow> {
    Ok(PasskeyRow {
        id: text_at(row, 0)?.unwrap_or_default(),
        identity_id: text_at(row, 1)?.unwrap_or_default(),
        credential_id: bytes_at(row, 2)?.unwrap_or_default(),
        public_key: bytes_at(row, 3)?.unwrap_or_default(),
        algorithm: integer_at(row, 4)?,
        fingerprint: text_at(row, 5)?.unwrap_or_default(),
        rp_id: text_at(row, 6)?.unwrap_or_default(),
        label: text_at(row, 7)?.unwrap_or_default(),
        aaguid: text_at(row, 8)?.unwrap_or_default(),
        sign_count: u32::try_from(integer_at(row, 9)?).unwrap_or(u32::MAX),
        created_at: Timestamp(integer_at(row, 10)?),
        created_web_session: text_at(row, 11)?,
        last_used_at: optional_time(row, 12)?,
        removed_at: optional_time(row, 13)?,
        removed_reason: text_at(row, 14)?,
        removed_web_session: text_at(row, 15)?,
    })
}

impl Db {
    /// Passkeys, by identity and then oldest added first: `identity_id`'s,
    /// or every identity's; removed ones only with `include_removed`.
    pub fn passkeys(
        &self,
        identity_id: Option<&str>,
        include_removed: bool,
    ) -> Result<Vec<PasskeyRow>, DbError> {
        let mut sql = format!("SELECT {COLUMNS} FROM passkeys WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(identity) = identity_id {
            sql.push_str(" AND identity_id = ?");
            values.push(SqlValue::Text(identity.to_owned()));
        }
        if !include_removed {
            sql.push_str(" AND removed_at IS NULL");
        }
        sql.push_str(" ORDER BY identity_id, created_at, id");
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), passkey_from)?
                .collect()
        })
    }

    /// The passkey with the id, removed or not.
    pub fn passkey(&self, id: &str) -> Result<Option<PasskeyRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM passkeys WHERE id = ?1"),
                [id],
                passkey_from,
            )
            .optional()
        })
    }

    /// The passkeys with these ids, removed or not; unknown ids are absent.
    pub fn passkeys_by_id(&self, ids: &[&str]) -> Result<Vec<PasskeyRow>, DbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; ids.len()].join(", ");
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM passkeys WHERE id IN ({marks})"
            ))?;
            stmt.query_map(params_from_iter(ids), passkey_from)?
                .collect()
        })
    }

    /// Whether a passkey, removed or not, has the id.
    pub fn passkey_id_taken(&self, id: &str) -> Result<bool, DbError> {
        Ok(self.passkey(id)?.is_some())
    }

    /// Stores `row` unless its identity already holds `limit` live
    /// passkeys, any passkey, removed or not, had its credential id, or a
    /// live one has its fingerprint.
    pub fn insert_passkey(
        &self,
        row: &PasskeyRow,
        limit: usize,
    ) -> Result<PasskeyInserted, DbError> {
        self.in_transaction(|tx| {
            let live: i64 = tx.query_row(
                "SELECT count(*) FROM passkeys WHERE identity_id = ?1 AND removed_at IS NULL",
                [&row.identity_id],
                |found| found.get(0),
            )?;
            if usize::try_from(live).unwrap_or(usize::MAX) >= limit {
                return Ok(PasskeyInserted::Full);
            }
            // A credential id is never registered again, even after its
            // passkey was removed: a registration that reuses a person's
            // credential id with another key would otherwise pass for the
            // person's authenticator wherever only the id is compared.
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM passkeys
                     WHERE credential_id = ?1 OR (removed_at IS NULL AND fingerprint = ?2)
                     ORDER BY removed_at IS NULL DESC LIMIT 1",
                    params![row.credential_id, row.fingerprint],
                    |found| found.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(PasskeyInserted::Exists(id));
            }
            tx.execute(
                &format!(
                    "INSERT INTO passkeys ({COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, NULL, NULL,
                             NULL)"
                ),
                params![
                    row.id,
                    row.identity_id,
                    row.credential_id,
                    row.public_key,
                    row.algorithm,
                    row.fingerprint,
                    row.rp_id,
                    row.label,
                    row.aaguid,
                    row.sign_count,
                    row.created_at,
                    row.created_web_session,
                ],
            )?;
            Ok(PasskeyInserted::Stored)
        })
    }

    /// Records an accepted assertion by live passkey `id` with `sign_count`
    /// at `at`, only when the count went up, or it and the stored one are
    /// both 0 (an authenticator that keeps none). The passkey as it is
    /// after; `None` when it is removed or the count did not go up.
    pub fn use_passkey(
        &self,
        id: &str,
        sign_count: u32,
        at: Timestamp,
    ) -> Result<Option<PasskeyRow>, DbError> {
        self.in_transaction(|tx| {
            let used = tx.execute(
                "UPDATE passkeys SET sign_count = ?2, last_used_at = ?3
                 WHERE id = ?1 AND removed_at IS NULL
                   AND (sign_count < ?2 OR (sign_count = 0 AND ?2 = 0))",
                params![id, sign_count, at],
            )?;
            if used == 0 {
                return Ok(None);
            }
            Ok(tx
                .query_row(
                    &format!("SELECT {COLUMNS} FROM passkeys WHERE id = ?1"),
                    [id],
                    passkey_from,
                )
                .optional()?)
        })
    }

    /// Removes live passkey `id` at `at`; the passkey as it is after, or
    /// `None` when no live passkey has the id.
    pub fn remove_passkey(
        &self,
        id: &str,
        at: Timestamp,
        reason: &str,
        web_session: Option<&str>,
    ) -> Result<Option<PasskeyRow>, DbError> {
        self.in_transaction(|tx| {
            let removed = tx.execute(
                "UPDATE passkeys SET removed_at = ?2, removed_reason = ?3, removed_web_session = ?4
                 WHERE id = ?1 AND removed_at IS NULL",
                params![id, at, reason, web_session],
            )?;
            if removed == 0 {
                return Ok(None);
            }
            Ok(tx
                .query_row(
                    &format!("SELECT {COLUMNS} FROM passkeys WHERE id = ?1"),
                    [id],
                    passkey_from,
                )
                .optional()?)
        })
    }

    /// Removes every live passkey of `identity_id` at `at` and revokes,
    /// from `at`, the session keys they certified that are unrevoked and
    /// unexpired, in one transaction: a key stored before it is revoked, and
    /// one stored after it is refused (`insert_session_key`). The passkeys
    /// as they are after, and the ids of the keys revoked.
    pub fn reset_passkeys(
        &self,
        identity_id: &str,
        at: Timestamp,
        reason: &str,
    ) -> Result<(Vec<PasskeyRow>, Vec<String>), DbError> {
        self.in_transaction(|tx| {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM passkeys WHERE identity_id = ?1 AND removed_at IS NULL",
                )?;
                stmt.query_map([identity_id], |found| found.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            if ids.is_empty() {
                return Ok((Vec::new(), Vec::new()));
            }
            tx.execute(
                "UPDATE passkeys SET removed_at = ?2, removed_reason = ?3
                 WHERE identity_id = ?1 AND removed_at IS NULL",
                params![identity_id, at, reason],
            )?;
            let marks = vec!["?"; ids.len()].join(", ");
            let rows: Vec<PasskeyRow> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {COLUMNS} FROM passkeys WHERE id IN ({marks})
                     ORDER BY created_at, id"
                ))?;
                stmt.query_map(params_from_iter(&ids), passkey_from)?
                    .collect::<rusqlite::Result<_>>()?
            };
            let mut values: Vec<SqlValue> = ids.into_iter().map(SqlValue::Text).collect();
            values.push(SqlValue::Integer(at.0));
            let keys: Vec<String> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT key_id FROM session_keys
                     WHERE passkey_id IN ({marks}) AND not_after >= ? AND revoked_at IS NULL
                     ORDER BY issued_at, key_id"
                ))?;
                stmt.query_map(params_from_iter(values), |found| found.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let revocation = RevocationRow {
                revoked_at: at,
                invalid_from: at,
                reason: Some(reason.to_owned()),
            };
            for key in &keys {
                revoke_in(tx, "session_keys", "key_id", key, &revocation)?;
            }
            Ok((rows, keys))
        })
    }
}
