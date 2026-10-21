//! The daemon's metadata database (ADR 0003): registered repositories,
//! workspaces, leases, the reclaim log, the base cache, and shared stores,
//! in one SQLite file per machine.
//!
//! [`Db`] owns one connection behind a [`std::sync::Mutex`]; every method
//! takes the lock for one short statement or transaction, so `&Db` can be
//! shared across threads. The daemon is SQLite's only writer. The methods
//! are synchronous and none is slow; a caller on a tokio worker may call
//! them directly.
//!
//! The schema is a list of numbered migrations. `PRAGMA user_version`
//! counts the ones applied, and `meta` records which. A database with
//! migrations this binary does not know is refused, never downgraded. Records are defined in [`crate::model`]; the queries are in
//! this module's submodules, one per table.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rand::RngExt as _;
use reef_lib::error::{Classify, ErrorKind};
use rusqlite::{Connection, ErrorCode, OptionalExtension as _, Transaction, TransactionBehavior};

use crate::model::{Timestamp, ids::ID_HEX_LEN};

mod base_cache;
mod leases;
mod reclaims;
mod repositories;
mod stores;
mod workspaces;

pub use leases::Renewal;

/// The migrations, in number order. A migration's number is fixed by the
/// Phase 1 plan, not by landing order: `0002_base_cache` (R1.02),
/// `0003_snapshots` (R1.16), `0004_stores` (R1.20), `0005_watchman` (R1.14).
/// They are independent of each other, so a database may have a later one
/// without an earlier one; see [`migrate`].
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_initial", include_str!("migrations/0001_initial.sql")),
    (
        "0002_base_cache",
        include_str!("migrations/0002_base_cache.sql"),
    ),
    (
        "0003_snapshots",
        include_str!("migrations/0003_snapshots.sql"),
    ),
    ("0004_stores", include_str!("migrations/0004_stores.sql")),
    (
        "0005_watchman",
        include_str!("migrations/0005_watchman.sql"),
    ),
];

/// The schema version (`user_version`) of a fully migrated database: the
/// number of migrations.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a handful of migrations, nowhere near u32::MAX"
)]
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// How long a statement waits for a lock another connection holds, such as
/// a backup reader's, before failing as busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The shortest id prefix a lookup accepts, as in jj.
pub const MIN_PREFIX_LEN: usize = 4;

/// `meta` key of the random id this database was created with.
pub const META_DAEMON_INSTANCE_ID: &str = "daemon_instance_id";
/// `meta` key of the creation time, as decimal unix milliseconds.
pub const META_CREATED_AT: &str = "created_at";
/// `meta` key of when the last backup was taken, as decimal unix
/// milliseconds; written by R1.18's backup, read by R1.17's GC.
pub const META_LAST_BACKUP_AT: &str = "last_backup_at";
/// Prefix of the `meta` keys recording applied migrations, such as
/// `migration:0001_initial`; the value is when it was applied, as decimal
/// unix milliseconds.
pub const META_MIGRATION_PREFIX: &str = "migration:";

/// The daemon's metadata database.
///
/// The lock is not reentrant: a `Db` method called from inside
/// `Db::with` or `Db::in_transaction` deadlocks. Code that needs several
/// statements in one transaction calls the `*_in(&Transaction, ..)` helpers
/// the query modules export, as [`Db::insert_workspace_with_lease`] does.
pub struct Db {
    conn: Mutex<Connection>,
}

