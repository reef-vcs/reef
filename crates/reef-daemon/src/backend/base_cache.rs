//! The base cache: every tree a workspace is materialized from, written once
//! per machine through jj-lib (ADR 0015).
//!
//! An entry is two directories under `<state>/repos/<repo-id>/bases/`:
//! `<tree-id>/`, the tree as jj checks it out, and `<tree-id>.state/`, jj's
//! working-copy state for it, whose `tree_state` seeds the working copy of
//! every workspace on that tree (R1.05). Both are written under `.tmp`
//! names, renamed (state first), and only then recorded in `base_cache`, so
//! a directory without a row is incomplete; [`BaseCache::sweep_incomplete`]
//! removes those at daemon start. GC ([`BaseCache::evict`], R1.17) deletes
//! the row first and renames both directories to `*.gc` names before
//! removing them, so an eviction cut short leaves only row-less leftovers
//! for the same sweep.
//!
//! A clone backend builds a new base from the most recently used one
//! ([`BaseCache::ensure_from`], R1.08): it clones that base's two
//! directories and has jj check out only the diff, instead of writing the
//! whole tree.
//!
//! Entries are immutable by convention: nothing writes under a base after
//! its rename. Their permissions stay as jj wrote them, because copy and
//! clone backends read them and overlayfs needs them readable. R1.11 checks
//! that a base is unchanged after a workspace built from it is modified and
//! torn down.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Instant, SystemTime};

use reef_lib::error::{Classify, ErrorChain, ErrorKind, classify_by_source};
use reef_lib::jj::materialize::{TREE_STATE_FILE, is_tree_key, tree_state_entries};
use reef_lib::jj::{MaterializeError, MaterializeStats};
use reef_lib::telemetry::elapsed_ms;

use super::BackendError;
use super::fs_util::allocated_bytes;
use crate::db::{Db, DbError};
use crate::model::{BaseCacheRecord, RepoId, Timestamp};

/// Suffix of an entry's state directory, beside the tree.
const STATE_SUFFIX: &str = ".state";
/// Suffix of a tree being materialized.
const TMP_SUFFIX: &str = ".tmp";
/// Suffix of a state directory being written.
const TMP_STATE_SUFFIX: &str = ".tmp.state";
/// Suffix of an evicted directory being removed (R1.17).
const GC_SUFFIX: &str = ".gc";
/// The one-byte file the APFS probe clones into a workspace directory to
/// see whether it can clone bases there (R1.08). Not an entry; the sweep
/// keeps it.
pub(crate) const PROBE_FILE: &str = ".probe";

/// Implemented by backends that can clone a whole directory cheaply, for
/// [`BaseCache::ensure_from`].
pub trait DirCloner: Sync {
    /// Clones the directory `from` to `to`, which must not exist (its parent
    /// does), keeping every file's mtime and size.
    fn clone_dir(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// One materialized tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseEntry {
    /// The repository it belongs to.
    pub repo_id: RepoId,
    /// The base cache key (`reef_lib::jj::materialize::tree_key`).
    pub tree_id: String,
    /// The tree: `bases/<tree-id>`.
    pub path: PathBuf,
    /// jj's working-copy state for it: `bases/<tree-id>.state`.
    pub state_dir: PathBuf,
    /// Total size of its files as jj recorded them.
    pub bytes: u64,
    /// Number of files and symlinks.
    pub files: u64,
}

impl BaseEntry {
    /// The `tree_state` file that seeds workspaces on this tree.
    pub fn tree_state_file(&self) -> PathBuf {
        self.state_dir.join(TREE_STATE_FILE)
    }

    fn from_record(record: BaseCacheRecord) -> Self {
        Self {
            state_dir: with_suffix(&record.path, STATE_SUFFIX),
            repo_id: record.repo_id,
            tree_id: record.tree_id,
            path: record.path,
            bytes: record.bytes,
            files: record.files,
        }
    }

    /// Whether both directories and the `tree_state` are on disk.
    pub(crate) fn is_complete(&self) -> bool {
        self.path.is_dir() && self.tree_state_file().is_file()
    }

