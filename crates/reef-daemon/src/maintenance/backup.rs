//! Local backup (R1.18, ADR 0020): a consistent copy of the daemon's
//! authoritative state, its SQLite database and each repository's jj state,
//! taken while agents keep writing.
//!
//! # Layout
//!
//! ```text
//! <dest>/manifest.json            written first, rewritten with `complete` last
//! <dest>/reef.sqlite3             SQLite's online backup of the whole database
//! <dest>/repos/<id>/repo/         `.jj/repo` without the index's contents
//! <dest>/repos/<id>/git/          a colocated repository's `.git`, without
//!                                 `index`, `logs/`, `hooks/`, and `*.lock`
//! <dest>/repos/<id>/heads.json    the operation heads, last
//! ```
//!
//! `repo/` is a `.jj/repo` jj can open as it is: its `index/` holds only its
//! `type` file and empty directories, which jj rebuilds into on first load,
//! and its `op_heads/heads/` the heads of `heads.json`.
//!
//! # Order
//!
//! The caller holds the maintenance lock. Then:
//!
//! 1. `meta.last_backup_started_at = now`.
//! 2. SQLite: a passive WAL checkpoint, then the online backup from a second
//!    connection to the database file, `backup.sqlite_pages_per_step` pages
//!    a step. Heartbeats go on meanwhile; SQLite restarts a step whose pages
//!    changed.
//! 3. Per repository: the immutable operation and view files and the git
//!    backend's extra-metadata tables are copied; then, with the
//!    repository thread paused (the "write lock held briefly"), the op
//!    heads and the extra-metadata heads are read, and the immutable files
//!    written since, the workspace and submodule stores, and every `type`
//!    file are copied. Reading heads before the files they reach makes the
//!    copy consistent even against a `jj` CLI writing outside the pause.
//!    The pause copies what was written since the first pass, and lists
//!    (without copying) the rest; it is never proportional to the object
//!    count.
//! 4. Without the pause: the git repository (`store/git`, or a colocated
//!    `.git`), refs first and objects after, so every copied ref names a
//!    copied object, mtimes preserved so R1.17's `git gc --prune` sees the
//!    same ages after a restore. Objects written meanwhile are harmless
//!    extras: jj writes objects before the operations naming them.
//! 5. The heads read at step 3 into `repo/` and `heads.json`. After every
//!    repository, the manifest again with `complete: true`, and
//!    `meta.last_backup_at = now`, which R1.17's GC reads.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reef_api::v1::{BackupResponse, BackupStep};
use reef_lib::error::{Classify, ErrorChain, ErrorKind, classify_by_source};
use reef_lib::jj::{JJ_LIB_VERSION, OperationIdHex};
use reef_lib::telemetry::elapsed_ms;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::backend::fs_util::restore_mtime;
use crate::daemon::{AppState, PrivateDirError};
use crate::db::{DbError, META_DAEMON_INSTANCE_ID, META_LAST_BACKUP_AT};
use crate::error::{Advice, Advise};
use crate::model::{RepoId, RepositoryRecord, Timestamp};
use crate::repo::RepoThreadError;

/// The manifest's `version`.
pub const MANIFEST_VERSION: u32 = 1;
/// The manifest's file name.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The database copy's file name.
pub const SQLITE_FILE: &str = "reef.sqlite3";
/// The operation heads' file name, in each repository's directory.
pub const HEADS_FILE: &str = "heads.json";
/// `meta` key of when the last backup began, as decimal unix milliseconds.
pub const META_LAST_BACKUP_STARTED_AT: &str = "last_backup_started_at";

/// The directories of a jj repository's index, created empty in a copy so
/// jj can rebuild into them.
pub const INDEX_DIRS: &[&str] = &["op_links", "segments", "changed_paths"];

/// Top-level entries of a `.git` a backup leaves out: git's own index,
/// reflogs, and hooks are not the repository's history.
const GIT_SKIP: &[&str] = &["index", "logs", "hooks"];

