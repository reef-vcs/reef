//! Offline restore from a backup (R1.18, ADR 0020): `reef restore` runs it
//! in-process with no daemon, holding the state directory's single-instance
//! lock so none can start meanwhile.
//!
//! It logs at `debug` only: it runs in the CLI, which reports what it did.
//!
//! 1. Refuse a manifest without `complete` (an interrupted backup), and a
//!    database copy from a newer schema than this binary knows.
//! 2. Check everything before writing anything: an existing database is
//!    replaced only with `--force`, and so is a repository whose live op
//!    heads are not all in the backup's `heads.json` (the live repository is
//!    newer than the backup; restoring it goes back in time).
//! 3. Each repository's `.jj/repo` copy staged beside its target (the
//!    manifest's, or `<root>/.jj/repo` with `--root`), so a failed copy
//!    changes nothing; then the database copy into place, the old database
//!    renamed aside with its WAL and shared-memory files, so SQLite cannot
//!    replay them over the copy.
//! 4. Each staged copy renamed into place, the live one renamed aside first.
//!    What was replaced is kept under `.replaced-<nanos>` names and listed
//!    in the report: the way back from a `--force`. A colocated
//!    repository's `.git` is created from the copy if missing; otherwise the
//!    backup's objects are added (none removed) and its refs written over.
//!    With `--root` on a directory without a working copy, jj's `default`
//!    working copy is recreated there and its files checked out.
//! 5. In the restored database: the repository's new paths with `--root`;
//!    with `--into` another state directory than the backup's, every path
//!    under the old one (workspaces, base cache entries, stores) moved to
//!    the same place under the new one;
//!    R1.15's restart grace on every active lease, `meta.last_backup_at`
//!    set to the backup's time (so GC's retention floor follows it),
//!    `restored_from`, `restored_at`, and `rematerialize_pending`, which
//!    makes the next daemon start rematerialize every active workspace
//!    (rebuilding a directory that is gone instead of reclaiming it).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::{OperationIdHex, RecreateWorkingCopyError, Repo, UserSettings};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::backup::{
    Existing, HEADS_FILE, MANIFEST_VERSION, Manifest, ManifestError, RepoManifest, SQLITE_FILE,
    copy_tree, copy_tree_with, read_op_heads, repo_dir,
};
use crate::config::{Config, ConfigError, LeaseSection, Paths};
use crate::db::{
    Db, DbError, META_LAST_BACKUP_AT, META_REMATERIALIZE_PENDING, META_RESTORED_AT,
    META_RESTORED_FROM, SCHEMA_VERSION,
};
use crate::error::{Advice, Advise};
use crate::lease::{LeaseError, LeaseManager};
use crate::lock::{InstanceLock, LockError};
use crate::model::{RepoId, Timestamp, WorkspaceState};

/// What to restore, and where.
#[derive(Clone, Debug)]
pub struct RestoreOptions {
    /// The state directory to restore into.
    pub into: PathBuf,
    /// Only this repository's jj state (the database is restored whole).
    pub repo: Option<RepoId>,
    /// Restore the repository at this root instead of its registered one;
    /// needs [`RestoreOptions::repo`] when the backup has several.
    pub root: Option<PathBuf>,
    /// Replace an existing database, and repositories newer than the
    /// backup.
    pub force: bool,
    /// `[lease]`: the restart grace given every active lease.
    pub lease: LeaseSection,
    /// The jj settings a recreated working copy is written with.
    pub settings: UserSettings,
}

impl RestoreOptions {
    /// Options from the effective configuration: its state directory, lease
    /// settings, and jj settings; nothing else set.
    pub fn from_config(config: &Config) -> Result<RestoreOptions, RestoreError> {
        Ok(RestoreOptions {
            into: config.paths.state_dir.clone(),
            repo: None,
            root: None,
            force: false,
            lease: config.file.lease.clone(),
            settings: config
                .jj_settings()
                .map_err(|error| RestoreError::Settings(error.to_string()))?,
        })
    }
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// The repositories restored, and where their `.jj/repo` went.
    pub repos: Vec<(RepoId, PathBuf)>,
    /// Active workspace records in the restored database.
    pub workspaces_kept: u64,
    /// Leases given the restart grace.
    pub leases_extended: u64,
    /// The `default` working copy was recreated at `--root`.
    pub default_workspace_recreated: bool,
    /// What the restore replaced, kept beside it under a `.replaced-<nanos>`
    /// name: the old database and each repository's old `.jj/repo`. Nothing
    /// removes them; they are the way back from a `--force`.
    pub replaced: Vec<PathBuf>,
}

