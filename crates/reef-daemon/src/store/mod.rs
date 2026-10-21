//! Shared dependency stores (R1.20, ADR 0022): a directory per `(store
//! name, lockfile hash)` that the daemon builds once from a `[[store]]` in
//! `reef.toml`, seals read-only, and links into every workspace whose
//! lockfile hashes the same.
//!
//! - **Identity**: blake3 of the lockfile's bytes in the target commit's
//!   tree, never on disk ([`hash_lockfile`]). The directory is
//!   `<state>/stores/<name>/<hash>/`; the row is in `stores`. Stores are
//!   per daemon: two repositories with the same store name and lockfile
//!   bytes share one.
//! - **Builds** ([`build`]) run the spec's `build` argv in a scratch
//!   workspace with the store's mount path linked to
//!   `<name>/<hash>.building/`. Exit 0 seals it; anything else renames it to
//!   `<hash>.failed-<ms>/`, kept until GC. Only the daemon writes a store.
//! - **Mounts** (`mount`): a symlink at the mount path, or on Linux with
//!   `CAP_SYS_ADMIN` a read-only bind mount, which a same-uid agent cannot
//!   undo the way it can `chmod` a sealed store's files.
//!
//! `crate::workspace` mounts stores at create, unmounts them before the
//! backend's tear-down, and checks each snapshot's lockfile against the
//! one the store was chosen by.

pub mod build;
pub(crate) mod mount;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use reef_lib::error::{Classify, ErrorKind, classify_by_source};

use crate::db::{Db, DbError};
use crate::model::{RepoId, StoreRecord, Timestamp};

/// The suffix of a store directory while its build runs.
const BUILDING: &str = "building";
/// The infix of a failed build's directory, `<hash>.failed-<ms>`.
pub(crate) const FAILED: &str = "failed-";
/// The infix of a store directory a forced rebuild replaced,
/// `<hash>.replaced-<ms>`.
pub(crate) const REPLACED: &str = "replaced-";

/// blake3 of a lockfile's bytes, in lowercase hex: a store's key.
pub fn hash_lockfile(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A store's identity: its name and its lockfile's hash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreKey {
    /// The `[[store]] name`.
    pub name: String,
    /// [`hash_lockfile`] of the lockfile at the commit, 64 hex characters.
    pub lockfile_hash: String,
}

impl std::fmt::Display for StoreKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.name, self.lockfile_hash)
    }
}

/// How a store appears in a workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountMode {
    /// A symlink to the store directory.
    Symlink,
    /// A read-only bind mount (Linux, `CAP_SYS_ADMIN`).
    Bind,
}

impl MountMode {
    /// The API's name: `symlink` or `bind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::Bind => "bind",
        }
    }
}

/// A store mounted in a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountedStore {
    /// Which store.
    pub key: StoreKey,
    /// Where, repository-root relative.
    pub mount: String,
    /// The lockfile, repository-root relative.
    pub lockfile: String,
    /// How.
    pub mode: MountMode,
    /// The lockfile's blob id in the tree the store was chosen by; a
    /// snapshot whose lockfile has another id no longer matches it.
    pub lockfile_id: String,
    /// Whether the last check found the workspace's lockfile unchanged.
    pub verified: bool,
}