    /// [`BaseEntry::is_complete`], and the `tree_state` decodes with one
    /// entry per recorded file. jj writes files without fsync, so after a
    /// power cut the directories and the row can survive with a truncated
    /// `tree_state`; this catches that case (not a truncated file of the
    /// tree itself whose state entry survived), at the cost of decoding
    /// the state, which is why only the start-up sweep does it.
    fn is_valid(&self) -> bool {
        self.is_complete()
            && tree_state_entries(&self.state_dir)
                .is_ok_and(|entries| entries.len() as u64 == self.files)
    }
}

/// An entry [`BaseCache::evict`] removed, or would remove in a dry run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evicted {
    /// The entry's tree.
    pub tree_id: String,
    /// What its two directories had allocated.
    pub bytes: u64,
}

/// Failure of a base cache operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BaseCacheError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A directory could not be created, renamed, read, or removed.
    #[error("{op} {}", path.display())]
    Io {
        /// What was being done, such as `rename`.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// jj could not materialize the tree.
    #[error("failed to materialize the tree")]
    Materialize(#[from] MaterializeError),
    /// A materialization reported success but left no `tree_state`, or a
    /// recorded entry's tree is on disk without its state. The entry is
    /// not removed while the daemon runs (an overlay may use the tree as
    /// its lower layer); the start-up sweep drops it.
    #[error("base cache entry {} is incomplete", .0.display())]
    Incomplete(PathBuf),
    /// The key is not a tree id (or `+`-joined tree ids) in lowercase hex.
    #[error("{0:?} is not a tree id")]
    InvalidTreeId(String),
    /// [`BaseCache::ensure_from`] was asked to build an entry from itself.
    #[error("base {0} cannot be built from itself")]
    SameBase(String),
    /// An incremental check-out left files unwritten because something
    /// unexpected was in their way (with a missing `tree_state`, jj sees an
    /// empty tree and skips every file), so the base would be incomplete.
    #[error("check-out of base {tree_id} skipped {count} files")]
    SkippedFiles {
        /// The base being built.
        tree_id: String,
        /// Files jj skipped.
        count: u32,
    },
    /// Measuring an entry's disk use failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
}

impl Classify for BaseCacheError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Db(error) => error.kind(),
            Self::Io { source, .. } => classify_by_source(source),
            Self::Materialize(error) => error.kind(),
            Self::Incomplete(_) | Self::SkippedFiles { .. } => ErrorKind::Internal,
            Self::InvalidTreeId(_) | Self::SameBase(_) => ErrorKind::InvalidArgument,
            Self::Backend(error) => error.kind(),
        }
    }
}

/// One repository's base cache.
pub struct BaseCache {
    db: Arc<Db>,
    repo_id: RepoId,
    /// `<state>/repos/<repo-id>/bases`.
    dir: PathBuf,
    /// One lock per tree id being worked on, so two creates on the same
    /// tree materialize it once. The outer lock is held only to find or
    /// insert an entry, never while materializing.
    key_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Held shared by `ensure` and `remove`, exclusively by
    /// `sweep_incomplete`, so a sweep never removes the temporary
    /// directories of a materialization in progress.
    sweep: RwLock<()>,
}

impl BaseCache {
    /// The base cache of repository `repo_id`, whose directory is `repo_dir`
    /// (`<state>/repos/<repo-id>`). Nothing is read or created yet.
    pub fn new(db: Arc<Db>, repo_dir: &Path, repo_id: RepoId) -> Self {
        Self {
            db,
            repo_id,
            dir: repo_dir.join("bases"),
            key_locks: Mutex::new(HashMap::new()),
            sweep: RwLock::new(()),
        }
    }

    /// The `bases/` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the entry for `tree_id`, materializing it first if absent,
    /// and bumps its `last_used_at`.
    ///
    /// `materialize` runs the jj check-out wherever the repository lives
    /// (its thread, R1.03); it receives the temporary tree directory and the
    /// temporary state directory, neither of which exists yet, and is
    /// normally `Repo::materialize_tree` on the commit. It is not called if
    /// the entry exists. Concurrent calls for one `tree_id` wait for each
    /// other and materialize once; calls for different trees run in
    /// parallel. The closure runs with the tree's lock held, so it must not
    /// call `ensure` for the same tree.
    ///
    /// An entry whose tree directory has gone missing is dropped and
    /// materialized again. One whose tree is on disk without its state is
    /// [`BaseCacheError::Incomplete`] until the next start-up sweep, because
    /// the tree may be a live overlay's lower layer.
    pub fn ensure(
        &self,
        tree_id: &str,
        materialize: impl FnOnce(&Path, &Path) -> Result<MaterializeStats, MaterializeError>,
    ) -> Result<BaseEntry, BaseCacheError> {
        check_key(tree_id)?;
        self.with_key_lock(tree_id, || {
            self.ensure_locked(tree_id, None, |paths| {
                Ok(materialize(&paths.tmp, &paths.tmp_state)?)
            })
        })
    }

