//! One repository's symbol cache: `repos/<id>/symbols.sqlite3` (ADR 0042).
//!
//! A cache, not state: no migrations, outside backup, deleted with its
//! repository, and recreated when it is unreadable or of another format.
//! One writer connection behind a mutex and a few readers, in WAL mode.
//!
//! An entry is one `(path, file id, source)`: a file's tags or its uploaded
//! SCIP document, with its occurrences in source order. Symbol strings and
//! names are interned. A tree row records a pass's coverage and counts; it
//! is a hint (GC can remove entries a `COMPLETE` tree uses), never trusted
//! blindly.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use reef_symbols::{Kind, Occurrence, Range, Role};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

/// The file's `meta.format`; another value recreates the file.
const FORMAT: &str = "1";

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE names (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE symbols (id INTEGER PRIMARY KEY, symbol TEXT NOT NULL UNIQUE,
                      name INTEGER NOT NULL REFERENCES names(id));
CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL, file_id BLOB NOT NULL,
                    source INTEGER NOT NULL, extractor TEXT NOT NULL, language TEXT NOT NULL,
                    skip INTEGER, has_errors INTEGER NOT NULL, occurrences INTEGER NOT NULL,
                    created_at INTEGER NOT NULL, used_at INTEGER NOT NULL,
                    UNIQUE (path, file_id, source));
CREATE INDEX files_by_used_at ON files (used_at);
CREATE TABLE occurrences (file INTEGER NOT NULL, seq INTEGER NOT NULL,
                    symbol INTEGER, name INTEGER NOT NULL, role INTEGER NOT NULL,
                    kind INTEGER NOT NULL, flags INTEGER NOT NULL,
                    line INTEGER NOT NULL, col INTEGER NOT NULL,
                    end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
                    def_line INTEGER, def_col INTEGER, def_end_line INTEGER, def_end_col INTEGER,
                    enclosing INTEGER, digest INTEGER,
                    PRIMARY KEY (file, seq)) WITHOUT ROWID;
CREATE INDEX occurrences_by_symbol ON occurrences (symbol, file) WHERE symbol IS NOT NULL;
CREATE INDEX occurrences_by_name ON occurrences (name, file);
CREATE TABLE trees (tree TEXT PRIMARY KEY, commit_id TEXT NOT NULL, state INTEGER NOT NULL,
                    files INTEGER NOT NULL, indexed INTEGER NOT NULL, skipped TEXT NOT NULL,
                    languages TEXT NOT NULL, scip_files INTEGER NOT NULL,
                    indexed_at INTEGER, used_at INTEGER NOT NULL);
";

/// Where an entry came from. The values are the API's `SymbolSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum Source {
    /// Tags the daemon extracted.
    Tags = 1,
    /// An uploaded SCIP document.
    Scip = 2,
}

impl Source {
    fn from_i64(value: i64) -> Self {
        if value == Source::Scip as i64 {
            Source::Scip
        } else {
            Source::Tags
        }
    }
}

/// The index's own skip value beside [`reef_symbols::Skip`]'s: a conflicted
/// path, counted in a tree but never stored.
pub(crate) const SKIP_CONFLICTED: u8 = 5;

/// Failure of the cache database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("the symbol index at {path} failed")]
    Sqlite {
        /// The file.
        path: PathBuf,
        /// SQLite's error.
        #[source]
        source: rusqlite::Error,
    },
    /// The file could not be removed to recreate it.
    #[error("could not recreate the symbol index at {path}")]
    Recreate {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// One entry to write.
#[derive(Debug)]
pub(crate) struct NewEntry {
    pub path: String,
    pub file_id: Vec<u8>,
    pub source: Source,
    pub extractor: String,
    pub language: String,
    /// A [`reef_symbols::Skip`] value; `None` when extracted.
    pub skip: Option<u8>,
    pub has_errors: bool,
    pub occurrences: Vec<Occurrence>,
}

/// An entry's row, without its occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryRow {
    pub id: i64,
    pub path: String,
    pub source: Source,
    pub extractor: String,
    pub language: String,
    pub skip: Option<u8>,
}

/// One stored occurrence found by name or symbol, with its entry.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    pub entry: EntryRow,
    pub file_id: Vec<u8>,
    pub seq: u32,
    pub occurrence: Occurrence,
    /// The enclosing definition's symbol, if any.
    pub enclosing_symbol: Option<String>,
}