/// Failure of a restore.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// The manifest could not be read.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The manifest has no `complete`: the backup was interrupted.
    #[error("{} is an interrupted backup: its manifest is not marked complete", .0.display())]
    Incomplete(PathBuf),
    /// The manifest is a newer format than this binary reads.
    #[error("{} is backup format {found}; this reef reads format {MANIFEST_VERSION}", path.display())]
    UnsupportedVersion {
        /// The manifest.
        path: PathBuf,
        /// Its version.
        found: u32,
    },
    /// The manifest holds a value this binary cannot read.
    #[error("{}: {what}", path.display())]
    BadManifest {
        /// The manifest.
        path: PathBuf,
        /// What is wrong.
        what: String,
    },
    /// The database copy is a newer schema than this binary knows.
    #[error("the backup's database is schema version {found}; this reef supports {SCHEMA_VERSION}")]
    SchemaTooNew {
        /// Its `user_version`.
        found: u32,
    },
    /// `--repo` names a repository the backup does not have.
    #[error("the backup has no repository {0}")]
    NoSuchRepo(RepoId),
    /// `--root` with several repositories and no `--repo`.
    #[error("--root needs --repo: the backup has {0} repositories")]
    RootNeedsRepo(usize),
    /// A database exists and `--force` was not given.
    #[error("{} exists; restoring replaces it", .0.display())]
    DbExists(PathBuf),
    /// A live repository has operations the backup does not.
    #[error("repository {repo} at {} has operations newer than the backup", path.display())]
    NewerLive {
        /// The repository.
        repo: RepoId,
        /// Its `.jj/repo`.
        path: PathBuf,
    },
    /// A daemon holds the state directory.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The state directory's paths did not resolve.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The jj settings could not be built.
    #[error("build the jj settings: {0}")]
    Settings(String),
    /// A file could not be read or written.
    #[error("restore {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The restored database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The restart grace could not be applied.
    #[error("apply the lease restart grace")]
    Leases(#[from] LeaseError),
    /// The `default` working copy could not be recreated.
    #[error("recreate the default working copy at {}", root.display())]
    WorkingCopy {
        /// The root.
        root: PathBuf,
        /// The underlying error.
        #[source]
        source: RecreateWorkingCopyError,
    },
}

impl Classify for RestoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Manifest(error) => error.kind(),
            Self::Incomplete(_)
            | Self::UnsupportedVersion { .. }
            | Self::RootNeedsRepo(_)
            | Self::BadManifest { .. }
            | Self::Settings(_) => ErrorKind::InvalidArgument,
            Self::NoSuchRepo(_) => ErrorKind::NotFound,
            Self::DbExists(_) => ErrorKind::AlreadyExists,
            Self::SchemaTooNew { .. } | Self::NewerLive { .. } => ErrorKind::Conflict,
            Self::Lock(error) => error.kind(),
            Self::Config(error) => error.kind(),
            Self::Io { source, .. } => classify_by_source(source),
            Self::Db(error) => error.kind(),
            Self::Leases(error) => error.kind(),
            Self::WorkingCopy { source, .. } => source.kind(),
        }
    }
}

