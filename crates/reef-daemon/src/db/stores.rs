//! Queries on `stores`, the shared dependency stores under `<state>/stores/`
//! (R1.20).
//!
//! The primary key includes `repo_id`, but a store is found by its name and
//! lockfile hash alone: stores are shared by every repository on the
//! daemon, and the daemon never inserts a second row for a key that has
//! one, so every query here goes by `(name, lockfile_hash)`.

use std::path::PathBuf;

use rusqlite::{ErrorCode, OptionalExtension as _, Row, params};

use super::{Db, DbError, path_text};
use crate::model::{StoreRecord, Timestamp};

const WHAT: &str = "store";

const COLUMNS: &str = "repo_id, name, lockfile_hash, path, built_at, sealed";

fn from_row(row: &Row<'_>) -> rusqlite::Result<StoreRecord> {
    Ok(StoreRecord {
        repo_id: row.get(0)?,
        name: row.get(1)?,
        lockfile_hash: row.get(2)?,
        path: PathBuf::from(row.get::<_, String>(3)?),
        built_at: row.get(4)?,
        sealed: row.get(5)?,
    })
}

impl Db {
    /// Records a store whose build is starting. [`DbError::AlreadyExists`]
    /// if any repository's row has the same name and lockfile hash.
    pub fn insert_store(&self, s: &StoreRecord) -> Result<(), DbError> {
        let path = path_text(&s.path)?;
        let inserted = self.in_transaction(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT repo_id FROM stores WHERE name = ?1 AND lockfile_hash = ?2",
                    params![s.name, s.lockfile_hash],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(DbError::AlreadyExists {
                    what: WHAT,
                    id: format!("{}/{}", s.name, s.lockfile_hash),
                });
            }
            tx.execute(
                "INSERT INTO stores(repo_id, name, lockfile_hash, path, built_at, sealed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    s.repo_id,
                    s.name,
                    s.lockfile_hash,
                    path,
                    s.built_at,
                    s.sealed
                ],
            )?;
            Ok(())
        });
        match inserted {
            Err(DbError::Sqlite(error))
                if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) =>
            {
                Err(DbError::AlreadyExists {
                    what: WHAT,
                    id: format!("{}/{}", s.name, s.lockfile_hash),
                })
            }
            other => other,
        }
    }

    /// The store with this name and lockfile hash, sealed or not.
    pub fn store(&self, name: &str, lockfile_hash: &str) -> Result<Option<StoreRecord>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM stores WHERE name = ?1 AND lockfile_hash = ?2
                     ORDER BY sealed DESC LIMIT 1"
                ),
                params![name, lockfile_hash],
                from_row,
            )
            .optional()
        })
    }

    /// Every store, by name and then lockfile hash.
    pub fn stores(&self) -> Result<Vec<StoreRecord>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM stores ORDER BY name, lockfile_hash, repo_id"
            ))?;
            stmt.query_map([], from_row)?.collect()
        })
    }

    /// Marks the store built and read-only at `built_at`.
    /// [`DbError::NotFound`] if there is no row.
    pub fn seal_store(
        &self,
        name: &str,
        lockfile_hash: &str,
        built_at: Timestamp,
    ) -> Result<(), DbError> {
        let changed = self.with(|conn| {
            conn.execute(
                "UPDATE stores SET sealed = 1, built_at = ?3 WHERE name = ?1 AND lockfile_hash = ?2",
                params![name, lockfile_hash, built_at],
            )
        })?;
        if changed == 0 {
            return Err(DbError::NotFound {
                what: WHAT,
                id: format!("{name}/{lockfile_hash}"),
            });
        }
        Ok(())
    }

    /// Deletes the store's row and returns it, or `None` if there was none.
    pub fn delete_store(
        &self,
        name: &str,
        lockfile_hash: &str,
    ) -> Result<Option<StoreRecord>, DbError> {
        self.in_transaction(|tx| {
            let record = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM stores WHERE name = ?1 AND lockfile_hash = ?2
                         ORDER BY sealed DESC LIMIT 1"
                    ),
                    params![name, lockfile_hash],
                    from_row,
                )
                .optional()?;
            tx.execute(
                "DELETE FROM stores WHERE name = ?1 AND lockfile_hash = ?2",
                params![name, lockfile_hash],
            )?;
            Ok(record)
        })
    }

    /// Deletes and returns every unsealed row: at daemon start, the builds
    /// a stop or crash interrupted, since no build outlives the daemon.
    pub fn delete_unsealed_stores(&self) -> Result<Vec<StoreRecord>, DbError> {
        self.in_transaction(|tx| {
            let records = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {COLUMNS} FROM stores WHERE sealed = 0 ORDER BY name, lockfile_hash"
                ))?;
                stmt.query_map([], from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            tx.execute("DELETE FROM stores WHERE sealed = 0", [])?;
            Ok(records)
        })
    }
}

#[cfg(test)]
mod tests {
    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;

    const HASH: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    fn store(repo: &str, name: &str, sealed: bool) -> StoreRecord {
        StoreRecord {
            repo_id: repo.parse().unwrap(),
            name: name.to_owned(),
            lockfile_hash: HASH.to_owned(),
            path: format!("/state/stores/{name}/{HASH}").into(),
            built_at: sealed.then_some(Timestamp(1_000)),
            sealed,
        }
    }

    #[test]
    fn insert_seal_list_delete() {
        let db = Db::open_in_memory().unwrap();
        let deps = store("aaaa000000000001", "deps", false);
        let tools = store("aaaa000000000001", "tools", true);
        db.insert_store(&tools).unwrap();
        db.insert_store(&deps).unwrap();

        assert_eq!(db.store("deps", HASH).unwrap(), Some(deps.clone()));
        assert_eq!(db.store("deps", "00").unwrap(), None);
        assert_eq!(db.stores().unwrap(), [deps.clone(), tools.clone()]);

        db.seal_store("deps", HASH, Timestamp(2_000)).unwrap();
        let sealed = db.store("deps", HASH).unwrap().unwrap();
        assert!(sealed.sealed);
        assert_eq!(sealed.built_at, Some(Timestamp(2_000)));
        let missing = db.seal_store("other", HASH, Timestamp(1)).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);

        assert_eq!(db.delete_store("deps", HASH).unwrap(), Some(sealed));
        assert_eq!(db.delete_store("deps", HASH).unwrap(), None);
        assert_eq!(db.stores().unwrap(), [tools]);
    }

    #[test]
    fn a_key_has_one_row_whichever_repository_builds_it() {
        let db = Db::open_in_memory().unwrap();
        db.insert_store(&store("aaaa000000000001", "deps", true))
            .unwrap();
        let error = db
            .insert_store(&store("aaaa000000000002", "deps", false))
            .unwrap_err();
        assert!(matches!(error, DbError::AlreadyExists { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }

    #[test]
    fn unsealed_rows_are_swept() {
        let db = Db::open_in_memory().unwrap();
        let building = store("aaaa000000000001", "deps", false);
        let built = store("aaaa000000000001", "tools", true);
        db.insert_store(&building).unwrap();
        db.insert_store(&built).unwrap();
        assert_eq!(db.delete_unsealed_stores().unwrap(), [building]);
        assert_eq!(db.stores().unwrap(), [built]);
    }
}
