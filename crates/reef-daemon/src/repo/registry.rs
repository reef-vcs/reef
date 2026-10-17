//! Registering and forgetting repositories (R1.05): the rows in
//! `repositories` and the threads in [`RepoRegistry`] together.

use std::io;
use std::path::{Path, PathBuf};

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::{OpenError, Repo, UserSettings};

use super::{RegistryError, RepoRegistry};
use crate::db::{Db, DbError};
use crate::model::{RepoId, RepositoryRecord, Timestamp};

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
            Self::NotAbsolute(_) => ErrorKind::InvalidArgument,
            Self::Open { source, .. } => source.kind(),
            Self::Resolve { source, .. } => classify_by_source(source),
            Self::Db(error) => error.kind(),
            Self::Registry(error) => error.kind(),
        }
    }
}

impl RepoRegistry {
    /// Registers the jj repository containing `path` (absolute): opens it
    /// to find its root, records it in `repositories` (root and `.jj/repo`
    /// canonical), and starts its thread. Returns the record and whether it
    /// was already registered, in which case only its thread is (re)started
    /// if needed.
    ///
    /// Blocks while the repository loads; call it from a blocking context.
    pub fn register(
        &self,
        db: &Db,
        path: &Path,
        settings: &UserSettings,
    ) -> Result<(RepositoryRecord, bool), RegisterError> {
        if !path.is_absolute() {
            return Err(RegisterError::NotAbsolute(path.to_owned()));
        }
        let repo = pollster::block_on(Repo::open(path, settings)).map_err(|source| {
            RegisterError::Open {
                path: path.to_owned(),
                source,
            }
        })?;
        let canonical = |path: &Path| {
            std::fs::canonicalize(path).map_err(|source| RegisterError::Resolve {
                path: path.to_owned(),
                source,
            })
        };
        let root = canonical(repo.workspace_root())?;
        if let Some(existing) = registered_at(db, &root)? {
            self.open(&existing, settings.clone())?;
            return Ok((existing, true));
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
                    return Ok((existing, true));
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
        Ok((record, false))
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

/// The repository registered at exactly `root`, if any.
fn registered_at(db: &Db, root: &Path) -> Result<Option<RepositoryRecord>, DbError> {
    Ok(db
        .repository_containing(root)?
        .filter(|record| record.root == root))
}