/// Failure of an operation on the database.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// The database file could not be opened or configured.
    #[error("open database {}", path.display())]
    Open {
        /// The database file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rusqlite::Error,
    },
    /// The database was written by a newer daemon. Downgrades are not
    /// supported.
    #[error("database schema version {found} is newer than this daemon supports ({supported})")]
    TooNew {
        /// The database's `user_version`.
        found: u32,
        /// [`SCHEMA_VERSION`].
        supported: u32,
    },
    /// A migration failed; the database stays at the previous version.
    #[error("migration {name} failed")]
    Migrate {
        /// The migration's name, such as `0001_initial`.
        name: String,
        /// The underlying error.
        #[source]
        source: rusqlite::Error,
    },
    /// No record has this id.
    #[error("{what} {id} not found")]
    NotFound {
        /// The kind of record, such as `workspace`.
        what: &'static str,
        /// The id or prefix looked up.
        id: String,
    },
    /// More than one record matches an id prefix.
    #[error(
        "{what} {prefix:?} is ambiguous; it matches {}",
        candidates_text(candidates, *truncated)
    )]
    Ambiguous {
        /// The kind of record.
        what: &'static str,
        /// The prefix.
        prefix: String,
        /// Ids of the matching records, at most [`MAX_CANDIDATES`] of them.
        candidates: Vec<String>,
        /// Whether more records match than `candidates` lists.
        truncated: bool,
    },
    /// An id prefix is shorter than [`MIN_PREFIX_LEN`], longer than an id,
    /// or not hex.
    #[error("{what} prefix {prefix:?} must be {MIN_PREFIX_LEN} to {ID_HEX_LEN} hex characters")]
    InvalidPrefix {
        /// The kind of record.
        what: &'static str,
        /// The prefix.
        prefix: String,
    },
    /// A record with this id, or this unique path, already exists.
    #[error("{what} {id} already exists")]
    AlreadyExists {
        /// The kind of record, such as `repository`.
        what: &'static str,
        /// The colliding id or path.
        id: String,
    },
    /// The record's state does not allow the change.
    #[error("{what} {id} {reason}")]
    Conflict {
        /// The kind of record.
        what: &'static str,
        /// Its id.
        id: String,
        /// What is in the way, as a fixed phrase such as
        /// `has active workspaces`.
        reason: &'static str,
    },
    /// A path to store is not valid UTF-8; the schema stores paths as text.
    #[error("path {} is not valid UTF-8", .0.display())]
    NonUtf8Path(PathBuf),
    /// A statement failed, or a stored value could not be read back.
    #[error("sqlite")]
    Sqlite(#[from] rusqlite::Error),
}

impl Classify for DbError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Ambiguous { .. } | Self::InvalidPrefix { .. } | Self::NonUtf8Path(_) => {
                ErrorKind::InvalidArgument
            }
            Self::AlreadyExists { .. } => ErrorKind::AlreadyExists,
            Self::Conflict { .. } => ErrorKind::Conflict,
            Self::TooNew { .. } => ErrorKind::Internal,
            Self::Open { source, .. } | Self::Migrate { source, .. } | Self::Sqlite(source) => {
                classify_sqlite(source)
            }
        }
    }
}

/// Classifies a SQLite failure by its result code: busy or locked is
/// [`ErrorKind::Unavailable`], refused access [`ErrorKind::PermissionDenied`],
/// a failing disk or file [`ErrorKind::Io`], and anything else, including a
/// stored value Reef did not write, [`ErrorKind::Internal`].
fn classify_sqlite(error: &rusqlite::Error) -> ErrorKind {
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => ErrorKind::Unavailable,
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => ErrorKind::PermissionDenied,
        Some(ErrorCode::SystemIoFailure | ErrorCode::DiskFull | ErrorCode::CannotOpen) => {
            ErrorKind::Io
        }
        _ => ErrorKind::Internal,
    }
}

impl Db {
    /// Opens the database at `path`, creating the file if it does not exist,
    /// sets the connection's pragmas (WAL, foreign keys, a 5 s busy timeout,
    /// `synchronous = NORMAL`), and applies pending migrations. The parent
    /// directory must exist.
    pub fn open(path: &Path) -> Result<Db, DbError> {
        let open = |source| DbError::Open {
            path: path.to_owned(),
            source,
        };
        let conn = Connection::open(path).map_err(open)?;
        // Before switching to WAL, which needs a lock another process may hold.
        conn.busy_timeout(BUSY_TIMEOUT).map_err(open)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(open)?;
        Self::init(conn, path)
    }

    /// Opens a private in-memory database with the current schema, for
    /// tests. Its journal mode stays `memory`.
    pub fn open_in_memory() -> Result<Db, DbError> {
        let path = Path::new(":memory:");
        let conn = Connection::open_in_memory().map_err(|source| DbError::Open {
            path: path.to_owned(),
            source,
        })?;
        Self::init(conn, path)
    }