    /// Like [`BaseCache::ensure`], but an absent entry is built from
    /// `nearest` (another entry, normally the most recently used one):
    /// `cloner` clones its tree to the temporary tree directory and its
    /// state to the temporary state directory, and `check_out_diff`
    /// receives those two and checks out the tree over them, which writes
    /// only the files that differ. It is normally
    /// `Repo::materialize_tree_from` on the commit.
    ///
    /// Any failure leaves nothing behind and is returned; the caller then
    /// falls back to [`BaseCache::ensure`], a full check-out. An existing
    /// entry is returned as `ensure` returns it, without touching
    /// `nearest`.
    pub fn ensure_from(
        &self,
        tree_id: &str,
        nearest: &BaseEntry,
        cloner: &dyn DirCloner,
        check_out_diff: impl FnOnce(&Path, &Path) -> Result<MaterializeStats, MaterializeError>,
    ) -> Result<BaseEntry, BaseCacheError> {
        check_key(tree_id)?;
        if nearest.tree_id == tree_id {
            // The build would first remove the very directories it clones.
            return Err(BaseCacheError::SameBase(tree_id.to_owned()));
        }
        self.with_key_lock(tree_id, || {
            self.ensure_locked(tree_id, Some(&nearest.tree_id), |paths| {
                for (from, to) in [
                    (&nearest.path, &paths.tmp),
                    (&nearest.state_dir, &paths.tmp_state),
                ] {
                    cloner
                        .clone_dir(from, to)
                        .map_err(|source| BaseCacheError::Io {
                            op: "clone",
                            path: from.clone(),
                            source,
                        })?;
                }
                let stats = check_out_diff(&paths.tmp, &paths.tmp_state)?;
                // Load-bearing: jj loads a missing or unreadable state as
                // the empty tree, and then skips every file in the clone.
                if stats.checkout.skipped_files > 0 {
                    return Err(BaseCacheError::SkippedFiles {
                        tree_id: tree_id.to_owned(),
                        count: stats.checkout.skipped_files,
                    });
                }
                Ok(stats)
            })
        })
    }

    /// [`BaseCache::ensure`]'s body, under the tree's lock: `build` writes
    /// the entry's two temporary directories. `from` names the entry it was
    /// cloned from, for the log.
    fn ensure_locked(
        &self,
        tree_id: &str,
        from: Option<&str>,
        build: impl FnOnce(&EntryPaths) -> Result<MaterializeStats, BaseCacheError>,
    ) -> Result<BaseEntry, BaseCacheError> {
        if let Some(record) = self.db.base(self.repo_id, tree_id)? {
            let entry = BaseEntry::from_record(record);
            if entry.is_complete() {
                self.db
                    .touch_base(self.repo_id, tree_id, Timestamp::now())?;
                return Ok(entry);
            }
            if entry.path.exists() {
                return Err(BaseCacheError::Incomplete(entry.path));
            }
            tracing::warn!(
                path = %entry.path.display(),
                "base cache entry is missing on disk; materializing it again"
            );
            self.db.delete_base(self.repo_id, tree_id)?;
        }

        let paths = EntryPaths::new(&self.dir, tree_id);
        // Leftovers of an interrupted materialization, or of an entry whose
        // row was dropped above, are not part of any entry.
        for path in [&paths.tmp, &paths.tmp_state, &paths.tree, &paths.state] {
            remove_path(path)?;
        }
        std::fs::create_dir_all(&self.dir).map_err(|source| BaseCacheError::Io {
            op: "create directory",
            path: self.dir.clone(),
            source,
        })?;

        let begun = Instant::now();
        let stats = match build(&paths) {
            Ok(stats) => stats,
            Err(error) => {
                paths.discard_tmp();
                return Err(error);
            }
        };
        if !paths.tmp.is_dir() || !paths.tmp_state.join(TREE_STATE_FILE).is_file() {
            paths.discard_tmp();
            return Err(BaseCacheError::Incomplete(paths.tmp_state));
        }
        // State first: a tree directory without its state is incomplete,
        // which the sweep detects; the reverse order would not be.
        rename(&paths.tmp_state, &paths.state)?;
        rename(&paths.tmp, &paths.tree)?;
        sync_dir(&self.dir)?;

        let now = Timestamp::now();
        let record = BaseCacheRecord {
            repo_id: self.repo_id,
            tree_id: tree_id.to_owned(),
            path: paths.tree,
            bytes: stats.bytes,
            files: stats.files,
            created_at: now,
            last_used_at: now,
        };
        self.db.insert_base(&record)?;
        if let Some(from_tree_id) = from {
            let checkout = stats.checkout;
            tracing::info!(
                tree_id,
                from_tree_id,
                path = %record.path.display(),
                file_count = stats.files,
                changed_count = checkout.updated_files + checkout.added_files + checkout.removed_files,
                elapsed_ms = elapsed_ms(begun),
                "base materialized incrementally"
            );
        } else {
            tracing::info!(
                tree_id,
                path = %record.path.display(),
                file_count = stats.files,
                total_bytes = stats.bytes,
                elapsed_ms = elapsed_ms(begun),
                "base materialized"
            );
        }
        Ok(BaseEntry::from_record(record))
    }