/// Failure of a store operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// No sealed store for the lockfile's hash; the request did not ask to
    /// skip missing stores.
    #[error(
        "store {name} has no build for lockfile hash {hash}; run: reef store build {name} --repo {repo} --rev {commit}"
    )]
    Missing {
        /// The store.
        name: String,
        /// Its lockfile's hash at `commit`.
        hash: String,
        /// The repository.
        repo: RepoId,
        /// The commit the workspace was to start from.
        commit: String,
    },
    /// The tree already has something at the mount path.
    #[error(
        "mount path {mount} exists in the tree; add it to .gitignore and remove it from the change"
    )]
    MountExists {
        /// The mount path.
        mount: String,
    },
    /// An ancestor of the mount path is a file or a symlink in the tree.
    #[error("mount path {mount}: {} is not a directory", path.display())]
    MountBlocked {
        /// The mount path.
        mount: String,
        /// The ancestor.
        path: PathBuf,
    },
    /// The spec at the revision declares no store by this name.
    #[error("no store named {name} in reef.toml at {commit}")]
    NotDeclared {
        /// The name asked for.
        name: String,
        /// The commit read.
        commit: String,
    },
    /// A build of the same store is running.
    #[error("store {name} build already running (lockfile hash {hash})")]
    BuildRunning {
        /// The store.
        name: String,
        /// Its lockfile hash.
        hash: String,
    },
    /// A forced rebuild of a store workspaces mount: it would move the
    /// directory out from under their bind mounts.
    #[error(
        "store {name} (lockfile hash {hash}) is mounted by {count} active workspace(s); release them before rebuilding it"
    )]
    InUse {
        /// The store.
        name: String,
        /// Its lockfile hash.
        hash: String,
        /// How many workspaces mount it.
        count: u32,
    },
    /// The build command exited non-zero or was killed by a signal.
    #[error("store {name} build exited with {code}")]
    BuildFailed {
        /// The store.
        name: String,
        /// The exit code; -1 for a signal.
        code: i32,
    },
    /// The build ran past its timeout; its process group was killed.
    #[error("store {name} build timed out after {secs}s")]
    BuildTimeout {
        /// The store.
        name: String,
        /// The timeout.
        secs: u64,
    },
    /// The daemon began shutting down during the build; its process group
    /// was killed.
    #[error("store {name} build cancelled: the daemon is shutting down")]
    BuildCancelled {
        /// The store.
        name: String,
    },
    /// The build command could not be started.
    #[error("start store {name}'s build command {program:?}")]
    Spawn {
        /// The store.
        name: String,
        /// `build[0]`.
        program: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A filesystem operation failed.
    #[error("{op} {}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// To what.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The database failed.
    #[error("database")]
    Db(#[from] DbError),
}

impl Classify for StoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Missing { .. } | Self::BuildCancelled { .. } => ErrorKind::Unavailable,
            Self::MountExists { .. } | Self::MountBlocked { .. } | Self::InUse { .. } => {
                ErrorKind::Conflict
            }
            Self::NotDeclared { .. } => ErrorKind::NotFound,
            Self::BuildRunning { .. } => ErrorKind::AlreadyExists,
            Self::BuildFailed { .. } | Self::BuildTimeout { .. } => ErrorKind::Internal,
            Self::Spawn { source, .. } | Self::Io { source, .. } => classify_by_source(source),
            Self::Db(error) => error.kind(),
        }
    }
}

/// The daemon's stores: where they are, which are sealed, which builds
/// run, and how workspaces get them. One per daemon, in
/// [`crate::workspace::Manager::stores`].
pub struct StoreManager {
    /// `<state>/stores`.
    dir: PathBuf,
    db: Arc<Db>,
    /// Whether workspaces get read-only bind mounts: Linux with
    /// `CAP_SYS_ADMIN`.
    privileged: bool,
    /// The builds running now. In memory, since no build outlives the
    /// daemon; [`StoreManager::sweep_interrupted`] clears what a stop left.
    running: Mutex<HashSet<StoreKey>>,
    /// Held shared by creates from looking their stores up again to their
    /// row, exclusively by GC while it removes stores and by a seal while
    /// it swaps a store's directory, so neither happens under a mount.
    pub(crate) mounting: tokio::sync::RwLock<()>,
}

impl std::fmt::Debug for StoreManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreManager")
            .field("dir", &self.dir)
            .field("privileged", &self.privileged)
            .finish_non_exhaustive()
    }
}

impl StoreManager {
    /// The stores under `state_dir/stores`, probing for the bind tier.
    pub fn new(state_dir: &Path, db: Arc<Db>) -> StoreManager {
        Self::with_privilege(state_dir, db, bind_tier_available())
    }

    /// [`StoreManager::new`] with the bind tier decided by the caller.
    pub fn with_privilege(state_dir: &Path, db: Arc<Db>, privileged: bool) -> StoreManager {
        StoreManager {
            dir: state_dir.join("stores"),
            db,
            privileged,
            running: Mutex::new(HashSet::new()),
            mounting: tokio::sync::RwLock::new(()),
        }
    }

    /// `<state>/stores`.
    pub fn root(&self) -> &Path {
        &self.dir
    }

    /// Whether new mounts are bind mounts where the backend allows it.
    pub fn privileged(&self) -> bool {
        self.privileged
    }

    /// `<state>/stores/<name>/<hash>`.
    pub fn dir(&self, key: &StoreKey) -> PathBuf {
        self.dir.join(&key.name).join(&key.lockfile_hash)
    }

    /// `<state>/stores/<name>/<hash>.<suffix>`.
    fn sibling(&self, key: &StoreKey, suffix: &str) -> PathBuf {
        self.dir
            .join(&key.name)
            .join(format!("{}.{suffix}", key.lockfile_hash))
    }

