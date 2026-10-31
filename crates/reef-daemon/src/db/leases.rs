//! Queries on `leases`.

use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension as _, Row, Transaction, params};

use super::{Db, DbError};
use crate::model::{
    LeaseEnd, LeaseId, LeaseRecord, RepoId, Timestamp, WorkspaceId, WorkspaceState, duration_millis,
};

const WHAT: &str = "lease";

const COLUMNS: &str = "id, repo_id, workspace_id, owner, issued_at, expires_at, \
     last_heartbeat_at, length_ms, grace_ms, ended_at, end_reason";

/// A stored duration below zero, which Reef never writes.
#[derive(Debug, thiserror::Error)]
#[error("negative duration {0} ms")]
struct NegativeDuration(i64);

fn duration(row: &Row<'_>, idx: usize) -> rusqlite::Result<Duration> {
    let millis: i64 = row.get(idx)?;
    u64::try_from(millis)
        .map(Duration::from_millis)
        .map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                Type::Integer,
                Box::new(NegativeDuration(millis)),
            )
        })
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<LeaseRecord> {
    Ok(LeaseRecord {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        workspace_id: row.get(2)?,
        owner: row.get(3)?,
        issued_at: row.get(4)?,
        expires_at: row.get(5)?,
        last_heartbeat_at: row.get(6)?,
        length: duration(row, 7)?,
        grace: duration(row, 8)?,
        ended_at: row.get(9)?,
        end_reason: row.get(10)?,
    })
}

fn not_found(id: LeaseId) -> DbError {
    DbError::NotFound {
        what: WHAT,
        id: id.as_hex(),
    }
}

/// After an update of an unended lease by id changed no row: the lease is
/// missing ([`DbError::NotFound`]) or has ended ([`DbError::Conflict`]).
fn missing_or_ended(tx: &Transaction<'_>, id: LeaseId) -> DbError {
    match tx
        .query_row("SELECT 1 FROM leases WHERE id = ?1", [id], |_| Ok(()))
        .optional()
    {
        Ok(Some(())) => DbError::Conflict {
            what: WHAT,
            id: id.as_hex(),
            reason: "has ended",
        },
        Ok(None) => not_found(id),
        Err(error) => error.into(),
    }
}

/// The lease tick's query. Its `WHERE` and `ORDER BY` repeat the expression
/// of the `leases_unended_grace_end` index so SQLite searches that index
/// rather than scanning; ordering by `expires_at` alone would make it scan
/// `leases_unended_expires_at` instead.
fn expired_sql() -> String {
    format!(
        "SELECT {COLUMNS} FROM leases
         WHERE ended_at IS NULL AND expires_at + grace_ms < ?1
         ORDER BY expires_at + grace_ms, id"
    )
}

/// What [`Db::renew_lease`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renewal {
    /// The lease was renewed; this is the lease as it now is.
    Renewed(LeaseRecord),
    /// No workspace has that id.
    NoWorkspace,
    /// The workspace was released or reclaimed; `at` is when its last lease
    /// ended.
    Ended {
        /// The workspace's repository.
        repo: RepoId,
        /// `released` or `reclaimed`.
        state: WorkspaceState,
        /// When its last lease ended, if it had one.
        at: Option<Timestamp>,
    },
    /// The workspace is being released or reclaimed.
    Releasing,
    /// The lease is not the workspace's current one.
    Superseded,
    /// The lease is past its grace; the next expiry tick reclaims the
    /// workspace.
    PastGrace {
        /// The workspace's repository.
        repo: RepoId,
        /// `expires_at + grace_ms`.
        grace_ended_at: Timestamp,
    },
}

