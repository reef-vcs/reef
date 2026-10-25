//! Registering and forgetting repositories (R1.05): the rows in
//! `repositories` and the threads in [`RepoRegistry`] together. A git
//! checkout without jj can be given jj's side of a colocated repository on
//! the way (R1.25, ADR 0024).

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use reef_lib::error::{Classify, ErrorKind, classify_by_source};
use reef_lib::jj::{InitError, OpenError, Repo, UserSettings};

use super::exclude::{exclude_path, write_jj_exclude};
use super::{RegistryError, RepoRegistry};
use crate::db::{Db, DbError};
use crate::error::{Advice, Advise};
use crate::model::{RepoId, RepositoryRecord, Timestamp};
use reef_api::proto::v1::register_request::IfNotJj;

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
    /// The repository's `.jj`, or the `.git` jj would be initialized
    /// beside, is another user's (RX.01, S-02).
    #[error(
        "{} is owned by uid {uid}; Reef registers repositories owned by the user running the daemon (or the sudo user)",
        root.display()
    )]
    ForeignOwner {
        /// The repository's root.
        root: PathBuf,
        /// The owner of its `.jj` or `.git`.
        uid: u32,
        /// The daemon's configuration file, for the hint.
        config_file: PathBuf,
    },
    /// The checkout's `HEAD` has a tree entry jj cannot read (RX.01).
    #[error(transparent)]
    TreeName(reef_lib::jj::TreeNameError),
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
            Self::ForeignOwner { .. } => ErrorKind::PermissionDenied,
            Self::TreeName(error) => error.kind(),
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

impl Advise for RegisterError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::NotARepository(_) => Some(Advice::hint(
                "Run `git init` there first, or name a directory inside a git or jj checkout.",
            )),
            Self::GitWorktree(_) => Some(Advice::hint(
                "Name a directory in the main checkout; Reef workspaces replace git worktrees.",
            )),
            Self::ForeignOwner { config_file, .. } => Some(Advice::hint(format!(
                "Run the daemon as that user, or set `[repo] allow_foreign_owner = true` in {}.",
                config_file.display()
            ))),
            Self::TreeName(_) => Some(Advice::hint(
                "HEAD's tree has an entry git itself never writes, which jj cannot read; check out another commit first.",
            )),
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

/// Whose repositories registration accepts (RX.01, S-02): git's
/// `safe.directory` rule. Registration walks up to `/` and then writes to
/// the repository, materializes its trees, and runs `git gc` with its
/// `.git/config`, so a `.git` another user planted in a shared directory
/// (`/tmp`, a CI scratch volume) must not be taken for the user's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerPolicy {
    /// The daemon's effective uid.
    pub euid: u32,
    /// The `sudo` user's uid (`SUDO_UID`, a decimal uid), read only when the
    /// euid is 0.
    pub sudo_uid: Option<u32>,
    /// `[repo] allow_foreign_owner`: accept every owner.
    pub allow_foreign: bool,
    /// The configuration file the refusal's hint names.
    pub config_file: PathBuf,
}

impl OwnerPolicy {
    /// The policy for this process: its euid, and `SUDO_UID` when that is
    /// 0.
    pub fn from_env(allow_foreign: bool, config_file: PathBuf) -> OwnerPolicy {
        let euid = nix::unistd::geteuid().as_raw();
        OwnerPolicy {
            euid,
            sudo_uid: sudo_uid(euid, std::env::var("SUDO_UID").ok().as_deref()),
            allow_foreign,
            config_file,
        }
    }

    /// Whether a repository whose `.jj` or `.git` `uid` owns is accepted.
    pub fn accepts(&self, uid: u32) -> bool {
        self.allow_foreign || uid == self.euid || self.sudo_uid == Some(uid)
    }

