//! The symbol index (R2.14, ADR 0042): definitions and references per
//! file, from tree-sitter tags the daemon extracts (`reef-symbols`) and SCIP
//! documents callers upload, in one SQLite cache per repository
//! (`store.rs`).
//!
//! - **Passes** (`index.rs`): indexing a commit's tree extracts every
//!   supported file without an entry and records the tree's counts. A pass
//!   diffs from the first parent's tree when that one is complete, and walks
//!   the tree otherwise. Concurrent requests for one tree share one pass,
//!   which outlives them. Passes run on the blocking pool; extraction runs
//!   on this index's own low-priority threads, so neither the repository
//!   thread nor the read pool waits on it.
//! - **Background**: snapshots that change a tree and `EditFiles` commits
//!   are [`SymbolIndex::notice`]d. A queue keeps one pending commit per
//!   workspace (a later snapshot replaces an earlier one) and up to 256
//!   edit commits, and indexes them one at a time.
//! - **Reads** (`query.rs`) and **uploads** (`upload.rs`) look up each file's
//!   entry and extract a missing one on the spot, so a tree whose entries
//!   GC removed heals itself.
//! - **GC** (`gc.rs`): a fifth step per repository.

pub(crate) mod diff;
pub(crate) mod gc;
pub(crate) mod index;
pub(crate) mod query;
pub mod store;
pub(crate) mod upload;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::CommitIdHex;
use reef_lib::jj::files::FileError;
use reef_symbols::{Language, Limits};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{Paths, SymbolsSection};
use crate::daemon::AppState;
use crate::error::{Advice, Advise};
use crate::model::{RepoId, WorkspaceId};
use crate::read::ReadError;
use store::{Counts, Store, StoreError};

/// The cache's file name in a repository's directory.
pub const SYMBOLS_FILE: &str = "symbols.sqlite3";

/// Edit commits the background queue holds; the oldest is dropped past it.
const MAX_PENDING_EDITS: usize = 256;

/// Passes that run at once; more wait.
const MAX_PASSES: usize = 2;

/// The longest an `IndexSymbols` call waits.
pub const MAX_WAIT: Duration = Duration::from_secs(600);

/// How long a symbol diff waits for the new tree's pass before its call
/// sites are `INDEX_INCOMPLETE`.
pub const CALL_SITE_WAIT: Duration = Duration::from_secs(10);

/// How far a tree's indexing has come. The values are the API's
/// `SymbolIndexState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No pass has run on the tree.
    NotIndexed = 1,
    /// A pass is running.
    Indexing = 2,
    /// A pass found or wrote an entry for every supported file.
    Complete = 3,
}

/// A tree's indexing, as `SymbolIndexStatus` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    /// The commit asked about.
    pub commit_id: String,
    /// How far it has come.
    pub state: State,
    /// Its counts: the last pass's, or the running pass's so far.
    pub(crate) counts: Counts,
    /// When the last pass ended, in milliseconds since the epoch.
    pub indexed_at: Option<i64>,
}