impl Advise for RestoreError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Lock(_) => Some(Advice::run(
                "Restore is offline: stop the daemon first.",
                ["reef", "daemon", "stop"],
            )),
            Self::DbExists(_) => Some(Advice::hint(
                "Pass --force to replace the daemon's current state with the backup's.",
            )),
            Self::NewerLive { .. } => Some(Advice::hint(
                "The repository has work the backup lacks; --force discards it and goes back to the backup.",
            )),
            Self::Incomplete(_) => Some(Advice::hint(
                "The backup did not finish; take a new one with `reef backup`.",
            )),
            Self::SchemaTooNew { .. } | Self::UnsupportedVersion { .. } => Some(Advice::hint(
                "A newer reef took this backup; restore it with that version.",
            )),
            _ => None,
        }
    }
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> RestoreError + use<> {
    let path = path.to_owned();
    move |source| RestoreError::Io { path, source }
}

/// One repository to restore, checked.
struct Target<'a> {
    entry: &'a RepoManifest,
    id: RepoId,
    /// Where `.jj/repo` goes.
    jj: PathBuf,
    /// The root it belongs to.
    root: PathBuf,
}

/// Restores the backup at `src`; see the module docs. Blocks; no daemon may
/// run on `opts.into`.
pub fn restore(src: &Path, opts: &RestoreOptions) -> Result<RestoreReport, RestoreError> {
    // 1.
    let manifest = Manifest::read(src)?;
    let manifest_path = src.join(super::backup::MANIFEST_FILE);
    if manifest.version > MANIFEST_VERSION {
        return Err(RestoreError::UnsupportedVersion {
            path: manifest_path,
            found: manifest.version,
        });
    }
    if !manifest.complete {
        return Err(RestoreError::Incomplete(manifest_path));
    }
    let schema = backup_schema(&src.join(SQLITE_FILE))?;
    if schema > SCHEMA_VERSION {
        return Err(RestoreError::SchemaTooNew { found: schema });
    }
    let paths = Paths::in_state_dir(&opts.into)?;
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.state_dir)
            .map_err(io_at(&paths.state_dir))?;
    }
    let _lock = InstanceLock::acquire(&paths.lock)?;

    // 2.
    let targets = targets(src, &manifest, opts)?;
    if paths.db.exists() && !opts.force {
        return Err(RestoreError::DbExists(paths.db.clone()));
    }

    // 3. Every copy staged first, so a failure leaves nothing changed.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let mut staged = Vec::with_capacity(targets.len());
    for target in &targets {
        match stage_repo(src, target, stamp) {
            Ok(path) => staged.push(path),
            Err(error) => {
                for path in &staged {
                    let _ = crate::backend::fs_util::remove_all(path);
                }
                return Err(error);
            }
        }
    }
    let mut replaced = Vec::new();
    replaced.extend(restore_db(&src.join(SQLITE_FILE), &paths.db, stamp)?);

    // 4.
    let mut repos = Vec::with_capacity(targets.len());
    let mut default_workspace_recreated = false;
    for (target, staged) in targets.iter().zip(staged) {
        replaced.extend(swap_repo(src, target, &staged, stamp)?);
        if opts.root.is_some() && !target.root.join(".jj/working_copy").exists() {
            pollster::block_on(Repo::recreate_default_working_copy(
                &target.root,
                &opts.settings,
            ))
            .map_err(|source| RestoreError::WorkingCopy {
                root: target.root.clone(),
                source,
            })?;
            default_workspace_recreated = true;
        }
        repos.push((target.id, target.jj.clone()));
    }

    // 5.
    let (workspaces_kept, leases_extended) = finish_db(src, &manifest, &targets, &paths, opts)?;
    tracing::debug!(
        src = %src.display(),
        into = %paths.state_dir.display(),
        repo_count = repos.len(),
        workspaces_kept,
        leases_extended,
        default_workspace_recreated,
        "restored from backup"
    );
    Ok(RestoreReport {
        repos,
        replaced,
        workspaces_kept: u64::try_from(workspaces_kept).unwrap_or(u64::MAX),
        leases_extended: u64::try_from(leases_extended).unwrap_or(u64::MAX),
        default_workspace_recreated,
    })
}

