//! Queries on `reclaims`, the log of workspace directories the daemon
//! removed.

use rusqlite::types::Type;
use rusqlite::{Connection, Row, params};

use super::{Db, DbError};
use crate::model::{ReclaimRecord, RepoId};

const COLUMNS: &str =
    "id, repo_id, workspace_id, reason, final_op_id, snapshot_taken, freed_bytes, at, detail";

/// A stored byte count below zero, which Reef never writes.
#[derive(Debug, thiserror::Error)]
#[error("negative byte count {0}")]
struct NegativeBytes(i64);

fn from_row(row: &Row<'_>) -> rusqlite::Result<ReclaimRecord> {
    let freed_bytes = row
        .get::<_, Option<i64>>(6)?
        .map(|bytes| {
            u64::try_from(bytes).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    Type::Integer,
                    Box::new(NegativeBytes(bytes)),
                )
            })
        })
        .transpose()?;
    Ok(ReclaimRecord {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        workspace_id: row.get(2)?,
        reason: row.get(3)?,
        final_op_id: row.get(4)?,
        snapshot_taken: row.get(5)?,
        freed_bytes,
        at: row.get(7)?,
        detail: row.get(8)?,
    })
}

/// [`Db::insert_reclaim`] on the caller's connection or transaction.
pub(crate) fn insert_reclaim_in(conn: &Connection, r: &ReclaimRecord) -> rusqlite::Result<i64> {
    // SQLite integers are signed; a byte count above i64::MAX is not a real disk.
    let freed_bytes = r
        .freed_bytes
        .map(|bytes| i64::try_from(bytes).unwrap_or(i64::MAX));
    conn.execute(
        "INSERT INTO reclaims(repo_id, workspace_id, reason, final_op_id, snapshot_taken,
                              freed_bytes, at, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            r.repo_id,
            r.workspace_id,
            r.reason,
            r.final_op_id,
            r.snapshot_taken,
            freed_bytes,
            r.at,
            r.detail,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

impl Db {
    /// Appends an entry to the reclaim log and returns its row id. `r.id` is
    /// ignored.
    pub fn insert_reclaim(&self, r: &ReclaimRecord) -> Result<i64, DbError> {
        self.with(|conn| insert_reclaim_in(conn, r))
    }

    /// Up to `limit` entries of the reclaim log for `repo` (every repository
    /// if `None`), newest first.
    pub fn reclaims(
        &self,
        repo: Option<RepoId>,
        limit: usize,
    ) -> Result<Vec<ReclaimRecord>, DbError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM reclaims
                 WHERE (?1 IS NULL OR repo_id = ?1)
                 ORDER BY at DESC, id DESC LIMIT ?2"
            ))?;
            stmt.query_map(params![repo, limit], from_row)?.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReclaimReason, Timestamp};

    fn reclaim(repo_id: &str, workspace_id: &str, at: i64) -> ReclaimRecord {
        ReclaimRecord {
            id: 0,
            repo_id: repo_id.parse().unwrap(),
            workspace_id: workspace_id.parse().unwrap(),
            reason: ReclaimReason::Expired,
            final_op_id: Some("ab".repeat(64)),
            snapshot_taken: true,
            freed_bytes: Some(4096),
            at: Timestamp(at),
            detail: None,
        }
    }

    #[test]
    fn insert_and_list_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let first = reclaim("aaaa000000000001", "bbbb000000000001", 10);
        let mut second = reclaim("aaaa000000000001", "bbbb000000000002", 20);
        second.reason = ReclaimReason::Gc;
        second.final_op_id = None;
        second.snapshot_taken = false;
        second.freed_bytes = None;
        second.detail = Some("directory already gone".into());
        let other = reclaim("aaaa000000000002", "bbbb000000000003", 30);

        let first_id = db.insert_reclaim(&first).unwrap();
        let second_id = db.insert_reclaim(&second).unwrap();
        db.insert_reclaim(&other).unwrap();
        assert!(second_id > first_id);

        let listed = db.reclaims(Some(first.repo_id), 10).unwrap();
        assert_eq!(
            listed,
            [
                ReclaimRecord {
                    id: second_id,
                    ..second
                },
                ReclaimRecord {
                    id: first_id,
                    ..first
                },
            ]
        );
        assert_eq!(db.reclaims(None, 10).unwrap().len(), 3);
        let newest = db.reclaims(None, 1).unwrap();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].repo_id, other.repo_id);
    }
}
