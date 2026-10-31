//! One repository's history index: `repos/<id>/history.sqlite3` (ADR 0044).
//!
//! A cache, not state: no migrations, outside backup, deleted with its
//! repository, and recreated (then backfilled from the operation log) when
//! it is unreadable or of another format. One writer connection behind a
//! mutex and a few readers, in WAL mode, as the symbol index's store.
//!
//! A row per change, with its outcome and latest commit; its activity per
//! (agent, workspace); and its touches: the paths and symbols its net diff
//! changes, computed for `touched_commit`. Strings are interned. Ids are
//! lowercase hex, and times milliseconds since the epoch.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

/// The file's `meta.format`; another value recreates the file.
const FORMAT: &str = "1";

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE names (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE symbols (id INTEGER PRIMARY KEY, symbol TEXT NOT NULL UNIQUE);
CREATE TABLE paths (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
CREATE TABLE agents (id INTEGER PRIMARY KEY, agent TEXT NOT NULL UNIQUE);
CREATE TABLE changes (id INTEGER PRIMARY KEY, change_id TEXT NOT NULL UNIQUE,
                      commit_id TEXT NOT NULL, commit_at INTEGER NOT NULL,
                      outcome INTEGER NOT NULL, divergent INTEGER NOT NULL,
                      subject TEXT NOT NULL, author_email TEXT NOT NULL,
                      first_at INTEGER NOT NULL, last_at INTEGER NOT NULL,
                      landed_at INTEGER, ended_at INTEGER,
                      touched_commit TEXT, touched_at INTEGER, empty INTEGER,
                      symbols_coverage INTEGER, paths_changed INTEGER,
                      paths_truncated INTEGER, symbols_changed INTEGER);
CREATE INDEX changes_by_last ON changes (last_at DESC, id DESC);
CREATE INDEX changes_untouched ON changes (last_at) WHERE touched_commit IS NOT commit_id;
CREATE TABLE activity (change INTEGER NOT NULL, agent INTEGER NOT NULL,
                       workspace_id TEXT NOT NULL, first_at INTEGER NOT NULL,
                       last_at INTEGER NOT NULL, operations INTEGER NOT NULL,
                       last_operation_id TEXT,
                       PRIMARY KEY (change, agent, workspace_id)) WITHOUT ROWID;
CREATE INDEX activity_by_agent ON activity (agent, last_at);
CREATE INDEX activity_by_workspace ON activity (workspace_id, last_at);
CREATE INDEX activity_by_first ON activity (first_at);
CREATE TABLE ends (change_id TEXT PRIMARY KEY, undo INTEGER NOT NULL,
                   at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE change_paths (change INTEGER NOT NULL, path INTEGER NOT NULL,
                           kind INTEGER NOT NULL,
                           PRIMARY KEY (change, path)) WITHOUT ROWID;
CREATE INDEX change_paths_by_path ON change_paths (path, change);
CREATE TABLE touches (change INTEGER NOT NULL, seq INTEGER NOT NULL,
                      symbol INTEGER NOT NULL, old_symbol INTEGER, name INTEGER NOT NULL,
                      kind INTEGER NOT NULL, change_kind INTEGER NOT NULL,
                      path INTEGER NOT NULL,
                      PRIMARY KEY (change, seq)) WITHOUT ROWID;
CREATE INDEX touches_by_symbol ON touches (symbol, change);
CREATE INDEX touches_by_old_symbol ON touches (old_symbol, change) WHERE old_symbol IS NOT NULL;
CREATE INDEX touches_by_name ON touches (name, change);
";

/// `meta` keys.
pub(crate) const META_FRONTIER: &str = "frontier";
pub(crate) const META_TRUNK: &str = "trunk";
pub(crate) const META_BACKFILL: &str = "backfill";
pub(crate) const META_CATCHUP: &str = "catch_up";

/// What became of a change. The values are the API's `ChangeOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Outcome {
    Open = 1,
    Landed = 2,
    Abandoned = 3,
    Undone = 4,
}

impl Outcome {
    pub(crate) fn from_i64(value: i64) -> Outcome {
        match value {
            2 => Outcome::Landed,
            3 => Outcome::Abandoned,
            4 => Outcome::Undone,
            _ => Outcome::Open,
        }
    }

    /// Whether the change is still visible.
    pub(crate) fn visible(self) -> bool {
        matches!(self, Outcome::Open | Outcome::Landed)
    }
}

/// How completely a change's symbols were computed. The values are the
/// API's `HistorySymbolCoverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolsCoverage {
    Covered = 1,
    Partial = 2,
    TooLarge = 3,
    Disabled = 4,
}

/// Failure of the history database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("the history index at {path} failed")]
    Sqlite {
        /// The file.
        path: PathBuf,
        /// SQLite's error.
        #[source]
        source: rusqlite::Error,
    },
    /// The file could not be removed to recreate it.
    #[error("could not recreate the history index at {path}")]
    Recreate {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// One repository's history index.
#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    max_readers: usize,
}

fn opened(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Whether `error` means the file is not a usable database.
fn unreadable(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

impl Store {
    /// Opens or creates the index at `path`, recreating a file that is not
    /// a database or is of another format. Returns whether it was created
    /// (so it must be backfilled).
    pub(crate) fn open(path: &Path, max_readers: usize) -> Result<(Store, bool), StoreError> {
        let sqlite = |source| StoreError::Sqlite {
            path: path.to_owned(),
            source,
        };
        let (conn, created) = match Self::init(path) {
            Ok(Some(opened)) => opened,
            Err(error) if !unreadable(&error) => return Err(sqlite(error)),
            outcome => {
                let cause = match &outcome {
                    Ok(_) => "another format".to_owned(),
                    Err(error) => error.to_string(),
                };
                tracing::warn!(path = %path.display(), cause, "recreating the history index");
                for suffix in ["", "-wal", "-shm"] {
                    let file = PathBuf::from(format!("{}{suffix}", path.display()));
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(source) => {
                            return Err(StoreError::Recreate {
                                path: path.to_owned(),
                                source,
                            });
                        }
                    }
                }
                Self::init(path)
                    .map_err(sqlite)?
                    .ok_or_else(|| sqlite(rusqlite::Error::InvalidQuery))?
            }
        };
        Ok((
            Store {
                path: path.to_owned(),
                writer: Mutex::new(conn),
                readers: Mutex::new(Vec::new()),
                max_readers,
            },
            created,
        ))
    }

    /// The connection and whether the schema was created; `None` when the
    /// file holds another format.
    fn init(path: &Path) -> rusqlite::Result<Option<(Connection, bool)>> {
        crate::db::private_file(path).map_err(|error| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(error.to_string()),
            )
        })?;
        let mut conn = opened(path)?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let has_meta: bool = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get(0),
        )?;
        if !has_meta {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('format', ?1)",
                [FORMAT],
            )?;
            tx.commit()?;
            return Ok(Some((conn, true)));
        }
        let format: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |row| {
                row.get(0)
            })
            .optional()?;
        if format.as_deref() != Some(FORMAT) {
            return Ok(None);
        }
        Ok(Some((conn, false)))
    }

    fn sqlite(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Sqlite {
            path: self.path.clone(),
            source,
        }
    }

    /// Runs `read` on a reader connection.
    pub(crate) fn read<T>(
        &self,
        read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, StoreError> {
        let taken = self
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let conn = match taken {
            Some(conn) => conn,
            None => opened(&self.path).map_err(|e| self.sqlite(e))?,
        };
        let result = read(&conn).map_err(|e| self.sqlite(e));
        let mut readers = self.readers.lock().unwrap_or_else(PoisonError::into_inner);
        if readers.len() < self.max_readers {
            readers.push(conn);
        }
        result
    }

    /// Runs `write` in one transaction on the writer.
    pub(crate) fn write<T>(
        &self,
        write: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, StoreError> {
        let mut conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction().map_err(|e| self.sqlite(e))?;
        let out = write(&tx).map_err(|e| self.sqlite(e))?;
        tx.commit().map_err(|e| self.sqlite(e))?;
        Ok(out)
    }

    /// The bytes the database's live pages hold.
    pub(crate) fn live_bytes(&self) -> Result<u64, StoreError> {
        let conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let pragma = |name: &str| -> rusqlite::Result<i64> {
            conn.pragma_query_value(None, name, |row| row.get(0))
        };
        let (pages, free, size) = (|| {
            Ok((
                pragma("page_count")?,
                pragma("freelist_count")?,
                pragma("page_size")?,
            ))
        })()
        .map_err(|e| self.sqlite(e))?;
        Ok(u64::try_from((pages - free).max(0) * size).unwrap_or(0))
    }

    /// Returns up to `pages` freed pages to the file system.
    pub(crate) fn vacuum_step(&self, pages: u32) -> Result<(), StoreError> {
        let conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages});"))
            .map_err(|e| self.sqlite(e))
    }
}

