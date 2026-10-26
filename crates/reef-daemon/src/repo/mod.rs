//! One thread per registered repository (ADR 0012).
//!
//! jj-lib's futures are not `Send` and do blocking I/O (ADR 0008), so no jj
//! work runs on tokio's workers. Each repository gets a dedicated
//! [`std::thread`] owning a [`RepoHead`]: the user's default workspace loaded
//! at head. Every jj transaction, reload, ref import, and GC of that
//! repository runs there, as a job sent through a [`RepoHandle`]. Jobs run one
//! at a time in the order sent, which makes the thread the repository's write
//! lock: [`RepoHandle::pause`] holds it for a backup (R1.18).
//!
//! What crosses threads is ids, plain structs, and [`Arc<ReadonlyRepo>`],
//! which is `Send + Sync`; the assertion at the bottom of this file turns a
//! jj-lib bump that breaks that into a compile error.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::{CheckedTrees, OpenError, ReadonlyRepo, Repo, UserSettings};
use tokio::sync::oneshot;

use crate::model::{RepoId, RepositoryRecord};

mod exclude;
mod registry;
mod thread;

pub use registry::{OwnerPolicy, RegisterError, Registered};
use thread::{Message, RepoThread};

/// A repository as its thread holds it.
pub struct RepoHead {
    /// The repository's id.
    pub id: RepoId,
    /// The root of the user's own workspace.
    pub root: PathBuf,
    /// Whether jj and git share the working copy.
    pub colocated: bool,
    /// The jj settings every operation on this repository is written with.
    pub settings: UserSettings,
    /// The user's `default` workspace, loaded at head.
    pub repo: Repo,
    /// Trees [`Repo::check_tree_names`] found readable (RX.01).
    pub checked_trees: CheckedTrees,
    dirty: bool,
    /// A job panicked: reopen the repository rather than reload it.
    reopen: bool,
    /// How many times the repository was reopened; shared with every
    /// [`RepoHandle`].
    generation: Arc<AtomicU64>,
}

impl RepoHead {
    /// The repository as loaded, to hand to other threads.
    pub fn readonly(&self) -> Arc<ReadonlyRepo> {
        self.repo.readonly()
    }

    /// Reloads [`RepoHead::repo`] at the head operation; after a panicked
    /// job, opens the repository afresh instead.
    pub async fn reload(&mut self) -> Result<(), OpenError> {
        if self.reopen {
            // The panic may have poisoned a lock in jj-lib's store (its git
            // backend's mutex, RX.01), which a reload keeps: a new store
            // has none.
            self.repo = Repo::open(&self.root, &self.settings).await?;
            self.reopen = false;
            self.generation.fetch_add(1, Ordering::Relaxed);
            tracing::info!(repo = %self.id, "repository reopened after a panicked job");
        } else {
            self.repo.reload_at_head().await?;
        }
        self.dirty = false;
        Ok(())
    }

    /// How many times [`RepoHead::reload`] has reopened the repository
    /// after a panicked job. A [`Repo`] opened from [`RepoHead::repo`]
    /// shares its store, so one opened at an earlier generation may hold a
    /// store the panic poisoned (#63).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Whether a job panicked and the reopen after it has not succeeded
    /// yet; [`RepoHead::refresh`] retries it.
    pub fn reopen_pending(&self) -> bool {
        self.reopen
    }

    /// Records that the current job panicked: the thread reopens the
    /// repository when it returns.
    pub(super) fn note_panicked(&mut self) {
        self.dirty = true;
        self.reopen = true;
    }

    /// The repository at head, reloading first if anything (the `jj` CLI,
    /// another process) has written an operation since the last load.
    /// Clears a pending reload either way.
    pub async fn refresh(&mut self) -> Result<Arc<ReadonlyRepo>, OpenError> {
        if self.dirty {
            self.reload().await?;
        } else {
            self.repo.reload_if_moved().await?;
        }
        Ok(self.readonly())
    }

    /// Makes `repo`, the result of a transaction this job committed on
    /// [`RepoHead::readonly`], the head, without reloading. Operations other
    /// processes wrote meanwhile are merged by the next
    /// [`RepoHead::refresh`].
    pub fn adopt(&mut self, repo: Arc<ReadonlyRepo>) {
        self.repo.set_head(repo);
        self.dirty = false;
    }

