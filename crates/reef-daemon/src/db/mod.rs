//! The daemon's metadata database (ADR 0003): registered repositories,
//! workspaces, leases, the reclaim log, the base cache, shared stores,
//! remote tokens, evidence (ADR 0055), the artifact store's index (ADR
//! 0056), operator identities and session keys (ADR 0057), provenance (ADR
//! 0058), and executor jobs (ADR 0059), in one SQLite file per machine.
//!
//! [`Db`] owns one connection behind a [`std::sync::Mutex`]; every method
//! takes the lock for one short statement or transaction, so `&Db` can be
//! shared across threads. The daemon is SQLite's only writer. The methods
//! are synchronous and none is slow; a caller on a tokio worker may call
//! them directly.
//!
//! The schema is a list of numbered migrations. `PRAGMA user_version`
//! counts the ones applied, and `meta` records which. A database with
//! migrations this binary does not know is refused, never downgraded. One
//! step after the migrations is Rust, not SQL: `index_evidence_artifacts`
//! (R3.02), which reads stored envelopes once. Records are defined in
//! [`crate::model`]; the queries are in this module's submodules, one per
//! table.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rand::RngExt as _;
use reef_lib::error::{Classify, ErrorKind};
use rusqlite::{Connection, ErrorCode, OptionalExtension as _, Transaction, TransactionBehavior};

use crate::error::{Advice, Advise};
use crate::model::{Timestamp, ids::ID_HEX_LEN};

mod artifacts;
mod base_cache;
mod evidence;
mod identity;
pub(crate) mod jobs;
pub(crate) mod landings;
mod leases;
pub(crate) mod lifecycle;
mod provenance;
mod reclaims;
mod remote_tokens;
mod repositories;
mod stores;
mod workspaces;

pub use artifacts::{
    ARTIFACT_DIGEST_LEN, ARTIFACT_EVIDENCE_IDS, ArtifactRow, ArtifactState, PlannedArtifact,
    index_artifacts_in,
};
pub use evidence::{
    EVIDENCE_ID_LEN, EvidenceFilter, EvidenceKeyRow, EvidenceLinkRow, EvidenceRow, Inserted,
    LIVE_PINNED, PINNED, StoredEvidence, add_evidence_in, insert_evidence_in,
};
pub use identity::{
    AuthorityRow, IdentityRow, Inserted as KeyInserted, KeyRecords, RevocationRow,
    SessionKeyFilter, SessionKeyRow, SessionRow,
};
pub use jobs::{
    ExecutorColumns, JobCursor, JobEvidenceRow, JobFilter, JobRow, JobState,
    insert_job_evidence_in, insert_job_in, job_evidence_in, job_in, next_job_evidence_seq_in,
    open_job_by_spec_in, open_job_count_in, queued_jobs_in, repository_exists_in, update_job_in,
};
pub use provenance::{
    DeclaredColumns, ProvenanceFilter, ProvenanceRow, ProvenanceSessionRow, StoredProvenance,
};
pub use remote_tokens::{Digest, RemoteTokenRow, TokenScope};

pub use landings::{LandingChangeRow, LandingFilter, LandingRow, StoredLanding};
pub use leases::Renewal;
pub use lifecycle::{
    LifecycleFilter, LifecycleRow, LifecycleUpdate, TransitionFilter, TransitionRow,
};