/// `meta`'s `key`.
pub(crate) fn meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
}

/// Sets `meta`'s `key`, or removes it with `None`.
pub(crate) fn set_meta(
    tx: &Transaction<'_>,
    key: &str,
    value: Option<&str>,
) -> rusqlite::Result<()> {
    match value {
        Some(value) => tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            [key, value],
        ),
        None => tx.execute("DELETE FROM meta WHERE key = ?1", [key]),
    }
    .map(drop)
}

/// The id of `value` in the interning `table` (`names`, `symbols`, `paths`
/// or `agents`, whose text column is `column`), added if new.
pub(crate) fn intern(
    tx: &Transaction<'_>,
    table: &'static str,
    column: &'static str,
    value: &str,
) -> rusqlite::Result<i64> {
    let found: Option<i64> = tx
        .prepare_cached(&format!("SELECT id FROM {table} WHERE {column} = ?1"))?
        .query_row([value], |row| row.get(0))
        .optional()?;
    if let Some(id) = found {
        return Ok(id);
    }
    tx.prepare_cached(&format!("INSERT INTO {table} ({column}) VALUES (?1)"))?
        .execute([value])?;
    Ok(tx.last_insert_rowid())
}

/// A change's row as the follower updates it.
#[derive(Debug, Clone)]
pub(crate) struct ChangeRow {
    pub id: i64,
    pub outcome: Outcome,
    pub landed_at: Option<i64>,
    pub ended_at: Option<i64>,
}

