//! The `snapshots` index (R1.16): one row per snapshot operation the daemon
//! writes, recording which workspace took it, why, and that it is local.
//!
//! Watcher snapshots stay on the machine; only a change that is explicitly
//! published leaves it (`docs/design.md`, Workspaces and Durability).
//! Nothing in Phase 1 exports anything, so this index is what makes that
//! rule enforceable later: R4's publish clears `local` on the rows of the
//! change it publishes ([`for_change`]), and an export refuses the rest.
//! Every row starts local, and nothing in Phase 1 clears it.
//!
//! jj's operation log stays authoritative (ADR 0003). [`reindex`] rebuilds
//! the index from it at daemon start, after R1.17's GC, and after R1.18's
//! restore: it adds a row for every Reef snapshot operation the table is
//! missing and drops rows whose operation has left the log. The log
//! carries no `kind`, so a rebuilt row of an explicit snapshot reads
//! `watcher` and one of a shutdown snapshot `final`
//! ([`reef_lib::jj::oplog::is_reef_snapshot`]).
//!
//! GC (R1.17) rewrites the operations it keeps, with new ids, and jj does
//! not say which old id became which. The reindex after GC therefore drops
//! the rows of rewritten operations and adds them again under their new
//! ids, as local and with the kind the log implies. R4's publish, which
//! clears `local`, has to survive that: carry the flag by commit id, or
//! record published changes elsewhere.
//!
//! As everywhere in the daemon, SQLite stays off the repository thread: the
//! pipeline writes a row on its blocking half after the repository thread
//! has committed the operation, and [`reindex`]'s job on the thread only
//! reads the log; the caller writes the difference.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::FutureExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::oplog::{OpLogError, SnapshotMarkKind, is_reef_snapshot};
use reef_lib::jj::{ChangeIdHex, CommitIdHex, OpenError, OperationIdHex};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension as _, Row, params};

use super::SnapshotKind;
use crate::db::{Db, DbError};
use crate::model::{RepoId, Timestamp, WorkspaceId};
use crate::repo::{RepoHandle, RepoHead, RepoThreadError};

/// How many rows `ws inspect` lists.
pub const RECENT_LIMIT: usize = 10;

/// One row of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// The snapshot's operation.
    pub op_id: OperationIdHex,
    /// The workspace that took it.
    pub workspace_id: WorkspaceId,
    /// The working-copy commit it wrote.
    pub commit_id: CommitIdHex,
    /// That commit's change.
    pub change_id: ChangeIdHex,
    /// Why it was taken.
    pub kind: SnapshotKind,
    /// The snapshot has not been published; always true in Phase 1.
    pub local: bool,
    /// When the operation ended, as jj recorded it.
    pub taken_at: Timestamp,
}

const COLUMNS: &str = "op_id, workspace_id, commit_id, change_id, kind, local, taken_at";

/// A hex id column that does not parse, which Reef never writes.
fn hex_column<T: std::str::FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let text: String = row.get(index)?;
    text.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