/// Step 5: the restored database's new paths, lease grace, and `meta`.
/// Returns the active workspaces kept and the leases extended.
fn finish_db(
    src: &Path,
    manifest: &Manifest,
    targets: &[Target<'_>],
    paths: &Paths,
    opts: &RestoreOptions,
) -> Result<(usize, usize), RestoreError> {
    let db = Arc::new(Db::open(&paths.db)?);
    if opts.root.is_some() {
        for target in targets {
            let canonical = |path: &Path| fs::canonicalize(path).map_err(io_at(path));
            db.set_repository_paths(
                target.id,
                &canonical(&target.root)?,
                &canonical(&target.jj)?,
            )?;
        }
    }
    // Rows hold the state directory both as configured and canonical (a
    // workspace's path is canonicalized at create): move both spellings.
    let canonical_or = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let to = canonical_or(&paths.state_dir);
    if canonical_or(&manifest.state_dir) != to {
        let mut spellings = vec![
            manifest.state_dir.clone(),
            canonical_or(&manifest.state_dir),
        ];
        spellings.dedup();
        for from in spellings {
            let relocated = db.relocate_state_dir(&from, &to)?;
            tracing::debug!(
                from = %from.display(),
                to = %to.display(),
                relocated,
                "paths moved to the new state directory"
            );
        }
    }
    let leases = LeaseManager::new(opts.lease.clone(), Arc::clone(&db), Weak::new());
    let leases_extended = leases.apply_restart_grace()?;
    let workspaces_kept = db
        .workspaces(None, &[WorkspaceState::Active, WorkspaceState::Releasing])?
        .len();
    if let Some(created) = parse_rfc3339(&manifest.created_at) {
        db.set_meta(META_LAST_BACKUP_AT, &created.0.to_string())?;
    }
    db.set_meta(META_RESTORED_FROM, &src.display().to_string())?;
    db.set_meta(META_RESTORED_AT, &Timestamp::now().0.to_string())?;
    db.set_meta(META_REMATERIALIZE_PENDING, "1")?;
    Ok((workspaces_kept, leases_extended))
}

/// The repositories to restore, checked against step 2's refusal rule.
fn targets<'a>(
    src: &Path,
    manifest: &'a Manifest,
    opts: &RestoreOptions,
) -> Result<Vec<Target<'a>>, RestoreError> {
    let chosen: Vec<&RepoManifest> = match opts.repo {
        Some(id) => vec![manifest.repo(id).ok_or(RestoreError::NoSuchRepo(id))?],
        None => manifest.repos.iter().collect(),
    };
    if opts.root.is_some() && chosen.len() > 1 {
        return Err(RestoreError::RootNeedsRepo(chosen.len()));
    }
    let mut targets = Vec::with_capacity(chosen.len());
    for entry in chosen {
        let id: RepoId = entry.id.parse().map_err(|_| RestoreError::BadManifest {
            path: src.join(super::backup::MANIFEST_FILE),
            what: format!("{:?} is not a repository id", entry.id),
        })?;
        let (root, jj) = match &opts.root {
            Some(root) => (root.clone(), root.join(".jj/repo")),
            None => (entry.root.clone(), entry.jj_repo_path.clone()),
        };
        if jj.exists() && !opts.force {
            let backed_up = backup_heads(&repo_dir(src, id))?;
            let live = read_op_heads(&jj).map_err(io_at(&jj))?;
            if live.iter().any(|head| !backed_up.contains(head)) {
                return Err(RestoreError::NewerLive { repo: id, path: jj });
            }
        }
        targets.push(Target {
            entry,
            id,
            jj,
            root,
        });
    }
    Ok(targets)
}

/// `heads.json` in a repository's backup directory.
fn backup_heads(dir: &Path) -> Result<Vec<OperationIdHex>, RestoreError> {
    let path = dir.join(HEADS_FILE);
    let text = fs::read_to_string(&path).map_err(io_at(&path))?;
    let heads: Vec<String> = serde_json::from_str(&text).map_err(|source| RestoreError::Io {
        path: path.clone(),
        source: io::Error::new(io::ErrorKind::InvalidData, source),
    })?;
    Ok(heads.iter().filter_map(|head| head.parse().ok()).collect())
}