/// The migrations, in number order. A migration's number is fixed by the
/// Phase 1 plan, not by landing order: `0002_base_cache` (R1.02),
/// `0003_snapshots` (R1.16), `0004_stores` (R1.20), `0005_watchman` (R1.14),
/// then `0006_remote_tokens` (R2.08), `0007_evidence` (R3.01),
/// `0008_artifacts` (R3.02), `0009_identity` (R3.03), `0010_provenance`
/// (R3.04), `0011_jobs` (R3.05), `0012_lifecycle` (R3.09) and
/// `0013_landings` (R3.13).
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
    (
        "0006_remote_tokens",
        include_str!("migrations/0006_remote_tokens.sql"),
    ),
    (
        "0007_evidence",
        include_str!("migrations/0007_evidence.sql"),
    ),
    (
        "0008_artifacts",
        include_str!("migrations/0008_artifacts.sql"),
    ),
    (
        "0009_identity",
        include_str!("migrations/0009_identity.sql"),
    ),
    (
        "0010_provenance",
        include_str!("migrations/0010_provenance.sql"),
    ),
    ("0011_jobs", include_str!("migrations/0011_jobs.sql")),
    (
        "0012_lifecycle",
        include_str!("migrations/0012_lifecycle.sql"),
    ),
    (
        "0013_landings",
        include_str!("migrations/0013_landings.sql"),
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
/// `meta` key of when GC's last full run ended, as decimal unix
/// milliseconds: written by timed runs and by manual runs over every
/// repository, read by the GC timer (#95).
pub const META_LAST_GC_AT: &str = "last_gc_at";
/// `meta` key set by a restore (R1.18): the daemon's next start
/// rematerializes every active workspace whose directory or working-copy
/// state is missing instead of reclaiming it, then clears it. `1` when set.
pub const META_REMATERIALIZE_PENDING: &str = "rematerialize_pending";
/// `meta` key of the backup directory the database was restored from.
pub const META_RESTORED_FROM: &str = "restored_from";
/// `meta` key of when the restore ran, as decimal unix milliseconds.
pub const META_RESTORED_AT: &str = "restored_at";
/// `meta` key set once the database's first open under schema 8 has indexed the
/// artifacts of the evidence stored before `0008_artifacts` (R3.02), as
/// decimal unix milliseconds.
pub const META_EVIDENCE_ARTIFACTS_INDEXED_AT: &str = "evidence_artifacts_indexed_at";
/// `meta` key of the operator identity the daemon created at its first
/// start under schema 9 (R3.03): the default identity, unless
/// `[session_keys] default_identity` names another.
pub const META_DEFAULT_IDENTITY_ID: &str = "default_identity_id";
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
    /// The database file could not be created or restricted to `0600`
    /// (RX.01, S-01).
    #[error("restrict database {} to 0600", path.display())]
    Mode {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
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
            Self::Mode { source, .. } => reef_lib::error::classify_by_source(source),
            Self::Open { source, .. } | Self::Migrate { source, .. } | Self::Sqlite(source) => {
                classify_sqlite(source)
            }
        }
    }
}

impl Advise for DbError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Ambiguous {
                what,
                prefix,
                candidates,
                truncated,
            } => Some(Advice::hint(format!(
                "Give more of the id: {prefix} matches {what} {}.",
                candidates_text(candidates, *truncated)
            ))),
            Self::NotFound {
                what: "workspace", ..
            } => Some(Advice::run(
                "List the workspaces to find its id.",
                ["reef", "ws", "list", "--all"],
            )),
            Self::NotFound {
                what: "repository", ..
            } => Some(Advice::run(
                "List the registered repositories to find its id, or name it by a path inside it.",
                ["reef", "repo", "list"],
            )),
            _ => None,
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