    /// The entry for `tree_id`, if one is recorded. Does not check the disk
    /// or bump `last_used_at`.
    pub fn get(&self, tree_id: &str) -> Result<Option<BaseEntry>, BaseCacheError> {
        Ok(self
            .db
            .base(self.repo_id, tree_id)?
            .map(BaseEntry::from_record))
    }

    /// Every recorded entry, least recently used first.
    pub fn entries(&self) -> Result<Vec<BaseEntry>, BaseCacheError> {
        Ok(self
            .db
            .bases(self.repo_id)?
            .into_iter()
            .map(BaseEntry::from_record)
            .collect())
    }

    /// Removes the entry for `tree_id` and returns the bytes its two
    /// directories had allocated. The row goes first, so a failure part way
    /// leaves only row-less directories for the next sweep.
    /// [`DbError::NotFound`] if there is no entry. Whether a workspace still
    /// uses the base is the caller's question (R1.17).
    pub fn remove(&self, tree_id: &str) -> Result<u64, BaseCacheError> {
        check_key(tree_id)?;
        self.with_key_lock(tree_id, || {
            let Some(record) = self.db.base(self.repo_id, tree_id)? else {
                return Err(DbError::NotFound {
                    what: "base cache entry",
                    id: format!("{}/{tree_id}", self.repo_id),
                }
                .into());
            };
            let entry = BaseEntry::from_record(record);
            let bytes = allocated_bytes(&entry.path)? + allocated_bytes(&entry.state_dir)?;
            self.db.delete_base(self.repo_id, tree_id)?;
            remove_path(&entry.path)?;
            remove_path(&entry.state_dir)?;
            tracing::info!(tree_id, freed_bytes = bytes, "base removed");
            Ok(bytes)
        })
    }

    /// GC's eviction (R1.17): removes entries whose tree is not in `keep`
    /// (the trees of `active` and `releasing` workspaces), least recently
    /// used first, until at most `max_entries` remain; an entry not in
    /// `keep` and last used before `older_than` goes even below the cap. An
    /// entry in `keep` is never removed, so the count can stay above the
    /// cap. With `dry_run`, removes nothing and returns what would go.
    ///
    /// Each removal deletes the row, renames both directories to `*.gc`
    /// names, and removes them; a crash part way leaves row-less `*.gc`
    /// directories, which the start-up sweep removes. The caller keeps
    /// creates of this repository out meanwhile, so `keep` stays true.
    pub fn evict(
        &self,
        keep: &HashSet<String>,
        max_entries: usize,
        older_than: SystemTime,
        dry_run: bool,
    ) -> Result<Vec<Evicted>, BaseCacheError> {
        let older_than = Timestamp::from(older_than);
        let entries = self.db.bases(self.repo_id)?;
        let mut remaining = entries.len();
        let mut evicted = Vec::new();
        for record in entries {
            if keep.contains(&record.tree_id) {
                continue;
            }
            if remaining <= max_entries && record.last_used_at >= older_than {
                continue;
            }
            let entry = BaseEntry::from_record(record);
            let bytes = if dry_run {
                allocated_bytes(&entry.path)? + allocated_bytes(&entry.state_dir)?
            } else if let Some(bytes) = self.evict_one(&entry)? {
                bytes
            } else {
                // Removed meanwhile; not this eviction's.
                remaining -= 1;
                continue;
            };
            remaining -= 1;
            evicted.push(Evicted {
                tree_id: entry.tree_id,
                bytes,
            });
        }
        Ok(evicted)
    }