/// `change_id`'s row, if any.
pub(crate) fn change_row(
    tx: &Transaction<'_>,
    change_id: &str,
) -> rusqlite::Result<Option<ChangeRow>> {
    tx.prepare_cached("SELECT id, outcome, landed_at, ended_at FROM changes WHERE change_id = ?1")?
        .query_row([change_id], |row| {
            Ok(ChangeRow {
                id: row.get(0)?,
                outcome: Outcome::from_i64(row.get(1)?),
                landed_at: row.get(2)?,
                ended_at: row.get(3)?,
            })
        })
        .optional()
}

/// Adds `change_id`'s row, open, first written `at` with `commit_id`.
pub(crate) fn insert_change(
    tx: &Transaction<'_>,
    change_id: &str,
    commit_id: &str,
    at: i64,
) -> rusqlite::Result<i64> {
    tx.prepare_cached(
        "INSERT INTO changes (change_id, commit_id, commit_at, outcome, divergent, subject,
                              author_email, first_at, last_at)
         VALUES (?1, ?2, ?3, ?4, 0, '', '', ?3, ?3)",
    )?
    .execute(params![change_id, commit_id, at, Outcome::Open as i64])?;
    Ok(tx.last_insert_rowid())
}

/// Moves a change's commit to `commit_id`, written at `at`, unless a later
/// write already gave it one (a backfill reads newest first).
pub(crate) fn set_commit(
    tx: &Transaction<'_>,
    id: i64,
    commit_id: &str,
    at: i64,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "UPDATE changes SET commit_id = ?2, commit_at = ?3 WHERE id = ?1 AND commit_at <= ?3",
    )?
    .execute(params![id, commit_id, at])
    .map(drop)
}