/// Milliseconds the online backup waits between steps.
const SQLITE_STEP_PAUSE: Duration = Duration::from_millis(10);

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// When the backup began, RFC 3339.
    pub created_at: String,
    /// The database's `meta.daemon_instance_id`.
    pub daemon_instance_id: String,
    /// The daemon's version.
    pub reef_version: String,
    /// [`JJ_LIB_VERSION`].
    pub jj_lib_version: String,
    /// The state directory backed up.
    pub state_dir: PathBuf,
    /// The repositories, in registration order.
    pub repos: Vec<RepoManifest>,
    /// The database copy.
    pub sqlite: SqliteManifest,
    /// Every repository's heads are written; a manifest without it is an
    /// interrupted backup, which restore refuses.
    #[serde(default)]
    pub complete: bool,
}

/// One repository in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoManifest {
    /// The repository's id, in hex.
    pub id: String,
    /// Its registered root.
    pub root: PathBuf,
    /// Its `.jj/repo` when backed up.
    pub jj_repo_path: PathBuf,
    /// Whether jj and git share the working copy (`git/` holds `.git`).
    pub colocated: bool,
    /// The operation heads, as `heads.json` has them.
    pub op_heads: Vec<String>,
    /// Files copied.
    pub files: u64,
    /// Bytes copied.
    pub bytes: u64,
    /// How long the repository thread was paused for step 3.
    pub pause_ms: u64,
}

/// The database copy in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteManifest {
    /// Pages copied.
    pub pages: u64,
    /// The copy's size.
    pub bytes: u64,
}

impl Manifest {
    /// Reads `<dir>/manifest.json`.
    pub fn read(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ManifestError::Parse { path, source })
    }

    /// Writes `<dir>/manifest.json` through a temporary file, so a crash
    /// leaves the previous manifest whole.
    fn write(&self, dir: &Path) -> Result<(), BackupError> {
        let path = dir.join(MANIFEST_FILE);
        let tmp = dir.join(format!("{MANIFEST_FILE}.tmp"));
        let text = serde_json::to_string_pretty(self).map_err(BackupError::Encode)?;
        fs::write(&tmp, format!("{text}\n")).map_err(io_at(&tmp))?;
        fs::rename(&tmp, &path).map_err(io_at(&path))
    }

    /// The repository with this id.
    pub fn repo(&self, id: RepoId) -> Option<&RepoManifest> {
        self.repos.iter().find(|repo| repo.id == id.as_hex())
    }
}

/// Failure to read a manifest.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The file could not be read.
    #[error("read {}", path.display())]
    Read {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a manifest.
    #[error("parse {}", path.display())]
    Parse {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },
}

impl Classify for ManifestError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Read { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                ErrorKind::NotFound
            }
            Self::Read { source, .. } => classify_by_source(source),
            Self::Parse { .. } => ErrorKind::InvalidArgument,
        }
    }
}

impl Advise for ManifestError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Read { source, .. } if source.kind() == io::ErrorKind::NotFound => Some(
                Advice::hint("Name the directory `reef backup` wrote; it holds manifest.json."),
            ),
            _ => None,
        }
    }
}