    /// Removes one entry for [`BaseCache::evict`]; returns its bytes, or
    /// `None` if its row was gone already.
    fn evict_one(&self, entry: &BaseEntry) -> Result<Option<u64>, BaseCacheError> {
        self.with_key_lock(&entry.tree_id, || {
            let bytes = allocated_bytes(&entry.path)? + allocated_bytes(&entry.state_dir)?;
            if self.db.delete_base(self.repo_id, &entry.tree_id)?.is_none() {
                // Removed meanwhile; nothing of it is ours to delete.
                return Ok(None);
            }
            for dir in [&entry.path, &entry.state_dir] {
                let doomed = with_suffix(dir, GC_SUFFIX);
                remove_path(&doomed)?;
                match std::fs::rename(dir, &doomed) {
                    Ok(()) => remove_path(&doomed)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(BaseCacheError::Io {
                            op: "rename",
                            path: dir.clone(),
                            source,
                        });
                    }
                }
            }
            tracing::info!(tree_id = %entry.tree_id, freed_bytes = bytes, "base evicted");
            Ok(Some(bytes))
        })
    }

    /// Removes everything under `bases/` that is not a complete, recorded
    /// entry: `*.tmp`, `*.tmp.state`, and GC's `*.gc` leftovers, directories without a
    /// row, and the rows (with whatever is left of their directories) of
    /// entries missing on disk or whose `tree_state` does not decode to the
    /// recorded file count. Returns the paths removed.
    ///
    /// Called at daemon start, before overlay mounts are re-established
    /// (R1.03, R1.09): a dropped entry's tree must not be a live lower
    /// layer. It waits for `ensure` and `remove` calls in progress.
    pub fn sweep_incomplete(&self) -> Result<Vec<PathBuf>, BaseCacheError> {
        let _sweep = self.sweep.write().unwrap_or_else(PoisonError::into_inner);
        let mut complete = HashSet::new();
        for record in self.db.bases(self.repo_id)? {
            let entry = BaseEntry::from_record(record);
            if entry.is_valid() {
                complete.insert(entry.tree_id);
            } else {
                tracing::warn!(
                    path = %entry.path.display(),
                    "base cache entry is missing or damaged on disk; dropping it"
                );
                self.db.delete_base(self.repo_id, &entry.tree_id)?;
            }
        }

        let read_dir = match std::fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(BaseCacheError::Io {
                    op: "read directory",
                    path: self.dir.clone(),
                    source,
                });
            }
        };
        let mut removed = Vec::new();
        for dir_entry in read_dir {
            let dir_entry = dir_entry.map_err(|source| BaseCacheError::Io {
                op: "read directory",
                path: self.dir.clone(),
                source,
            })?;
            let name = dir_entry.file_name();
            let keep = name == PROBE_FILE
                || name.to_str().is_some_and(|name| {
                    let key = name.strip_suffix(STATE_SUFFIX).unwrap_or(name);
                    complete.contains(key)
                });
            if !keep {
                let path = dir_entry.path();
                remove_path(&path)?;
                removed.push(path);
            }
        }
        removed.sort();
        if !removed.is_empty() {
            tracing::info!(
                path = %self.dir.display(),
                removed_count = removed.len(),
                "incomplete base cache entries removed"
            );
        }
        Ok(removed)
    }

    /// Runs `f` holding the lock for `tree_id`, then forgets the lock if no
    /// one else is waiting on it.
    fn with_key_lock<T>(&self, tree_id: &str, f: impl FnOnce() -> T) -> T {
        let _sweep = self.sweep.read().unwrap_or_else(PoisonError::into_inner);
        let lock = {
            let mut locks = self
                .key_locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            locks.entry(tree_id.to_owned()).or_default().clone()
        };
        let value = {
            // A panic while materializing leaves nothing a later call cannot
            // clean up, so a poisoned key lock is taken over.
            let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            f()
        };
        let mut locks = self
            .key_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Two references: the map's and ours. Another holder would have
        // cloned it under this same outer lock.
        if Arc::strong_count(&lock) == 2 {
            locks.remove(tree_id);
        }
        value
    }
}