/// A `count(*)` or `sum` column; SQLite's are never negative here.
fn count(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    Ok(u64::try_from(row.get::<_, i64>(index)?).unwrap_or(0))
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<SnapshotEntry> {
    Ok(SnapshotEntry {
        op_id: hex_column(row, 0)?,
        workspace_id: row.get(1)?,
        commit_id: hex_column(row, 2)?,
        change_id: hex_column(row, 3)?,
        kind: row.get(4)?,
        local: row.get(5)?,
        taken_at: row.get(6)?,
    })
}

/// Inserts `entry`'s row as local, whatever `entry.local` says: a snapshot
/// is local when taken, and only R4's publish clears the flag. Returns
/// whether a row was added; an operation already indexed keeps its row.
fn insert_in(conn: &Connection, repo: RepoId, entry: &SnapshotEntry) -> rusqlite::Result<bool> {
    let added = conn.execute(
        "INSERT INTO snapshots(op_id, repo_id, workspace_id, commit_id, change_id, kind, taken_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(op_id) DO NOTHING",
        params![
            entry.op_id.as_str(),
            repo,
            entry.workspace_id,
            entry.commit_id.as_str(),
            entry.change_id.as_str(),
            entry.kind,
            entry.taken_at,
        ],
    )?;
    Ok(added > 0)
}

/// Indexes a snapshot operation of `repo`, as local. Called by the
/// snapshot pipeline after the operation is committed and before the
/// working-copy state is saved, so a crash between the two leaves a row
/// naming an operation that exists, never an operation without a row that
/// the next start's [`reindex`] cannot restore.
pub fn record(db: &Db, repo: RepoId, entry: &SnapshotEntry) -> Result<(), DbError> {
    db.with(|conn| insert_in(conn, repo, entry).map(drop))
}

/// A workspace's snapshot count and how many of them are local.
pub fn counts(db: &Db, ws: WorkspaceId) -> Result<(u64, u64), DbError> {
    db.with(|conn| {
        conn.query_row(
            "SELECT count(*), coalesce(sum(local), 0) FROM snapshots WHERE workspace_id = ?1",
            [ws],
            |row| Ok((count(row, 0)?, count(row, 1)?)),
        )
    })
}

/// The local snapshot count of every workspace in `repo` (every repository
/// if `None`) that has one, for `ws list`.
pub fn local_counts(db: &Db, repo: Option<RepoId>) -> Result<HashMap<WorkspaceId, u64>, DbError> {
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT workspace_id, count(*) FROM snapshots
             WHERE local = 1 AND (?1 IS NULL OR repo_id = ?1)
             GROUP BY workspace_id",
        )?;
        let rows = stmt.query_map([repo], |row| Ok((row.get(0)?, count(row, 1)?)))?;
        rows.collect()
    })
}

/// A workspace's newest `limit` snapshots, newest first.
pub fn recent(db: &Db, ws: WorkspaceId, limit: usize) -> Result<Vec<SnapshotEntry>, DbError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    db.with(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM snapshots WHERE workspace_id = ?1
             ORDER BY taken_at DESC, op_id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![ws, limit], from_row)?;
        rows.collect()
    })
}

/// Every workspace's snapshots of `change` in `repo`, oldest first: what
/// R4's publish of the change flips.
pub fn for_change(
    db: &Db,
    repo: RepoId,
    change: &ChangeIdHex,
) -> Result<Vec<SnapshotEntry>, DbError> {
    db.with(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM snapshots WHERE repo_id = ?1 AND change_id = ?2
             ORDER BY taken_at, op_id"
        ))?;
        let rows = stmt.query_map(params![repo, change.as_str()], from_row)?;
        rows.collect()
    })
}

/// The row of one operation, if indexed.
pub fn get(db: &Db, op: &OperationIdHex) -> Result<Option<SnapshotEntry>, DbError> {
    db.with(|conn| {
        conn.query_row(
            &format!("SELECT {COLUMNS} FROM snapshots WHERE op_id = ?1"),
            [op.as_str()],
            from_row,
        )
        .optional()
    })
}

/// Test hook: deletes every row of `repo`, as an older database restored
/// from a backup would lack them.
#[cfg(any(test, feature = "testing"))]
pub fn clear(db: &Db, repo: RepoId) -> Result<usize, DbError> {
    db.with(|conn| conn.execute("DELETE FROM snapshots WHERE repo_id = ?1", [repo]))
}

/// What [`reindex`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Rows added for operations the table was missing.
    pub added: u64,
    /// Rows removed because their operation has left the log.
    pub removed: u64,
    /// Reef snapshot operations with no row that were left unindexed: their
    /// view has no working-copy commit for the workspace, or it could not
    /// be read (logged).
    pub skipped: u64,
    /// Operations walked, Reef's or not.
    pub walked: u64,
    /// The walk stopped at `gc.reindex_limit` before the root operation.
    /// Nothing is removed then, since an operation past the limit may still
    /// be in the log.
    pub truncated: bool,
}

/// Failure of [`reindex`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReindexError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The repository thread is gone.
    #[error(transparent)]
    Thread(#[from] RepoThreadError),
    /// The repository could not be reloaded at head.
    #[error("reload the repository")]
    Reload(#[from] OpenError),
    /// The operation log could not be read.
    #[error("read the operation log")]
    OpLog(#[from] OpLogError),
}

impl Classify for ReindexError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Db(error) => error.kind(),
            Self::Thread(error) => error.kind(),
            Self::Reload(error) => error.kind(),
            Self::OpLog(error) => error.kind(),
        }
    }
}

