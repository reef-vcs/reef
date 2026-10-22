//! Queries on `workspaces`.

use std::path::PathBuf;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension as _, Row, Transaction, params};

use super::leases::issue_lease_in;
use super::reclaims::insert_reclaim_in;
use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, normalize_prefix, path_text, unique_match};
use crate::model::{
    Attention, LeaseEnd, LeaseRecord, ReclaimRecord, RepoId, Timestamp, WorkspaceId,
    WorkspaceRecord, WorkspaceState,
};

const WHAT: &str = "workspace";

const COLUMNS: &str = "id, repo_id, owner, purpose, change_id, commit_id, base_tree_id, backend, \
     path, state_dir, lease_id, state, divergent, attention, created_at, last_snapshot_at, \
     last_snapshot_op, watcher";

fn from_row(row: &Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        owner: row.get(2)?,
        purpose: row.get(3)?,
        change_id: row.get(4)?,
        commit_id: row.get(5)?,
        base_tree_id: row.get(6)?,
        backend: row.get(7)?,
        path: PathBuf::from(row.get::<_, String>(8)?),
        state_dir: PathBuf::from(row.get::<_, String>(9)?),
        lease_id: row.get(10)?,
        state: row.get(11)?,
        divergent: row.get(12)?,
        attention: row.get(13)?,
        created_at: row.get(14)?,
        last_snapshot_at: row.get(15)?,
        last_snapshot_op: row.get(16)?,
        watcher: row.get(17)?,
    })
}

fn not_found(id: WorkspaceId) -> DbError {
    DbError::NotFound {
        what: WHAT,
        id: id.as_hex(),
    }
}

/// Fails with [`DbError::NotFound`] if `changed`, the row count of an update
/// by id, is zero.
fn updated(id: WorkspaceId, changed: usize) -> Result<(), DbError> {
    if changed == 0 {
        Err(not_found(id))
    } else {
        Ok(())
    }
}