    /// Where the store's build writes until it is sealed.
    pub fn building_dir(&self, key: &StoreKey) -> PathBuf {
        self.sibling(key, BUILDING)
    }

    /// The store's sealed row, if it has one.
    pub fn sealed(&self, key: &StoreKey) -> Result<Option<StoreRecord>, StoreError> {
        Ok(self
            .db
            .store(&key.name, &key.lockfile_hash)?
            .filter(|row| row.sealed))
    }

    /// Mounts the sealed store `key` at `mount` in the workspace at
    /// `ws_root`: a bind mount when the daemon is privileged and
    /// `bind_allowed` (not on a FUSE overlay), a symlink otherwise.
    pub fn mount(
        &self,
        key: &StoreKey,
        ws_root: &Path,
        mount: &str,
        bind_allowed: bool,
    ) -> Result<MountMode, StoreError> {
        let store = self.dir(key);
        #[cfg(target_os = "linux")]
        if self.privileged && bind_allowed {
            mount::bind_mount(&store, ws_root, mount)?;
            return Ok(MountMode::Bind);
        }
        let _ = bind_allowed;
        mount::symlink_mount(&store, ws_root, mount)?;
        Ok(MountMode::Symlink)
    }

    /// At daemon start: whether `key` is still mounted at `mount` in the
    /// workspace, mounting it again where a reboot dropped a bind mount
    /// (`bind_allowed` as for [`StoreManager::mount`]). `None` when the
    /// workspace does not have it (it was skipped, or the agent removed it).
    pub fn remount(
        &self,
        key: &StoreKey,
        ws_root: &Path,
        mount: &str,
        bind_allowed: bool,
    ) -> Result<Option<MountMode>, StoreError> {
        let store = self.dir(key);
        let found = mount::find(&store, ws_root, mount)?;
        Ok(match found {
            mount::Found::Symlink => Some(MountMode::Symlink),
            mount::Found::Bound => Some(MountMode::Bind),
            mount::Found::EmptyDir if self.privileged && bind_allowed => {
                let target = mount::mount_path(ws_root, mount);
                std::fs::remove_dir(&target).map_err(|source| StoreError::Io {
                    op: "remove mount point",
                    path: target,
                    source,
                })?;
                Some(self.mount(key, ws_root, mount, true)?)
            }
            mount::Found::EmptyDir | mount::Found::Nothing | mount::Found::Other => None,
        })
    }

    /// Takes a store out of a workspace: removes the symlink, or detaches
    /// the bind mount and removes its mount point. Something else at the
    /// path is left for the backend's tear-down.
    pub fn unmount(&self, ws_root: &Path, mount: &str, mode: MountMode) -> Result<(), StoreError> {
        mount::unmount(ws_root, mount, mode)
    }