/// The `user_version` of the database copy.
fn backup_schema(db: &Path) -> Result<u32, RestoreError> {
    use rusqlite::{Connection, OpenFlags};
    let failed = |error: rusqlite::Error| RestoreError::Io {
        path: db.to_owned(),
        source: io::Error::other(error),
    };
    if !db.is_file() {
        return Err(RestoreError::Io {
            path: db.to_owned(),
            source: io::Error::from(io::ErrorKind::NotFound),
        });
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(failed)?;
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(failed)
}

/// Step 3: the database copy at `to`, through a temporary file. An old
/// database is renamed aside with its `-wal` and `-shm`, so SQLite cannot
/// replay them over the copy and the old one still opens; returns its new
/// path.
fn restore_db(from: &Path, to: &Path, stamp: u128) -> Result<Option<PathBuf>, RestoreError> {
    let tmp = to.with_extension("sqlite3.restore");
    fs::copy(from, &tmp).map_err(io_at(&tmp))?;
    let aside = sibling(to, &format!("replaced-{stamp}"));
    let kept = if to.exists() {
        fs::rename(to, &aside).map_err(io_at(to))?;
        Some(aside.clone())
    } else {
        None
    };
    for suffix in ["-wal", "-shm"] {
        let (mut old, mut new) = (to.as_os_str().to_owned(), aside.as_os_str().to_owned());
        old.push(suffix);
        new.push(suffix);
        match fs::rename(&old, &new) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RestoreError::Io {
                    path: PathBuf::from(old),
                    source,
                });
            }
        }
    }
    fs::rename(&tmp, to).map_err(io_at(to))?;
    Ok(kept)
}

/// `path` with `.<suffix>` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Step 3 for one repository: its `.jj/repo` copied beside the target.
/// Returns the staged copy.
fn stage_repo(src: &Path, target: &Target<'_>, stamp: u128) -> Result<PathBuf, RestoreError> {
    let parent = target.jj.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(parent).map_err(io_at(parent))?;
    let staged = parent.join(format!("repo.restore-{stamp}"));
    copy_tree(&repo_dir(src, target.id).join("repo"), &staged, &[]).map_err(io_at(&staged))?;
    Ok(staged)
}

/// Step 4 for one repository: the staged copy renamed into place, a live
/// `.jj/repo` renamed aside (and returned) first; then a colocated
/// repository's `.git`.
fn swap_repo(
    src: &Path,
    target: &Target<'_>,
    staged: &Path,
    stamp: u128,
) -> Result<Option<PathBuf>, RestoreError> {
    let kept = if target.jj.exists() {
        let aside = sibling(&target.jj, &format!("replaced-{stamp}"));
        fs::rename(&target.jj, &aside).map_err(io_at(&target.jj))?;
        Some(aside)
    } else {
        None
    };
    fs::rename(staged, &target.jj).map_err(io_at(&target.jj))?;
    if target.entry.colocated {
        restore_git(
            &repo_dir(src, target.id).join("git"),
            &target.root.join(".git"),
        )?;
    }
    tracing::debug!(repo = %target.id, path = %target.jj.display(), "repository restored");
    Ok(kept)
}

/// A colocated repository's `.git` from the copy `from`: created whole if
/// missing (without git's index, which jj's next colocated command writes);
/// otherwise the copy's objects added (existing ones kept, none removed)
/// and everything else of the copy (loose refs, `packed-refs`, `HEAD`,
/// `config`) written over. Loose refs the copy lacks are left alone; a ref
/// only in the live `packed-refs` is replaced with the copy's file.
fn restore_git(from: &Path, to: &Path) -> Result<(), RestoreError> {
    if !from.is_dir() {
        return Ok(());
    }
    if !to.exists() {
        copy_tree(from, to, &[]).map_err(io_at(to))?;
        return Ok(());
    }
    copy_tree_with(
        &from.join("objects"),
        &to.join("objects"),
        &[],
        Existing::Keep,
    )
    .map_err(io_at(to))?;
    copy_tree(from, to, &["objects"]).map_err(io_at(to))?;
    Ok(())
}

fn parse_rfc3339(text: &str) -> Option<Timestamp> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(Timestamp::from)
}
