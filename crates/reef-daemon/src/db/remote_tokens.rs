//! Queries on `remote_tokens` (R2.08, ADR 0049). The remote listener keeps
//! every live token in memory (`crate::remote::tokens`); this table is the
//! record it loads at start and writes through.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Row, params};

use super::{Db, DbError};
use crate::model::Timestamp;

const COLUMNS: &str = "id, name, scope, digest, previous_digest, previous_until, created_at, \
     expires_at, rotated_at, revoked_at, last_used_at, last_used_from, identity_id, \
     device_origin";

/// What a remote token allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenScope {
    /// `ACCESS_READ` methods.
    Read,
    /// `ACCESS_READ` and `ACCESS_WRITE` methods, under the write rules.
    Write,
    /// A paired device's (R3.40): `ACCESS_READ` methods, and the writes of
    /// `crate::remote::rules::DEVICE`, at the address it was paired at.
    /// Stored as `read` with a `device_origin`.
    Device,
}

impl TokenScope {
    /// Every scope, for the `CHECK` constraint's test.
    pub const ALL: [TokenScope; 3] = [TokenScope::Read, TokenScope::Write, TokenScope::Device];

    /// The name errors and the log use.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScope::Read => "read",
            TokenScope::Write => "write",
            TokenScope::Device => "device",
        }
    }

    /// The `scope` column's value.
    pub fn column(self) -> &'static str {
        match self {
            TokenScope::Read | TokenScope::Device => "read",
            TokenScope::Write => "write",
        }
    }
}

impl ToSql for TokenScope {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.column()))
    }
}

impl FromSql for TokenScope {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "read" => Ok(TokenScope::Read),
            "write" => Ok(TokenScope::Write),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

/// A blake3 digest of a token's secret.
pub type Digest = [u8; 32];

/// One row of `remote_tokens`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTokenRow {
    /// 16 lowercase hex characters, never reused.
    pub id: String,
    /// Unique among live tokens.
    pub name: String,
    /// What it allows.
    pub scope: TokenScope,
    /// blake3 of its secret.
    pub digest: Digest,
    /// blake3 of the secret before the last rotation, valid until
    /// `previous_until`.
    pub previous_digest: Option<Digest>,
    /// When `previous_digest` stops working.
    pub previous_until: Option<Timestamp>,
    /// When it was issued.
    pub created_at: Timestamp,
    /// When it stops working; `None`: never.
    pub expires_at: Option<Timestamp>,
    /// When it was last rotated.
    pub rotated_at: Option<Timestamp>,
    /// When it was revoked.
    pub revoked_at: Option<Timestamp>,
    /// When it was last used, as last flushed.
    pub last_used_at: Option<Timestamp>,
    /// The peer address of that use.
    pub last_used_from: Option<String>,
    /// The operator identity it is linked to (R3.03): the only one it gets
    /// session keys for. A device's person (R3.40).
    pub identity_id: Option<String>,
    /// The address a device was paired at (R3.40), `Some` exactly for
    /// [`TokenScope::Device`].
    pub device_origin: Option<String>,
}

impl RemoteTokenRow {
    /// Neither revoked nor expired at `now`.
    pub fn is_live(&self, now: Timestamp) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > now)
    }
}

fn digest(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Digest>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    bytes
        .map(|bytes| {
            Digest::try_from(bytes.as_slice()).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Blob,
                    "a token digest is not 32 bytes".into(),
                )
            })
        })
        .transpose()
}

/// The scope the `scope` column `stored` and a `device_origin` make.
fn scope(stored: TokenScope, device_origin: Option<&str>) -> rusqlite::Result<TokenScope> {
    match (stored, device_origin) {
        (scope, None) => Ok(scope),
        (TokenScope::Read, Some(_)) => Ok(TokenScope::Device),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            "a write token has a device_origin".into(),
        )),
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<RemoteTokenRow> {
    let device_origin: Option<String> = row.get(13)?;
    Ok(RemoteTokenRow {
        id: row.get(0)?,
        name: row.get(1)?,
        scope: scope(row.get(2)?, device_origin.as_deref())?,
        digest: digest(row, 3)?.unwrap_or_default(),
        previous_digest: digest(row, 4)?,
        previous_until: row.get(5)?,
        created_at: row.get(6)?,
        expires_at: row.get(7)?,
        rotated_at: row.get(8)?,
        revoked_at: row.get(9)?,
        last_used_at: row.get(10)?,
        last_used_from: row.get(11)?,
        identity_id: row.get(12)?,
        device_origin,
    })
}

fn revoke(conn: &rusqlite::Connection, id: &str, at: Timestamp) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE remote_tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id, at],
    )
    .map(drop)
}