/// Failure of a backup.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackupError {
    /// The destination is not an absolute path.
    #[error("{} is not an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// The destination exists and is not an empty directory.
    #[error("{} exists and is not empty", .0.display())]
    DestNotEmpty(PathBuf),
    /// The destination is inside the state directory.
    #[error("{} is inside the daemon's state directory", .0.display())]
    DestInStateDir(PathBuf),
    /// The destination is an empty directory another user owns (RX.01,
    /// S-01).
    #[error("{} is owned by uid {uid}, not by uid {euid}, which runs the daemon", path.display())]
    DestNotOwned {
        /// The destination.
        path: PathBuf,
        /// Its owner.
        uid: u32,
        /// The daemon's effective uid.
        euid: u32,
    },
    /// Another maintenance task holds the lock.
    #[error(transparent)]
    Busy(#[from] super::Busy),
    /// A file could not be read or written.
    #[error("copy {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// SQLite's online backup failed.
    #[error("back up the database")]
    Sqlite(#[source] rusqlite::Error),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A repository thread could not be paused.
    #[error(transparent)]
    RepoThread(#[from] RepoThreadError),
    /// The manifest could not be encoded.
    #[error("encode the manifest")]
    Encode(#[source] serde_json::Error),
    /// A blocking task panicked.
    #[error("a backup task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Classify for BackupError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotAbsolute(_) | Self::DestInStateDir(_) => ErrorKind::InvalidArgument,
            Self::DestNotEmpty(_) => ErrorKind::AlreadyExists,
            Self::DestNotOwned { .. } => ErrorKind::PermissionDenied,
            Self::Busy(error) => error.kind(),
            Self::Io { source, .. } => classify_by_source(source),
            Self::Sqlite(_) => ErrorKind::Io,
            Self::Db(error) => error.kind(),
            Self::RepoThread(error) => error.kind(),
            Self::Encode(_) | Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for BackupError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::DestNotEmpty(_) => Some(Advice::hint(
                "Name a new directory, or an empty one; a backup never overwrites another.",
            )),
            Self::DestNotOwned { .. } => Some(Advice::hint(
                "Name a new directory, or an empty one the daemon's user owns; a backup is private to that user.",
            )),
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> BackupError + use<> {
    let path = path.to_owned();
    move |source| BackupError::Io { path, source }
}

/// Runs a backup of every registered repository, or of `repo` (the
/// database is copied whole either way), into `dest`, which must be absolute
/// and new or an empty directory; see the module docs. Sends progress on
/// `progress` (a closed receiver is ignored). The caller holds the
/// maintenance lock.
pub async fn run(
    state: &AppState,
    dest: &Path,
    repo: Option<RepoId>,
    progress: mpsc::Sender<BackupResponse>,
) -> Result<Manifest, BackupError> {
    let begun = Instant::now();
    prepare_dest(state, dest)?;
    let report = |step: BackupStep, repo: Option<RepoId>, files: u64, bytes: u64| {
        let message = BackupResponse {
            repo_id: repo.map(|id| id.as_hex()).unwrap_or_default(),
            step: step.into(),
            files,
            bytes,
            manifest_path: String::new(),
        };
        // Progress is best effort: a client that stopped reading does not
        // stop the backup.
        let _ = progress.try_send(message);
    };
    let records: Vec<RepositoryRecord> = match repo {
        Some(id) => vec![state.db.repository(id)?],
        None => state.db.repositories()?,
    };

    // 1.
    let started = Timestamp::now();
    state
        .db
        .set_meta(META_LAST_BACKUP_STARTED_AT, &started.0.to_string())?;
    let mut manifest = Manifest {
        version: MANIFEST_VERSION,
        created_at: started.to_rfc3339(),
        daemon_instance_id: state.db.meta(META_DAEMON_INSTANCE_ID)?.unwrap_or_default(),
        reef_version: env!("CARGO_PKG_VERSION").to_owned(),
        jj_lib_version: JJ_LIB_VERSION.to_owned(),
        state_dir: state.config.paths.state_dir.clone(),
        repos: Vec::new(),
        sqlite: SqliteManifest { pages: 0, bytes: 0 },
        complete: false,
    };
    manifest.write(dest)?;

    // 2.
    super::quiesce::checkpoint(state, "PASSIVE");
    let sqlite = {
        let (db, to) = (state.config.paths.db.clone(), dest.join(SQLITE_FILE));
        let pages = state.config.file.backup.sqlite_pages_per_step;
        tokio::task::spawn_blocking(move || backup_sqlite(&db, &to, pages)).await??
    };
    report(BackupStep::Sqlite, None, 1, sqlite.bytes);
    manifest.sqlite = sqlite;

    // 3. and 4., per repository.
    let mut heads = Vec::with_capacity(records.len());
    for record in &records {
        let (entry, repo_heads) = backup_repo(state, record, dest, &report).await?;
        manifest.repos.push(entry);
        heads.push(repo_heads);
    }

    // 5.
    for (record, repo_heads) in records.iter().zip(heads) {
        let dir = repo_dir(dest, record.id);
        tokio::task::spawn_blocking(move || repo_heads.write(&dir)).await??;
        report(BackupStep::Heads, Some(record.id), 0, 0);
    }
    manifest.complete = true;
    manifest.write(dest)?;
    let finished = Timestamp::now();
    state
        .db
        .set_meta(META_LAST_BACKUP_AT, &finished.0.to_string())?;
    tracing::info!(
        dest = %dest.display(),
        repo_count = manifest.repos.len(),
        sqlite_bytes = manifest.sqlite.bytes,
        files = manifest.repos.iter().map(|repo| repo.files).sum::<u64>(),
        bytes = manifest.repos.iter().map(|repo| repo.bytes).sum::<u64>(),
        elapsed_ms = elapsed_ms(begun),
        "backup finished"
    );
    Ok(manifest)
}

/// `<dest>/repos/<id>`.
pub fn repo_dir(dest: &Path, id: RepoId) -> PathBuf {
    dest.join("repos").join(id.as_hex())
}

/// Checks the destination and creates it (`0700`): absolute, outside the
/// state directory, and new or an empty directory, which must be the
/// daemon's user's and is `chmod`ed `0700` (RX.01, S-01). [`run`] does this
/// itself; `Maintenance.Backup` also does it before answering, so a bad
/// destination is the RPC's status rather than its stream's first item.
pub fn prepare_dest(state: &AppState, dest: &Path) -> Result<(), BackupError> {
    if !dest.is_absolute() {
        return Err(BackupError::NotAbsolute(dest.to_owned()));
    }
    let state_dir = &state.config.paths.state_dir;
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    // The destination may not exist yet: compare its nearest existing
    // ancestor's canonical path, with the rest appended.
    let resolved = dest
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .map_or_else(
            || dest.to_owned(),
            |ancestor| {
                canonical(ancestor).join(dest.strip_prefix(ancestor).unwrap_or(Path::new("")))
            },
        );
    if resolved.starts_with(canonical(state_dir)) {
        return Err(BackupError::DestInStateDir(dest.to_owned()));
    }
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(BackupError::DestNotEmpty(dest.to_owned()));
            }
            match crate::daemon::ensure_private_dir(dest, false) {
                Ok(Some(mode)) => crate::daemon::log_tightened(dest, mode),
                Ok(None) => {}
                Err(PrivateDirError::NotOwned { path, uid, euid }) => {
                    return Err(BackupError::DestNotOwned { path, uid, euid });
                }
                Err(PrivateDirError::Io { path, source }) => {
                    return Err(BackupError::Io { path, source });
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt as _;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dest)
                .map_err(io_at(dest))
        }
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            Err(BackupError::DestNotEmpty(dest.to_owned()))
        }
        Err(source) => Err(BackupError::Io {
            path: dest.to_owned(),
            source,
        }),
    }
}

/// Step 2: SQLite's online backup of `db` into the new file `to`, from a
/// connection of its own.
fn backup_sqlite(db: &Path, to: &Path, pages_per_step: u32) -> Result<SqliteManifest, BackupError> {
    use rusqlite::{Connection, OpenFlags};
    let source = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::Sqlite)?;
    source
        .busy_timeout(Duration::from_secs(5))
        .map_err(BackupError::Sqlite)?;
    // Private whatever the umask (RX.01, S-01).
    crate::db::private_file(to).map_err(io_at(to))?;
    let mut dest = Connection::open(to).map_err(BackupError::Sqlite)?;
    {
        let backup =
            rusqlite::backup::Backup::new(&source, &mut dest).map_err(BackupError::Sqlite)?;
        let pages = i32::try_from(pages_per_step.max(1)).unwrap_or(i32::MAX);
        backup
            .run_to_completion(pages, SQLITE_STEP_PAUSE, None)
            .map_err(BackupError::Sqlite)?;
    }
    let pages: i64 = dest
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .map_err(BackupError::Sqlite)?;
    let pages = u64::try_from(pages).unwrap_or(0);
    drop(dest);
    let bytes = fs::metadata(to).map_err(io_at(to))?.len();
    Ok(SqliteManifest { pages, bytes })
}

