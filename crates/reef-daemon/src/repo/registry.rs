//! Registering and forgetting repositories (R1.05): the rows in
//! `repositories` and the threads in [`RepoRegistry`] together. A git
//! checkout without jj can be given jj's side of a colocated repository on
//! the way (R1.25, ADR 0024).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::{InitError, OpenError, Repo, UserSettings};

use super::exclude::{exclude_path, write_jj_exclude};
use super::{RegistryError, RepoRegistry};
use crate::db::{Db, DbError};
use crate::model::{RepoId, RepositoryRecord, Timestamp};
use crate::proto::v0::register_request::IfNotJj;

/// Failure to register a repository.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegisterError {
    /// The path is not absolute.
    #[error("{} is not an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// No jj repository contains the path, or it could not be opened.
    #[error("open the jj repository at {}", path.display())]
    Open {
        /// The path as given.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: OpenError,
    },
    /// The git checkout's `.git` is a file (R1.25).
    #[error("{} is a git worktree or submodule; run from the main checkout", .0.display())]
    GitWorktree(PathBuf),
    /// Neither jj nor git has a repository containing the path (R1.25).
    #[error("{} is not a git or jj repository; run git init first", .0.display())]
    NotARepository(PathBuf),
    /// jj could not be initialized alongside git (R1.25).
    #[error("initialize jj alongside git at {}", root.display())]
    Init {
        /// The git checkout's root.
        root: PathBuf,
        /// The underlying error.
        #[source]
        source: InitError,
    },
    /// `/.jj/` could not be added to `.git/info/exclude` (R1.25).
    #[error("add /.jj/ to {}", path.display())]
    Exclude {
        /// The exclude file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The repository's root could not be resolved.
    #[error("resolve {}", path.display())]
    Resolve {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The repository's thread could not be started.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

impl Classify for RegisterError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotAbsolute(_) | Self::GitWorktree(_) => ErrorKind::InvalidArgument,
            Self::NotARepository(_) => ErrorKind::NotFound,
            Self::Open { source, .. } => source.kind(),
            Self::Init { source, .. } => source.kind(),
            Self::Resolve { source, .. } | Self::Exclude { source, .. } => {
                classify_by_source(source)
            }
            Self::Db(error) => error.kind(),
            Self::Registry(error) => error.kind(),
        }
    }
}

/// What [`RepoRegistry::register`] did.
#[derive(Clone, Debug)]
pub struct Registered {
    /// The repository's record.
    pub record: RepositoryRecord,
    /// It was registered before this call.
    pub already_registered: bool,
    /// This call created its `.jj` alongside git (R1.25).
    pub initialized_jj: bool,
}

impl RepoRegistry {
    /// Registers the jj repository containing `path` (absolute): opens it
    /// to find its root, records it in `repositories` (root and `.jj/repo`
    /// canonical), and starts its thread. A repository registered already
    /// only has its thread (re)started if needed.
    ///
    /// With [`IfNotJj::InitAlongsideGit`], a path in no jj repository is
    /// looked up in git instead: if the nearest ancestor with a `.git` has
    /// it as a directory, `/.jj/` is added to its `.git/info/exclude` and
    /// [`Repo::init_in_git_repo`] creates the jj side there (R1.25).
    ///
    /// Registrations take turns, so two of the same git checkout never both
    /// initialize it, nor open a `.jj` the other is still writing, and the
    /// one that initialized it is the one that records it.
    ///
    /// Blocks while the repository loads; call it from a blocking context.
    pub fn register(
        &self,
        db: &Db,
        path: &Path,
        settings: &UserSettings,
        if_not_jj: IfNotJj,
    ) -> Result<Registered, RegisterError> {
        if !path.is_absolute() {
            return Err(RegisterError::NotAbsolute(path.to_owned()));
        }
        let _turn = self
            .register_turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (repo, initialized_jj) = Self::open_or_init(path, settings, if_not_jj)?;
        let registered = |record, already_registered| Registered {
            record,
            already_registered,
            initialized_jj,
        };
        let canonical = |path: &Path| {
            std::fs::canonicalize(path).map_err(|source| RegisterError::Resolve {
                path: path.to_owned(),
                source,
            })
        };
        let root = canonical(repo.workspace_root())?;
        if let Some(existing) = registered_at(db, &root)? {
            self.open(&existing, settings.clone())?;
            return Ok(registered(existing, true));
        }
        let record = RepositoryRecord {
            id: RepoId::generate(),
            jj_repo_path: canonical(repo.repo_path())?,
            colocated: repo.is_colocated(),
            root,
            registered_at: Timestamp::now(),
            tracked_files: None,
        };
        drop(repo);
        match db.insert_repository(&record) {
            Ok(()) => {}
            // Another register of the same root won the race.
            Err(DbError::AlreadyExists { .. }) => {
                if let Some(existing) = registered_at(db, &record.root)? {
                    self.open(&existing, settings.clone())?;
                    return Ok(registered(existing, true));
                }
                return Err(DbError::AlreadyExists {
                    what: "repository",
                    id: record.id.as_hex(),
                }
                .into());
            }
            Err(error) => return Err(error.into()),
        }
        if let Err(error) = self.open(&record, settings.clone()) {
            // Leave no row behind for a repository the daemon cannot serve.
            if let Err(cleanup) = db.delete_repository(record.id) {
                tracing::error!(
                    repo = %record.id,
                    error = %reef_lib::error::ErrorChain(&cleanup),
                    "cannot remove the row of a repository that failed to open"
                );
            }
            return Err(error.into());
        }
        tracing::info!(
            repo = %record.id,
            root = %record.root.display(),
            colocated = record.colocated,
            "repository registered"
        );
        if initialized_jj {
            tracing::info!(
                repo = %record.id,
                root = %record.root.display(),
                "repository initialized alongside git"
            );
        }
        Ok(registered(record, false))
    }

