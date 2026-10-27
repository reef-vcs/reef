//! Reads over jj state (R2.02, ADR 0030): status, the log, changes,
//! diffs, and the operation log.
//!
//! A read takes its repository from one short job on the repository's
//! thread, then does its work on the blocking pool under
//! `pollster::block_on`, holding one of a bounded number of permits, so a
//! slow diff never delays a snapshot's commit and reads of one repository
//! run in parallel (ADR 0012's scan pattern). jj-lib's futures are not
//! `Send`; they are built and driven on the blocking thread.
//!
//! A read stops between entries when its RPC is dropped or the daemon shuts
//! down. A read that panics fails with `INTERNAL` and has the repository's
//! thread reopen its store, which the panic may have poisoned (#63).
//!
//! Two caches: repositories loaded at an operation other than the head (so
//! later pages at a pinned operation do not reload its index), and
//! computed operation effects. Operations are immutable, so neither entry
//! goes stale; GC rewrites operation ids, and `CollectGarbage` and
//! `ForgetRepository` clear the repository's entries.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::FutureExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::effects::{self, OpEffects};
use reef_lib::jj::oplog::{self, OpLookupError};
use reef_lib::jj::{OpenError, ReadonlyRepo};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::{Advice, Advise};
use crate::model::{RepoId, WorkspaceId};
use crate::repo::{RepoHandle, RepoThreadError};

/// Repositories at an operation kept loaded.
const AT_CACHE: usize = 8;
/// Operations whose effects are kept.
const EFFECTS_CACHE: usize = 4096;

/// A map that forgets its oldest entry past `capacity`.
#[derive(Debug)]
struct Fifo<K, V> {
    capacity: usize,
    order: VecDeque<K>,
    entries: HashMap<K, V>,
}

impl<K: Clone + Eq + Hash, V: Clone> Fifo<K, V> {
    fn new(capacity: usize) -> Self {
        Fifo {
            capacity,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: K, value: V) {
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.order.retain(&keep);
        self.entries.retain(|key, _| keep(key));
    }
}

/// Failure of a read's machinery, not of what it read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The read panicked; the repository's store is reopened.
    #[error("a read of repository {repo} panicked")]
    Panicked {
        /// The repository.
        repo: RepoId,
    },
    /// The blocking pool is gone: the daemon is shutting down.
    #[error("the read was cancelled")]
    Cancelled,
    /// The repository's thread could not hand over the repository.
    #[error(transparent)]
    RepoThread(#[from] RepoThreadError),
    /// The repository could not be reloaded at head.
    #[error("reload the repository")]
    Open(#[from] OpenError),
    /// The snapshot a read takes first failed: reason `SNAPSHOT_FAILED`
    /// (R2.27).
    #[error("snapshot before reading")]
    Snapshot {
        /// The workspace.
        workspace: WorkspaceId,
        /// Why.
        #[source]
        source: Box<crate::workspace::ManagerError>,
    },
}

impl Classify for ReadError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Panicked { .. } => ErrorKind::Internal,
            Self::Cancelled => ErrorKind::Unavailable,
            Self::RepoThread(error) => error.kind(),
            Self::Open(error) => error.kind(),
            Self::Snapshot { source, .. } => source.kind(),
        }
    }
}

impl Advise for ReadError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Snapshot { source, .. } => Some(source.advice().unwrap_or_else(|| {
                Advice::hint("Pass skip_snapshot to read the last recorded snapshot.")
            })),
            _ => None,
        }
    }
}

/// The operation a page token pinned is gone (GC rewrote the log):
/// reason `OPERATION_NOT_FOUND`.
#[derive(Debug, thiserror::Error)]
#[error("the operation the first page was read at is gone")]
pub struct PinnedGone(#[source] pub OpLookupError);

impl Classify for PinnedGone {
    fn kind(&self) -> ErrorKind {
        self.0.kind()
    }
}

impl Advise for PinnedGone {
    fn advice(&self) -> Option<Advice> {
        Some(Advice::hint(
            "The operation the first page was read at is gone; read from the first page again.",
        ))
    }
}

/// Whether a read should stop: its RPC was dropped, or the daemon is
/// shutting down. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Stop {
    dropped: Arc<AtomicBool>,
    shutdown: CancellationToken,
}