/// [`Db::issue_lease`] inside the caller's transaction.
pub(crate) fn issue_lease_in(tx: &Transaction<'_>, l: &LeaseRecord) -> Result<(), DbError> {
    let state: WorkspaceState = tx
        .query_row(
            "SELECT state FROM workspaces WHERE id = ?1",
            [l.workspace_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| DbError::NotFound {
            what: "workspace",
            id: l.workspace_id.as_hex(),
        })?;
    if state != WorkspaceState::Active {
        return Err(DbError::Conflict {
            what: "workspace",
            id: l.workspace_id.as_hex(),
            reason: "is not active",
        });
    }
    let unended = tx
        .query_row(
            "SELECT 1 FROM leases WHERE workspace_id = ?1 AND ended_at IS NULL",
            [l.workspace_id],
            |_| Ok(()),
        )
        .optional()?;
    if unended.is_some() {
        return Err(DbError::Conflict {
            what: "workspace",
            id: l.workspace_id.as_hex(),
            reason: "already has an unended lease",
        });
    }
    let taken = tx
        .query_row("SELECT 1 FROM leases WHERE id = ?1", [l.id], |_| Ok(()))
        .optional()?;
    if taken.is_some() {
        return Err(DbError::AlreadyExists {
            what: WHAT,
            id: l.id.as_hex(),
        });
    }
    tx.execute(
        &format!(
            "INSERT INTO leases({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
        ),
        params![
            l.id,
            l.repo_id,
            l.workspace_id,
            l.owner,
            l.issued_at,
            l.expires_at,
            l.last_heartbeat_at,
            duration_millis(l.length),
            duration_millis(l.grace),
            l.ended_at,
            l.end_reason,
        ],
    )?;
    tx.execute(
        "UPDATE workspaces SET lease_id = ?2 WHERE id = ?1",
        params![l.workspace_id, l.id],
    )?;
    Ok(())
}

impl Db {
    /// Issues a lease on its workspace, in one transaction: inserts the
    /// lease and points `workspaces.lease_id` at it. The workspace must
    /// exist and be `active` ([`DbError::NotFound`], [`DbError::Conflict`]),
    /// and must not already hold an unended lease ([`DbError::Conflict`];
    /// a unique partial index also guarantees this).
    pub fn issue_lease(&self, l: &LeaseRecord) -> Result<(), DbError> {
        self.in_transaction(|tx| issue_lease_in(tx, l))
    }

    /// The lease with this id, ended or not.
    pub fn lease(&self, id: LeaseId) -> Result<LeaseRecord, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM leases WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()
        })?
        .ok_or_else(|| not_found(id))
    }

    /// The workspace's unended lease, if it has one. An unended lease may be
    /// past its expiry until the lease manager ends it.
    pub fn active_lease(&self, workspace: WorkspaceId) -> Result<Option<LeaseRecord>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM leases WHERE workspace_id = ?1 AND ended_at IS NULL"
                ),
                [workspace],
                from_row,
            )
            .optional()
        })
    }

    /// Records a heartbeat at `now` that moves the expiry to `expires_at`.
    /// [`DbError::Conflict`] if the lease has ended.
    pub fn touch_lease(
        &self,
        id: LeaseId,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let changed = tx.execute(
                "UPDATE leases SET last_heartbeat_at = ?2, expires_at = ?3
                 WHERE id = ?1 AND ended_at IS NULL",
                params![id, now, expires_at],
            )?;
            if changed == 0 {
                return Err(missing_or_ended(tx, id));
            }
            Ok(())
        })
    }

    /// Every lease the workspace was issued, oldest first (R2.12).
    pub fn leases_of(&self, workspace: WorkspaceId) -> Result<Vec<LeaseRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM leases WHERE workspace_id = ?1
                 ORDER BY issued_at, rowid"
            ))?;
            let rows = stmt.query_map([workspace], from_row)?;
            rows.collect()
        })
    }

    /// The workspace's most recently issued lease, ended or not.
    pub fn last_lease(&self, workspace: WorkspaceId) -> Result<Option<LeaseRecord>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM leases WHERE workspace_id = ?1
                     ORDER BY issued_at DESC, rowid DESC LIMIT 1"
                ),
                [workspace],
                from_row,
            )
            .optional()
        })
    }

    /// A heartbeat at `now` on `lease`, in one transaction: if `lease` is
    /// the current lease of the `active` workspace `workspace` and is not
    /// past its grace, moves its expiry to `now + length_ms` and records
    /// the heartbeat. Otherwise writes nothing and says why. Serialized
    /// with [`Db::begin_reclaim`], so a lease is never renewed after a
    /// reclaim has started.
    pub fn renew_lease(
        &self,
        workspace: WorkspaceId,
        lease: LeaseId,
        now: Timestamp,
    ) -> Result<Renewal, DbError> {
        self.in_transaction(|tx| {
            let row: Option<(WorkspaceState, Option<LeaseId>, RepoId)> = tx
                .query_row(
                    "SELECT state, lease_id, repo_id FROM workspaces WHERE id = ?1",
                    [workspace],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((state, current, repo)) = row else {
                return Ok(Renewal::NoWorkspace);
            };
            match state {
                WorkspaceState::Active => {}
                WorkspaceState::Releasing => return Ok(Renewal::Releasing),
                WorkspaceState::Released | WorkspaceState::Reclaimed => {
                    let at = tx.query_row(
                        "SELECT MAX(ended_at) FROM leases WHERE workspace_id = ?1",
                        [workspace],
                        |row| row.get(0),
                    )?;
                    return Ok(Renewal::Ended { repo, state, at });
                }
            }
            if current != Some(lease) {
                return Ok(Renewal::Superseded);
            }
            let record = tx.query_row(
                &format!("SELECT {COLUMNS} FROM leases WHERE id = ?1"),
                [lease],
                from_row,
            )?;
            let grace_ended_at = Timestamp(
                record
                    .expires_at
                    .0
                    .saturating_add(duration_millis(record.grace)),
            );
            if now > grace_ended_at {
                return Ok(Renewal::PastGrace {
                    repo,
                    grace_ended_at,
                });
            }
            let expires_at = Timestamp(now.0.saturating_add(duration_millis(record.length)));
            tx.execute(
                "UPDATE leases SET last_heartbeat_at = ?2, expires_at = ?3 WHERE id = ?1",
                params![lease, now, expires_at],
            )?;
            Ok(Renewal::Renewed(LeaseRecord {
                expires_at,
                last_heartbeat_at: now,
                ..record
            }))
        })
    }

    /// Ends a lease at `at` for `reason` and clears `workspaces.lease_id`,
    /// in one transaction. [`DbError::Conflict`] if it has already ended.
    pub fn end_lease(&self, id: LeaseId, at: Timestamp, reason: LeaseEnd) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let changed = tx.execute(
                "UPDATE leases SET ended_at = ?2, end_reason = ?3
                 WHERE id = ?1 AND ended_at IS NULL",
                params![id, at, reason],
            )?;
            if changed == 0 {
                return Err(missing_or_ended(tx, id));
            }
            tx.execute(
                "UPDATE workspaces SET lease_id = NULL WHERE lease_id = ?1",
                [id],
            )?;
            Ok(())
        })
    }

    /// Unended leases whose grace has run out by `cutoff`
    /// (`expires_at + grace_ms < cutoff`), the one whose grace ran out first
    /// first.
    pub fn leases_expired_before(&self, cutoff: Timestamp) -> Result<Vec<LeaseRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&expired_sql())?;
            stmt.query_map([cutoff], from_row)?.collect()
        })
    }

    /// Moves every unended lease's expiry to at least `at_least`, as the
    /// daemon does on start so nothing is reclaimed on the first tick
    /// (R1.15). Returns how many leases moved.
    pub fn extend_all_active_leases_to(&self, at_least: Timestamp) -> Result<usize, DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE leases SET expires_at = ?1 WHERE ended_at IS NULL AND expires_at < ?1",
                [at_least],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::db::repositories::tests::repo;
    use crate::db::workspaces::tests::workspace;
    use crate::model::RepoId;

    const MINUTE: Duration = Duration::from_secs(60);

    fn lease(repo_id: RepoId, workspace_id: WorkspaceId, id: &str) -> LeaseRecord {
        LeaseRecord {
            id: id.parse().unwrap(),
            repo_id,
            workspace_id,
            owner: "alice@host:42".into(),
            issued_at: Timestamp(0),
            expires_at: Timestamp(30 * 60_000),
            last_heartbeat_at: Timestamp(0),
            length: 30 * MINUTE,
            grace: 5 * MINUTE,
            ended_at: None,
            end_reason: None,
        }
    }

    /// A database with one repository and two active, unleased workspaces.
    fn fixture() -> (Db, RepoId, WorkspaceId, WorkspaceId) {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let one = workspace(record.id, "bbbb000000000001", "/ws/1");
        let two = workspace(record.id, "bbbb000000000002", "/ws/2");
        db.insert_workspace(&one).unwrap();
        db.insert_workspace(&two).unwrap();
        (db, record.id, one.id, two.id)
    }

    #[test]
    fn finish_ends_the_lease_and_logs_the_reclaim_together() {
        use crate::model::{ReclaimReason, ReclaimRecord};

        let (db, repo_id, ws, other) = fixture();
        db.issue_lease(&lease(repo_id, ws, "cccc000000000001"))
            .unwrap();
        let reclaim = |workspace_id| ReclaimRecord {
            id: 0,
            repo_id,
            workspace_id,
            reason: ReclaimReason::Released,
            final_op_id: Some("ab".into()),
            snapshot_taken: true,
            freed_bytes: Some(4096),
            at: Timestamp(60_000),
            detail: None,
        };
        // Released is reached only through releasing; nothing is written.
        let error = db
            .finish_workspace(
                ws,
                WorkspaceState::Released,
                LeaseEnd::Released,
                &reclaim(ws),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(db.reclaims(None, 10).unwrap().len(), 0);
        assert!(db.active_lease(ws).unwrap().is_some());

        db.set_workspace_state(ws, WorkspaceState::Releasing)
            .unwrap();
        db.finish_workspace(
            ws,
            WorkspaceState::Released,
            LeaseEnd::Released,
            &reclaim(ws),
        )
        .unwrap();
        let record = db.workspace(ws).unwrap();
        assert_eq!(record.state, WorkspaceState::Released);
        assert_eq!(record.lease_id, None);
        assert!(db.active_lease(ws).unwrap().is_none());
        let ended = db.lease("cccc000000000001".parse().unwrap()).unwrap();
        assert_eq!(ended.ended_at, Some(Timestamp(60_000)));
        assert_eq!(ended.end_reason, Some(LeaseEnd::Released));
        let log = db.reclaims(Some(repo_id), 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].freed_bytes, Some(4096));

        // An unleased active workspace can be reclaimed directly.
        db.finish_workspace(
            other,
            WorkspaceState::Reclaimed,
            LeaseEnd::Expired,
            &reclaim(other),
        )
        .unwrap();
        assert_eq!(
            db.workspace(other).unwrap().state,
            WorkspaceState::Reclaimed
        );
        let again = db.finish_workspace(
            other,
            WorkspaceState::Reclaimed,
            LeaseEnd::Expired,
            &reclaim(other),
        );
        assert_eq!(again.unwrap_err().kind(), ErrorKind::Conflict);
    }

    #[test]
    fn issue_sets_the_workspace_lease() {
        let (db, repo_id, ws, _) = fixture();
        let l = lease(repo_id, ws, "cccc000000000001");
        db.issue_lease(&l).unwrap();
        assert_eq!(db.lease(l.id).unwrap(), l);
        assert_eq!(db.active_lease(ws).unwrap(), Some(l.clone()));
        assert_eq!(db.workspace(ws).unwrap().lease_id, Some(l.id));
    }

    #[test]
    fn second_unended_lease_is_a_conflict() {
        let (db, repo_id, ws, _) = fixture();
        db.issue_lease(&lease(repo_id, ws, "cccc000000000001"))
            .unwrap();
        let error = db
            .issue_lease(&lease(repo_id, ws, "cccc000000000002"))
            .unwrap_err();
        assert!(matches!(error, DbError::Conflict { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(
            db.lease("cccc000000000002".parse().unwrap())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound,
            "nothing written"
        );
    }

    #[test]
    fn issue_needs_an_active_workspace() {
        let (db, repo_id, ws, _) = fixture();
        let missing = lease(
            repo_id,
            "bbbb00000000000f".parse().unwrap(),
            "cccc000000000001",
        );
        assert_eq!(
            db.issue_lease(&missing).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        db.set_workspace_state(ws, WorkspaceState::Reclaimed)
            .unwrap();
        let ended = lease(repo_id, ws, "cccc000000000001");
        assert_eq!(
            db.issue_lease(&ended).unwrap_err().kind(),
            ErrorKind::Conflict
        );
    }

    #[test]
    fn end_clears_the_workspace_and_allows_a_new_lease() {
        let (db, repo_id, ws, _) = fixture();
        let first = lease(repo_id, ws, "cccc000000000001");
        db.issue_lease(&first).unwrap();
        db.end_lease(first.id, Timestamp(10), LeaseEnd::Restored)
            .unwrap();
        let ended = db.lease(first.id).unwrap();
        assert_eq!(ended.ended_at, Some(Timestamp(10)));
        assert_eq!(ended.end_reason, Some(LeaseEnd::Restored));
        assert_eq!(db.workspace(ws).unwrap().lease_id, None);
        assert_eq!(db.active_lease(ws).unwrap(), None);

        let again = db.end_lease(first.id, Timestamp(11), LeaseEnd::Released);
        assert_eq!(again.unwrap_err().kind(), ErrorKind::Conflict);
        let touched = db.touch_lease(first.id, Timestamp(11), Timestamp(12));
        assert_eq!(touched.unwrap_err().kind(), ErrorKind::Conflict);

        let second = lease(repo_id, ws, "cccc000000000002");
        db.issue_lease(&second).unwrap();
        assert_eq!(db.workspace(ws).unwrap().lease_id, Some(second.id));
    }

    #[test]
    fn renew_checks_state_lease_and_grace() {
        let (db, repo_id, ws, other) = fixture();
        let mut l = lease(repo_id, ws, "cccc000000000001");
        l.expires_at = Timestamp(1_000);
        l.length = Duration::from_millis(2_000);
        l.grace = Duration::from_millis(500);
        db.issue_lease(&l).unwrap();

        let renewed = db.renew_lease(ws, l.id, Timestamp(1_200)).unwrap();
        let Renewal::Renewed(record) = renewed else {
            panic!("{renewed:?}")
        };
        assert_eq!(record.expires_at, Timestamp(3_200), "revived within grace");
        assert_eq!(record.last_heartbeat_at, Timestamp(1_200));
        assert_eq!(db.lease(l.id).unwrap(), record);

        assert_eq!(
            db.renew_lease(ws, l.id, Timestamp(3_701)).unwrap(),
            Renewal::PastGrace {
                repo: repo_id,
                grace_ended_at: Timestamp(3_700)
            }
        );
        assert_eq!(
            db.renew_lease(ws, "cccc00000000000f".parse().unwrap(), Timestamp(2_000))
                .unwrap(),
            Renewal::Superseded
        );
        assert_eq!(
            db.renew_lease(other, l.id, Timestamp(2_000)).unwrap(),
            Renewal::Superseded,
            "another workspace's lease"
        );
        assert_eq!(
            db.renew_lease("bbbb00000000000f".parse().unwrap(), l.id, Timestamp(2_000))
                .unwrap(),
            Renewal::NoWorkspace
        );

        assert!(db.begin_reclaim(ws, None).unwrap());
        assert_eq!(
            db.renew_lease(ws, l.id, Timestamp(2_000)).unwrap(),
            Renewal::Releasing
        );
        db.end_lease(l.id, Timestamp(2_500), LeaseEnd::Expired)
            .unwrap();
        db.set_workspace_state(ws, WorkspaceState::Reclaimed)
            .unwrap();
        assert_eq!(
            db.renew_lease(ws, l.id, Timestamp(2_600)).unwrap(),
            Renewal::Ended {
                repo: repo_id,
                state: WorkspaceState::Reclaimed,
                at: Some(Timestamp(2_500))
            }
        );
        assert_eq!(db.last_lease(ws).unwrap().unwrap().id, l.id);
        assert_eq!(db.last_lease(other).unwrap(), None);
    }

    #[test]
    fn expiry_reclaim_starts_only_past_grace() {
        let (db, repo_id, ws, unleased) = fixture();
        let mut l = lease(repo_id, ws, "cccc000000000001");
        l.expires_at = Timestamp(1_000);
        l.grace = Duration::from_millis(500);
        db.issue_lease(&l).unwrap();

        assert!(!db.begin_reclaim(ws, Some(Timestamp(1_500))).unwrap());
        assert!(
            !db.begin_reclaim(unleased, Some(Timestamp(9_000))).unwrap(),
            "no lease, nothing expired"
        );
        assert_eq!(db.workspace(ws).unwrap().state, WorkspaceState::Active);
        assert!(db.begin_reclaim(ws, Some(Timestamp(1_501))).unwrap());
        assert_eq!(db.workspace(ws).unwrap().state, WorkspaceState::Releasing);
        assert!(!db.begin_reclaim(ws, None).unwrap(), "only one caller wins");
    }

    #[test]
    fn touch_moves_expiry() {
        let (db, repo_id, ws, _) = fixture();
        let l = lease(repo_id, ws, "cccc000000000001");
        db.issue_lease(&l).unwrap();
        db.touch_lease(l.id, Timestamp(1_000), Timestamp(99_000))
            .unwrap();
        let touched = db.lease(l.id).unwrap();
        assert_eq!(touched.last_heartbeat_at, Timestamp(1_000));
        assert_eq!(touched.expires_at, Timestamp(99_000));
        let missing = db.touch_lease(
            "cccc00000000000f".parse().unwrap(),
            Timestamp(1),
            Timestamp(2),
        );
        assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn expired_query_includes_grace() {
        let (db, repo_id, one, two) = fixture();
        let mut early = lease(repo_id, one, "cccc000000000001");
        early.expires_at = Timestamp(1_000);
        early.grace = Duration::from_millis(500);
        let mut late = lease(repo_id, two, "cccc000000000002");
        late.expires_at = Timestamp(2_000);
        late.grace = Duration::from_millis(500);
        db.issue_lease(&early).unwrap();
        db.issue_lease(&late).unwrap();

        let expired = |cutoff| {
            db.leases_expired_before(Timestamp(cutoff))
                .unwrap()
                .into_iter()
                .map(|l| l.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(expired(1_200), [], "within grace");
        assert_eq!(expired(1_500), [], "grace ends at, not before, the cutoff");
        assert_eq!(expired(1_501), [early.id]);
        assert_eq!(expired(3_000), [early.id, late.id]);

        db.end_lease(early.id, Timestamp(1_600), LeaseEnd::Expired)
            .unwrap();
        assert_eq!(expired(3_000), [late.id], "ended leases are skipped");
    }

    #[test]
    fn workspace_and_first_lease_insert_together() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let mut ws = workspace(record.id, "bbbb000000000001", "/ws/1");
        ws.lease_id = Some("cccc00000000000f".parse().unwrap());
        let l = lease(record.id, ws.id, "cccc000000000001");
        db.insert_workspace_with_lease(&ws, &l).unwrap();
        assert_eq!(db.workspace(ws.id).unwrap().lease_id, Some(l.id));
        assert_eq!(db.active_lease(ws.id).unwrap(), Some(l.clone()));

        // The lease insert fails (its id is taken): no workspace row is left.
        let second = workspace(record.id, "bbbb000000000002", "/ws/2");
        let reused = lease(record.id, second.id, "cccc000000000001");
        let error = db
            .insert_workspace_with_lease(&second, &reused)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            db.workspace(second.id).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn expired_query_searches_the_grace_end_index() {
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .with(|conn| {
                let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {}", expired_sql()))?;
                stmt.query_map([0], |row| row.get(3))?.collect()
            })
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH leases USING INDEX leases_unended_grace_end")),
            "{plan:?}"
        );
    }

    #[test]
    fn unended_leases_are_unique_per_workspace_in_the_schema() {
        let (db, repo_id, ws, _) = fixture();
        db.issue_lease(&lease(repo_id, ws, "cccc000000000001"))
            .unwrap();
        let bypass = db.with(|conn| {
            conn.execute(
                "INSERT INTO leases(id, repo_id, workspace_id, owner, issued_at, expires_at,
                                    last_heartbeat_at, length_ms, grace_ms)
                 VALUES ('cccc000000000002', ?1, ?2, 'x', 0, 0, 0, 0, 0)",
                params![repo_id, ws],
            )
        });
        assert!(bypass.is_err(), "the unique partial index refuses it");
    }

    #[test]
    fn restart_grace_extends_only_earlier_unended_leases() {
        let (db, repo_id, one, two) = fixture();
        let mut soon = lease(repo_id, one, "cccc000000000001");
        soon.expires_at = Timestamp(1_000);
        let mut later = lease(repo_id, two, "cccc000000000002");
        later.expires_at = Timestamp(9_000);
        db.issue_lease(&soon).unwrap();
        db.issue_lease(&later).unwrap();

        assert_eq!(db.extend_all_active_leases_to(Timestamp(5_000)).unwrap(), 1);
        assert_eq!(db.lease(soon.id).unwrap().expires_at, Timestamp(5_000));
        assert_eq!(db.lease(later.id).unwrap().expires_at, Timestamp(9_000));

        db.end_lease(soon.id, Timestamp(5_500), LeaseEnd::Released)
            .unwrap();
        assert_eq!(
            db.extend_all_active_leases_to(Timestamp(10_000)).unwrap(),
            1
        );
        assert_eq!(db.lease(soon.id).unwrap().expires_at, Timestamp(5_000));
    }
}