    /// Records that the current job committed an operation, possibly
    /// through another [`Repo`] or a transaction on [`RepoHead::readonly`].
    /// The thread reloads at head when the job returns, so the next job
    /// starts from the new operation.
    pub fn note_committed(&mut self) {
        self.dirty = true;
    }
}

/// Failure to run a job on a repository thread.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepoThreadError {
    /// The thread has stopped, or stopped before running the job: the
    /// repository was closed or the daemon is shutting down.
    #[error("repository {id} is closed")]
    Closed {
        /// The repository.
        id: RepoId,
    },
    /// The job panicked. The thread survives and reopens the repository.
    #[error("a job on repository {id} panicked")]
    Panicked {
        /// The repository.
        id: RepoId,
    },
}

impl Classify for RepoThreadError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Closed { .. } => ErrorKind::Unavailable,
            Self::Panicked { .. } => ErrorKind::Internal,
        }
    }
}

/// Failure to open a repository's thread.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The thread could not be created.
    #[error("start the thread for repository {id}")]
    Spawn {
        /// The repository.
        id: RepoId,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The repository could not be loaded.
    #[error("open repository {id} at {}", root.display())]
    Open {
        /// The repository.
        id: RepoId,
        /// Its registered root.
        root: PathBuf,
        /// The underlying error.
        #[source]
        source: OpenError,
    },
}

impl Classify for RegistryError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Spawn { .. } => ErrorKind::Internal,
            Self::Open { source, .. } => source.kind(),
        }
    }
}

/// Sends jobs to one repository's thread. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RepoHandle {
    id: RepoId,
    tx: std::sync::mpsc::Sender<Message>,
    generation: Arc<AtomicU64>,
}

impl RepoHandle {
    /// The repository.
    pub fn id(&self) -> RepoId {
        self.id
    }

    /// [`RepoHead::generation`], read without a job: it may lag a reopen
    /// the thread is making.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Runs `f` on the repository thread after every job sent before it,
    /// and returns its result.
    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut RepoHead) -> T + Send + 'static,
    ) -> impl Future<Output = Result<T, RepoThreadError>> + Send + 'static {
        let id = self.id;
        let (reply, result) = oneshot::channel();
        let job = Box::new(move |head: &mut RepoHead| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(head)));
            let outcome = outcome.map_err(|_| {
                tracing::error!(repo = %id, "repository job panicked");
                head.note_panicked();
                RepoThreadError::Panicked { id }
            });
            // The caller may have stopped waiting; the job ran either way.
            let _ = reply.send(outcome);
        });
        let sent = self.tx.send(Message::Job(job));
        async move {
            sent.map_err(|_| RepoThreadError::Closed { id })?;
            result.await.map_err(|_| RepoThreadError::Closed { id })?
        }
    }

    /// Runs the future `f` returns on the repository thread, driving it with
    /// `pollster::block_on`: jj-lib's futures are not `Send` and cannot run
    /// on tokio. Otherwise as [`RepoHandle::call`].
    pub fn call_async<T, F>(
        &self,
        f: F,
    ) -> impl Future<Output = Result<T, RepoThreadError>> + Send + 'static
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut RepoHead) -> LocalBoxFuture<'a, T> + Send + 'static,
    {
        self.call(move |head| pollster::block_on(f(head)))
    }

    /// Marks the repository for reopening, as a panicked job does: a read
    /// that panicked off the thread (R2.02) may have poisoned a lock in the
    /// store it shares. Sent as a job; does not wait.
    pub fn reopen_after_panic(&self) {
        // The job runs whether or not anyone waits for its reply.
        drop(self.call(RepoHead::note_panicked));
    }

    /// Pauses the thread after the jobs already sent, until at most `max`
    /// from now (time spent waiting for those jobs counts against `max`):
    /// the returned future resolves once they have run and the thread is
    /// blocked. Jobs sent meanwhile wait. Dropping the [`PauseGuard`]
    /// resumes the thread; after `max` it resumes by itself with a warning.
    pub async fn pause(&self, max: Duration) -> Result<PauseGuard, RepoThreadError> {
        let id = self.id;
        let (resume, resumed) = std::sync::mpsc::channel();
        let (paused_tx, paused) = oneshot::channel();
        self.tx
            .send(Message::Pause {
                resumed,
                until: Instant::now() + max,
                paused: paused_tx,
            })
            .map_err(|_| RepoThreadError::Closed { id })?;
        paused.await.map_err(|_| RepoThreadError::Closed { id })?;
        Ok(PauseGuard { _resume: resume })
    }
}