impl Stop {
    /// True once the read should end at the next entry.
    pub fn requested(&self) -> bool {
        self.dropped.load(Ordering::Relaxed) || self.shutdown.is_cancelled()
    }
}

/// Held by a handler for the length of its RPC: dropping it, as tonic does
/// when the client goes away, stops the read.
#[derive(Debug)]
pub struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The daemon's reads; one in [`crate::AppState::reads`].
#[derive(Debug)]
pub struct Reads {
    permits: Arc<Semaphore>,
    at: Mutex<Fifo<(RepoId, String), Arc<ReadonlyRepo>>>,
    effects: Mutex<Fifo<(RepoId, String), OpEffects>>,
    shutdown: CancellationToken,
}

impl Reads {
    /// Reads with `max(2, available_parallelism())` permits, stopped by
    /// `shutdown`.
    pub fn new(shutdown: CancellationToken) -> Reads {
        let parallel = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Reads {
            permits: Arc::new(Semaphore::new(parallel.max(2))),
            at: Mutex::new(Fifo::new(AT_CACHE)),
            effects: Mutex::new(Fifo::new(EFFECTS_CACHE)),
            shutdown,
        }
    }

    /// A stop flag and the guard that sets it.
    pub fn stop(&self) -> (Stop, StopGuard) {
        let dropped = Arc::new(AtomicBool::new(false));
        (
            Stop {
                dropped: Arc::clone(&dropped),
                shutdown: self.shutdown.clone(),
            },
            StopGuard(dropped),
        )
    }

    /// The repository at head, reloaded first if another process wrote an
    /// operation since (as a snapshot does), from a job on `handle`'s
    /// thread.
    pub async fn current(&self, handle: &RepoHandle) -> Result<Arc<ReadonlyRepo>, ReadError> {
        let repo = handle
            .call_async(|head| async move { head.refresh().await }.boxed_local())
            .await??;
        Ok(repo)
    }

    /// The repository as the thread last loaded it, without reloading:
    /// operation reads look up the operation heads themselves.
    pub async fn loaded(&self, handle: &RepoHandle) -> Result<Arc<ReadonlyRepo>, ReadError> {
        Ok(handle.call(|head| head.readonly()).await?)
    }