    fn init(mut conn: Connection, path: &Path) -> Result<Db, DbError> {
        let open = |source| DbError::Open {
            path: path.to_owned(),
            source,
        };
        conn.busy_timeout(BUSY_TIMEOUT).map_err(open)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(open)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(open)?;
        migrate(&mut conn, path)?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.init_meta()?;
        Ok(db)
    }

    /// Records the instance id and creation time the first time the database
    /// is opened. Later opens leave both alone.
    fn init_meta(&self) -> Result<(), DbError> {
        let instance_id = format!("{:016x}", rand::rng().random::<u64>());
        let created_at = Timestamp::now().0.to_string();
        self.with(|conn| {
            let mut insert = conn
                .prepare("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT DO NOTHING")?;
            insert.execute((META_DAEMON_INSTANCE_ID, instance_id))?;
            insert.execute((META_CREATED_AT, created_at))?;
            Ok(())
        })
    }

    /// The database's `PRAGMA user_version`, which after a successful open
    /// is [`SCHEMA_VERSION`].
    pub fn schema_version(&self) -> Result<u32, DbError> {
        self.with(|conn| conn.pragma_query_value(None, "user_version", |row| row.get(0)))
    }

    /// Runs `f` on the connection under the lock. `f` must not call another
    /// `Db` method (see [`Db`]).
    pub(crate) fn with<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, DbError> {
        Ok(f(&self.lock())?)
    }

    /// Runs `f` in an immediate transaction under the lock, committing if it
    /// returns `Ok` and rolling back otherwise. `f` must not call another
    /// `Db` method; it uses the `*_in` helpers on `tx` instead (see [`Db`]).
    pub(crate) fn in_transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// The connection, locked, for R1.18's online backup. Hold it only for
    /// one backup step at a time.
    #[expect(dead_code, reason = "R1.18's backup is the first caller")]
    pub(crate) fn raw_connection_for_backup(&self) -> MutexGuard<'_, Connection> {
        self.lock()
    }

    /// The value stored under `key` in `meta`, if any.
    pub fn meta(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with(|conn| {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
        })
    }

    /// Stores `value` under `key` in `meta`, replacing any previous value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map(drop)
        })
    }

    /// The lock on the connection. A panic while it was held cannot have
    /// left a transaction open (dropping one rolls it back), so a poisoned
    /// lock is taken over.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Applies every migration not yet applied, each in its own transaction.
///
/// Migrations are independent and may land in any order, so which ones a
/// database has is recorded by name, as `meta` rows keyed
/// [`META_MIGRATION_PREFIX`]`<name>`; `user_version` is kept equal to how many
/// are applied. A database with more migrations than this binary has, or
/// with one it does not know, was written by a newer daemon and is refused.
fn migrate(conn: &mut Connection, path: &Path) -> Result<(), DbError> {
    let found: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let too_new = DbError::TooNew {
        found,
        supported: SCHEMA_VERSION,
    };
    if found > SCHEMA_VERSION {
        return Err(too_new);
    }
    let recorded = applied_migrations(conn)?;
    if recorded
        .iter()
        .any(|name| !MIGRATIONS.iter().any(|(known, _)| known == name))
    {
        return Err(too_new);
    }
    let mut applied = 0;
    for (name, sql) in MIGRATIONS {
        if recorded.iter().any(|done| done == name) {
            continue;
        }
        let failed = |source| DbError::Migrate {
            name: (*name).to_owned(),
            source,
        };
        // IMMEDIATE takes the write lock before the check, so of two
        // processes opening the same new file only one applies a migration.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failed)?;
        if applied_migrations(&tx)
            .map_err(failed)?
            .iter()
            .any(|done| done == name)
        {
            continue;
        }
        tx.execute_batch(sql).map_err(failed)?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)",
            (
                format!("{META_MIGRATION_PREFIX}{name}"),
                Timestamp::now().0.to_string(),
            ),
        )
        .map_err(failed)?;
        let count =
            i64::try_from(applied_migrations(&tx).map_err(failed)?.len()).unwrap_or(i64::MAX);
        tx.pragma_update(None, "user_version", count)
            .map_err(failed)?;
        tx.commit().map_err(failed)?;
        applied += 1;
    }
    if applied > 0 {
        tracing::info!(
            path = %path.display(),
            migration_count = applied,
            "database migrated"
        );
    }
    Ok(())
}