/// Records one operation's direct write of a change by `agent` in
/// `workspace_id`, at `at`.
pub(crate) fn add_activity(
    tx: &Transaction<'_>,
    change: i64,
    agent: &str,
    workspace_id: &str,
    at: i64,
    operation_id: &str,
) -> rusqlite::Result<()> {
    let agent = intern(tx, "agents", "agent", agent)?;
    tx.prepare_cached(
        "INSERT INTO activity (change, agent, workspace_id, first_at, last_at, operations,
                               last_operation_id)
         VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5)
         ON CONFLICT (change, agent, workspace_id) DO UPDATE SET
           first_at = min(first_at, ?4),
           last_operation_id = CASE WHEN ?4 >= last_at THEN ?5 ELSE last_operation_id END,
           last_at = max(last_at, ?4),
           operations = operations + 1",
    )?
    .execute(params![change, agent, workspace_id, at, operation_id])?;
    tx.prepare_cached(
        "UPDATE changes SET first_at = min(first_at, ?2), last_at = max(last_at, ?2) WHERE id = ?1",
    )?
    .execute(params![change, at])?;
    Ok(())
}

/// Records that an operation ended `at` hid `change_id`, by an undo or
/// not; the latest such record wins. Kept by change id, apart from the
/// change's row, so a backfill reading the hiding operation before the
/// one that created the change still knows how it ended.
pub(crate) fn record_end(
    tx: &Transaction<'_>,
    change_id: &str,
    undo: bool,
    at: i64,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO ends (change_id, undo, at) VALUES (?1, ?2, ?3)
         ON CONFLICT (change_id) DO UPDATE SET undo = ?2, at = ?3 WHERE ?3 >= at",
    )?
    .execute(params![change_id, undo, at])
    .map(drop)
}

/// How `change_id` was last hidden: by an undo or not, and when.
pub(crate) fn end_of(
    tx: &Transaction<'_>,
    change_id: &str,
) -> rusqlite::Result<Option<(bool, i64)>> {
    tx.prepare_cached("SELECT undo, at FROM ends WHERE change_id = ?1")?
        .query_row([change_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
}

/// Sets a change's commit to `commit_id`, the one the repository shows,
/// written at `at` at the latest, when it differs.
pub(crate) fn settle_commit(
    tx: &Transaction<'_>,
    id: i64,
    commit_id: &str,
    at: i64,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "UPDATE changes SET commit_id = ?2, commit_at = max(commit_at, ?3)
         WHERE id = ?1 AND commit_id != ?2",
    )?
    .execute(params![id, commit_id, at])
    .map(drop)
}

/// What the follower read of a change at the tip.
#[derive(Debug, Clone)]
pub(crate) struct ChangeUpdate {
    pub outcome: Outcome,
    pub divergent: bool,
    pub subject: String,
    pub author_email: String,
    pub landed_at: Option<i64>,
    pub ended_at: Option<i64>,
}

/// Sets a change's outcome and what goes with it.
pub(crate) fn update_change(
    tx: &Transaction<'_>,
    id: i64,
    update: &ChangeUpdate,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "UPDATE changes SET outcome = ?2, divergent = ?3, subject = ?4, author_email = ?5,
                            landed_at = ?6, ended_at = ?7 WHERE id = ?1",
    )?
    .execute(params![
        id,
        update.outcome as i64,
        update.divergent,
        update.subject,
        update.author_email,
        update.landed_at,
        update.ended_at,
    ])
    .map(drop)
}

/// One path a change's net diff touches.
#[derive(Debug, Clone)]
pub(crate) struct NewPath {
    pub path: String,
    /// The `FileChangeKind` value.
    pub kind: i64,
}

/// One symbol a change's net diff touches.
#[derive(Debug, Clone)]
pub(crate) struct NewTouch {
    pub symbol: String,
    pub old_symbol: Option<String>,
    pub name: String,
    /// The `SymbolKind` value.
    pub kind: i64,
    /// The `SymbolChangeKind` value.
    pub change_kind: i64,
    pub path: String,
}