    /// Runs `read` on the blocking pool with a permit, driving its future
    /// there. The permit is held by the blocking task, so a read whose RPC
    /// was dropped keeps its slot until it stops. A panic fails with
    /// [`ReadError::Panicked`], forgets the repository's cached entries,
    /// and has `handle`'s thread reopen the repository.
    pub async fn run<T, F, Fut>(&self, handle: &RepoHandle, read: F) -> Result<T, ReadError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| ReadError::Cancelled)?;
        self.blocking(handle, Some(permit), read).await
    }

    /// [`Reads::run`] holding `permit`, one of another pool's (R2.09's
    /// subscriptions have their own).
    pub async fn run_holding<T, F, Fut>(
        &self,
        handle: &RepoHandle,
        permit: tokio::sync::OwnedSemaphorePermit,
        read: F,
    ) -> Result<T, ReadError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        self.blocking(handle, Some(permit), read).await
    }

    /// [`Reads::run`] without a permit, for the effects a write's response
    /// reports: writes are bounded by the repository's thread already, and
    /// must not queue behind slow reads.
    pub async fn run_for_write<T, F, Fut>(
        &self,
        handle: &RepoHandle,
        read: F,
    ) -> Result<T, ReadError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        self.blocking(handle, None, read).await
    }

    async fn blocking<T, F, Fut>(
        &self,
        handle: &RepoHandle,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
        read: F,
    ) -> Result<T, ReadError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T>,
    {
        let span = tracing::Span::current();
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _entered = span.enter();
            pollster::block_on(read())
        })
        .await;
        match joined {
            Ok(value) => Ok(value),
            Err(error) if error.is_panic() => {
                tracing::error!(repo = %handle.id(), "read panicked; reopening the repository");
                // Repositories loaded from the store the panic may have
                // poisoned are not reused.
                self.clear(handle.id());
                handle.reopen_after_panic();
                Err(ReadError::Panicked { repo: handle.id() })
            }
            Err(_) => Err(ReadError::Cancelled),
        }
    }

    /// `head`'s store at the operation `text` names, which may be the head
    /// itself; cached by operation id. Runs on the calling (blocking)
    /// thread.
    pub async fn at(
        &self,
        repo: RepoId,
        head: &Arc<ReadonlyRepo>,
        text: &str,
    ) -> Result<Arc<ReadonlyRepo>, OpLookupError> {
        let operation = oplog::resolve_operation(head, text).await?;
        self.at_operation(repo, head, &operation).await
    }

    /// `head`'s store at `operation`, cached by its id.
    pub async fn at_operation(
        &self,
        repo: RepoId,
        head: &Arc<ReadonlyRepo>,
        operation: &reef_lib::jj::Operation,
    ) -> Result<Arc<ReadonlyRepo>, OpLookupError> {
        if operation.id() == head.op_id() {
            return Ok(Arc::clone(head));
        }
        let key = (repo, oplog::entry(operation).id.to_string());
        if let Some(found) = self
            .at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(found);
        }
        let loaded = oplog::load_at(head, operation).await?;
        self.at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Arc::clone(&loaded));
        Ok(loaded)
    }

    /// The effects of `operation`, computed over `head`'s store and cached;
    /// `None`, logged, when they cannot be computed. Runs on the calling
    /// (blocking) thread.
    pub async fn effects(
        &self,
        repo: RepoId,
        head: &Arc<ReadonlyRepo>,
        operation: &reef_lib::jj::Operation,
    ) -> Option<OpEffects> {
        let key = (repo, oplog::entry(operation).id.to_string());
        if let Some(found) = self
            .effects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Some(found);
        }
        match effects::operation_effects(head, operation, effects::MAX_EFFECTS).await {
            Ok(found) => {
                self.effects
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key, found.clone());
                Some(found)
            }
            Err(error) => {
                tracing::warn!(
                    op_id = %key.1,
                    error = %ErrorChain(&error),
                    "cannot compute the operation's effects; reporting it without them"
                );
                None
            }
        }
    }

    /// Forgets `repo`'s cached entries: GC rewrote its operation ids, or
    /// it was forgotten.
    pub fn clear(&self, repo: RepoId) {
        self.at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != repo);
        self.effects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_forgets_the_oldest() {
        let mut fifo = Fifo::new(2);
        fifo.insert(1, "a");
        fifo.insert(2, "b");
        fifo.insert(1, "a again");
        assert_eq!(
            fifo.get(&1),
            Some("a again"),
            "a replaced key keeps its place"
        );
        fifo.insert(3, "c");
        assert_eq!(fifo.get(&1), None);
        assert_eq!(fifo.get(&2), Some("b"));
        assert_eq!(fifo.get(&3), Some("c"));
        fifo.retain(|key| *key != 2);
        assert_eq!(fifo.get(&2), None);
        assert_eq!(fifo.order.len(), 1);
    }

    #[test]
    fn a_dropped_guard_stops_the_read() {
        let reads = Reads::new(CancellationToken::new());
        let (stop, guard) = reads.stop();
        assert!(!stop.requested());
        drop(guard);
        assert!(stop.requested());
        let shutdown = CancellationToken::new();
        let reads = Reads::new(shutdown.clone());
        let (stop, _guard) = reads.stop();
        shutdown.cancel();
        assert!(stop.requested());
    }
}