/// Names of the migrations recorded in `meta`; none before `0001_initial`
/// has created the table.
fn applied_migrations(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let has_meta = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_meta {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT key FROM meta WHERE key GLOB ?1 ORDER BY key")?;
    let keys = stmt.query_map([format!("{META_MIGRATION_PREFIX}*")], |row| {
        row.get::<_, String>(0)
    })?;
    keys.map(|key| {
        key.map(|key| {
            key.strip_prefix(META_MIGRATION_PREFIX)
                .unwrap_or(&key)
                .to_owned()
        })
    })
    .collect()
}

/// Lowercases and checks an id prefix for a lookup of `what`.
pub(crate) fn normalize_prefix(what: &'static str, prefix: &str) -> Result<String, DbError> {
    let valid = (MIN_PREFIX_LEN..=ID_HEX_LEN).contains(&prefix.len())
        && prefix.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid {
        return Err(DbError::InvalidPrefix {
            what,
            prefix: prefix.to_owned(),
        });
    }
    Ok(prefix.to_ascii_lowercase())
}

/// How many matching ids an ambiguous prefix's error lists.
pub const MAX_CANDIDATES: usize = 10;

/// The row limit for a prefix lookup: enough to list [`MAX_CANDIDATES`] and
/// see that there are more.
pub(crate) const PREFIX_LOOKUP_LIMIT: usize = MAX_CANDIDATES + 1;

/// The one record in `matches`, a lookup by `prefix` limited to
/// [`PREFIX_LOOKUP_LIMIT`] rows. `id_of` names a record for the error when
/// several match.
pub(crate) fn unique_match<T>(
    what: &'static str,
    prefix: &str,
    mut matches: Vec<T>,
    id_of: impl Fn(&T) -> String,
) -> Result<T, DbError> {
    match matches.len() {
        0 => Err(DbError::NotFound {
            what,
            id: prefix.to_owned(),
        }),
        1 => Ok(matches.remove(0)),
        count => Err(DbError::Ambiguous {
            what,
            prefix: prefix.to_owned(),
            candidates: matches.iter().take(MAX_CANDIDATES).map(id_of).collect(),
            truncated: count > MAX_CANDIDATES,
        }),
    }
}

/// `a, b, c`, or `a, b, c, and more` when `truncated`.
fn candidates_text(candidates: &[String], truncated: bool) -> String {
    let mut text = candidates.join(", ");
    if truncated {
        text.push_str(", and more");
    }
    text
}

/// `path` as the text the schema stores.
pub(crate) fn path_text(path: &Path) -> Result<&str, DbError> {
    path.to_str()
        .ok_or_else(|| DbError::NonUtf8Path(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 5);
        assert_eq!(SCHEMA_VERSION, 5);
        let instance_id = db.meta(META_DAEMON_INSTANCE_ID).unwrap().unwrap();
        assert!(instance_id.parse::<crate::model::RepoId>().is_ok());
        let created_at = db.meta(META_CREATED_AT).unwrap().unwrap();
        assert!(created_at.parse::<i64>().unwrap() > 0);
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 5);
        assert_eq!(
            db.meta(META_DAEMON_INSTANCE_ID).unwrap().unwrap(),
            instance_id
        );
        assert_eq!(db.meta(META_CREATED_AT).unwrap().unwrap(), created_at);
    }

    #[test]
    fn open_sets_wal_and_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("reef.sqlite3")).unwrap();
        let (journal, foreign_keys, synchronous, busy): (String, i64, i64, i64) = db
            .with(|conn| {
                Ok((
                    conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?,
                    conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
                    conn.pragma_query_value(None, "synchronous", |row| row.get(0))?,
                    conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 1, "NORMAL");
        assert_eq!(busy, 5000);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);

        let error = Db::open(&path).err().unwrap();
        assert!(
            matches!(
                error,
                DbError::TooNew {
                    found: 99,
                    supported: 5
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Internal);
    }

    #[test]
    fn migrations_are_recorded_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let db = Db::open(&path).unwrap();
        let applied = db.with(applied_migrations).unwrap();
        assert_eq!(
            applied,
            [
                "0001_initial",
                "0002_base_cache",
                "0003_snapshots",
                "0004_stores",
                "0005_watchman"
            ]
        );
        // A newer daemon applied a migration this binary does not have, without
        // raising the count past ours (it may lack one of ours).
        db.set_meta("migration:0099_future", "0").unwrap();
        drop(db);
        let error = Db::open(&path).err().unwrap();
        assert!(matches!(error, DbError::TooNew { .. }), "{error:?}");
    }

    #[test]
    fn a_version_one_database_gains_the_base_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('migration:0001_initial', '0')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 5);
        assert_eq!(
            db.with(applied_migrations).unwrap(),
            [
                "0001_initial",
                "0002_base_cache",
                "0003_snapshots",
                "0004_stores",
                "0005_watchman"
            ]
        );
        let repo = "aaaa000000000001".parse().unwrap();
        assert_eq!(db.bases(repo).unwrap().len(), 0);
    }

    // R1.14: rows written before `0005_watchman` were watched by notify and
    // have no tracked file count.
    #[test]
    fn existing_rows_gain_the_watcher_columns() {
        use crate::db::repositories::tests::repo;
        use crate::db::workspaces::tests::workspace;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..2].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, '0')",
                [format!("{META_MIGRATION_PREFIX}{name}")],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", i64::try_from(index + 1).unwrap())
                .unwrap();
        }
        let record = repo("aaaa000000000001", "/src/app");
        let ws = workspace(record.id, "bbbb000000000001", "/ws/one");
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspaces(id, repo_id, owner, change_id, commit_id, base_tree_id,
                                    backend, path, state_dir, state, created_at)
             VALUES (?1, ?2, 'alice@host', 'c', 'd', 'e', 'copy', '/ws/one', '/state/wc', 'active', 2000)",
            rusqlite::params![ws.id, record.id],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 5);
        assert_eq!(db.repository(record.id).unwrap().tracked_files, None);
        assert_eq!(
            db.workspace(ws.id).unwrap().watcher,
            crate::model::WatcherKind::Notify
        );
    }

    // R1.16: a database from before `0003_snapshots` landed has `0005`
    // already; the earlier-numbered migration still applies.
    #[test]
    fn a_later_migration_does_not_block_an_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        let without_snapshots = MIGRATIONS
            .iter()
            .filter(|(name, _)| *name != "0003_snapshots");
        for (index, (name, sql)) in without_snapshots.enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, '0')",
                [format!("{META_MIGRATION_PREFIX}{name}")],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", i64::try_from(index + 1).unwrap())
                .unwrap();
        }
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let ws = "bbbb000000000001".parse().unwrap();
        assert_eq!(crate::snapshot::index::counts(&db, ws).unwrap(), (0, 0));
    }

    #[test]
    fn meta_round_trips() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.meta("last_backup_at").unwrap(), None);
        db.set_meta("last_backup_at", "1").unwrap();
        db.set_meta("last_backup_at", "2").unwrap();
        assert_eq!(db.meta("last_backup_at").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn failed_transaction_rolls_back() {
        let db = Db::open_in_memory().unwrap();
        let result: Result<(), DbError> = db.in_transaction(|tx| {
            tx.execute("INSERT INTO meta(key, value) VALUES ('k', 'v')", [])?;
            Err(DbError::Conflict {
                what: "test",
                id: "x".into(),
                reason: "fails on purpose",
            })
        });
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Conflict);
        assert_eq!(db.meta("k").unwrap(), None);
    }

    #[test]
    fn prefixes_are_checked_and_lowercased() {
        assert_eq!(normalize_prefix("workspace", "ABCD").unwrap(), "abcd");
        for bad in ["abc", "", "abcz", "0123456789abcdef0"] {
            let error = normalize_prefix("workspace", bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{bad:?}");
        }
    }

    #[test]
    fn ambiguous_prefixes_list_at_most_ten_candidates() {
        let ids: Vec<String> = (0..PREFIX_LOOKUP_LIMIT)
            .map(|n| format!("cafe{n:012}"))
            .collect();
        let error = unique_match("workspace", "cafe", ids.clone(), Clone::clone).unwrap_err();
        let text = error.to_string();
        assert!(text.ends_with("cafe000000000009, and more"), "{text}");
        assert!(!text.contains("cafe000000000010"), "{text}");
        let error = unique_match("workspace", "cafe", ids[..2].to_vec(), Clone::clone).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("matches cafe000000000000, cafe000000000001"),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    }
}