    /// Refuses `root` unless its `dir` (`.jj` or `.git`, not the root: on a
    /// shared volume the root may be world-writable while the repository
    /// is the user's) has an accepted owner. A `dir` that is a symlink must
    /// have one too, as must what it points at: another user's `/tmp/.jj`
    /// pointing at the user's own repository would otherwise register
    /// `/tmp` as its root.
    fn check(&self, root: &Path, dir: &Path) -> Result<(), RegisterError> {
        if self.allow_foreign {
            return Ok(());
        }
        let resolve = |source| RegisterError::Resolve {
            path: dir.to_owned(),
            source,
        };
        let link = std::fs::symlink_metadata(dir).map_err(resolve)?;
        let uid = if link.file_type().is_symlink() && self.accepts(link.uid()) {
            std::fs::metadata(dir).map_err(resolve)?.uid()
        } else {
            link.uid()
        };
        if self.accepts(uid) {
            return Ok(());
        }
        tracing::warn!(
            root = %root.display(),
            uid,
            euid = self.euid,
            sudo_uid = self.sudo_uid,
            "refused to register a repository another user owns"
        );
        Err(RegisterError::ForeignOwner {
            root: root.to_owned(),
            uid,
            config_file: self.config_file.clone(),
        })
    }
}

/// `SUDO_UID` as a uid when `euid` is 0 and it is a decimal number, as git
/// reads it.
fn sudo_uid(euid: u32, var: Option<&str>) -> Option<u32> {
    let var = var.filter(|_| euid == 0)?;
    if var.is_empty() || !var.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    var.parse().ok()
}

impl Default for OwnerPolicy {
    fn default() -> Self {
        OwnerPolicy::from_env(false, PathBuf::from("config.toml"))
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
        let (repo, initialized_jj) = Self::open_or_init(path, settings, if_not_jj, &self.owners)?;
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
    /// Its `.jj`, or the `.git` it is created beside, must have an owner
    /// `owners` accepts, checked before it is opened or written.
    fn open_or_init(
        path: &Path,
        settings: &UserSettings,
        if_not_jj: IfNotJj,
        owners: &OwnerPolicy,
    ) -> Result<(Repo, bool), RegisterError> {
        let open = |at: &Path| {
            // jj's own discovery: the nearest ancestor with a `.jj`.
            if let Some(root) = at.ancestors().find(|dir| dir.join(".jj").is_dir()) {
                owners.check(root, &root.join(".jj"))?;
            }
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
        owners.check(&root, &root.join(".git"))?;
        // jj checks HEAD out as it initializes, and a tree it cannot read
        // panics it there, leaving a half-made `.jj`.
        reef_lib::jj::check_git_head_tree_names(&root).map_err(RegisterError::TreeName)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(euid: u32, sudo_uid: Option<u32>) -> OwnerPolicy {
        OwnerPolicy {
            euid,
            sudo_uid,
            allow_foreign: false,
            config_file: PathBuf::from("/state/config.toml"),
        }
    }

    #[test]
    fn sudo_uid_counts_only_for_root_and_only_as_a_decimal() {
        assert_eq!(sudo_uid(0, Some("1000")), Some(1000));
        assert_eq!(sudo_uid(1000, Some("1000")), None);
        assert_eq!(sudo_uid(0, None), None);
        for bad in ["", "+1", "-1", " 1", "1 ", "0x10", "4294967296"] {
            assert_eq!(sudo_uid(0, Some(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn owners_accepted_are_the_euid_and_the_sudo_user() {
        let user = policy(1000, None);
        assert!(user.accepts(1000));
        assert!(!user.accepts(0) && !user.accepts(1001));
        let sudo = policy(0, Some(1000));
        assert!(sudo.accepts(0) && sudo.accepts(1000));
        assert!(!sudo.accepts(1001));
        let open = OwnerPolicy {
            allow_foreign: true,
            ..user
        };
        assert!(open.accepts(1001));
    }

    /// The check on a real directory, with the owner the policy expects
    /// injected rather than chowned.
    #[test]
    fn a_dir_another_uid_owns_is_refused_with_the_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        std::fs::create_dir(&dot_git).unwrap();
        let mine = std::fs::metadata(&dot_git).unwrap().uid();
        policy(mine, None).check(tmp.path(), &dot_git).unwrap();
        let other = mine.wrapping_add(1);
        let error = policy(other, None).check(tmp.path(), &dot_git).unwrap_err();
        assert!(
            matches!(&error, RegisterError::ForeignOwner { uid, .. } if *uid == mine),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        let hint = error.advice().unwrap().hint;
        assert!(
            hint.contains("[repo] allow_foreign_owner = true")
                && hint.contains("/state/config.toml"),
            "{hint}"
        );
        let open = OwnerPolicy {
            allow_foreign: true,
            ..policy(other, None)
        };
        open.check(tmp.path(), &dot_git).unwrap();
        policy(0, Some(mine)).check(tmp.path(), &dot_git).unwrap();
    }
}