/// Holds a repository thread paused; see [`RepoHandle::pause`].
#[must_use = "dropping the guard resumes the thread at once"]
#[derive(Debug)]
pub struct PauseGuard {
    // Dropping the sender disconnects the channel the thread waits on.
    _resume: std::sync::mpsc::Sender<()>,
}

/// The repository threads, by repository id.
#[derive(Debug, Default)]
pub struct RepoRegistry {
    threads: Mutex<HashMap<RepoId, RepoThread>>,
    /// Threads closed by [`RepoRegistry::close`], joined by
    /// [`RepoRegistry::close_all`].
    closed: Mutex<Vec<RepoThread>>,
    /// Held for the whole of each [`RepoRegistry::register`].
    register_turn: Mutex<()>,
    /// Whose repositories [`RepoRegistry::register`] accepts (RX.01, S-02).
    owners: OwnerPolicy,
}

impl RepoRegistry {
    /// An empty registry, accepting repositories owned by the daemon's
    /// user ([`OwnerPolicy::from_env`] without `allow_foreign_owner`).
    pub fn new() -> RepoRegistry {
        RepoRegistry::default()
    }

    /// An empty registry accepting the repositories `owners` does.
    pub fn with_owners(owners: OwnerPolicy) -> RepoRegistry {
        RepoRegistry {
            owners,
            ..RepoRegistry::default()
        }
    }

    /// Starts the thread for `record`, which loads the repository's default
    /// workspace at head, and returns its handle once loaded. Returns the
    /// existing handle if the repository is already open.
    ///
    /// Blocks while the repository loads; call it from a blocking context
    /// (`spawn_blocking`) in async code.
    pub fn open(
        &self,
        record: &RepositoryRecord,
        settings: UserSettings,
    ) -> Result<RepoHandle, RegistryError> {
        if let Some(handle) = self.get(record.id) {
            return Ok(handle);
        }
        let thread = RepoThread::spawn(record, settings)?;
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = threads.get(&record.id) {
            // Another caller opened it meanwhile; keep theirs.
            let handle = existing.handle.clone();
            drop(threads);
            thread.stop();
            self.lock_closed().push(thread);
            return Ok(handle);
        }
        let handle = thread.handle.clone();
        threads.insert(record.id, thread);
        Ok(handle)
    }

    /// The handle of an open repository.
    pub fn get(&self, id: RepoId) -> Option<RepoHandle> {
        self.threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .map(|thread| thread.handle.clone())
    }

    /// The ids of the open repositories.
    pub fn ids(&self) -> Vec<RepoId> {
        self.threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
    }

    /// Stops a repository's thread after the jobs already sent. Jobs sent
    /// later fail with [`RepoThreadError::Closed`]. Does not wait; the
    /// thread is joined by [`RepoRegistry::close_all`].
    pub fn close(&self, id: RepoId) {
        let thread = self
            .threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(thread) = thread {
            thread.stop();
            self.lock_closed().push(thread);
        }
    }

    /// Stops every thread and waits for each to finish its current and
    /// queued jobs. Blocks; run it on a blocking thread.
    pub fn close_all(&self) {
        let open: Vec<_> = self
            .threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, thread)| thread)
            .collect();
        for thread in &open {
            thread.stop();
        }
        let closed = std::mem::take(&mut *self.lock_closed());
        for thread in open.into_iter().chain(closed) {
            thread.join();
        }
    }

    fn lock_closed(&self) -> std::sync::MutexGuard<'_, Vec<RepoThread>> {
        self.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// `Arc<ReadonlyRepo>` and `MergedTree` cross from repository threads to
// workspace threads (ADR 0012). jj-lib 0.46.0 declares everything inside them
// `Send + Sync`; if a bump stops doing so, this fails to compile.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<ReadonlyRepo>>();
    assert_send_sync::<reef_lib::jj::MergedTree>();
};