/// The four paths of one entry.
struct EntryPaths {
    tree: PathBuf,
    state: PathBuf,
    tmp: PathBuf,
    tmp_state: PathBuf,
}

impl EntryPaths {
    fn new(dir: &Path, tree_id: &str) -> Self {
        Self {
            tree: dir.join(tree_id),
            state: dir.join(format!("{tree_id}{STATE_SUFFIX}")),
            tmp: dir.join(format!("{tree_id}{TMP_SUFFIX}")),
            tmp_state: dir.join(format!("{tree_id}{TMP_STATE_SUFFIX}")),
        }
    }

    /// Best-effort removal of the temporary directories after a failure;
    /// whatever stays is swept at the next daemon start.
    fn discard_tmp(&self) {
        for path in [&self.tmp, &self.tmp_state] {
            if let Err(error) = remove_path(path) {
                tracing::warn!(error = %ErrorChain(&error), "cannot remove a failed materialization");
            }
        }
    }
}

fn check_key(tree_id: &str) -> Result<(), BaseCacheError> {
    if is_tree_key(tree_id) {
        Ok(())
    } else {
        Err(BaseCacheError::InvalidTreeId(tree_id.to_owned()))
    }
}

/// `path` with `suffix` appended to its last component.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = OsString::from(path.as_os_str());
    text.push(suffix);
    PathBuf::from(text)
}

/// Removes a file, symlink, or directory tree without following symlinks.
/// A missing path is not an error.
fn remove_path(path: &Path) -> Result<(), BaseCacheError> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BaseCacheError::Io {
            op: "remove",
            path: path.to_owned(),
            source,
        }),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), BaseCacheError> {
    std::fs::rename(from, to).map_err(|source| BaseCacheError::Io {
        op: "rename",
        path: from.to_owned(),
        source,
    })
}