/// A tree's coverage and counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Supported files, conflicted ones included.
    pub files: u64,
    /// Supported files with an entry, skipped ones included.
    pub indexed: u64,
    /// Skipped files by [`reef_symbols::Skip`] value or
    /// [`SKIP_CONFLICTED`].
    pub skipped: BTreeMap<u8, u64>,
    /// Supported files by language.
    pub languages: BTreeMap<String, u64>,
    /// Files with an uploaded SCIP document.
    pub scip_files: u64,
}

impl Counts {
    /// Skipped files in all.
    pub(crate) fn skipped_total(&self) -> u64 {
        self.skipped.values().sum()
    }

    /// Adds (`sign` 1) or removes (`sign` -1) one file's contribution.
    pub(crate) fn add(&mut self, file: &Contribution, sign: i64) {
        let apply = |n: &mut u64| {
            *n = if sign > 0 {
                *n + 1
            } else {
                n.saturating_sub(1)
            };
        };
        if let Some(language) = &file.language {
            apply(&mut self.files);
            apply(self.languages.entry(language.clone()).or_default());
            if file.indexed {
                apply(&mut self.indexed);
            }
            if let Some(skip) = file.skip {
                apply(self.skipped.entry(skip).or_default());
            }
        }
        if file.scip {
            apply(&mut self.scip_files);
        }
        self.languages.retain(|_, n| *n > 0);
        self.skipped.retain(|_, n| *n > 0);
    }
}

/// What one file adds to a tree's [`Counts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Contribution {
    /// Its language, when supported.
    pub language: Option<String>,
    /// It has an entry.
    pub indexed: bool,
    /// Why it was skipped, if it was.
    pub skip: Option<u8>,
    /// It has an uploaded SCIP document.
    pub scip: bool,
}

/// A tree's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeRow {
    pub commit_id: String,
    pub complete: bool,
    pub counts: Counts,
    /// Milliseconds since the epoch the last pass ended.
    pub indexed_at: Option<i64>,
}

/// How often a read refreshes an entry's `used_at`, in milliseconds: at
/// most once a day.
const TOUCH_EVERY: i64 = 86_400_000;

