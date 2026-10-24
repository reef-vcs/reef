//! The repository thread's loop.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Instant;

use reef_lib::error::ErrorChain;
use reef_lib::jj::{CheckedTrees, Repo, UserSettings};
use tokio::sync::oneshot;

use super::{RegistryError, RepoHandle, RepoHead};
use crate::model::RepositoryRecord;

/// A job: runs on the thread with the repository and replies by itself.
pub(super) type Job = Box<dyn FnOnce(&mut RepoHead) + Send>;

/// What a [`RepoHandle`] sends its thread.
pub(super) enum Message {
    /// Run a job.
    Job(Job),
    /// Report through `paused`, then block until `resumed` is disconnected
    /// or signalled, or `until` passes.
    Pause {
        resumed: mpsc::Receiver<()>,
        until: Instant,
        paused: oneshot::Sender<()>,
    },
    /// Stop after the messages before this one.
    Shutdown,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Job(_) => "Job",
            Self::Pause { .. } => "Pause",
            Self::Shutdown => "Shutdown",
        })
    }
}

/// A running repository thread.
#[derive(Debug)]
pub(super) struct RepoThread {
    pub(super) handle: RepoHandle,
    join: JoinHandle<()>,
}

impl RepoThread {
    /// Starts the thread and waits for it to load the repository.
    pub(super) fn spawn(
        record: &RepositoryRecord,
        settings: UserSettings,
    ) -> Result<RepoThread, RegistryError> {
        let id = record.id;
        let (tx, rx) = mpsc::channel();
        let (loaded_tx, loaded) = mpsc::channel();
        let root = record.root.clone();
        let record = record.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&generation);
        let join = std::thread::Builder::new()
            .name(format!("reef-repo-{id}"))
            .spawn(move || {
                let span = tracing::info_span!("repo_thread", repo = %id);
                let _entered = span.enter();
                let repo = match pollster::block_on(Repo::open(&record.root, &settings)) {
                    Ok(repo) => repo,
                    Err(error) => {
                        let _ = loaded_tx.send(Err(error));
                        return;
                    }
                };
                let _ = loaded_tx.send(Ok(()));
                let head = RepoHead {
                    id,
                    root: record.root,
                    colocated: record.colocated,
                    settings,
                    repo,
                    checked_trees: CheckedTrees::default(),
                    dirty: false,
                    reopen: false,
                    generation: shared,
                };
                run(head, &rx);
            })
            .map_err(|source| RegistryError::Spawn { id, source })?;
        match loaded.recv() {
            Ok(Ok(())) => Ok(RepoThread {
                handle: RepoHandle { id, tx, generation },
                join,
            }),
            Ok(Err(source)) => {
                let _ = join.join();
                Err(RegistryError::Open { id, root, source })
            }
            // The thread panicked while loading; `join` re-raises nothing
            // here, the panic was already reported by the hook.
            Err(_) => {
                let _ = join.join();
                Err(RegistryError::Spawn {
                    id,
                    source: std::io::Error::other("the repository thread panicked while loading"),
                })
            }
        }
    }

    /// Asks the thread to stop after the messages already sent.
    pub(super) fn stop(&self) {
        // Fails only if the thread has already stopped.
        let _ = self.handle.tx.send(Message::Shutdown);
    }

    /// Waits for the thread to end.
    pub(super) fn join(self) {
        let id = self.handle.id;
        drop(self.handle);
        if self.join.join().is_err() {
            tracing::error!(repo = %id, "repository thread panicked");
        }
    }
}

fn run(mut head: RepoHead, rx: &mpsc::Receiver<Message>) {
    tracing::debug!(root = %head.root.display(), "repository thread started");
    // Ends on `Shutdown`, or when every handle is gone.
    for message in rx {
        match message {
            Message::Job(job) => {
                if head.dirty {
                    // A reload failed after the previous job; try again so
                    // this job does not start from a stale operation.
                    reload(&mut head);
                }
                // `call` catches the job's own panic; this guards the loop
                // against anything else.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut head)));
                if ran.is_err() {
                    head.note_panicked();
                }
                if head.dirty {
                    reload(&mut head);
                }
            }
            Message::Pause {
                resumed,
                until,
                paused,
            } => {
                if paused.send(()).is_err() {
                    // The caller gave up before the pause began.
                    continue;
                }
                tracing::info!("repository thread paused");
                let wait = until.saturating_duration_since(Instant::now());
                match resumed.recv_timeout(wait) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                        tracing::info!("repository thread resumed");
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        tracing::warn!(
                            waited_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                            "repository thread pause hit its deadline; resuming"
                        );
                    }
                }
            }
            Message::Shutdown => break,
        }
    }
    tracing::debug!("repository thread stopped");
}

fn reload(head: &mut RepoHead) {
    if let Err(error) = pollster::block_on(head.reload()) {
        // Stays dirty; the next job retries first.
        tracing::error!(error = %ErrorChain(&error), "reload at head failed");
    }
}