impl Db {
    /// Every token, newest first.
    pub fn remote_tokens(&self) -> Result<Vec<RemoteTokenRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM remote_tokens ORDER BY created_at DESC, id"
            ))?;
            stmt.query_map([], from_row)?.collect()
        })
    }

    /// Records a new token; with `replaced`, that token's id is marked
    /// revoked when this one was created, in the same transaction.
    pub fn insert_remote_token(
        &self,
        row: &RemoteTokenRow,
        replaced: Option<&str>,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            if let Some(replaced) = replaced {
                revoke(tx, replaced, row.created_at)?;
            }
            tx.execute(
                &format!(
                    "INSERT INTO remote_tokens ({COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                ),
                params![
                    row.id,
                    row.name,
                    row.scope,
                    row.digest.as_slice(),
                    row.previous_digest.as_ref().map(<[u8; 32]>::as_slice),
                    row.previous_until,
                    row.created_at,
                    row.expires_at,
                    row.rotated_at,
                    row.revoked_at,
                    row.last_used_at,
                    row.last_used_from,
                    row.identity_id,
                    row.device_origin,
                ],
            )?;
            Ok(())
        })
    }

    /// Writes a rotation: the new digest, the previous one and its end, and
    /// when.
    pub fn rotate_remote_token(&self, row: &RemoteTokenRow) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE remote_tokens SET digest = ?2, previous_digest = ?3, \
                 previous_until = ?4, rotated_at = ?5 WHERE id = ?1",
                params![
                    row.id,
                    row.digest.as_slice(),
                    row.previous_digest.as_ref().map(<[u8; 32]>::as_slice),
                    row.previous_until,
                    row.rotated_at,
                ],
            )
            .map(drop)
        })
    }

    /// Marks a token revoked at `at`.
    pub fn revoke_remote_token(&self, id: &str, at: Timestamp) -> Result<(), DbError> {
        self.with(|conn| revoke(conn, id, at))
    }

    /// Records each token's last use: `(id, when, from where)`.
    pub fn touch_remote_tokens(&self, uses: &[(String, Timestamp, String)]) -> Result<(), DbError> {
        if uses.is_empty() {
            return Ok(());
        }
        self.in_transaction(|tx| {
            let mut stmt = tx.prepare(
                "UPDATE remote_tokens SET last_used_at = ?2, last_used_from = ?3 WHERE id = ?1",
            )?;
            for (id, at, from) in uses {
                stmt.execute(params![id, at, from])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str) -> RemoteTokenRow {
        RemoteTokenRow {
            id: id.to_owned(),
            name: name.to_owned(),
            scope: TokenScope::Write,
            digest: [7; 32],
            previous_digest: None,
            previous_until: None,
            created_at: Timestamp(1_000),
            expires_at: Some(Timestamp(5_000)),
            rotated_at: None,
            revoked_at: None,
            last_used_at: None,
            last_used_from: None,
            identity_id: None,
            device_origin: None,
        }
    }

    #[test]
    fn round_trip() {
        let db = Db::open_in_memory().unwrap();
        let mut laptop = row("00112233aabbccdd", "laptop");
        db.insert_remote_token(&laptop, None).unwrap();
        let mut phone = row("ffeeddccbbaa9988", "phone");
        phone.scope = TokenScope::Read;
        phone.created_at = Timestamp(2_000);
        phone.expires_at = None;
        db.insert_remote_token(&phone, None).unwrap();
        assert_eq!(db.remote_tokens().unwrap(), [phone.clone(), laptop.clone()]);

        laptop.previous_digest = Some(laptop.digest);
        laptop.digest = [9; 32];
        laptop.previous_until = Some(Timestamp(3_000));
        laptop.rotated_at = Some(Timestamp(2_500));
        db.rotate_remote_token(&laptop).unwrap();
        db.revoke_remote_token(&phone.id, Timestamp(4_000)).unwrap();
        db.revoke_remote_token(&phone.id, Timestamp(4_500)).unwrap();
        db.touch_remote_tokens(&[(laptop.id.clone(), Timestamp(2_600), "[::1]:5".to_owned())])
            .unwrap();
        laptop.last_used_at = Some(Timestamp(2_600));
        laptop.last_used_from = Some("[::1]:5".to_owned());
        phone.revoked_at = Some(Timestamp(4_000));
        assert_eq!(db.remote_tokens().unwrap(), [phone.clone(), laptop.clone()]);
        assert!(laptop.is_live(Timestamp(4_999)));
        assert!(!laptop.is_live(Timestamp(5_000)));
        assert!(!phone.is_live(Timestamp(0)));
    }

    // R3.40 12: a device is a `read` row with an address, which code that
    // does not know the column reads as a read token; a `write` row with one
    // does not load.
    #[test]
    fn devices_are_read_rows_with_an_address() {
        let db = Db::open_in_memory().unwrap();
        let mut phone = row("00112233aabbccdd", "phone");
        phone.scope = TokenScope::Device;
        phone.device_origin = Some("https://mac.example:7470".to_owned());
        db.insert_remote_token(&phone, None).unwrap();
        assert_eq!(db.remote_tokens().unwrap(), [phone.clone()]);
        let stored: String = db
            .with(|conn| conn.query_row("SELECT scope FROM remote_tokens", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(stored, "read");

        let mut again = row("ffeeddccbbaa9988", "phone");
        again.created_at = Timestamp(2_000);
        db.insert_remote_token(&again, Some(&phone.id)).unwrap();
        phone.revoked_at = Some(Timestamp(2_000));
        assert_eq!(db.remote_tokens().unwrap(), [again, phone]);

        db.with(|conn| {
            conn.execute(
                "UPDATE remote_tokens SET device_origin = 'https://x' WHERE scope = 'write'",
                [],
            )
        })
        .unwrap();
        let error = db.remote_tokens().unwrap_err();
        assert!(
            reef_lib::error::ErrorChain(&error)
                .to_string()
                .contains("a write token has a device_origin"),
            "{error}"
        );
    }
}