/// The heads step 3 read, written at step 5.
struct RepoHeads {
    op_heads: Vec<String>,
    /// The git backend's `store/extra/heads/` file names, if it has any.
    extra_heads: Option<Vec<String>>,
}

impl RepoHeads {
    /// Writes `repo/op_heads/heads/<id>` (and `repo/store/extra/heads/`)
    /// files, then `heads.json`.
    fn write(&self, dir: &Path) -> Result<(), BackupError> {
        let repo = dir.join("repo");
        let write_names = |at: &Path, names: &[String]| -> Result<(), BackupError> {
            fs::create_dir_all(at).map_err(io_at(at))?;
            for name in names {
                let path = at.join(name);
                fs::write(&path, b"").map_err(io_at(&path))?;
            }
            Ok(())
        };
        write_names(&repo.join("op_heads/heads"), &self.op_heads)?;
        if let Some(extra) = &self.extra_heads {
            write_names(&repo.join("store/extra/heads"), extra)?;
        }
        let path = dir.join(HEADS_FILE);
        let text = serde_json::to_string(&self.op_heads).map_err(BackupError::Encode)?;
        fs::write(&path, format!("{text}\n")).map_err(io_at(&path))
    }
}

/// Steps 3 and 4 for one repository.
async fn backup_repo(
    state: &AppState,
    record: &RepositoryRecord,
    dest: &Path,
    report: &impl Fn(BackupStep, Option<RepoId>, u64, u64),
) -> Result<(RepoManifest, RepoHeads), BackupError> {
    let id = record.id;
    let dir = repo_dir(dest, id);
    let jj = record.jj_repo_path.clone();
    let to = dir.join("repo");

    // 3. The immutable operation files first, without the pause, so the
    // pause copies only those written since.
    let (mut files, mut bytes) = {
        let (jj, to) = (jj.clone(), to.clone());
        tokio::task::spawn_blocking(move || copy_immutable(&jj, &to, Existing::Replace)).await??
    };
    let pause = if let Some(handle) = state.repos.get(id) {
        Some(handle.pause(state.config.file.backup.quiesce_max).await?)
    } else {
        tracing::warn!(repo = %id, "repository is not open; copying its jj state without the pause");
        None
    };
    let paused = Instant::now();
    let copied = {
        let (jj, to) = (jj.clone(), to.clone());
        tokio::task::spawn_blocking(move || copy_operations(&jj, &to)).await?
    };
    let pause_ms = elapsed_ms(paused);
    drop(pause);
    let (heads, paused_files, paused_bytes) = copied?;
    tracing::info!(
        repo = %id,
        pause_ms,
        paused_files,
        files = files + paused_files,
        "operations copied; the pause copied what was written meanwhile"
    );
    files += paused_files;
    bytes += paused_bytes;
    report(BackupStep::Operations, Some(id), files, bytes);

    // 4.
    let git = if record.colocated {
        Some((record.root.join(".git"), dir.join("git")))
    } else {
        let internal = jj.join("store/git");
        internal.is_dir().then(|| (internal, to.join("store/git")))
    };
    if let Some((from, into)) = git {
        let (git_files, git_bytes) =
            tokio::task::spawn_blocking(move || copy_git(&from, &into)).await??;
        files += git_files;
        bytes += git_bytes;
        report(BackupStep::Objects, Some(id), git_files, git_bytes);
    }
    let entry = RepoManifest {
        id: id.as_hex(),
        root: record.root.clone(),
        jj_repo_path: jj,
        colocated: record.colocated,
        op_heads: heads.op_heads.clone(),
        files,
        bytes,
        pause_ms,
    };
    Ok((entry, heads))
}