/// Milliseconds since the Unix epoch: `used_at`, `created_at` and
/// `indexed_at`.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// One repository's cache.
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
    /// Opens or creates the cache at `path`, recreating a file that is not
    /// a database or is of another format.
    pub(crate) fn open(
        path: &Path,
        max_readers: usize,
        extractors: &str,
    ) -> Result<Store, StoreError> {
        let sqlite = |source| StoreError::Sqlite {
            path: path.to_owned(),
            source,
        };
        let conn = match Self::init(path, extractors) {
            Ok(Some(conn)) => conn,
            Err(error) if !unreadable(&error) => return Err(sqlite(error)),
            outcome => {
                let cause = match &outcome {
                    Ok(_) => "another format".to_owned(),
                    Err(error) => error.to_string(),
                };
                tracing::warn!(path = %path.display(), cause, "recreating the symbol index");
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
                Self::init(path, extractors)
                    .map_err(sqlite)?
                    .ok_or_else(|| sqlite(rusqlite::Error::InvalidQuery))?
            }
        };
        Ok(Store {
            path: path.to_owned(),
            writer: Mutex::new(conn),
            readers: Mutex::new(Vec::new()),
            max_readers,
        })
    }

    /// The connection, or `None` when the file holds another format. Tree
    /// rows written under another set of `extractors` are dropped: a tree
    /// complete for an older extractor has no current entries.
    fn init(path: &Path, extractors: &str) -> rusqlite::Result<Option<Connection>> {
        crate::db::private_file(path).map_err(|error| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(error.to_string()),
            )
        })?;
        let mut conn = opened(path)?;
        // Set before the first table, so the file can return freed pages.
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
                "INSERT INTO meta (key, value) VALUES ('format', ?1), ('extractors', ?2)",
                [FORMAT, extractors],
            )?;
            tx.commit()?;
            return Ok(Some(conn));
        }
        let meta = |key: &str| -> rusqlite::Result<Option<String>> {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
        };
        if meta("format")?.as_deref() != Some(FORMAT) {
            return Ok(None);
        }
        if meta("extractors")?.as_deref() != Some(extractors) {
            tracing::info!(path = %path.display(), extractors, "symbol extractors changed; trees will be indexed again");
            conn.execute_batch("DELETE FROM trees;")?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('extractors', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = ?1",
                [extractors],
            )?;
        }
        Ok(Some(conn))
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

    /// The file's size in bytes, the WAL included.
    pub(crate) fn bytes(&self) -> u64 {
        ["", "-wal"]
            .iter()
            .filter_map(|suffix| std::fs::metadata(format!("{}{suffix}", self.path.display())).ok())
            .map(|m| m.len())
            .sum()
    }

    /// The bytes the database's live pages hold: what eviction shrinks,
    /// whether or not freed pages have gone back to the file system yet.
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

    /// The entry for `(path, file_id, source)`, and for tags only when its
    /// extractor is `extractor`.
    pub(crate) fn entry(
        &self,
        path: &str,
        file_id: &[u8],
        source: Source,
        extractor: Option<&str>,
    ) -> Result<Option<EntryRow>, StoreError> {
        self.read(|conn| entry_in(conn, path, file_id, source, extractor))
    }

    /// Entries for many files at once, as [`Store::entry`]: `None` where a
    /// file has none.
    pub(crate) fn entries(
        &self,
        files: &[(&str, &[u8])],
        source: Source,
        extractor: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Vec<Option<EntryRow>>, StoreError> {
        self.read(|conn| {
            files
                .iter()
                .map(|(path, id)| entry_in(conn, path, id, source, extractor(path).as_deref()))
                .collect()
        })
    }

    /// Writes `entries`, each replacing any entry with the same path, file
    /// id and source, in one transaction.
    pub(crate) fn put(&self, entries: &[NewEntry]) -> Result<(), StoreError> {
        let now = now_ms();
        self.write(|tx| {
            let mut interned = Interned::default();
            for entry in entries {
                put_one(tx, &mut interned, entry, now)?;
            }
            Ok(())
        })
    }

    /// The entry for `(path, file_id, source)` as [`Store::entry`] finds it,
    /// with its occurrences, read in one transaction: a writer replacing the
    /// entry between the two reads cannot leave it empty.
    pub(crate) fn entry_with_occurrences(
        &self,
        path: &str,
        file_id: &[u8],
        source: Source,
        extractor: Option<&str>,
    ) -> Result<Option<(EntryRow, Vec<Occurrence>)>, StoreError> {
        self.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let found = match entry_in(&tx, path, file_id, source, extractor)? {
                Some(row) => {
                    let occurrences = occurrences_of(&tx, row.id)?;
                    Some((row, occurrences))
                }
                None => None,
            };
            tx.finish()?;
            Ok(found)
        })
    }

    /// Every occurrence of `name` (or of `symbol`, when set), with its
    /// entry, from any source and any file version.
    pub(crate) fn find(
        &self,
        name: Option<&str>,
        symbol: Option<&str>,
    ) -> Result<Vec<Found>, StoreError> {
        self.read(|conn| {
            let (sql, key) = match (symbol, name) {
                (Some(symbol), _) => (
                    "SELECT o.file, o.seq FROM occurrences o JOIN symbols s ON o.symbol = s.id
                     WHERE s.symbol = ?1",
                    symbol,
                ),
                (None, Some(name)) => (
                    "SELECT o.file, o.seq FROM occurrences o JOIN names n ON o.name = n.id
                     WHERE n.name = ?1",
                    name,
                ),
                (None, None) => return Ok(Vec::new()),
            };
            let hits: Vec<(i64, u32)> = conn
                .prepare_cached(sql)?
                .query_map([key], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut by_entry: HashMap<i64, (EntryRow, Vec<u8>, Vec<Occurrence>)> = HashMap::new();
            let mut found = Vec::with_capacity(hits.len());
            for (file, seq) in hits {
                if let std::collections::hash_map::Entry::Vacant(slot) = by_entry.entry(file) {
                    let Some((row, file_id)) = entry_by_id(conn, file)? else {
                        continue;
                    };
                    slot.insert((row, file_id, occurrences_of(conn, file)?));
                }
                let (row, file_id, occurrences) = &by_entry[&file];
                let Some(occurrence) = occurrences.get(seq as usize) else {
                    continue;
                };
                let enclosing_symbol = occurrence
                    .enclosing
                    .and_then(|e| occurrences.get(e as usize))
                    .and_then(|e| e.symbol.clone());
                found.push(Found {
                    entry: row.clone(),
                    file_id: file_id.clone(),
                    seq,
                    occurrence: occurrence.clone(),
                    enclosing_symbol,
                });
            }
            Ok(found)
        })
    }

    /// A tree's row.
    pub(crate) fn tree(&self, key: &str) -> Result<Option<TreeRow>, StoreError> {
        self.read(|conn| {
            conn.prepare_cached(
                "SELECT commit_id, state, files, indexed, skipped, languages, scip_files, indexed_at
                 FROM trees WHERE tree = ?1",
            )?
            .query_row([key], |row| {
                let skipped: String = row.get(4)?;
                let languages: String = row.get(5)?;
                Ok(TreeRow {
                    commit_id: row.get(0)?,
                    complete: row.get::<_, i64>(1)? == 3,
                    counts: Counts {
                        files: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
                        indexed: row.get::<_, i64>(3)?.try_into().unwrap_or(0),
                        skipped: serde_json::from_str(&skipped).unwrap_or_default(),
                        languages: serde_json::from_str(&languages).unwrap_or_default(),
                        scip_files: row.get::<_, i64>(6)?.try_into().unwrap_or(0),
                    },
                    indexed_at: row.get(7)?,
                })
            })
            .optional()
        })
    }

    /// Records a tree's pass as complete, with its counts.
    pub(crate) fn put_tree(
        &self,
        key: &str,
        commit_id: &str,
        counts: &Counts,
    ) -> Result<(), StoreError> {
        let skipped = serde_json::to_string(&counts.skipped).unwrap_or_default();
        let languages = serde_json::to_string(&counts.languages).unwrap_or_default();
        let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO trees (tree, commit_id, state, files, indexed, skipped, languages,
                                    scip_files, indexed_at, used_at)
                 VALUES (?1, ?2, 3, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (tree) DO UPDATE SET commit_id = ?2, state = 3, files = ?3,
                     indexed = ?4, skipped = ?5, languages = ?6, scip_files = ?7,
                     indexed_at = ?8, used_at = ?9",
                params![
                    key,
                    commit_id,
                    n(counts.files),
                    n(counts.indexed),
                    skipped,
                    languages,
                    n(counts.scip_files),
                    now_ms(),
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    /// Adds `added` to a tree's SCIP count, when it has a row.
    pub(crate) fn add_scip_files(&self, key: &str, added: u64) -> Result<(), StoreError> {
        self.write(|tx| {
            tx.execute(
                "UPDATE trees SET scip_files = scip_files + ?2 WHERE tree = ?1",
                params![key, i64::try_from(added).unwrap_or(i64::MAX)],
            )?;
            Ok(())
        })
    }

    /// Marks the entries of `files`, and the trees `trees`, used at `at`.
    pub(crate) fn mark(
        &self,
        files: &[(String, Vec<u8>)],
        trees: &[String],
        at: i64,
    ) -> Result<(), StoreError> {
        self.write(|tx| {
            let mut entries = tx.prepare_cached(
                "UPDATE files SET used_at = ?3 WHERE path = ?1 AND file_id = ?2 AND used_at < ?3",
            )?;
            for (path, id) in files {
                entries.execute(params![path, id, at])?;
            }
            let mut rows = tx.prepare_cached("UPDATE trees SET used_at = ?2 WHERE tree = ?1")?;
            for tree in trees {
                rows.execute(params![tree, at])?;
            }
            Ok(())
        })
    }

    /// Refreshes the `used_at` of entries a read used, at most once a day.
    pub(crate) fn touch(&self, entries: &[i64]) -> Result<(), StoreError> {
        let now = now_ms();
        self.write(|tx| {
            let mut touch =
                tx.prepare_cached("UPDATE files SET used_at = ?2 WHERE id = ?1 AND used_at < ?3")?;
            for id in entries {
                touch.execute(params![id, now, now - TOUCH_EVERY])?;
            }
            Ok(())
        })
    }

    /// Deletes tree rows and entries last used before `before` (milliseconds
    /// since the epoch), except `live` trees, and tags entries whose
    /// extractor is not `current(language)`. Returns the entries deleted.
    pub(crate) fn sweep(
        &self,
        before: i64,
        live: &[String],
        current: &dyn Fn(&str) -> Option<String>,
    ) -> Result<u64, StoreError> {
        self.write(|tx| {
            let mut deleted = 0;
            let stale: Vec<String> = tx
                .prepare("SELECT tree FROM trees WHERE used_at < ?1")?
                .query_map([before], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for tree in stale.iter().filter(|t| !live.contains(t)) {
                tx.execute("DELETE FROM trees WHERE tree = ?1", [tree])?;
            }
            let old: Vec<i64> = tx
                .prepare("SELECT id FROM files WHERE used_at < ?1")?
                .query_map([before], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let outdated: Vec<i64> = tx
                .prepare("SELECT id, language, extractor FROM files WHERE source = 1")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .filter_map(Result::ok)
                .filter(|(_, language, extractor)| {
                    current(language).as_deref() != Some(extractor.as_str())
                })
                .map(|(id, ..)| id)
                .collect();
            let any_outdated = !outdated.is_empty();
            for id in old.into_iter().chain(outdated) {
                deleted += delete_entry(tx, id)?;
            }
            // A row says its tree is complete; one whose entries may be gone
            // must not, or a name query would answer from part of it. Live
            // trees' entries were marked and stay, unless they were of an
            // outdated extractor.
            if any_outdated {
                tx.execute("DELETE FROM trees", [])?;
            } else if deleted > 0 {
                drop_trees_but(tx, live)?;
            }
            prune_interned(tx)?;
            Ok(deleted)
        })
    }

    /// Deletes up to `batch` entries not marked at `marked` (used before
    /// it), least recently used first, and every tree row but `live`'s, whose
    /// entries are all marked. Returns how many entries. Names and symbols
    /// are left to [`Store::prune`].
    pub(crate) fn evict(
        &self,
        marked: i64,
        batch: usize,
        live: &[String],
    ) -> Result<u64, StoreError> {
        self.write(|tx| {
            let ids: Vec<i64> = tx
                .prepare("SELECT id FROM files WHERE used_at < ?1 ORDER BY used_at, id LIMIT ?2")?
                .query_map(
                    params![marked, i64::try_from(batch).unwrap_or(i64::MAX)],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<_>>()?;
            let mut deleted = 0;
            for id in ids {
                deleted += delete_entry(tx, id)?;
            }
            if deleted > 0 {
                drop_trees_but(tx, live)?;
            }
            Ok(deleted)
        })
    }

    /// Evicts unmarked entries, `batch` at a time and least recently used
    /// first, while the index holds more than `max` bytes; then prunes names
    /// and symbols and returns the freed pages. Returns the entries evicted,
    /// and whether marked entries alone still pass `max`.
    ///
    /// The size is estimated from the share of occurrences left: deletes
    /// leave B-tree pages part full, so the live page count falls far more
    /// slowly than the data and would evict nearly everything.
    pub(crate) fn cap(
        &self,
        max: u64,
        marked: i64,
        live: &[String],
        batch: usize,
    ) -> Result<(u64, bool), StoreError> {
        let mut evicted = 0;
        let mut over = false;
        // Names and symbols the evicted entries used are pruned after
        // batches 1, 2, 4, 8 and so on: each prune scans the occurrences, so
        // once per batch would be quadratic, and never would leave them
        // holding the space.
        let (mut batches, mut prune_at, mut unpruned) = (0_u64, 1_u64, false);
        let start = self.weight()?.max(1);
        let per = self.live_bytes()? / start;
        while self.weight()?.saturating_mul(per.max(1)) > max {
            let n = self.evict(marked, batch, live)?;
            if n == 0 {
                if unpruned {
                    self.prune()?;
                    unpruned = false;
                    continue;
                }
                over = true;
                break;
            }
            evicted += n;
            batches += 1;
            unpruned = true;
            if batches == prune_at {
                self.prune()?;
                unpruned = false;
                prune_at *= 2;
            }
        }
        if unpruned {
            self.prune()?;
        }
        self.vacuum()?;
        Ok((evicted, over))
    }

    /// Occurrences stored, plus one per entry: what the index's size is
    /// proportional to.
    fn weight(&self) -> Result<u64, StoreError> {
        self.read(|conn| {
            conn.query_row(
                "SELECT coalesce(sum(occurrences), 0) + count(*) FROM files",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|w| u64::try_from(w).unwrap_or(0))
    }

    /// Deletes names and symbols no occurrence refers to.
    pub(crate) fn prune(&self) -> Result<(), StoreError> {
        self.write(prune_interned)
    }

    /// Returns freed pages to the file system and folds the WAL in.
    pub(crate) fn vacuum(&self) -> Result<(), StoreError> {
        let conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        (|| {
            // One row per page freed: it frees pages only as it is stepped.
            let mut vacuum = conn.prepare("PRAGMA incremental_vacuum")?;
            let mut rows = vacuum.query([])?;
            while rows.next()?.is_some() {}
            drop(rows);
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        })()
        .map_err(|e| self.sqlite(e))
    }

    /// Entries in all.
    pub(crate) fn entry_count(&self) -> Result<u64, StoreError> {
        self.read(|conn| {
            conn.query_row("SELECT count(*) FROM files", [], |row| row.get::<_, i64>(0))
                .map(|n| u64::try_from(n).unwrap_or(0))
        })
    }
}

fn entry_in(
    conn: &Connection,
    path: &str,
    file_id: &[u8],
    source: Source,
    extractor: Option<&str>,
) -> rusqlite::Result<Option<EntryRow>> {
    conn.prepare_cached(
        "SELECT id, extractor, language, skip FROM files
         WHERE path = ?1 AND file_id = ?2 AND source = ?3",
    )?
    .query_row(params![path, file_id, source as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<u8>>(3)?,
        ))
    })
    .optional()
    .map(|found| {
        found
            .filter(|(_, stored, ..)| extractor.is_none_or(|want| want == stored))
            .map(|(id, extractor, language, skip)| EntryRow {
                id,
                path: path.to_owned(),
                source,
                extractor,
                language,
                skip,
            })
    })
}

fn entry_by_id(conn: &Connection, id: i64) -> rusqlite::Result<Option<(EntryRow, Vec<u8>)>> {
    conn.prepare_cached(
        "SELECT path, file_id, source, language, skip, extractor FROM files WHERE id = ?1",
    )?
    .query_row([id], |row| {
        Ok((
            EntryRow {
                id,
                path: row.get(0)?,
                source: Source::from_i64(row.get(2)?),
                extractor: row.get(5)?,
                language: row.get(3)?,
                skip: row.get(4)?,
            },
            row.get(1)?,
        ))
    })
    .optional()
}

/// Ids of names and symbols written in one transaction.
#[derive(Default)]
struct Interned {
    names: HashMap<String, i64>,
    symbols: HashMap<String, i64>,
}

impl Interned {
    fn name(&mut self, tx: &Transaction<'_>, name: &str) -> rusqlite::Result<i64> {
        if let Some(&id) = self.names.get(name) {
            return Ok(id);
        }
        tx.prepare_cached("INSERT INTO names (name) VALUES (?1) ON CONFLICT (name) DO NOTHING")?
            .execute([name])?;
        let id = tx
            .prepare_cached("SELECT id FROM names WHERE name = ?1")?
            .query_row([name], |row| row.get(0))?;
        self.names.insert(name.to_owned(), id);
        Ok(id)
    }

    fn symbol(&mut self, tx: &Transaction<'_>, symbol: &str, name: i64) -> rusqlite::Result<i64> {
        if let Some(&id) = self.symbols.get(symbol) {
            return Ok(id);
        }
        tx.prepare_cached(
            "INSERT INTO symbols (symbol, name) VALUES (?1, ?2) ON CONFLICT (symbol) DO NOTHING",
        )?
        .execute(params![symbol, name])?;
        let id = tx
            .prepare_cached("SELECT id FROM symbols WHERE symbol = ?1")?
            .query_row([symbol], |row| row.get(0))?;
        self.symbols.insert(symbol.to_owned(), id);
        Ok(id)
    }
}

fn delete_entry(tx: &Transaction<'_>, id: i64) -> rusqlite::Result<u64> {
    tx.prepare_cached("DELETE FROM occurrences WHERE file = ?1")?
        .execute([id])?;
    let n = tx
        .prepare_cached("DELETE FROM files WHERE id = ?1")?
        .execute([id])?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Deletes every tree row but `live`'s.
fn drop_trees_but(tx: &Transaction<'_>, live: &[String]) -> rusqlite::Result<()> {
    let trees: Vec<String> = tx
        .prepare("SELECT tree FROM trees")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for tree in trees.iter().filter(|t| !live.contains(t)) {
        tx.execute("DELETE FROM trees WHERE tree = ?1", [tree])?;
    }
    Ok(())
}

/// Deletes names and symbols no occurrence refers to.
fn prune_interned(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "DELETE FROM symbols WHERE id NOT IN (SELECT symbol FROM occurrences WHERE symbol IS NOT NULL);
         DELETE FROM names WHERE id NOT IN (SELECT name FROM occurrences)
                             AND id NOT IN (SELECT name FROM symbols);",
    )
}

fn put_one(
    tx: &Transaction<'_>,
    interned: &mut Interned,
    entry: &NewEntry,
    now: i64,
) -> rusqlite::Result<()> {
    let existing: Option<(i64, String)> = tx
        .prepare_cached(
            "SELECT id, extractor FROM files WHERE path = ?1 AND file_id = ?2 AND source = ?3",
        )?
        .query_row(
            params![entry.path, entry.file_id, entry.source as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((id, extractor)) = existing {
        // Extraction is deterministic: a pass and a read racing on one file
        // write the same tags, and the one already there keeps its id.
        if entry.source == Source::Tags && extractor == entry.extractor {
            return Ok(());
        }
        delete_entry(tx, id)?;
    }
    tx.prepare_cached(
        "INSERT INTO files (path, file_id, source, extractor, language, skip, has_errors,
                            occurrences, created_at, used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
    )?
    .execute(params![
        entry.path,
        entry.file_id,
        entry.source as i64,
        entry.extractor,
        entry.language,
        entry.skip,
        entry.has_errors,
        i64::try_from(entry.occurrences.len()).unwrap_or(i64::MAX),
        now
    ])?;
    let file = tx.last_insert_rowid();
    let mut insert = tx.prepare_cached(
        "INSERT INTO occurrences (file, seq, symbol, name, role, kind, flags, line, col,
                                  end_line, end_col, def_line, def_col, def_end_line, def_end_col,
                                  enclosing, digest)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?;
    for (seq, o) in entry.occurrences.iter().enumerate() {
        let name = interned.name(tx, &o.name)?;
        let symbol = match &o.symbol {
            Some(symbol) => Some(interned.symbol(tx, symbol, name)?),
            None => None,
        };
        let d = o.definition_range;
        insert.execute(params![
            file,
            i64::try_from(seq).unwrap_or(i64::MAX),
            symbol,
            name,
            o.role as u8,
            o.kind as u8,
            o.flags,
            o.range.start_line,
            o.range.start_col,
            o.range.end_line,
            o.range.end_col,
            d.map(|r| r.start_line),
            d.map(|r| r.start_col),
            d.map(|r| r.end_line),
            d.map(|r| r.end_col),
            o.enclosing,
            // SQLite integers are signed; the digest's bits are kept.
            o.digest.map(|v| i64::from_le_bytes(v.to_le_bytes())),
        ])?;
    }
    Ok(())
}

fn occurrences_of(conn: &Connection, entry: i64) -> rusqlite::Result<Vec<Occurrence>> {
    conn.prepare_cached(
        "SELECT s.symbol, n.name, o.role, o.kind, o.flags, o.line, o.col, o.end_line, o.end_col,
                o.def_line, o.def_col, o.def_end_line, o.def_end_col, o.enclosing, o.digest
         FROM occurrences o JOIN names n ON o.name = n.id LEFT JOIN symbols s ON o.symbol = s.id
         WHERE o.file = ?1 ORDER BY o.seq",
    )?
    .query_map([entry], |row| {
        let definition_range = match (
            row.get::<_, Option<u32>>(9)?,
            row.get::<_, Option<u32>>(10)?,
            row.get::<_, Option<u32>>(11)?,
            row.get::<_, Option<u32>>(12)?,
        ) {
            (Some(start_line), Some(start_col), Some(end_line), Some(end_col)) => Some(Range {
                start_line,
                start_col,
                end_line,
                end_col,
            }),
            _ => None,
        };
        Ok(Occurrence {
            symbol: row.get(0)?,
            name: row.get(1)?,
            role: Role::from_u8(row.get(2)?).unwrap_or(Role::Reference),
            kind: Kind::from_u8(row.get(3)?).unwrap_or(Kind::Unspecified),
            flags: row.get(4)?,
            range: Range {
                start_line: row.get(5)?,
                start_col: row.get(6)?,
                end_line: row.get(7)?,
                end_col: row.get(8)?,
            },
            definition_range,
            enclosing: row.get(13)?,
            digest: row
                .get::<_, Option<i64>>(14)?
                .map(|v| u64::from_le_bytes(v.to_le_bytes())),
        })
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str, extractors: &str) -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!(
            "reef-symbols-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("symbols.sqlite3");
        let store = Store::open(&path, 2, extractors).unwrap();
        (dir, store)
    }

    fn entry(path: &str, id: u8, occurrences: usize) -> NewEntry {
        let occurrence = |i: usize| Occurrence {
            symbol: Some(format!("reef-tags . . . `{path}`/f{i}().")),
            name: format!("f{i}"),
            kind: Kind::Function,
            role: Role::Definition,
            flags: 0,
            range: Range::default(),
            definition_range: Some(Range::default()),
            enclosing: None,
            digest: Some(1),
        };
        NewEntry {
            path: path.to_owned(),
            file_id: vec![id; 20],
            source: Source::Tags,
            extractor: "tags/1/python/0.25.0".into(),
            language: "python".into(),
            skip: None,
            has_errors: false,
            occurrences: (0..occurrences).map(occurrence).collect(),
        }
    }

    /// GC's cap evicts unmarked entries, least recently used first, until
    /// the live pages fit, and never a marked one; the freed pages go back.
    #[test]
    fn cap_evicts_unmarked_entries_until_under() {
        let (dir, store) = temp_store("cap", "x");
        let entries: Vec<NewEntry> = (0..400)
            .map(|i| entry(&format!("f{i}.py"), 1, 100))
            .collect();
        store.put(&entries).unwrap();
        store.put_tree("live", "c1", &Counts::default()).unwrap();
        store.put_tree("dead", "c2", &Counts::default()).unwrap();
        let marked = now_ms() + 1;
        let live: Vec<(String, Vec<u8>)> =
            (0..40).map(|i| (format!("f{i}.py"), vec![1; 20])).collect();
        store.mark(&live, &["live".to_owned()], marked).unwrap();
        let full = store.live_bytes().unwrap();
        let target = full / 4;
        let (evicted, over) = store.cap(target, marked, &["live".to_owned()], 50).unwrap();
        assert!(!over, "the marked tenth fits in a quarter");
        assert!((250..360).contains(&evicted), "{evicted}");
        for (path, id) in &live {
            assert!(
                store.entry(path, id, Source::Tags, None).unwrap().is_some(),
                "{path} is marked"
            );
        }
        assert!(
            store.tree("dead").unwrap().is_none(),
            "a row whose entries may be gone is dropped"
        );
        assert!(store.tree("live").unwrap().is_some());
        assert!(
            store.bytes() < full,
            "freed pages return to the file system"
        );

        // Marked entries alone over the cap: they stay, and it says so.
        let (_, over) = store.cap(1, marked, &["live".to_owned()], 50).unwrap();
        assert!(over);
        assert_eq!(store.entry_count().unwrap(), 40);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Tree rows made under other extractors are dropped at open, so a tree
    /// complete for an older extractor is indexed again.
    #[test]
    fn new_extractors_drop_tree_rows() {
        let (dir, store) = temp_store("extractors", "tags/1/a");
        store.put_tree("t", "c", &Counts::default()).unwrap();
        drop(store);
        let path = dir.join("symbols.sqlite3");
        let same = Store::open(&path, 2, "tags/1/a").unwrap();
        assert!(same.tree("t").unwrap().is_some());
        drop(same);
        let newer = Store::open(&path, 2, "tags/2/a").unwrap();
        assert!(newer.tree("t").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Writing the same tags again keeps the entry and its id.
    #[test]
    fn same_tags_keep_their_entry() {
        let (dir, store) = temp_store("same", "x");
        store.put(&[entry("a.py", 1, 3)]).unwrap();
        let before = store
            .entry("a.py", &[1; 20], Source::Tags, None)
            .unwrap()
            .unwrap();
        store.put(&[entry("a.py", 1, 3)]).unwrap();
        let after = store
            .entry("a.py", &[1; 20], Source::Tags, None)
            .unwrap()
            .unwrap();
        assert_eq!(before.id, after.id);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