    /// The jj repository containing `path`, created alongside git first if
    /// there is none and `if_not_jj` allows it, and whether it was created.
    fn open_or_init(
        path: &Path,
        settings: &UserSettings,
        if_not_jj: IfNotJj,
    ) -> Result<(Repo, bool), RegisterError> {
        let open = |at: &Path| {
            pollster::block_on(Repo::open(at, settings)).map_err(|source| RegisterError::Open {
                path: path.to_owned(),
                source,
            })
        };
        match open(path) {
            Err(RegisterError::Open {
                source: OpenError::NotAJjRepo(_),
                ..
            }) if if_not_jj == IfNotJj::InitAlongsideGit => {}
            opened => return opened.map(|repo| (repo, false)),
        }
        let root = match find_git_root(path) {
            Found::Git(root) => root,
            // `.jj` appeared since the open: it is a jj repository after all.
            Found::Jj(root) => return open(&root).map(|repo| (repo, false)),
            Found::GitFile(root) => return Err(RegisterError::GitWorktree(root)),
            Found::Nothing => return Err(RegisterError::NotARepository(path.to_owned())),
        };
        // The exclude line first: a failure or crash after it leaves only a
        // line that ignores a `.jj` that is not there, and the next attempt
        // finds the line and initializes.
        write_jj_exclude(&root).map_err(|source| RegisterError::Exclude {
            path: exclude_path(&root),
            source,
        })?;
        let repo = pollster::block_on(Repo::init_in_git_repo(&root, settings))
            .map_err(|source| RegisterError::Init { root, source })?;
        Ok((repo, true))
    }

    /// Forgets a repository: deletes its rows ([`Db::delete_repository`],
    /// which refuses while any of its workspaces is active) and stops its
    /// thread. Returns the forgotten record. Its state directory is the
    /// caller's to remove.
    pub fn forget(&self, db: &Db, id: RepoId) -> Result<RepositoryRecord, DbError> {
        let record = db.repository(id)?;
        db.delete_repository(id)?;
        self.close(id);
        tracing::info!(repo = %id, root = %record.root.display(), "repository forgotten");
        Ok(record)
    }
}

/// What [`find_git_root`] found.
enum Found {
    /// A directory with `.jj`.
    Jj(PathBuf),
    /// A directory whose `.git` is a directory.
    Git(PathBuf),
    /// A directory whose `.git` is a file.
    GitFile(PathBuf),
    /// Neither, up to the filesystem root.
    Nothing,
}

/// The nearest of `path` and its ancestors with a `.jj` directory or a
/// `.git`, `.jj` winning in the same directory. A `.git` directory without
/// `HEAD` is not a repository, as git's own discovery has it, and the walk
/// goes on past it.
fn find_git_root(path: &Path) -> Found {
    for dir in path.ancestors() {
        if dir.join(".jj").is_dir() {
            return Found::Jj(dir.to_owned());
        }
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            if dot_git.join("HEAD").is_file() {
                return Found::Git(dir.to_owned());
            }
        } else if dot_git.exists() {
            return Found::GitFile(dir.to_owned());
        }
    }
    Found::Nothing
}

/// The repository registered at exactly `root`, if any.
fn registered_at(db: &Db, root: &Path) -> Result<Option<RepositoryRecord>, DbError> {
    Ok(db
        .repository_containing(root)?
        .filter(|record| record.root == root))
}