fn exists(conn: &Connection, sql: &str, value: &str) -> rusqlite::Result<bool> {
    conn.query_row(sql, [value], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
}

/// [`Db::insert_workspace`] inside the caller's transaction.
pub(crate) fn insert_workspace_in(
    tx: &Transaction<'_>,
    w: &WorkspaceRecord,
) -> Result<(), DbError> {
    let path = path_text(&w.path)?;
    let state_dir = path_text(&w.state_dir)?;
    if !exists(
        tx,
        "SELECT 1 FROM repositories WHERE id = ?1",
        &w.repo_id.as_hex(),
    )? {
        return Err(DbError::NotFound {
            what: "repository",
            id: w.repo_id.as_hex(),
        });
    }
    if exists(tx, "SELECT 1 FROM workspaces WHERE id = ?1", &w.id.as_hex())? {
        return Err(DbError::AlreadyExists {
            what: WHAT,
            id: w.id.as_hex(),
        });
    }
    if exists(tx, "SELECT 1 FROM workspaces WHERE path = ?1", path)? {
        return Err(DbError::AlreadyExists {
            what: "workspace path",
            id: path.to_owned(),
        });
    }
    tx.execute(
        &format!(
            "INSERT INTO workspaces({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18)"
        ),
        params![
            w.id,
            w.repo_id,
            w.owner,
            w.purpose,
            w.change_id,
            w.commit_id,
            w.base_tree_id,
            w.backend,
            path,
            state_dir,
            w.lease_id,
            w.state,
            w.divergent,
            w.attention,
            w.created_at,
            w.last_snapshot_at,
            w.last_snapshot_op,
            w.watcher,
        ],
    )?;
    Ok(())
}

impl Db {
    /// Records a new workspace. Its repository must exist
    /// ([`DbError::NotFound`]); its id and path must be unused
    /// ([`DbError::AlreadyExists`]). A workspace is normally inserted with its
    /// first lease by [`Db::insert_workspace_with_lease`].
    pub fn insert_workspace(&self, w: &WorkspaceRecord) -> Result<(), DbError> {
        self.in_transaction(|tx| insert_workspace_in(tx, w))
    }

    /// Records a new workspace and issues its first lease in one
    /// transaction: the row is inserted with `lease_id` NULL (whatever
    /// `w.lease_id` says), then the lease, then the pointer, so the
    /// `workspaces.lease_id ↔ leases.workspace_id` cycle never fails a
    /// foreign key. If any step fails, neither row is written.
    pub fn insert_workspace_with_lease(
        &self,
        w: &WorkspaceRecord,
        l: &LeaseRecord,
    ) -> Result<(), DbError> {
        let unleased = WorkspaceRecord {
            lease_id: None,
            ..w.clone()
        };
        self.in_transaction(|tx| {
            insert_workspace_in(tx, &unleased)?;
            issue_lease_in(tx, l)
        })
    }

    /// The workspace with this id, in any state.
    pub fn workspace(&self, id: WorkspaceId) -> Result<WorkspaceRecord, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM workspaces WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()
        })?
        .ok_or_else(|| not_found(id))
    }

    /// The one workspace whose id starts with `prefix` (at least four hex
    /// characters, either case), in `repo` if given, in any state.
    pub fn workspace_by_prefix(
        &self,
        repo: Option<RepoId>,
        prefix: &str,
    ) -> Result<WorkspaceRecord, DbError> {
        let pattern = format!("{}*", normalize_prefix(WHAT, prefix)?);
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM workspaces
                 WHERE id GLOB ?1 AND (?2 IS NULL OR repo_id = ?2)
                 ORDER BY id LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map(params![pattern, repo], from_row)?.collect()
        })?;
        unique_match(WHAT, prefix, matches, |record| record.id.as_hex())
    }

    /// The workspace recorded at `path`, in any state: `workspaces.path` is
    /// unique, and rows outlive their directories until R1.17 deletes them.
    /// `path` is compared as given, so the caller canonicalizes it.
    pub fn workspace_at_path(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<WorkspaceRecord>, DbError> {
        let path = path_text(path)?;
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM workspaces WHERE path = ?1"),
                [path],
                from_row,
            )
            .optional()
        })
    }

    /// The active workspace whose path is `path` or an ancestor of it, the
    /// deepest if several are (a workspace made with `--path` inside
    /// another). `path` is compared as given, so the caller canonicalizes
    /// it.
    pub fn active_workspace_containing(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<WorkspaceRecord>, DbError> {
        let path = path_text(path)?;
        self.with(|conn| {
            // A prefix test by `substr`, not `LIKE`: paths may hold `%` and
            // `_`.
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM workspaces
                     WHERE state = 'active'
                       AND (path = ?1 OR substr(?1, 1, length(path) + 1) = path || '/')
                     ORDER BY length(path) DESC LIMIT 1"
                ),
                [path],
                from_row,
            )
            .optional()
        })
    }

    /// Workspaces in `repo` (every repository if `None`) whose state is one
    /// of `states` (any state if empty), oldest first.
    pub fn workspaces(
        &self,
        repo: Option<RepoId>,
        states: &[WorkspaceState],
    ) -> Result<Vec<WorkspaceRecord>, DbError> {
        let states: Vec<Value> = states
            .iter()
            .map(|state| Value::from(state.as_str().to_owned()))
            .collect();
        self.with(|conn| {
            let placeholders = vec!["?"; states.len()].join(", ");
            let state_filter = if states.is_empty() {
                String::new()
            } else {
                format!("AND state IN ({placeholders})")
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM workspaces
                 WHERE (?1 IS NULL OR repo_id = ?1) {state_filter}
                 ORDER BY created_at, id"
            ))?;
            let repo = repo.map(|id| Value::from(id.as_hex()));
            let args = std::iter::once(repo.unwrap_or(Value::Null)).chain(states.iter().cloned());
            stmt.query_map(rusqlite::params_from_iter(args), from_row)?
                .collect()
        })
    }

    /// Moves a workspace to `state`. [`DbError::Conflict`] if the lifecycle
    /// does not allow the move (see [`WorkspaceState::can_become`]).
    pub fn set_workspace_state(
        &self,
        id: WorkspaceId,
        state: WorkspaceState,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let current: WorkspaceState = tx
                .query_row("SELECT state FROM workspaces WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| not_found(id))?;
            if !current.can_become(state) {
                return Err(DbError::Conflict {
                    what: WHAT,
                    id: id.as_hex(),
                    reason: match current {
                        WorkspaceState::Released | WorkspaceState::Reclaimed => "has ended",
                        WorkspaceState::Active | WorkspaceState::Releasing => {
                            "is not being released"
                        }
                    },
                });
            }
            tx.execute(
                "UPDATE workspaces SET state = ?2 WHERE id = ?1",
                params![id, state],
            )?;
            Ok(())
        })
    }

    /// Starts a release or reclaim: `active` → `releasing`, only if the
    /// workspace is `active` and, with `expired_by`, only if its current
    /// lease is unended and past its grace at that time (`expires_at +
    /// grace_ms < expired_by`), all in one statement so a heartbeat that
    /// renewed the lease since the expiry tick read it wins. Returns whether
    /// the workspace moved; when two callers race, exactly one does.
    pub fn begin_reclaim(
        &self,
        id: WorkspaceId,
        expired_by: Option<Timestamp>,
    ) -> Result<bool, DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE workspaces SET state = 'releasing'
                 WHERE id = ?1 AND state = 'active'
                   AND (?2 IS NULL OR EXISTS (
                        SELECT 1 FROM leases
                        WHERE leases.id = workspaces.lease_id AND leases.ended_at IS NULL
                          AND leases.expires_at + leases.grace_ms < ?2))",
                params![id, expired_by],
            )
        })?;
        Ok(changed == 1)
    }

    /// Ends a workspace's life in one transaction: moves it to `state`
    /// (`released` or `reclaimed`, clearing `attention`), ends its unended
    /// lease (if any) at
    /// `reclaim.at` for `lease_end`, and appends `reclaim` to the reclaim
    /// log. [`DbError::Conflict`] if the lifecycle does not allow the move;
    /// then nothing is written.
    pub fn finish_workspace(
        &self,
        id: WorkspaceId,
        state: WorkspaceState,
        lease_end: LeaseEnd,
        reclaim: &ReclaimRecord,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let current: WorkspaceState = tx
                .query_row("SELECT state FROM workspaces WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| not_found(id))?;
            if !state.is_terminal() || !current.can_become(state) || current == state {
                return Err(DbError::Conflict {
                    what: WHAT,
                    id: id.as_hex(),
                    reason: "cannot end in that state",
                });
            }
            tx.execute(
                "UPDATE leases SET ended_at = ?2, end_reason = ?3
                 WHERE workspace_id = ?1 AND ended_at IS NULL",
                params![id, reclaim.at, lease_end],
            )?;
            // Whatever needed attention (a failed final snapshot) is moot
            // once the directory is gone.
            tx.execute(
                "UPDATE workspaces SET state = ?2, lease_id = NULL, attention = NULL WHERE id = ?1",
                params![id, state],
            )?;
            insert_reclaim_in(tx, reclaim)?;
            Ok(())
        })
    }

    /// Records the change and commit a workspace is on, and the tree its
    /// directory was built from if `base_tree_id` is given (a move swaps the
    /// base; a snapshot does not).
    pub fn set_workspace_commit(
        &self,
        id: WorkspaceId,
        change_id: &str,
        commit_id: &str,
        base_tree_id: Option<&str>,
    ) -> Result<(), DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE workspaces
                 SET change_id = ?2, commit_id = ?3, base_tree_id = coalesce(?4, base_tree_id)
                 WHERE id = ?1",
                params![id, change_id, commit_id, base_tree_id],
            )
        })?;
        updated(id, changed)
    }

    /// Records the operation and time of a workspace's latest snapshot.
    pub fn record_snapshot(
        &self,
        id: WorkspaceId,
        op_id: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE workspaces SET last_snapshot_op = ?2, last_snapshot_at = ?3 WHERE id = ?1",
                params![id, op_id, at],
            )
        })?;
        updated(id, changed)
    }

    /// Sets (or with `None` clears) why a workspace needs attention, and its
    /// divergence flag if `divergent` is given.
    pub fn set_workspace_attention(
        &self,
        id: WorkspaceId,
        attention: Option<Attention>,
        divergent: Option<bool>,
    ) -> Result<(), DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE workspaces SET attention = ?2, divergent = coalesce(?3, divergent)
                 WHERE id = ?1",
                params![id, attention, divergent],
            )
        })?;
        updated(id, changed)
    }

    /// GC's candidates (R1.17): `released` and `reclaimed` workspaces of
    /// `repo` whose latest reclaim log entry is before `before`, oldest
    /// first. A workspace with no log entry is not one.
    pub fn ended_workspaces_before(
        &self,
        repo: RepoId,
        before: Timestamp,
    ) -> Result<Vec<WorkspaceRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM workspaces
                 WHERE repo_id = ?1 AND state IN ('released', 'reclaimed')
                   AND (SELECT max(r.at) FROM reclaims r WHERE r.workspace_id = workspaces.id) < ?2
                 ORDER BY created_at, id"
            ))?;
            stmt.query_map(params![repo, before], from_row)?.collect()
        })
    }

    /// Deletes an ended workspace's row and its leases in one transaction
    /// (R1.17); its `reclaims` entries stay, as the audit log.
    /// [`DbError::Conflict`] if it is not `released` or `reclaimed`.
    pub fn delete_ended_workspace(&self, id: WorkspaceId) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let state: WorkspaceState = tx
                .query_row("SELECT state FROM workspaces WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| not_found(id))?;
            if !state.is_terminal() {
                return Err(DbError::Conflict {
                    what: WHAT,
                    id: id.as_hex(),
                    reason: "has not ended",
                });
            }
            tx.execute("UPDATE workspaces SET lease_id = NULL WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM leases WHERE workspace_id = ?1", [id])?;
            tx.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::db::repositories::tests::repo;
    use crate::model::WatcherKind;

    /// An active, unleased workspace record.
    pub(crate) fn workspace(repo_id: RepoId, id: &str, path: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            id: id.parse().unwrap(),
            repo_id,
            owner: "alice@host".into(),
            purpose: String::new(),
            change_id: "0123456789abcdef0123456789abcdef".into(),
            commit_id: "aa".repeat(20),
            base_tree_id: "bb".repeat(20),
            backend: "copy".into(),
            path: path.into(),
            state_dir: Path::new("/state/wc").join(id),
            lease_id: None,
            state: WorkspaceState::Active,
            divergent: false,
            attention: None,
            created_at: Timestamp(2_000),
            last_snapshot_at: None,
            last_snapshot_op: None,
            watcher: WatcherKind::Notify,
        }
    }

    fn fixture() -> (Db, RepoId) {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        (db, record.id)
    }

    #[test]
    fn insert_and_fetch() {
        let (db, repo_id) = fixture();
        let mut ws = workspace(repo_id, "bbbb000000000001", "/state/ws/one");
        ws.purpose = "fix the build".into();
        ws.attention = Some(Attention::StoreMissing);
        ws.last_snapshot_at = Some(Timestamp(3_000));
        ws.last_snapshot_op = Some("cc".repeat(64));
        db.insert_workspace(&ws).unwrap();
        assert_eq!(db.workspace(ws.id).unwrap(), ws);
        assert_eq!(
            db.workspace("bbbb000000000002".parse().unwrap())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn insert_checks_repository_id_and_path() {
        let (db, repo_id) = fixture();
        db.insert_workspace(&workspace(repo_id, "bbbb000000000001", "/state/ws/one"))
            .unwrap();
        let same_id = workspace(repo_id, "bbbb000000000001", "/state/ws/two");
        assert_eq!(
            db.insert_workspace(&same_id).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        let same_path = workspace(repo_id, "bbbb000000000002", "/state/ws/one");
        assert_eq!(
            db.insert_workspace(&same_path).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        let no_repo = workspace(
            "ffff000000000001".parse().unwrap(),
            "bbbb000000000003",
            "/x",
        );
        assert_eq!(
            db.insert_workspace(&no_repo).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn prefix_lookup_scoped_by_repository() {
        let (db, repo_id) = fixture();
        let other = repo("aaaa000000000002", "/src/other");
        db.insert_repository(&other).unwrap();
        db.insert_workspace(&workspace(repo_id, "cafe000000000001", "/ws/1"))
            .unwrap();
        db.insert_workspace(&workspace(other.id, "cafe000000000002", "/ws/2"))
            .unwrap();

        let ambiguous = db.workspace_by_prefix(None, "cafe").unwrap_err();
        assert!(
            matches!(&ambiguous, DbError::Ambiguous { candidates, .. }
                if candidates == &["cafe000000000001", "cafe000000000002"]),
            "{ambiguous:?}"
        );
        assert_eq!(
            ambiguous.to_string(),
            "workspace \"cafe\" is ambiguous; it matches cafe000000000001, cafe000000000002"
        );
        assert_eq!(
            db.workspace_by_prefix(None, "cafe").unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
        let in_other = db.workspace_by_prefix(Some(other.id), "CAFE").unwrap();
        assert_eq!(in_other.path, Path::new("/ws/2"));
        assert_eq!(
            db.workspace_by_prefix(None, "caf").unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
        assert_eq!(
            db.workspace_by_prefix(Some(repo_id), "cafe000000000002")
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn state_transitions_keep_the_row() {
        let (db, repo_id) = fixture();
        let ws = workspace(repo_id, "bbbb000000000001", "/ws/1");
        db.insert_workspace(&ws).unwrap();

        let released_early = db.set_workspace_state(ws.id, WorkspaceState::Released);
        assert_eq!(released_early.unwrap_err().kind(), ErrorKind::Conflict);
        db.set_workspace_state(ws.id, WorkspaceState::Releasing)
            .unwrap();
        db.set_workspace_state(ws.id, WorkspaceState::Released)
            .unwrap();
        assert_eq!(db.workspace(ws.id).unwrap().state, WorkspaceState::Released);
        let revived = db.set_workspace_state(ws.id, WorkspaceState::Active);
        assert_eq!(revived.unwrap_err().kind(), ErrorKind::Conflict);
        let missing = db.set_workspace_state(
            "bbbb000000000002".parse().unwrap(),
            WorkspaceState::Releasing,
        );
        assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn list_filters_by_repository_and_state() {
        let (db, repo_id) = fixture();
        let other = repo("aaaa000000000002", "/src/other");
        db.insert_repository(&other).unwrap();
        let mut first = workspace(repo_id, "bbbb000000000001", "/ws/1");
        first.created_at = Timestamp(1);
        let mut second = workspace(repo_id, "bbbb000000000002", "/ws/2");
        second.created_at = Timestamp(2);
        let third = workspace(other.id, "bbbb000000000003", "/ws/3");
        for ws in [&second, &first, &third] {
            db.insert_workspace(ws).unwrap();
        }
        db.set_workspace_state(second.id, WorkspaceState::Reclaimed)
            .unwrap();

        let ids = |list: Vec<WorkspaceRecord>| list.into_iter().map(|w| w.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.workspaces(Some(repo_id), &[]).unwrap()),
            [first.id, second.id]
        );
        assert_eq!(
            ids(db
                .workspaces(Some(repo_id), &[WorkspaceState::Active])
                .unwrap()),
            [first.id]
        );
        assert_eq!(
            ids(db
                .workspaces(None, &[WorkspaceState::Active, WorkspaceState::Reclaimed])
                .unwrap()),
            [first.id, second.id, third.id]
        );
        assert_eq!(
            db.workspaces(None, &[WorkspaceState::Releasing]).unwrap(),
            []
        );
    }

    #[test]
    fn commit_snapshot_and_attention_updates() {
        let (db, repo_id) = fixture();
        let ws = workspace(repo_id, "bbbb000000000001", "/ws/1");
        db.insert_workspace(&ws).unwrap();

        db.set_workspace_commit(ws.id, "change2", "commit2", None)
            .unwrap();
        let after = db.workspace(ws.id).unwrap();
        assert_eq!(
            (after.change_id.as_str(), after.commit_id.as_str()),
            ("change2", "commit2")
        );
        assert_eq!(after.base_tree_id, ws.base_tree_id, "kept when not given");
        db.set_workspace_commit(ws.id, "change3", "commit3", Some("tree3"))
            .unwrap();
        assert_eq!(db.workspace(ws.id).unwrap().base_tree_id, "tree3");

        db.record_snapshot(ws.id, "op1", Timestamp(5_000)).unwrap();
        let after = db.workspace(ws.id).unwrap();
        assert_eq!(after.last_snapshot_op.as_deref(), Some("op1"));
        assert_eq!(after.last_snapshot_at, Some(Timestamp(5_000)));

        db.set_workspace_attention(ws.id, Some(Attention::Divergent), Some(true))
            .unwrap();
        let after = db.workspace(ws.id).unwrap();
        assert_eq!(
            (after.attention, after.divergent),
            (Some(Attention::Divergent), true)
        );
        db.set_workspace_attention(ws.id, None, None).unwrap();
        let after = db.workspace(ws.id).unwrap();
        assert_eq!(
            (after.attention, after.divergent),
            (None, true),
            "divergent kept"
        );

        let missing: WorkspaceId = "bbbb000000000002".parse().unwrap();
        assert_eq!(
            db.record_snapshot(missing, "op", Timestamp(1))
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            db.set_workspace_commit(missing, "c", "c", None)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            db.set_workspace_attention(missing, None, None)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn ended_workspaces_are_listed_by_last_reclaim_and_deleted_with_leases() {
        use std::time::Duration;

        use crate::model::{LeaseRecord, ReclaimReason};

        let (db, repo_id) = fixture();
        let ended = |id: &str, path: &str, at: i64| {
            let ws = workspace(repo_id, id, path);
            let lease = LeaseRecord {
                id: format!("cccc{}", &id[4..]).parse().unwrap(),
                repo_id,
                workspace_id: ws.id,
                owner: ws.owner.clone(),
                issued_at: Timestamp(0),
                expires_at: Timestamp(60_000),
                last_heartbeat_at: Timestamp(0),
                length: Duration::from_mins(1),
                grace: Duration::from_mins(1),
                ended_at: None,
                end_reason: None,
            };
            db.insert_workspace_with_lease(&ws, &lease).unwrap();
            let reclaim = ReclaimRecord {
                id: 0,
                repo_id,
                workspace_id: ws.id,
                reason: ReclaimReason::Released,
                final_op_id: None,
                snapshot_taken: false,
                freed_bytes: None,
                at: Timestamp(at),
                detail: None,
            };
            db.set_workspace_state(ws.id, WorkspaceState::Releasing)
                .unwrap();
            db.finish_workspace(
                ws.id,
                WorkspaceState::Released,
                LeaseEnd::Released,
                &reclaim,
            )
            .unwrap();
            ws.id
        };
        let old = ended("bbbb000000000001", "/ws/1", 100);
        let recent = ended("bbbb000000000002", "/ws/2", 900);
        let active = workspace(repo_id, "bbbb000000000003", "/ws/3");
        db.insert_workspace(&active).unwrap();

        let ids = |list: Vec<WorkspaceRecord>| list.into_iter().map(|w| w.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.ended_workspaces_before(repo_id, Timestamp(500)).unwrap()),
            [old]
        );
        assert_eq!(
            ids(db
                .ended_workspaces_before(repo_id, Timestamp(1_000))
                .unwrap()),
            [old, recent]
        );

        db.delete_ended_workspace(old).unwrap();
        assert_eq!(db.workspace(old).unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(db.last_lease(old).unwrap(), None, "its leases went too");
        assert_eq!(
            db.reclaims(Some(repo_id), 10).unwrap().len(),
            2,
            "the log stays"
        );
        assert_eq!(
            db.delete_ended_workspace(active.id).unwrap_err().kind(),
            ErrorKind::Conflict
        );
        assert_eq!(
            db.delete_ended_workspace(old).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn unknown_stored_state_is_an_error_not_a_panic() {
        let (db, repo_id) = fixture();
        let ws = workspace(repo_id, "bbbb000000000001", "/ws/1");
        db.insert_workspace(&ws).unwrap();
        db.with(|conn| conn.execute("UPDATE workspaces SET attention = 'bogus'", []))
            .unwrap();
        assert_eq!(db.workspace(ws.id).unwrap_err().kind(), ErrorKind::Internal);
    }
}