/// The immutable files of `.jj/repo` at `jj` into `to`: operations, views,
/// and the git backend's extra-metadata tables (not their `heads/`). With
/// [`Existing::Keep`], only files not copied yet. Returns files and bytes.
fn copy_immutable(jj: &Path, to: &Path, existing: Existing) -> Result<(u64, u64), BackupError> {
    let mut files = 0;
    let mut bytes = 0;
    for (from, skip) in [("op_store", &[][..]), ("store/extra", &["heads"][..])] {
        let src = jj.join(from);
        if !src.exists() {
            continue;
        }
        let (f, b) = copy_tree_with(&src, &to.join(from), skip, existing)
            .map_err(|source| BackupError::Io { path: src, source })?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

/// Step 3's copy under the pause, from `.jj/repo` at `jj` into `to`: heads
/// read first, then the files they reach: the immutable ones written since
/// [`copy_immutable`] ran, and the small mutable ones. Returns the heads,
/// files, and bytes.
fn copy_operations(jj: &Path, to: &Path) -> Result<(RepoHeads, u64, u64), BackupError> {
    let names = |dir: &Path| -> Result<Vec<String>, BackupError> {
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(dir).map_err(io_at(dir))? {
            let entry = entry.map_err(io_at(dir))?;
            if let Some(name) = entry.file_name().to_str()
                && !is_lock(name)
                && !name.starts_with('.')
            {
                names.insert(name.to_owned());
            }
        }
        Ok(names.into_iter().collect())
    };
    let op_heads = names(&jj.join("op_heads/heads"))?;
    let extra = jj.join("store/extra");
    let extra_heads = if extra.join("heads").is_dir() {
        Some(names(&extra.join("heads"))?)
    } else {
        None
    };
    let mut files = 0;
    let mut bytes = 0;
    let mut add = |(f, b): (u64, u64)| {
        files += f;
        bytes += b;
    };
    let copy = |from: &str, skip: &[&str]| -> Result<(u64, u64), BackupError> {
        let src = jj.join(from);
        if !src.exists() {
            return Ok((0, 0));
        }
        copy_tree(&src, &to.join(from), skip)
            .map_err(|source| BackupError::Io { path: src, source })
    };
    add(copy_immutable(jj, to, Existing::Keep)?);
    add(copy("op_heads", &["heads"])?);
    add(copy("workspace_store", &[])?);
    add(copy("submodule_store", &[])?);
    for file in ["type", "store/type", "store/git_target", "index/type"] {
        add(copy(file, &[])?);
    }
    for dir in INDEX_DIRS {
        let path = to.join("index").join(dir);
        fs::create_dir_all(&path).map_err(io_at(&path))?;
    }
    Ok((
        RepoHeads {
            op_heads,
            extra_heads,
        },
        files,
        bytes,
    ))
}

/// Step 4: a git repository at `from` into `into`, everything but the
/// objects first (refs before the objects they name), then the objects.
/// Leaves out [`GIT_SKIP`] at the top level and `*.lock` everywhere.
fn copy_git(from: &Path, into: &Path) -> Result<(u64, u64), BackupError> {
    let failed = |source| BackupError::Io {
        path: from.to_owned(),
        source,
    };
    let mut skip = GIT_SKIP.to_vec();
    skip.push("objects");
    let (files, bytes) = copy_tree(from, into, &skip).map_err(failed)?;
    let objects = from.join("objects");
    let (object_files, object_bytes) = if objects.is_dir() {
        copy_tree(&objects, &into.join("objects"), &[]).map_err(failed)?
    } else {
        (0, 0)
    };
    Ok((files + object_files, bytes + object_bytes))
}

/// Copies the file or tree `src` to `dst`, creating parents, and returns
/// the files and bytes copied. Entries of `src` (when a directory) named in
/// `skip` are left out at the top level only; `*.lock` and `.tmp*` files
/// everywhere.
/// File mtimes are preserved (`utimensat` by path, which works on git's
/// read-only objects), and symlinks are copied as symlinks. A file that
/// exists at `dst` is replaced.
pub fn copy_tree(src: &Path, dst: &Path, skip: &[&str]) -> io::Result<(u64, u64)> {
    copy_tree_with(src, dst, skip, Existing::Replace)
}

/// What [`copy_tree_with`] does with a file that exists at the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    /// Replace it.
    Replace,
    /// Keep it: content-addressed files (git objects) are the same.
    Keep,
}

/// [`copy_tree`] with a choice for files that exist at the destination.
pub fn copy_tree_with(
    src: &Path,
    dst: &Path,
    skip: &[&str],
    existing: Existing,
) -> io::Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    let walk = walkdir::WalkDir::new(src)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            let top_level_skip = entry.depth() == 1 && skip.iter().any(|skip| name == *skip);
            !top_level_skip && !is_lock(&name) && !is_temporary(&name)
        });
    for entry in walk {
        // A file renamed or removed since the directory was listed (a
        // writer's temporary file) was never part of what is copied.
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if vanished(error.io_error()) => continue,
            Err(error) => return Err(io::Error::from(error)),
        };
        // `src` itself is `dst`: joining the empty path would add a slash.
        let to = match entry.path().strip_prefix(src) {
            Ok(relative) if !relative.as_os_str().is_empty() => dst.join(relative),
            _ => dst.to_owned(),
        };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            fs::create_dir_all(&to)?;
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let exists = fs::symlink_metadata(&to).is_ok();
        if exists && existing == Existing::Keep {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if vanished(error.io_error()) => continue,
            Err(error) => return Err(io::Error::from(error)),
        };
        if exists {
            // A read-only file (a git object) cannot be opened for writing;
            // replace it instead.
            fs::remove_file(&to)?;
        }
        if file_type.is_symlink() {
            match fs::read_link(entry.path()) {
                Ok(target) => std::os::unix::fs::symlink(target, &to)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        } else {
            match fs::copy(entry.path(), &to) {
                Ok(copied) => bytes += copied,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            files += 1;
        }
        restore_mtime(&to, &metadata)?;
    }
    Ok((files, bytes))
}

