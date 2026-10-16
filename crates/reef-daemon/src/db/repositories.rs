//! Queries on `repositories`.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension as _, Row, params};

use super::{Db, DbError, normalize_prefix, path_text, unique_match};
use crate::model::{RepoId, RepositoryRecord};

const WHAT: &str = "repository";

const COLUMNS: &str = "id, root, jj_repo_path, colocated, registered_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<RepositoryRecord> {
    Ok(RepositoryRecord {
        id: row.get(0)?,
        root: PathBuf::from(row.get::<_, String>(1)?),
        jj_repo_path: PathBuf::from(row.get::<_, String>(2)?),
        colocated: row.get(3)?,
        registered_at: row.get(4)?,
    })
}

impl Db {
    /// Registers a repository. [`DbError::AlreadyExists`] if its root or id
    /// is already registered.
    pub fn insert_repository(&self, r: &RepositoryRecord) -> Result<(), DbError> {
        let root = path_text(&r.root)?;
        let jj_repo_path = path_text(&r.jj_repo_path)?;
        self.in_transaction(|tx| {
            let taken = |sql: &str, value: &str| {
                tx.query_row(sql, [value], |_| Ok(()))
                    .optional()
                    .map(|found| found.is_some())
            };
            if taken("SELECT 1 FROM repositories WHERE root = ?1", root)? {
                return Err(DbError::AlreadyExists {
                    what: WHAT,
                    id: root.to_owned(),
                });
            }
            if taken("SELECT 1 FROM repositories WHERE id = ?1", &r.id.as_hex())? {
                return Err(DbError::AlreadyExists {
                    what: WHAT,
                    id: r.id.as_hex(),
                });
            }
            tx.execute(
                "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![r.id, root, jj_repo_path, r.colocated, r.registered_at],
            )?;
            Ok(())
        })
    }