/// Failure of the symbol index.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SymbolError {
    /// `[symbols] enabled = false`.
    #[error("the symbol index is disabled")]
    Disabled,
    /// A query that needs the tree indexed, on one that is not.
    #[error("commit {commit_id} is not indexed: {missing} supported files have no entry")]
    NotIndexed {
        /// The commit.
        commit_id: String,
        /// Supported files without an entry, as far as counted.
        missing: u64,
    },
    /// A malformed `ListSymbols` request.
    #[error("invalid symbol query: {message}")]
    InvalidQuery {
        /// The field.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// A malformed upload.
    #[error("invalid upload: {message}")]
    InvalidUpload {
        /// The field.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// The repository is not open.
    #[error("repository {0} is not open")]
    NotOpen(RepoId),
    /// The cache failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A tree or file could not be read.
    #[error(transparent)]
    File(#[from] FileError),
    /// The repository could not be loaded.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// A pass failed; its error, as text, is shared by everyone waiting.
    #[error("indexing failed: {0}")]
    Pass(String),
    /// The daemon is shutting down.
    #[error("the daemon is shutting down")]
    ShuttingDown,
}

impl Classify for SymbolError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Disabled | Self::NotIndexed { .. } => ErrorKind::Conflict,
            Self::InvalidQuery { .. } | Self::InvalidUpload { .. } => ErrorKind::InvalidArgument,
            Self::NotOpen(_) | Self::ShuttingDown => ErrorKind::Unavailable,
            Self::File(error) => error.kind(),
            Self::Read(error) => error.kind(),
            Self::Store(_) | Self::Pass(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for SymbolError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Disabled => Some(Advice::hint(
                "Set `[symbols] enabled = true` in config.toml and restart the daemon.",
            )),
            Self::NotIndexed { commit_id, .. } => Some(Advice::run(
                "Index the revision first; a large repository's first index takes a while.",
                ["reef", "symbols", "index", "-r", commit_id.as_str()],
            )),
            // Transparent: their sources are the inner error's sources.
            Self::Read(error) => error.advice(),
            _ => None,
        }
    }
}

/// A commit to index in the background.
#[derive(Debug)]
struct Notice {
    repo: RepoId,
    workspace: Option<WorkspaceId>,
    commit: CommitIdHex,
}

/// The background queue: the latest commit per workspace, in the order
/// workspaces first asked, then edit commits, oldest first.
#[derive(Debug, Default)]
struct Queue {
    workspaces: VecDeque<WorkspaceId>,
    latest: HashMap<WorkspaceId, (RepoId, CommitIdHex)>,
    edits: VecDeque<(RepoId, CommitIdHex)>,
}

impl Queue {
    fn take(&mut self, notice: Notice) {
        if let Some(ws) = notice.workspace {
            if self
                .latest
                .insert(ws, (notice.repo, notice.commit))
                .is_none()
            {
                self.workspaces.push_back(ws);
            }
            return;
        }
        if self.edits.len() == MAX_PENDING_EDITS {
            self.edits.pop_front();
        }
        self.edits.push_back((notice.repo, notice.commit));
    }

    fn next(&mut self) -> Option<(RepoId, CommitIdHex)> {
        while let Some(ws) = self.workspaces.pop_front() {
            if let Some(next) = self.latest.remove(&ws) {
                return Some(next);
            }
        }
        self.edits.pop_front()
    }
}

/// A running pass: its progress, and its outcome once it ends.
#[derive(Debug, Clone)]
pub(crate) struct Pass {
    progress: Arc<Mutex<Counts>>,
    done: watch::Receiver<Option<Result<IndexStatus, String>>>,
}

/// The symbol index of every repository.
pub struct SymbolIndex {
    config: SymbolsSection,
    paths: Paths,
    limits: Limits,
    stores: Mutex<HashMap<RepoId, Arc<Store>>>,
    pool: Option<Arc<rayon::ThreadPool>>,
    passes: Mutex<HashMap<(RepoId, String), Pass>>,
    notices: Mutex<Option<mpsc::UnboundedSender<Notice>>>,
    /// Passes running at once at most.
    pass_permits: Arc<tokio::sync::Semaphore>,
    /// Cancelled at shutdown; passes stop between batches.
    stopping: CancellationToken,
    /// Files extracted since start, for tests.
    extracted: AtomicU64,
    /// Passes run since start, for tests.
    passes_run: AtomicU64,
    /// Call-site lookups since start, for tests.
    lookups: AtomicU64,
    /// [`CALL_SITE_WAIT`], or a test's.
    call_site_wait: Mutex<Duration>,
}

impl std::fmt::Debug for SymbolIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolIndex")
            .field("enabled", &self.config.enabled)
            .field("extracted", &self.extracted.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Lowers the calling thread's priority, so a first index never competes
/// with agents' snapshots.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscalls on the calling thread, with no pointers.
        #[allow(unsafe_code, reason = "setpriority(2) on the calling thread")]
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid);
            let _ = libc::setpriority(
                libc::PRIO_PROCESS,
                libc::id_t::try_from(tid).unwrap_or(0),
                10,
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: sets the calling thread's own QoS class; no pointers.
        #[allow(
            unsafe_code,
            reason = "pthread_set_qos_class_self_np on the calling thread"
        )]
        unsafe {
            let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
        }
    }
}