/// Whether a walk error is a file or directory that went away meanwhile.
fn vanished(error: Option<&io::Error>) -> bool {
    error.is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Whether `name` is a writer's temporary file: jj writes operations,
/// views, and tables through `.tmp*` files it renames into place.
fn is_temporary(name: &str) -> bool {
    name.starts_with(".tmp")
}

/// Whether `name` is a lock file (`*.lock`), which a copy leaves out.
fn is_lock(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "lock")
}

/// The op heads of `.jj/repo` at `jj`, sorted.
pub fn read_op_heads(jj: &Path) -> io::Result<Vec<OperationIdHex>> {
    let mut heads = Vec::new();
    for entry in fs::read_dir(jj.join("op_heads/heads"))? {
        let entry = entry?;
        if let Some(head) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<OperationIdHex>().ok())
        {
            heads.push(head);
        }
    }
    heads.sort();
    Ok(heads)
}

/// Logged when a backup fails, so the reason is in the daemon log too.
pub fn log_failure(error: &BackupError) {
    tracing::error!(error = %ErrorChain(error), "backup failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_tree_skips_top_level_names_and_locks_and_keeps_mtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("objects/ab")).unwrap();
        fs::create_dir_all(src.join("logs")).unwrap();
        fs::write(src.join("objects/ab/cd"), "object").unwrap();
        fs::write(src.join("objects/ab/index"), "nested index stays").unwrap();
        fs::write(src.join("logs/HEAD"), "log").unwrap();
        fs::write(src.join("index"), "git index").unwrap();
        fs::write(src.join("HEAD.lock"), "lock").unwrap();
        fs::write(src.join("HEAD"), "ref").unwrap();
        std::os::unix::fs::symlink("HEAD", src.join("link")).unwrap();
        let single = tmp.path().join("single/type");
        assert_eq!(copy_tree(&src.join("HEAD"), &single, &[]).unwrap(), (1, 3));
        assert_eq!(fs::read_to_string(&single).unwrap(), "ref");
        let old = fs::File::open(src.join("objects/ab/cd")).unwrap();
        old.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        let mut perms = fs::metadata(src.join("objects/ab/cd"))
            .unwrap()
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(src.join("objects/ab/cd"), perms).unwrap();

        let dst = tmp.path().join("dst");
        let (files, bytes) = copy_tree(&src, &dst, &["index", "logs"]).unwrap();
        assert_eq!(files, 3, "cd, nested index, HEAD");
        assert_eq!(bytes, 6 + 18 + 3);
        assert!(!dst.join("index").exists());
        assert!(!dst.join("logs").exists());
        assert!(!dst.join("HEAD.lock").exists());
        assert!(dst.join("objects/ab/index").exists());
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("HEAD"));
        let mtime = fs::metadata(dst.join("objects/ab/cd"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime,
            std::time::UNIX_EPOCH + Duration::from_secs(1_000_000)
        );

        // Again over the copy: read-only files are replaced, or kept.
        assert_eq!(copy_tree(&src, &dst, &["index", "logs"]).unwrap().0, 3);
        let kept = copy_tree_with(&src, &dst, &["index", "logs"], Existing::Keep).unwrap();
        assert_eq!(kept, (0, 0));
    }
}
