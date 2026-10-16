//! Queries on `base_cache`, the trees materialized under each repository's
//! `bases/` directory (R1.02).

use std::path::PathBuf;

use rusqlite::types::Type;
use rusqlite::{ErrorCode, OptionalExtension as _, Row, params};

use super::{Db, DbError, path_text};
use crate::model::{BaseCacheRecord, RepoId, Timestamp};

const WHAT: &str = "base cache entry";

const COLUMNS: &str = "repo_id, tree_id, path, bytes, files, created_at, last_used_at";

/// A stored count below zero, which Reef never writes.
#[derive(Debug, thiserror::Error)]
#[error("negative count {0}")]
struct NegativeCount(i64);

fn count(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Integer,
            Box::new(NegativeCount(value)),
        )
    })
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<BaseCacheRecord> {
    Ok(BaseCacheRecord {
        repo_id: row.get(0)?,
        tree_id: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        bytes: count(row, 3)?,
        files: count(row, 4)?,
        created_at: row.get(5)?,
        last_used_at: row.get(6)?,
    })
}

/// A count as SQLite's signed integer; a count above `i64::MAX` is not a
/// real disk.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl Db {
    /// Records a materialized base. [`DbError::AlreadyExists`] if the
    /// repository already has an entry for the tree.
    pub fn insert_base(&self, b: &BaseCacheRecord) -> Result<(), DbError> {
        let path = path_text(&b.path)?;
        let inserted = self.with(|conn| {
            conn.execute(
                "INSERT INTO base_cache(repo_id, tree_id, path, bytes, files, created_at, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    b.repo_id,
                    b.tree_id,
                    path,
                    signed(b.bytes),
                    signed(b.files),
                    b.created_at,
                    b.last_used_at
                ],
            )
        });
        match inserted {
            Ok(_) => Ok(()),
            Err(DbError::Sqlite(error))
                if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) =>
            {
                Err(DbError::AlreadyExists {
                    what: WHAT,
                    id: format!("{}/{}", b.repo_id, b.tree_id),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// The repository's entry for `tree_id`, if any.
    pub fn base(&self, repo: RepoId, tree_id: &str) -> Result<Option<BaseCacheRecord>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM base_cache WHERE repo_id = ?1 AND tree_id = ?2"),
                params![repo, tree_id],
                from_row,
            )
            .optional()
        })
    }

    /// Every entry of the repository, least recently used first (the order
    /// R1.17 evicts in).
    pub fn bases(&self, repo: RepoId) -> Result<Vec<BaseCacheRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM base_cache WHERE repo_id = ?1
                 ORDER BY last_used_at, tree_id"
            ))?;
            stmt.query_map([repo], from_row)?.collect()
        })
    }

    /// Sets the entry's `last_used_at` to `at`. [`DbError::NotFound`] if
    /// there is no entry.
    pub fn touch_base(&self, repo: RepoId, tree_id: &str, at: Timestamp) -> Result<(), DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE base_cache SET last_used_at = ?3 WHERE repo_id = ?1 AND tree_id = ?2",
                params![repo, tree_id, at],
            )
        })?;
        if changed == 0 {
            return Err(DbError::NotFound {
                what: WHAT,
                id: format!("{repo}/{tree_id}"),
            });
        }
        Ok(())
    }

    /// Deletes the entry and returns it, or `None` if there was none.
    pub fn delete_base(
        &self,
        repo: RepoId,
        tree_id: &str,
    ) -> Result<Option<BaseCacheRecord>, DbError> {
        self.in_transaction(|tx| {
            let record = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM base_cache WHERE repo_id = ?1 AND tree_id = ?2"
                    ),
                    params![repo, tree_id],
                    from_row,
                )
                .optional()?;
            tx.execute(
                "DELETE FROM base_cache WHERE repo_id = ?1 AND tree_id = ?2",
                params![repo, tree_id],
            )?;
            Ok(record)
        })
    }
}

#[cfg(test)]
mod tests {
    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;

    fn base(repo: &str, tree_id: &str, last_used_at: i64) -> BaseCacheRecord {
        BaseCacheRecord {
            repo_id: repo.parse().unwrap(),
            tree_id: tree_id.to_owned(),
            path: format!("/state/repos/{repo}/bases/{tree_id}").into(),
            bytes: 1 << 20,
            files: 100,
            created_at: Timestamp(1_000),
            last_used_at: Timestamp(last_used_at),
        }
    }

    #[test]
    fn insert_get_list_touch_delete() {
        let db = Db::open_in_memory().unwrap();
        let old = base("aaaa000000000001", "0a0a", 10);
        let new = base("aaaa000000000001", "0b0b+0c0c+0d0d", 20);
        let other_repo = base("aaaa000000000002", "0a0a", 5);
        for record in [&new, &old, &other_repo] {
            db.insert_base(record).unwrap();
        }
        let repo = old.repo_id;

        assert_eq!(db.base(repo, "0a0a").unwrap(), Some(old.clone()));
        assert_eq!(db.base(repo, "ffff").unwrap(), None);
        assert_eq!(db.bases(repo).unwrap(), [old.clone(), new.clone()]);

        db.touch_base(repo, "0a0a", Timestamp(30)).unwrap();
        let listed = db.bases(repo).unwrap();
        assert_eq!(listed[0].tree_id, new.tree_id, "least recently used first");
        assert_eq!(listed[1].last_used_at, Timestamp(30));
        let missing = db.touch_base(repo, "ffff", Timestamp(40)).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);

        let deleted = db.delete_base(repo, &new.tree_id).unwrap();
        assert_eq!(deleted, Some(new.clone()));
        assert_eq!(db.delete_base(repo, &new.tree_id).unwrap(), None);
        assert_eq!(db.bases(repo).unwrap().len(), 1);
        assert_eq!(db.bases(other_repo.repo_id).unwrap(), [other_repo]);
    }

    #[test]
    fn duplicate_tree_already_exists() {
        let db = Db::open_in_memory().unwrap();
        let record = base("aaaa000000000001", "0a0a", 10);
        db.insert_base(&record).unwrap();
        let error = db.insert_base(&record).unwrap_err();
        assert!(matches!(error, DbError::AlreadyExists { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }
}