    /// The repository with this id.
    pub fn repository(&self, id: RepoId) -> Result<RepositoryRecord, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM repositories WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()
        })?
        .ok_or_else(|| DbError::NotFound {
            what: WHAT,
            id: id.as_hex(),
        })
    }

    /// The one repository whose id starts with `prefix` (at least four hex
    /// characters, either case).
    pub fn repository_by_prefix(&self, prefix: &str) -> Result<RepositoryRecord, DbError> {
        let pattern = format!("{}*", normalize_prefix(WHAT, prefix)?);
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM repositories WHERE id GLOB ?1 LIMIT 2"
            ))?;
            stmt.query_map([pattern], from_row)?.collect()
        })?;
        unique_match(WHAT, prefix, matches)
    }

    /// The registered repository whose root contains `path`, comparing whole
    /// path components; the longest root wins when roots nest. `path` is
    /// compared as given, so the caller canonicalizes it.
    pub fn repository_containing(&self, path: &Path) -> Result<Option<RepositoryRecord>, DbError> {
        Ok(self
            .repositories()?
            .into_iter()
            .filter(|repo| path.starts_with(&repo.root))
            .max_by_key(|repo| repo.root.components().count()))
    }

    /// Every registered repository, oldest registration first.
    pub fn repositories(&self) -> Result<Vec<RepositoryRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM repositories ORDER BY registered_at, id"
            ))?;
            stmt.query_map([], from_row)?.collect()
        })
    }

    /// Forgets a repository and every row that names it: its workspaces,
    /// leases, and reclaim log. [`DbError::Conflict`] while any of its
    /// workspaces is `active` or `releasing`, since those still have a
    /// directory on disk.
    pub fn delete_repository(&self, id: RepoId) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let exists = tx
                .query_row("SELECT 1 FROM repositories WHERE id = ?1", [id], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Err(DbError::NotFound {
                    what: WHAT,
                    id: id.as_hex(),
                });
            }
            let live: i64 = tx.query_row(
                "SELECT count(*) FROM workspaces
                 WHERE repo_id = ?1 AND state IN ('active', 'releasing')",
                [id],
                |row| row.get(0),
            )?;
            if live > 0 {
                return Err(DbError::Conflict {
                    what: WHAT,
                    id: id.as_hex(),
                    reason: "has active workspaces",
                });
            }
            // Leases and workspaces reference each other; break the cycle first.
            tx.execute(
                "UPDATE workspaces SET lease_id = NULL WHERE repo_id = ?1",
                [id],
            )?;
            tx.execute("DELETE FROM leases WHERE repo_id = ?1", [id])?;
            tx.execute("DELETE FROM workspaces WHERE repo_id = ?1", [id])?;
            tx.execute("DELETE FROM reclaims WHERE repo_id = ?1", [id])?;
            tx.execute("DELETE FROM repositories WHERE id = ?1", [id])?;
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::model::Timestamp;

    /// A repository record with this id and root.
    pub(crate) fn repo(id: &str, root: &str) -> RepositoryRecord {
        RepositoryRecord {
            id: id.parse().unwrap(),
            root: root.into(),
            jj_repo_path: Path::new(root).join(".jj/repo"),
            colocated: false,
            registered_at: Timestamp(1_000),
        }
    }

    #[test]
    fn insert_and_fetch() {
        let db = Db::open_in_memory().unwrap();
        let mut record = repo("aaaa000000000001", "/src/app");
        record.colocated = true;
        db.insert_repository(&record).unwrap();
        assert_eq!(db.repository(record.id).unwrap(), record);
        assert_eq!(db.repositories().unwrap(), vec![record.clone()]);

        let missing = db.repository("aaaa000000000002".parse().unwrap());
        assert_eq!(missing.unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn root_and_id_collisions_already_exist() {
        let db = Db::open_in_memory().unwrap();
        db.insert_repository(&repo("aaaa000000000001", "/src/app"))
            .unwrap();
        let same_root = db.insert_repository(&repo("aaaa000000000002", "/src/app"));
        assert_eq!(same_root.unwrap_err().kind(), ErrorKind::AlreadyExists);
        let same_id = db.insert_repository(&repo("aaaa000000000001", "/src/other"));
        assert_eq!(same_id.unwrap_err().kind(), ErrorKind::AlreadyExists);
    }

    #[test]
    fn prefix_lookup() {
        let db = Db::open_in_memory().unwrap();
        db.insert_repository(&repo("abcd000000000001", "/src/one"))
            .unwrap();
        db.insert_repository(&repo("abcd000000000002", "/src/two"))
            .unwrap();
        db.insert_repository(&repo("ef01000000000003", "/src/three"))
            .unwrap();

        assert_eq!(
            db.repository_by_prefix("EF01").unwrap().root,
            Path::new("/src/three")
        );
        assert_eq!(
            db.repository_by_prefix("abcd0000000000")
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidArgument,
            "ambiguous"
        );
        assert!(matches!(
            db.repository_by_prefix("abcd").unwrap_err(),
            DbError::Ambiguous { .. }
        ));
        assert_eq!(
            db.repository_by_prefix("abcd000000000002").unwrap().root,
            Path::new("/src/two")
        );
        assert_eq!(
            db.repository_by_prefix("abc").unwrap_err().kind(),
            ErrorKind::InvalidArgument,
            "too short"
        );
        assert_eq!(
            db.repository_by_prefix("9999").unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn containing_picks_the_longest_root() {
        let db = Db::open_in_memory().unwrap();
        db.insert_repository(&repo("aaaa000000000001", "/src/app"))
            .unwrap();
        db.insert_repository(&repo("aaaa000000000002", "/src/app/vendor/lib"))
            .unwrap();
        let containing = |path: &str| {
            db.repository_containing(Path::new(path))
                .unwrap()
                .map(|repo| repo.id.as_hex())
        };
        assert_eq!(
            containing("/src/app/src/main.rs").unwrap(),
            "aaaa000000000001"
        );
        assert_eq!(containing("/src/app").unwrap(), "aaaa000000000001");
        assert_eq!(
            containing("/src/app/vendor/lib/x").unwrap(),
            "aaaa000000000002"
        );
        assert_eq!(
            containing("/src/application"),
            None,
            "whole components only"
        );
        assert_eq!(containing("/elsewhere"), None);
    }

    #[test]
    fn delete_refuses_while_a_workspace_is_live() {
        use crate::db::workspaces::tests::workspace;
        use crate::model::WorkspaceState;

        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let ws = workspace(record.id, "bbbb000000000001", "/state/ws/one");
        db.insert_workspace(&ws).unwrap();

        let error = db.delete_repository(record.id).unwrap_err();
        assert!(matches!(error, DbError::Conflict { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Conflict);

        db.set_workspace_state(ws.id, WorkspaceState::Reclaimed)
            .unwrap();
        db.delete_repository(record.id).unwrap();
        assert_eq!(
            db.repository(record.id).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(db.workspace(ws.id).unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(
            db.delete_repository(record.id).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }
}