/// A change's computed touches.
#[derive(Debug, Clone, Default)]
pub(crate) struct Touches {
    pub paths: Vec<NewPath>,
    pub paths_changed: u64,
    pub paths_truncated: bool,
    pub symbols: Vec<NewTouch>,
    pub symbols_changed: u64,
    pub coverage: Option<SymbolsCoverage>,
}

/// Replaces a change's touches with `touches`, computed for `commit_id`
/// at `at`; nothing when the change's row is gone.
pub(crate) fn set_touches(
    tx: &Transaction<'_>,
    change: i64,
    commit_id: &str,
    at: i64,
    touches: &Touches,
) -> rusqlite::Result<()> {
    let updated = tx
        .prepare_cached(
            "UPDATE changes SET touched_commit = ?2, touched_at = ?3, empty = ?4,
                                symbols_coverage = ?5, paths_changed = ?6, paths_truncated = ?7,
                                symbols_changed = ?8 WHERE id = ?1",
        )?
        .execute(params![
            change,
            commit_id,
            at,
            touches.paths_changed == 0,
            touches.coverage.map(|c| c as i64),
            i64::try_from(touches.paths_changed).unwrap_or(i64::MAX),
            touches.paths_truncated,
            i64::try_from(touches.symbols_changed).unwrap_or(i64::MAX),
        ])?;
    if updated == 0 {
        // GC removed the change meanwhile.
        return Ok(());
    }
    tx.prepare_cached("DELETE FROM change_paths WHERE change = ?1")?
        .execute([change])?;
    tx.prepare_cached("DELETE FROM touches WHERE change = ?1")?
        .execute([change])?;
    for path in &touches.paths {
        let id = intern(tx, "paths", "path", &path.path)?;
        tx.prepare_cached(
            "INSERT OR REPLACE INTO change_paths (change, path, kind) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![change, id, path.kind])?;
    }
    for (seq, touch) in touches.symbols.iter().enumerate() {
        let symbol = intern(tx, "symbols", "symbol", &touch.symbol)?;
        let old_symbol = touch
            .old_symbol
            .as_deref()
            .map(|old| intern(tx, "symbols", "symbol", old))
            .transpose()?;
        let name = intern(tx, "names", "name", &touch.name)?;
        let path = intern(tx, "paths", "path", &touch.path)?;
        tx.prepare_cached(
            "INSERT INTO touches (change, seq, symbol, old_symbol, name, kind, change_kind, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            change,
            i64::try_from(seq).unwrap_or(i64::MAX),
            symbol,
            old_symbol,
            name,
            touch.kind,
            touch.change_kind,
            path
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn store_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite3");
        let (store, created) = Store::open(&path, 2).unwrap();
        assert!(created);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let id = store
            .write(|tx| {
                let id = insert_change(tx, "ab", "c1", 10)?;
                add_activity(tx, id, "bot-1", "ws", 20, "op1")?;
                add_activity(tx, id, "bot-1", "ws", 15, "op0")?;
                Ok(id)
            })
            .unwrap();
        let (first, last, ops, last_op): (i64, i64, i64, String) = store
            .read(|conn| {
                conn.query_row(
                    "SELECT first_at, last_at, operations, last_operation_id FROM activity WHERE change = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
            })
            .unwrap();
        assert_eq!((first, last, ops, last_op.as_str()), (15, 20, 2, "op1"));
        drop(store);

        // Reopened, it is not created again.
        let (store, created) = Store::open(&path, 2).unwrap();
        assert!(!created);
        drop(store);

        // Garbage is recreated.
        std::fs::write(&path, b"not a database at all, not even close........").unwrap();
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let (store, created) = Store::open(&path, 2).unwrap();
        assert!(created);
        drop(store);

        // Another format is recreated.
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = '0' WHERE key = 'format'", [])
            .unwrap();
        drop(conn);
        let (_reopened, created) = Store::open(&path, 2).unwrap();
        assert!(created);
    }
}