    /// Marks `key`'s build as running; [`StoreError::BuildRunning`] if one
    /// already is. The guard clears the mark.
    pub fn start_build(&self, key: &StoreKey) -> Result<BuildGuard<'_>, StoreError> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if !running.insert(key.clone()) {
            return Err(StoreError::BuildRunning {
                name: key.name.clone(),
                hash: key.lockfile_hash.clone(),
            });
        }
        Ok(BuildGuard {
            stores: self,
            key: key.clone(),
        })
    }

    /// Whether a build of `key` is running.
    pub fn is_building(&self, key: &StoreKey) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(key)
    }

    /// Seals the finished build in `building` as `key`: clears every write
    /// bit, renames it into place (a store it replaces moves aside to
    /// `<hash>.replaced-<ms>`) under the mount lock, and marks the row
    /// sealed. Returns the row and the bytes it allocates. Blocks; call it
    /// on the blocking pool.
    pub fn seal(&self, building: &Path, key: &StoreKey) -> Result<(StoreRecord, u64), StoreError> {
        let bytes = mount::seal_tree(building)?;
        let dir = self.dir(key);
        // No create mounts the store while its directory is swapped.
        let _no_mounts = self.mounting.blocking_write();
        if std::fs::symlink_metadata(&dir).is_ok() {
            let aside = self.sibling(key, &format!("{REPLACED}{}", now_millis()));
            rename(&dir, &aside)?;
        }
        rename(building, &dir)?;
        let built_at = Timestamp::now();
        self.db
            .seal_store(&key.name, &key.lockfile_hash, built_at)?;
        let row = self
            .db
            .store(&key.name, &key.lockfile_hash)?
            .ok_or_else(|| {
                StoreError::Db(DbError::NotFound {
                    what: "store",
                    id: key.to_string(),
                })
            })?;
        Ok((row, bytes))
    }

    /// Moves a failed build's directory aside to `<hash>.failed-<ms>`,
    /// kept for inspection until GC. Returns where it went, or `None` if
    /// the build left no directory.
    pub fn fail(&self, building: &Path, key: &StoreKey) -> Result<Option<PathBuf>, StoreError> {
        if std::fs::symlink_metadata(building).is_err() {
            return Ok(None);
        }
        let failed = self.sibling(key, &format!("{FAILED}{}", now_millis()));
        rename(building, &failed)?;
        Ok(Some(failed))
    }

    /// At daemon start: deletes the rows of builds a stop or crash
    /// interrupted and moves their `.building` directories aside as failed.
    /// Failures are logged. Returns how many it swept.
    pub fn sweep_interrupted(&self) -> usize {
        let rows = match self.db.delete_unsealed_stores() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(error = %reef_lib::error::ErrorChain(&error), "cannot sweep interrupted store builds");
                return 0;
            }
        };
        let mut keys: HashSet<StoreKey> = rows
            .into_iter()
            .map(|row| StoreKey {
                name: row.name,
                lockfile_hash: row.lockfile_hash,
            })
            .collect();
        // A forced rebuild keeps its sealed row; only its directory says
        // it was running.
        keys.extend(self.building_dirs());
        // A crash between renaming a build into place and sealing its row
        // leaves a store directory nothing records.
        for key in self.unrecorded_dirs() {
            match self.fail(&self.dir(&key), &key) {
                Ok(_) => tracing::warn!(
                    store = %key.name,
                    lockfile_hash = %key.lockfile_hash,
                    "a store directory has no sealed row; kept as failed"
                ),
                Err(error) => tracing::error!(
                    store = %key.name,
                    error = %reef_lib::error::ErrorChain(&error),
                    "cannot move an unrecorded store directory aside"
                ),
            }
        }
        for key in &keys {
            let building = self.building_dir(key);
            match self.fail(&building, key) {
                Ok(Some(path)) => tracing::warn!(
                    store = %key.name,
                    lockfile_hash = %key.lockfile_hash,
                    path = %path.display(),
                    "a store build was interrupted; its directory is kept as failed"
                ),
                Ok(None) => {}
                Err(error) => tracing::error!(
                    store = %key.name,
                    error = %reef_lib::error::ErrorChain(&error),
                    "cannot move an interrupted store build aside"
                ),
            }
        }
        keys.len()
    }

    /// At daemon start: removes what background deletions of stores (GC)
    /// left behind, `*.trash-*` under each `<state>/stores/<name>/`.
    /// Failures are logged. Returns how many it removed.
    pub fn sweep_trash(&self) -> u64 {
        let Ok(names) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        names
            .flatten()
            .map(|name| match crate::backend::fs_util::sweep_trash(&name.path()) {
                Ok(removed) => removed,
                Err(error) => {
                    tracing::warn!(path = %name.path().display(), %error, "store trash sweep failed");
                    0
                }
            })
            .sum()
    }

    /// The keys of every `<name>/<hash>` store directory with no sealed row.
    fn unrecorded_dirs(&self) -> Vec<StoreKey> {
        let Ok(names) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for name in names.flatten() {
            let (Ok(name_text), Ok(entries)) = (
                name.file_name().into_string(),
                std::fs::read_dir(name.path()),
            ) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file) = entry.file_name().into_string() else {
                    continue;
                };
                if file.contains('.') {
                    continue;
                }
                let key = StoreKey {
                    name: name_text.clone(),
                    lockfile_hash: file,
                };
                if matches!(self.sealed(&key), Ok(None)) {
                    found.push(key);
                }
            }
        }
        found
    }

    /// The keys of every `<hash>.building` directory on disk.
    fn building_dirs(&self) -> Vec<StoreKey> {
        let suffix = format!(".{BUILDING}");
        self.leftovers()
            .into_iter()
            .filter_map(|(name, file, _)| {
                let hash = file.strip_suffix(&suffix)?;
                Some(StoreKey {
                    name,
                    lockfile_hash: hash.to_owned(),
                })
            })
            .collect()
    }

    /// Every `(name, file name, path)` under `<state>/stores/<name>/` that
    /// is not a store directory: `.building`, `.failed-*`, `.replaced-*`,
    /// and `.trash-*` siblings. Unreadable directories are skipped.
    pub(crate) fn leftovers(&self) -> Vec<(String, String, PathBuf)> {
        let mut found = Vec::new();
        let Ok(names) = std::fs::read_dir(&self.dir) else {
            return found;
        };
        for name in names.flatten() {
            let Ok(name_text) = name.file_name().into_string() else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(name.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file) = entry.file_name().into_string() else {
                    continue;
                };
                if file.contains('.') {
                    found.push((name_text.clone(), file, entry.path()));
                }
            }
        }
        found
    }
}