/// Creates the SQLite file `path` with mode `0600` if it is missing, and
/// `chmod`s it and any `-wal` and `-shm` beside it to `0600` if they have
/// group or other bits (RX.01, S-01). SQLite gives the journal files it
/// creates the database's mode, so they follow.
pub(crate) fn private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let restrict = |file: &std::fs::File| -> std::io::Result<()> {
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    restrict(&file)?;
    for suffix in ["-wal", "-shm"] {
        let mut journal = path.as_os_str().to_owned();
        journal.push(suffix);
        match std::fs::OpenOptions::new().write(true).open(&journal) {
            Ok(file) => restrict(&file)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Db {
    /// Opens the database at `path`, creating the file if it does not exist,
    /// sets the connection's pragmas (WAL, foreign keys, a 5 s busy timeout,
    /// `synchronous = NORMAL`), and applies pending migrations. The parent
    /// directory must exist. The file is `0600` whatever the umask, an
    /// existing one `chmod`ed (RX.01).
    pub fn open(path: &Path) -> Result<Db, DbError> {
        let open = |source| DbError::Open {
            path: path.to_owned(),
            source,
        };
        private_file(path).map_err(|source| DbError::Mode {
            path: path.to_owned(),
            source,
        })?;
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
        index_evidence_artifacts(&mut conn)?;
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

    /// The value stored under `key` in `meta`, if any.
    pub fn meta(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with(|conn| {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
        })
    }

    /// Rewrites every path recorded under the state directory `from` to the
    /// same place under `to`: workspace directories and state directories,
    /// base cache entries, and stores. A restore into another state
    /// directory does this (R1.18). Returns how many values changed.
    pub fn relocate_state_dir(&self, from: &Path, to: &Path) -> Result<usize, DbError> {
        let (from, to) = (path_text(from)?, path_text(to)?);
        self.in_transaction(|tx| {
            let mut changed = 0;
            for (table, column) in [
                ("workspaces", "path"),
                ("workspaces", "state_dir"),
                ("base_cache", "path"),
                ("stores", "path"),
            ] {
                changed += tx.execute(
                    &format!(
                        "UPDATE {table} SET {column} = ?2 || substr({column}, length(?1) + 1)
                         WHERE {column} = ?1 OR substr({column}, 1, length(?1) + 1) = ?1 || '/'"
                    ),
                    [from, to],
                )?;
            }
            Ok(changed)
        })
    }

    /// Removes `key` from `meta`.
    pub fn delete_meta(&self, key: &str) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute("DELETE FROM meta WHERE key = ?1", [key])
                .map(drop)
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

/// The one Rust step after the migrations (R3.02): while the `meta` key
/// [`META_EVIDENCE_ARTIFACTS_INDEXED_AT`] is unset, reads every stored
/// evidence envelope's payload, records the artifact digests it names in
/// `evidence_artifacts`, and sets the key, in one `IMMEDIATE` transaction.
/// Evidence stored under schema 7 has no index rows, and SQL cannot read the
/// base64 payloads. The signature is not checked: the index only steers
/// retention. A row whose payload does not parse indexes nothing, and is
/// logged.
fn index_evidence_artifacts(conn: &mut Connection) -> Result<(), DbError> {
    let failed = |source| DbError::Migrate {
        name: META_EVIDENCE_ARTIFACTS_INDEXED_AT.to_owned(),
        source,
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(failed)?;
    let done = tx
        .query_row(
            "SELECT 1 FROM meta WHERE key = ?1",
            [META_EVIDENCE_ARTIFACTS_INDEXED_AT],
            |_| Ok(()),
        )
        .optional()
        .map_err(failed)?
        .is_some();
    if done {
        return Ok(());
    }
    let (mut indexed, mut skipped) = (0u64, 0u64);
    {
        // One row at a time: the envelopes are never all in memory.
        let mut select = tx
            .prepare("SELECT repo_id, id, envelope FROM evidence")
            .map_err(failed)?;
        let mut insert = tx
            .prepare(
                "INSERT INTO evidence_artifacts(repo_id, evidence_id, sha256) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO NOTHING",
            )
            .map_err(failed)?;
        let mut rows = select.query([]).map_err(failed)?;
        while let Some(row) = rows.next().map_err(failed)? {
            let (repo, id, envelope): (String, String, Vec<u8>) = (
                row.get(0).map_err(failed)?,
                row.get(1).map_err(failed)?,
                row.get(2).map_err(failed)?,
            );
            let Some(digests) = evidence::payload_artifacts(&envelope) else {
                tracing::warn!(
                    repo = %repo,
                    evidence_id = %id,
                    "a stored evidence payload does not parse; its artifacts are not indexed"
                );
                skipped += 1;
                continue;
            };
            for digest in digests {
                insert.execute((&repo, &id, digest)).map_err(failed)?;
            }
            indexed += 1;
        }
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)",
        (
            META_EVIDENCE_ARTIFACTS_INDEXED_AT,
            Timestamp::now().0.to_string(),
        ),
    )
    .map_err(failed)?;
    tx.commit().map_err(failed)?;
    if indexed + skipped > 0 {
        tracing::info!(
            evidence_count = indexed,
            skipped_count = skipped,
            "indexed the artifacts of stored evidence"
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
    normalize_prefix_up_to(what, prefix, ID_HEX_LEN)
}

/// [`normalize_prefix`] for ids of `max` hex digits.
pub(crate) fn normalize_prefix_up_to(
    what: &'static str,
    prefix: &str,
    max: usize,
) -> Result<String, DbError> {
    let valid = (MIN_PREFIX_LEN..=max).contains(&prefix.len())
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
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn open_creates_schema_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        let instance_id = db.meta(META_DAEMON_INSTANCE_ID).unwrap().unwrap();
        assert!(instance_id.parse::<crate::model::RepoId>().is_ok());
        let created_at = db.meta(META_CREATED_AT).unwrap().unwrap();
        assert!(created_at.parse::<i64>().unwrap() > 0);
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
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
                    supported: 13
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
                "0005_watchman",
                "0006_remote_tokens",
                "0007_evidence",
                "0008_artifacts",
                "0009_identity",
                "0010_provenance",
                "0011_jobs",
                "0012_lifecycle",
                "0013_landings"
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
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(
            db.with(applied_migrations).unwrap(),
            [
                "0001_initial",
                "0002_base_cache",
                "0003_snapshots",
                "0004_stores",
                "0005_watchman",
                "0006_remote_tokens",
                "0007_evidence",
                "0008_artifacts",
                "0009_identity",
                "0010_provenance",
                "0011_jobs",
                "0012_lifecycle",
                "0013_landings"
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
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(db.repository(record.id).unwrap().tracked_files, None);
        assert_eq!(
            db.workspace(ws.id).unwrap().watcher,
            crate::model::WatcherKind::Notify
        );
    }

    // R3.01 test 8: a schema 6 database gains the evidence tables with its
    // rows intact; one at 8 is refused.
    #[test]
    fn migration_7() {
        use crate::db::repositories::tests::repo;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..6].iter().enumerate() {
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
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        assert_eq!(db.repository(record.id).unwrap().root, record.root);
        assert_eq!(db.evidence_counts().unwrap(), (0, 0, 0));
        drop(db);

        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 14).unwrap();
        drop(conn);
        let error = Db::open(&path).err().unwrap();
        assert!(
            matches!(
                error,
                DbError::TooNew {
                    found: 14,
                    supported: 13
                }
            ),
            "{error:?}"
        );
    }

    /// A stored envelope naming `artifacts`, for [`migration_8`]: unsigned,
    /// since the backfill does not check signatures.
    fn envelope_naming(artifacts: &[&str]) -> Vec<u8> {
        use base64::Engine as _;
        let statement = serde_json::json!({
            "_type": reef_evidence::STATEMENT_TYPE,
            "subject": [{"name": "tree", "digest": {"gitTree": "0".repeat(40)}}],
            "predicateType": reef_evidence::PREDICATE_TYPE,
            "predicate": {
                "kind": "test",
                "result": "pass",
                "startedOn": "2026-11-05T10:00:00Z",
                "finishedOn": "2026-11-05T10:01:00Z",
                "artifacts": artifacts
                    .iter()
                    .map(|sha| serde_json::json!({"name": "a.log", "digest": {"sha256": sha}}))
                    .collect::<Vec<_>>(),
            },
        });
        let payload = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&statement).unwrap());
        serde_json::to_vec(&serde_json::json!({
            "payload": payload,
            "payloadType": reef_evidence::PAYLOAD_TYPE,
            "signatures": [{"keyid": "k1", "sig": "AAAA"}],
        }))
        .unwrap()
    }

    // R3.02 test 1: a schema 7 database with evidence migrates to 8, and the
    // backfill indexes what the stored payloads name, once.
    #[test]
    fn migration_8() {
        use crate::db::repositories::tests::repo;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..7].iter().enumerate() {
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
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
             VALUES ('k1', 'ed25519', x'00', 1)",
            [],
        )
        .unwrap();
        let (a, b, c) = ("aa".repeat(32), "bb".repeat(32), "cc".repeat(32));
        for (id, envelope) in [
            ("e1", envelope_naming(&[&a, &b])),
            ("e2", envelope_naming(&[&b, &c])),
            ("e3", b"not an envelope".to_vec()),
        ] {
            conn.execute(
                "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, key_id,
                                      started_at, finished_at, added_at)
                 VALUES (?1, ?2, ?3, 1, 't', 'test', 'pass', 'k1', 1, 2, 3)",
                rusqlite::params![record.id, id, envelope],
            )
            .unwrap();
        }
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        assert_eq!(
            db.artifacts_of_evidence(record.id, "e1").unwrap(),
            [a.clone(), b.clone()]
        );
        assert_eq!(
            db.artifacts_of_evidence(record.id, "e2").unwrap(),
            [b.clone(), c.clone()]
        );
        assert_eq!(db.artifacts_of_evidence(record.id, "e3").unwrap().len(), 0);
        let distinct: BTreeSet<String> = ["e1", "e2"]
            .iter()
            .flat_map(|id| db.artifacts_of_evidence(record.id, id).unwrap())
            .collect();
        assert_eq!(distinct, BTreeSet::from([a, b, c]));
        let indexed_at = db
            .meta(META_EVIDENCE_ARTIFACTS_INDEXED_AT)
            .unwrap()
            .unwrap();
        // The next open does not run it again: an index row removed stays
        // removed.
        db.with(|conn| {
            conn.execute(
                "DELETE FROM evidence_artifacts WHERE evidence_id = 'e2'",
                [],
            )
        })
        .unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.artifacts_of_evidence(record.id, "e2").unwrap().len(), 0);
        assert_eq!(
            db.meta(META_EVIDENCE_ARTIFACTS_INDEXED_AT)
                .unwrap()
                .unwrap(),
            indexed_at
        );
    }

    // R3.03 test 4: a schema 8 database with evidence and remote tokens
    // migrates to 9 with its rows intact; the tokens gain no link.
    #[test]
    fn migration_9() {
        use crate::db::repositories::tests::repo;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..8].iter().enumerate() {
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
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
             VALUES ('k1', 'ed25519', x'00', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, key_id,
                                  started_at, finished_at, added_at)
             VALUES (?1, 'e1', x'7b7d', 1, 't', 'test', 'pass', 'k1', 1, 2, 3)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO remote_tokens(id, name, scope, digest, created_at)
             VALUES ('00112233aabbccdd', 'laptop', 'write', zeroblob(32), 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        assert_eq!(db.evidence_counts().unwrap(), (1, 0, 1));
        let tokens = db.remote_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "laptop");
        assert_eq!(tokens[0].identity_id, None);
        assert_eq!(db.identity_counts().unwrap(), (0, 0, 0, 0));
    }

    // R3.04 test 4: a schema 9 database with evidence, identities and
    // workspaces migrates to 10 with its rows intact.
    #[test]
    fn migration_10() {
        use crate::db::repositories::tests::repo;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..9].iter().enumerate() {
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
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
             VALUES ('k1', 'ed25519', x'00', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, key_id,
                                  started_at, finished_at, added_at)
             VALUES (?1, 'e1', x'7b7d', 1, 't', 'test', 'pass', 'k1', 1, 2, 3)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identities(id, name, kind, created_at)
             VALUES ('idn_0000000000000001', 'alice', 'person', 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        assert_eq!(db.evidence_counts().unwrap(), (1, 0, 1));
        assert_eq!(db.identities().unwrap().len(), 1);
        assert_eq!(db.provenance_counts().unwrap(), (0, 0));
    }

    // R3.05 test 1: a schema 10 database with evidence, provenance and
    // workspaces migrates to 11 with its rows intact.
    #[test]
    fn migration_11() {
        use crate::db::repositories::tests::repo;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reef.sqlite3");
        let conn = Connection::open(&path).unwrap();
        for (index, (name, sql)) in MIGRATIONS[..10].iter().enumerate() {
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
        conn.execute(
            "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
             VALUES (?1, '/src/app', '/src/app/.jj/repo', 0, 1000)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
             VALUES ('k1', 'ed25519', x'00', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, key_id,
                                  started_at, finished_at, added_at)
             VALUES (?1, 'e1', x'7b7d', 1, 't', 'test', 'pass', 'k1', 1, 2, 3)",
            [record.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provenance_sessions(session_id, repo_id, workspace_id, actor, channel,
                                             started_at)
             VALUES ('ses_0000000000000001', ?1, 'w1', 'agent', 'socket', 1)",
            [record.id],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 13);
        assert_eq!(SCHEMA_VERSION, 13);
        assert_eq!(db.evidence_counts().unwrap(), (1, 0, 1));
        assert_eq!(db.repository(record.id).unwrap().root, record.root);
        assert_eq!(db.open_jobs().unwrap(), Vec::new());
        let sessions: i64 = db
            .with(|conn| {
                conn.query_row("SELECT count(*) FROM provenance_sessions", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(sessions, 1);
    }

    // R1.18: a restore into another state directory moves every path under
    // the old one, and only those; a path that merely shares the prefix
    // text stays.
    #[test]
    fn relocate_moves_paths_under_the_state_dir_only() {
        use crate::db::repositories::tests::repo;
        use crate::db::workspaces::tests::workspace;
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let mut inside = workspace(record.id, "bbbb000000000001", "/old/state/repos/a/ws/1");
        inside.state_dir = "/old/state/repos/a/wc/1".into();
        let mut outside = workspace(record.id, "bbbb000000000002", "/old/state-other/ws/2");
        outside.state_dir = "/elsewhere/wc/2".into();
        db.insert_workspace(&inside).unwrap();
        db.insert_workspace(&outside).unwrap();

        let changed = db
            .relocate_state_dir(Path::new("/old/state"), Path::new("/new/state"))
            .unwrap();
        assert_eq!(changed, 2, "the path and state dir of the one inside");
        let moved = db.workspace(inside.id).unwrap();
        assert_eq!(moved.path, Path::new("/new/state/repos/a/ws/1"));
        assert_eq!(moved.state_dir, Path::new("/new/state/repos/a/wc/1"));
        let kept = db.workspace(outside.id).unwrap();
        assert_eq!(kept.path, outside.path);
        assert_eq!(kept.state_dir, outside.state_dir);
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