/// A Reef snapshot operation the table is missing, as the repository
/// thread found it.
#[derive(Debug)]
struct SnapshotOpRow {
    op_id: OperationIdHex,
    workspace_id: WorkspaceId,
    kind: SnapshotKind,
    commit_id: CommitIdHex,
    change_id: ChangeIdHex,
    end_time: Timestamp,
}

/// What the repository thread's walk found.
#[derive(Debug)]
struct Scan {
    /// Reef snapshot operations of known workspaces with no row yet.
    missing: Vec<SnapshotOpRow>,
    /// Reef snapshot operations with no row whose commit could not be read.
    skipped: u64,
    /// Indexed operations the walk reached.
    present: HashSet<String>,
    walked: u64,
    truncated: bool,
}

/// Brings `repo_id`'s rows in line with its operation log: walks up to
/// `limit` operations from head on the repository thread, adds a local row
/// for every Reef snapshot operation of a workspace the database knows
/// that has none, and, unless the walk was cut short, removes the rows of
/// operations it did not reach. Operations of workspaces the database no
/// longer has (reclaimed, then deleted by R1.17's GC) are skipped.
///
/// Rows recorded while it runs are kept: they are not in the table when
/// the walk's input is read, so they are never candidates for removal.
pub async fn reindex(
    db: &Db,
    repo: &RepoHandle,
    repo_id: RepoId,
    limit: usize,
) -> Result<ReindexReport, ReindexError> {
    // Read before the walk, whose head is then at least as new as every
    // operation these rows name.
    let (known, workspaces) = db.with(|conn| {
        let mut ops = conn.prepare("SELECT op_id FROM snapshots WHERE repo_id = ?1")?;
        let known = ops
            .query_map([repo_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let mut ids = conn.prepare("SELECT id FROM workspaces WHERE repo_id = ?1")?;
        let workspaces = ids
            .query_map([repo_id], |row| row.get::<_, WorkspaceId>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok((known, workspaces))
    })?;
    let known = Arc::new(known);
    let scan = {
        let known = Arc::clone(&known);
        repo.call_async(move |head| scan(head, known, workspaces, limit).boxed_local())
            .await??
    };

    let mut report = ReindexReport {
        skipped: scan.skipped,
        walked: scan.walked,
        truncated: scan.truncated,
        ..ReindexReport::default()
    };
    db.in_transaction(|tx| {
        for row in &scan.missing {
            let entry = SnapshotEntry {
                op_id: row.op_id.clone(),
                workspace_id: row.workspace_id,
                commit_id: row.commit_id.clone(),
                change_id: row.change_id.clone(),
                kind: row.kind,
                local: true,
                taken_at: row.end_time,
            };
            if insert_in(tx, repo_id, &entry)? {
                report.added += 1;
            }
        }
        if !scan.truncated {
            let mut delete = tx.prepare("DELETE FROM snapshots WHERE op_id = ?1")?;
            for op in known.iter().filter(|op| !scan.present.contains(*op)) {
                report.removed += u64::try_from(delete.execute([op])?).unwrap_or(0);
            }
        }
        Ok(())
    })?;
    Ok(report)
}

/// The repository thread's half of [`reindex`]: reads only.
async fn scan(
    head: &mut RepoHead,
    known: Arc<HashSet<String>>,
    workspaces: HashSet<WorkspaceId>,
    limit: usize,
) -> Result<Scan, ReindexError> {
    head.refresh().await?;
    let mut present = HashSet::new();
    let mut marked = Vec::new();
    let walk = head
        .repo
        .walk_op_log(limit, |entry| {
            if known.contains(entry.id.as_str()) {
                present.insert(entry.id.as_str().to_owned());
                return;
            }
            let Some(mark) = is_reef_snapshot(&entry) else {
                return;
            };
            let Ok(workspace_id) = mark.workspace_id.parse::<WorkspaceId>() else {
                return;
            };
            if !workspaces.contains(&workspace_id) {
                return;
            }
            let kind = match mark.kind {
                SnapshotMarkKind::Snapshot => SnapshotKind::Watcher,
                SnapshotMarkKind::Final => SnapshotKind::Final,
                SnapshotMarkKind::Move => SnapshotKind::Move,
            };
            marked.push((entry.id, workspace_id, kind, Timestamp::from(entry.end)));
        })
        .await?;
    let mut missing = Vec::with_capacity(marked.len());
    let mut skipped = 0;
    for (op_id, workspace_id, kind, end_time) in marked {
        let name = workspace_id.jj_workspace_name();
        // One unreadable operation or commit must not keep the rest of the
        // repository from being indexed.
        let (commit_id, change_id) = match head.repo.wc_commit_at(&op_id, &name).await {
            Ok(Some(found)) => found,
            Ok(None) => {
                // A snapshot of a workspace its own view lacks.
                tracing::debug!(op_id = %op_id, workspace_id = %workspace_id, "snapshot operation without a working-copy commit; skipped");
                skipped += 1;
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    op_id = %op_id,
                    workspace_id = %workspace_id,
                    error = %ErrorChain(&error),
                    "cannot read a snapshot operation's working-copy commit; not indexed"
                );
                skipped += 1;
                continue;
            }
        };
        missing.push(SnapshotOpRow {
            op_id,
            workspace_id,
            kind,
            commit_id,
            change_id,
            end_time,
        });
    }
    Ok(Scan {
        missing,
        present,
        skipped,
        walked: walk.walked,
        truncated: walk.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "aaaa000000000001";

    fn entry(op: &str, ws: &str, change: &str, kind: SnapshotKind, at: i64) -> SnapshotEntry {
        SnapshotEntry {
            op_id: op.parse().unwrap(),
            workspace_id: ws.parse().unwrap(),
            commit_id: "c0ffee".parse().unwrap(),
            change_id: change.parse().unwrap(),
            kind,
            local: true,
            taken_at: Timestamp(at),
        }
    }

    #[test]
    fn rows_round_trip_and_count() {
        let db = Db::open_in_memory().unwrap();
        let repo = REPO.parse().unwrap();
        let ws: WorkspaceId = "bbbb000000000001".parse().unwrap();
        let other: WorkspaceId = "bbbb000000000002".parse().unwrap();
        let first = entry("01", "bbbb000000000001", "aa", SnapshotKind::Watcher, 1);
        record(&db, repo, &first).unwrap();
        // A second record of the same operation keeps the first row.
        record(
            &db,
            repo,
            &SnapshotEntry {
                kind: SnapshotKind::Final,
                ..first.clone()
            },
        )
        .unwrap();
        record(
            &db,
            repo,
            &entry("02", "bbbb000000000001", "aa", SnapshotKind::Explicit, 2),
        )
        .unwrap();
        record(
            &db,
            repo,
            &entry("03", "bbbb000000000002", "aa", SnapshotKind::Final, 3),
        )
        .unwrap();
        record(
            &db,
            repo,
            &entry("04", "bbbb000000000002", "bb", SnapshotKind::Shutdown, 4),
        )
        .unwrap();

        assert_eq!(counts(&db, ws).unwrap(), (2, 2));
        assert_eq!(
            counts(&db, "bbbb000000000009".parse().unwrap()).unwrap(),
            (0, 0)
        );
        let recent_ops: Vec<_> = recent(&db, ws, 10)
            .unwrap()
            .into_iter()
            .map(|row| (row.op_id.to_string(), row.kind))
            .collect();
        assert_eq!(
            recent_ops,
            [
                ("02".to_owned(), SnapshotKind::Explicit),
                ("01".to_owned(), SnapshotKind::Watcher)
            ]
        );
        assert_eq!(recent(&db, ws, 1).unwrap().len(), 1);
        assert_eq!(get(&db, &"01".parse().unwrap()).unwrap(), Some(first));

        let on_change: Vec<_> = for_change(&db, repo, &"aa".parse().unwrap())
            .unwrap()
            .into_iter()
            .map(|row| row.op_id.to_string())
            .collect();
        assert_eq!(on_change, ["01", "02", "03"]);
        assert_eq!(
            local_counts(&db, Some(repo)).unwrap(),
            HashMap::from([(ws, 2), (other, 2)])
        );
        assert!(
            local_counts(&db, Some("aaaa000000000002".parse().unwrap()))
                .unwrap()
                .is_empty()
        );
        assert_eq!(local_counts(&db, None).unwrap().len(), 2);
    }

    #[test]
    fn kinds_parse_back() {
        for kind in SnapshotKind::ALL {
            assert_eq!(kind.as_str().parse::<SnapshotKind>().unwrap(), *kind);
        }
        assert!("Watcher".parse::<SnapshotKind>().is_err());
    }
}