impl SymbolIndex {
    /// The index for `config`, with its extraction threads started unless
    /// it is disabled.
    pub fn new(config: &SymbolsSection, paths: &Paths) -> Arc<SymbolIndex> {
        let workers =
            config.worker_count(std::thread::available_parallelism().map_or(4, usize::from));
        let pool = config.enabled.then(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .thread_name(|i| format!("reef-symbols-{i}"))
                .start_handler(|_| lower_priority())
                .build()
                .map(Arc::new)
                .ok()
        });
        Arc::new(SymbolIndex {
            config: config.clone(),
            paths: paths.clone(),
            limits: Limits {
                max_file_size: config.max_file_size.0,
                ..Limits::default()
            },
            stores: Mutex::new(HashMap::new()),
            pool: pool.flatten(),
            passes: Mutex::new(HashMap::new()),
            notices: Mutex::new(None),
            pass_permits: Arc::new(tokio::sync::Semaphore::new(MAX_PASSES)),
            stopping: CancellationToken::new(),
            extracted: AtomicU64::new(0),
            passes_run: AtomicU64::new(0),
            lookups: AtomicU64::new(0),
            call_site_wait: Mutex::new(CALL_SITE_WAIT),
        })
    }

    /// Whether the index runs.
    pub fn enabled(&self) -> bool {
        self.config.enabled && self.pool.is_some()
    }

    fn check_enabled(&self) -> Result<(), SymbolError> {
        if self.enabled() {
            Ok(())
        } else {
            Err(SymbolError::Disabled)
        }
    }

    /// Files extracted since the daemon started.
    pub fn extracted(&self) -> u64 {
        self.extracted.load(Ordering::Relaxed)
    }

    /// Passes run since the daemon started.
    pub fn passes_run(&self) -> u64 {
        self.passes_run.load(Ordering::Relaxed)
    }

    /// Call-site lookups (pages of a symbol diff that read references)
    /// since the daemon started.
    pub fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    pub(crate) fn count_lookup(&self) {
        self.lookups.fetch_add(1, Ordering::Relaxed);
    }

    /// How long a symbol diff waits for the new tree's pass.
    pub(crate) fn call_site_wait(&self) -> Duration {
        *self
            .call_site_wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets how long a symbol diff waits for the new tree's pass, so a test
    /// can see `INDEX_INCOMPLETE`.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_call_site_wait(&self, wait: Duration) {
        *self
            .call_site_wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = wait;
    }

    /// The bounds of one file's extraction.
    pub(crate) fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The cache file of `repo`.
    pub fn path_of(&self, repo: RepoId) -> PathBuf {
        self.paths.repo_dir(repo).join(SYMBOLS_FILE)
    }

    /// `repo`'s cache, opened on first use.
    pub(crate) fn store(&self, repo: RepoId) -> Result<Arc<Store>, SymbolError> {
        self.check_enabled()?;
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(store) = stores.get(&repo) {
            return Ok(Arc::clone(store));
        }
        let path = self.path_of(repo);
        // Callers resolve a registered repository first (`ensure` checks
        // the registry before this), so the directory is its own.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Recreate {
                path: path.clone(),
                source,
            })?;
        }
        let readers = self
            .pool
            .as_ref()
            .map_or(1, |pool| pool.current_num_threads())
            + 2;
        let extractors = Language::ALL.map(reef_symbols::extractor).join(",");
        let store = Arc::new(Store::open(&path, readers, &extractors)?);
        stores.insert(repo, Arc::clone(&store));
        Ok(store)
    }

    /// Closes `repo`'s cache, before its directory is removed. Noticed
    /// commits of it still queued fail `NotOpen` when they run, and create
    /// nothing.
    pub fn forget(&self, repo: RepoId) {
        self.stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&repo);
        self.passes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _), _| *id != repo);
    }

    /// Runs `work` on the extraction threads.
    pub(crate) fn install<T: Send>(&self, work: impl FnOnce() -> T + Send) -> T {
        match &self.pool {
            Some(pool) => pool.install(work),
            None => work(),
        }
    }

    /// Whether the daemon is shutting down: passes stop between batches.
    pub(crate) fn stopping(&self) -> bool {
        self.stopping.is_cancelled()
    }

    pub(crate) fn count_extracted(&self, n: u64) {
        self.extracted.fetch_add(n, Ordering::Relaxed);
    }

    /// Queues `commit` of `workspace` (or of an edit, with none) for
    /// indexing in the background. Never blocks.
    pub fn notice(&self, repo: RepoId, workspace: Option<WorkspaceId>, commit: CommitIdHex) {
        if !self.enabled() || !self.config.background {
            return;
        }
        if let Some(tx) = &*self.notices.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = tx.send(Notice {
                repo,
                workspace,
                commit,
            });
        }
    }

    /// Starts the background queue, which indexes noticed commits one at a
    /// time until `shutdown`.
    pub fn start(self: &Arc<Self>, state: Weak<AppState>, shutdown: CancellationToken) {
        if !self.enabled() || !self.config.background {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Notice>();
        *self.notices.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        let index = Arc::clone(self);
        tokio::spawn(async move {
            let mut queue = Queue::default();
            loop {
                while let Ok(notice) = rx.try_recv() {
                    queue.take(notice);
                }
                let Some((repo, commit)) = queue.next() else {
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        notice = rx.recv() => match notice {
                            Some(notice) => {
                                queue.take(notice);
                                continue;
                            }
                            None => return,
                        },
                    }
                };
                let Some(state) = state.upgrade() else {
                    return;
                };
                let result = tokio::select! {
                    () = shutdown.cancelled() => return,
                    result = index.ensure(&state, repo, commit.clone(), MAX_WAIT) => result,
                };
                if let Err(error) = result {
                    tracing::warn!(repo = %repo, commit = %commit, error = %error, "background symbol indexing failed");
                }
            }
        });
    }

    /// Indexes `commit`'s tree in `repo`, joining a running pass, and waits
    /// up to `wait` for it. Returns the tree's status: `COMPLETE` when the
    /// pass ended in time, `INDEXING` with its progress otherwise.
    pub async fn ensure(
        self: &Arc<Self>,
        state: &AppState,
        repo: RepoId,
        commit: CommitIdHex,
        wait: Duration,
    ) -> Result<IndexStatus, SymbolError> {
        let handle = state.repos.get(repo).ok_or(SymbolError::NotOpen(repo))?;
        let store = self.store(repo)?;
        let loaded = state.reads.loaded(&handle).await?;
        let read = Arc::clone(&loaded);
        let at = commit.clone();
        let key = tokio::task::spawn_blocking(move || {
            pollster::block_on(reef_lib::jj::files::commit_tree(&read, &at))
                .map(|tree| reef_lib::jj::files::tree_key(&tree))
        })
        .await
        .map_err(|error| SymbolError::Pass(error.to_string()))??;
        if let Some(row) = store.tree(&key)?
            && row.complete
        {
            return Ok(IndexStatus {
                commit_id: commit.to_string(),
                state: State::Complete,
                counts: row.counts,
                indexed_at: row.indexed_at,
            });
        }
        let pass = self.pass(repo, &key, || {
            let index = Arc::clone(self);
            let store = Arc::clone(&store);
            let commit = commit.clone();
            move |progress: Arc<Mutex<Counts>>| {
                index.passes_run.fetch_add(1, Ordering::Relaxed);
                index::run(&index, &store, &loaded, &commit, &progress)
            }
        });
        let mut done = pass.done.clone();
        let outcome = tokio::time::timeout(wait, done.wait_for(Option::is_some)).await;
        match outcome {
            Ok(Ok(finished)) => match finished.clone() {
                Some(Ok(mut status)) => {
                    status.commit_id = commit.to_string();
                    Ok(status)
                }
                Some(Err(error)) => Err(SymbolError::Pass(error)),
                None => Err(SymbolError::ShuttingDown),
            },
            Ok(Err(_)) => Err(SymbolError::ShuttingDown),
            Err(_) => Ok(IndexStatus {
                commit_id: commit.to_string(),
                state: State::Indexing,
                counts: pass
                    .progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
                indexed_at: None,
            }),
        }
    }

    /// The running pass over `key`, or a new one running `start()`'s work
    /// on the blocking pool.
    fn pass<W>(self: &Arc<Self>, repo: RepoId, key: &str, start: impl FnOnce() -> W) -> Pass
    where
        W: FnOnce(Arc<Mutex<Counts>>) -> Result<IndexStatus, SymbolError> + Send + 'static,
    {
        let mut passes = self.passes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pass) = passes.get(&(repo, key.to_owned())) {
            return pass.clone();
        }
        let (tx, rx) = watch::channel(None);
        let progress = Arc::new(Mutex::new(Counts::default()));
        let pass = Pass {
            progress: Arc::clone(&progress),
            done: rx,
        };
        passes.insert((repo, key.to_owned()), pass.clone());
        let work = start();
        let index = Arc::clone(self);
        let key = key.to_owned();
        tokio::spawn(async move {
            // Bounded: each pass walks or diffs a tree on a blocking thread.
            let permit = Arc::clone(&index.pass_permits).acquire_owned().await;
            let outcome = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                work(progress)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            index
                .passes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&(repo, key));
            let _ = tx.send(Some(outcome));
        });
        pass
    }

    /// The status of `key` without starting anything: `COMPLETE` from its
    /// row, `INDEXING` while a pass runs, else `NOT_INDEXED`.
    pub(crate) fn status_of(
        &self,
        store: &Store,
        repo: RepoId,
        key: &str,
        commit_id: &str,
    ) -> Result<IndexStatus, SymbolError> {
        if let Some(pass) = self
            .passes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(repo, key.to_owned()))
        {
            return Ok(IndexStatus {
                commit_id: commit_id.to_owned(),
                state: State::Indexing,
                counts: pass
                    .progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
                indexed_at: None,
            });
        }
        Ok(match store.tree(key)? {
            Some(row) => IndexStatus {
                commit_id: commit_id.to_owned(),
                state: if row.complete {
                    State::Complete
                } else {
                    State::Indexing
                },
                counts: row.counts,
                indexed_at: row.indexed_at,
            },
            None => IndexStatus {
                commit_id: commit_id.to_owned(),
                state: State::NotIndexed,
                counts: Counts::default(),
                indexed_at: None,
            },
        })
    }

    /// Queues every snapshot that changes a tree, and starts the queue:
    /// before any snapshot can run.
    pub(crate) fn attach(state: &Arc<AppState>) {
        let (shutdown, stopping) = (state.shutdown.clone(), state.symbols.stopping.clone());
        tokio::spawn(async move {
            shutdown.cancelled().await;
            stopping.cancel();
        });
        state
            .manager
            .scheduler()
            .on_snapshot(Self::snapshot_hook(Arc::downgrade(state)));
        state
            .symbols
            .start(Arc::downgrade(state), state.shutdown.clone());
    }

    /// The snapshot hook that queues every snapshot that changed a tree.
    fn snapshot_hook(state: Weak<AppState>) -> crate::snapshot::SnapshotHook {
        Box::new(move |outcome: &crate::snapshot::SnapshotOutcome| {
            if !outcome.changed {
                return;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            if let Some(entry) = state.manager.entry(outcome.workspace) {
                state.symbols.notice(
                    entry.record().repo_id,
                    Some(outcome.workspace),
                    outcome.commit_id.clone(),
                );
            }
        })
    }

    /// The current extractor of a supported path's language.
    pub(crate) fn extractor_for(path: &str) -> Option<String> {
        Language::from_path(path).map(reef_symbols::extractor)
    }
}