/// Makes the renames in `dir` durable before the row that relies on them
/// is written.
fn sync_dir(dir: &Path) -> Result<(), BaseCacheError> {
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|source| BaseCacheError::Io {
            op: "sync",
            path: dir.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use reef_lib::jj::materialize::CheckoutStatsSummary;

    use super::*;

    const REPO: &str = "aaaa000000000001";

    /// A stand-in for jj: writes one file and an empty `tree_state`, which
    /// decodes as a state with no entries, so it reports no files (only the
    /// bookkeeping is under test here; `tests/base_cache.rs` uses jj).
    #[allow(
        clippy::unnecessary_wraps,
        reason = "has the signature of the closure ensure takes"
    )]
    fn fake_materialize(dir: &Path, state: &Path) -> Result<MaterializeStats, MaterializeError> {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::create_dir_all(state).unwrap();
        std::fs::write(dir.join("file"), "contents").unwrap();
        std::fs::write(state.join(TREE_STATE_FILE), "").unwrap();
        Ok(MaterializeStats {
            files: 0,
            bytes: 8,
            elapsed: Duration::ZERO,
            checkout: CheckoutStatsSummary::default(),
        })
    }

    fn cache(dir: &Path) -> BaseCache {
        let db = Arc::new(Db::open_in_memory().unwrap());
        BaseCache::new(db, &dir.join("repos").join(REPO), REPO.parse().unwrap())
    }

    #[test]
    fn ensure_records_and_reuses() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let entry = cache.ensure("0a0b", fake_materialize).unwrap();
        assert_eq!(entry.path, cache.dir().join("0a0b"));
        assert_eq!(entry.state_dir, cache.dir().join("0a0b.state"));
        assert_eq!((entry.files, entry.bytes), (0, 8));
        assert!(entry.tree_state_file().is_file());
        assert!(!cache.dir().join("0a0b.tmp").exists());
        assert!(!cache.dir().join("0a0b.tmp.state").exists());

        let again = cache
            .ensure("0a0b", |_, _| panic!("materialized twice"))
            .unwrap();
        assert_eq!(again, entry);
        assert_eq!(cache.get("0a0b").unwrap(), Some(entry.clone()));
        assert_eq!(cache.entries().unwrap(), [entry]);
        assert!(
            cache.key_locks.lock().unwrap().is_empty(),
            "locks are forgotten"
        );
    }

    /// Clones with `std::fs` copies, which is all the bookkeeping needs.
    struct CopyCloner;

    impl DirCloner for CopyCloner {
        fn clone_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
            std::fs::create_dir(to)?;
            for entry in std::fs::read_dir(from)? {
                let entry = entry?;
                std::fs::copy(entry.path(), to.join(entry.file_name()))?;
            }
            Ok(())
        }
    }

    #[test]
    fn ensure_from_refuses_skipped_files_and_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let first = cache.ensure("0a0a", fake_materialize).unwrap();

        let error = cache
            .ensure_from("0b0b", &first, &CopyCloner, |dir, state| {
                assert!(dir.join("file").is_file(), "the clone is in place");
                assert!(state.join(TREE_STATE_FILE).is_file());
                let mut stats = fake_materialize(&tmp.path().join("x"), &tmp.path().join("y"))?;
                stats.checkout.skipped_files = 2;
                Ok(stats)
            })
            .unwrap_err();
        assert!(
            matches!(&error, BaseCacheError::SkippedFiles { count: 2, .. }),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Internal);
        assert_eq!(cache.get("0b0b").unwrap(), None);
        let mut left: Vec<_> = std::fs::read_dir(cache.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["0a0a", "0a0a.state"], "nothing of 0b0b is left");

        let error = cache
            .ensure_from("0a0a", &first, &CopyCloner, |_, _| {
                panic!("built from itself")
            })
            .unwrap_err();
        assert!(matches!(error, BaseCacheError::SameBase(_)), "{error:?}");
        assert!(first.is_complete(), "the base is untouched");

        let second = cache
            .ensure_from("0b0b", &first, &CopyCloner, |_, _| {
                Ok(MaterializeStats {
                    files: 0,
                    bytes: 8,
                    elapsed: Duration::ZERO,
                    checkout: CheckoutStatsSummary::default(),
                })
            })
            .unwrap();
        assert!(second.is_complete());
        assert!(second.path.join("file").is_file(), "cloned from the first");
    }

    #[test]
    fn conflicted_keys_are_valid_paths_and_bad_keys_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let entry = cache.ensure("0a+0b+0c", fake_materialize).unwrap();
        assert!(entry.path.ends_with("0a+0b+0c"));
        for bad in ["", "../escape", "0A", "a/b", "0a.tmp"] {
            let error = cache.ensure(bad, fake_materialize).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{bad:?}");
        }
    }

    #[test]
    fn failed_materialization_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let error = cache
            .ensure("0a0b", |dir, state| {
                fake_materialize(dir, state).unwrap();
                Err(MaterializeError::NotEmpty(dir.to_owned()))
            })
            .unwrap_err();
        assert!(matches!(error, BaseCacheError::Materialize(_)), "{error:?}");
        assert_eq!(std::fs::read_dir(cache.dir()).unwrap().count(), 0);
        assert_eq!(cache.get("0a0b").unwrap(), None);

        let error = cache
            .ensure("0a0b", |dir, _| {
                std::fs::create_dir_all(dir).unwrap();
                fake_materialize(&tmp.path().join("elsewhere"), &tmp.path().join("s"))
            })
            .unwrap_err();
        assert!(matches!(error, BaseCacheError::Incomplete(_)), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Internal);
        assert_eq!(std::fs::read_dir(cache.dir()).unwrap().count(), 0);
    }

    #[test]
    fn entry_missing_on_disk_is_materialized_again() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let entry = cache.ensure("0a0b", fake_materialize).unwrap();
        std::fs::remove_dir_all(&entry.state_dir).unwrap();
        let error = cache
            .ensure("0a0b", |_, _| panic!("a tree on disk is not replaced"))
            .unwrap_err();
        assert!(matches!(error, BaseCacheError::Incomplete(_)), "{error:?}");
        assert!(entry.path.is_dir(), "the tree is left for the sweep");

        std::fs::remove_dir_all(&entry.path).unwrap();
        let calls = AtomicUsize::new(0);
        cache
            .ensure("0a0b", |dir, state| {
                calls.fetch_add(1, Ordering::SeqCst);
                fake_materialize(dir, state)
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(entry.is_complete());
    }

    #[test]
    fn concurrent_ensures_materialize_once() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let calls = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    cache
                        .ensure("0a0b", |dir, state| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            fake_materialize(dir, state)
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.key_locks.lock().unwrap().is_empty());
    }

    #[test]
    fn remove_deletes_row_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let entry = cache.ensure("0a0b", fake_materialize).unwrap();
        let freed = cache.remove("0a0b").unwrap();
        assert!(freed > 0);
        assert!(!entry.path.exists() && !entry.state_dir.exists());
        assert_eq!(cache.get("0a0b").unwrap(), None);
        let error = cache.remove("0a0b").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn evict_keeps_used_entries_and_goes_down_to_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        for (tree, used) in [("0a0a", 10), ("0b0b", 20), ("0c0c", 30), ("0d0d", 40)] {
            cache.ensure(tree, fake_materialize).unwrap();
            cache
                .db
                .touch_base(cache.repo_id, tree, Timestamp(used))
                .unwrap();
        }
        let keep: HashSet<String> = ["0a0a".to_owned()].into();
        let long_ago = std::time::UNIX_EPOCH;

        let would = cache.evict(&keep, 2, long_ago, true).unwrap();
        let names = |evicted: &[Evicted]| {
            evicted
                .iter()
                .map(|entry| entry.tree_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&would), ["0b0b", "0c0c"], "least recently used first");
        assert!(would.iter().all(|entry| entry.bytes > 0));
        assert_eq!(
            cache.entries().unwrap().len(),
            4,
            "a dry run removes nothing"
        );

        let evicted = cache.evict(&keep, 2, long_ago, false).unwrap();
        assert_eq!(evicted, would);
        let left: Vec<_> = cache
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.tree_id)
            .collect();
        assert_eq!(left, ["0a0a", "0d0d"]);
        let mut on_disk: Vec<_> = std::fs::read_dir(cache.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        on_disk.sort();
        assert_eq!(on_disk, ["0a0a", "0a0a.state", "0d0d", "0d0d.state"]);

        // Under the cap, only what is unused and old enough goes; a kept
        // entry never does, even over the cap.
        let now = SystemTime::now();
        assert_eq!(
            names(&cache.evict(&keep, 2, long_ago, false).unwrap()),
            [] as [&str; 0]
        );
        assert_eq!(
            names(&cache.evict(&keep, 0, long_ago, false).unwrap()),
            ["0d0d"]
        );
        assert_eq!(
            names(&cache.evict(&keep, 0, now, false).unwrap()),
            [] as [&str; 0]
        );
        assert_eq!(cache.entries().unwrap().len(), 1);
    }

    #[test]
    fn gc_leftovers_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        let kept = cache.ensure("0a0a", fake_materialize).unwrap();
        for leftover in ["0b0b.gc", "0b0b.state.gc"] {
            std::fs::create_dir(cache.dir().join(leftover)).unwrap();
        }
        let removed = cache.sweep_incomplete().unwrap();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(kept.is_complete());
    }

    #[test]
    fn sweep_keeps_complete_entries_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = cache(tmp.path());
        assert!(
            cache.sweep_incomplete().unwrap().is_empty(),
            "no bases/ yet"
        );

        let kept = cache.ensure("0a0a", fake_materialize).unwrap();
        let gone = cache.ensure("0b0b", fake_materialize).unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        let damaged = cache.ensure("0f0f", fake_materialize).unwrap();
        std::fs::write(damaged.tree_state_file(), b"\xff\xff\xff").unwrap();
        let dir = cache.dir().to_owned();
        for leftover in ["0c0c.tmp", "0c0c.tmp.state", "0d0d", "0d0d.state", "0e0e"] {
            std::fs::create_dir(dir.join(leftover)).unwrap();
        }
        std::fs::write(dir.join("stray-file"), "").unwrap();
        std::fs::write(dir.join(PROBE_FILE), "\n").unwrap();

        let removed = cache.sweep_incomplete().unwrap();
        let names: Vec<_> = removed
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "0b0b.state",
                "0c0c.tmp",
                "0c0c.tmp.state",
                "0d0d",
                "0d0d.state",
                "0e0e",
                "0f0f",
                "0f0f.state",
                "stray-file"
            ]
        );
        assert!(kept.is_complete());
        assert!(
            dir.join(PROBE_FILE).is_file(),
            "the APFS probe's file stays"
        );
        assert_eq!(cache.entries().unwrap(), [kept]);
    }
}