/// Clears a running build's mark when dropped; see
/// [`StoreManager::start_build`].
#[derive(Debug)]
pub struct BuildGuard<'a> {
    stores: &'a StoreManager,
    key: StoreKey,
}

impl Drop for BuildGuard<'_> {
    fn drop(&mut self) {
        self.stores
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), StoreError> {
    crate::backend::fs_util::rename_even_read_only(from, to).map_err(|source| StoreError::Io {
        op: "rename",
        path: from.to_owned(),
        source,
    })
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

/// Whether workspaces get read-only bind mounts: Linux, with
/// `CAP_SYS_ADMIN` (the probe R1.09's kernel overlay tier uses).
fn bind_tier_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::backend::overlay::has_cap_sys_admin().unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_blake3_hex() {
        // blake3("") from the reference test vectors.
        assert_eq!(
            hash_lockfile(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(hash_lockfile(b"v1\n").len(), 64);
        assert_ne!(hash_lockfile(b"v1\n"), hash_lockfile(b"v2\n"));
    }

    fn manager(state: &Path) -> StoreManager {
        StoreManager::with_privilege(state, Arc::new(Db::open_in_memory().unwrap()), false)
    }

    fn key() -> StoreKey {
        StoreKey {
            name: "deps".to_owned(),
            lockfile_hash: hash_lockfile(b"v1\n"),
        }
    }

    #[test]
    fn a_second_build_of_a_key_is_refused_until_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let stores = manager(tmp.path());
        let guard = stores.start_build(&key()).unwrap();
        assert!(stores.is_building(&key()));
        let error = stores.start_build(&key()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        drop(guard);
        assert!(!stores.is_building(&key()));
        drop(stores.start_build(&key()).unwrap());
    }

    #[test]
    fn seal_replaces_and_interrupted_builds_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let stores = manager(tmp.path());
        let key = key();
        stores
            .db
            .insert_store(&StoreRecord {
                repo_id: "aaaa000000000001".parse().unwrap(),
                name: key.name.clone(),
                lockfile_hash: key.lockfile_hash.clone(),
                path: stores.dir(&key),
                built_at: None,
                sealed: false,
            })
            .unwrap();
        let building = stores.building_dir(&key);
        std::fs::create_dir_all(&building).unwrap();
        std::fs::write(building.join("manifest"), "v1").unwrap();
        let (row, _) = stores.seal(&building, &key).unwrap();
        assert!(row.sealed);
        assert_eq!(stores.sealed(&key).unwrap(), Some(row));
        assert!(stores.dir(&key).join("manifest").is_file());

        // A forced rebuild interrupted by a stop: its sealed row stays, its
        // directory goes aside as failed.
        std::fs::create_dir_all(&building).unwrap();
        assert_eq!(stores.sweep_interrupted(), 1);
        assert!(!building.exists());
        assert!(stores.sealed(&key).unwrap().is_some());
        let leftovers = stores.leftovers();
        assert_eq!(leftovers.len(), 1);
        assert!(leftovers[0].1.contains(".failed-"), "{leftovers:?}");

        // A rebuild that seals moves the old store aside.
        std::fs::create_dir_all(&building).unwrap();
        std::fs::write(building.join("manifest"), "v2").unwrap();
        stores.seal(&building, &key).unwrap();
        assert_eq!(
            std::fs::read_to_string(stores.dir(&key).join("manifest")).unwrap(),
            "v2"
        );
        assert!(
            stores
                .leftovers()
                .iter()
                .any(|(_, file, _)| file.contains(".replaced-"))
        );
        crate::backend::fs_util::remove_all(stores.root()).unwrap();
    }

    #[test]
    fn a_store_directory_without_a_sealed_row_is_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let stores = manager(tmp.path());
        let key = key();
        // A crash between renaming the build into place and sealing its row.
        std::fs::create_dir_all(stores.dir(&key)).unwrap();
        assert_eq!(stores.sweep_interrupted(), 0);
        assert!(!stores.dir(&key).exists());
        let leftovers = stores.leftovers();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert!(leftovers[0].1.contains(".failed-"), "{leftovers:?}");
    }
}
