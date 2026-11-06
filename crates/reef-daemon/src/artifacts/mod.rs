//! The artifact store (R3.02, ADR 0056): the bytes evidence names by
//! SHA-256 (logs, reports, build outputs), once per repository.
//!
//! - [`store`]: [`ArtifactBytes`], the seam: where the bytes live. The
//!   index (the `artifacts` rows, and which evidence names what) stays in
//!   `reef.sqlite3` whichever store holds them, and nothing above the trait
//!   touches a file.
//! - [`local`]: [`LocalArtifacts`], files under the repository's state
//!   directory:
//!
//!   ```text
//!   <state>/repos/<id>/artifacts/sha256/<first 2 hex>/<remaining 62 hex>   stored, mode 0444
//!   <state>/repos/<id>/artifacts/incoming/<sha256>.<size>                   a partial upload
//!   ```
//!
//! - [`retention`]: garbage collection's seventh step, which deletes
//!   evidence and artifacts that no live work keeps.
//!
//! A path is built only from a [`Digest`], which is checked as 64 lowercase
//! hex digits, so no request text reaches the filesystem. Each repository
//! has an [`ArtifactLock`], held by GC while it collects, by
//! `ForgetRepository`, by an upload's commit (the rename and the row, after
//! the bytes were hashed without it), by an upload's stored-digest answer
//! when a stored row matches, and by a damaged read's quarantine while it
//! deletes, so a client never hears "stored" for bytes GC is deleting.
//! Appends, hashing, and a stored-digest check that finds no row do not
//! take it.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;

use crate::model::RepoId;

pub mod local;
pub mod retention;
pub mod store;

pub use local::LocalArtifacts;
pub use store::{AppendError, ArtifactBytes, CommitError};

/// The largest chunk an upload sends, and a read returns, in one message:
/// 1 MiB, well under gRPC's 4 MiB message limit.
pub const MAX_CHUNK: usize = 1 << 20;

/// The directory under a repository's state directory that holds its
/// artifacts.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// An artifact's digest: SHA-256, 64 lowercase hex digits. The only way to
/// name an artifact's path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    /// The length of a digest in hex.
    pub const LEN: usize = 64;

    /// `text` as a digest when it is exactly 64 lowercase hex digits.
    pub fn parse(text: &str) -> Option<Digest> {
        (text.len() == Self::LEN && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Digest(text.to_owned()))
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(reef_evidence::sha256_hex(bytes))
    }

    /// The digest in lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who holds an [`ArtifactLock`], as its test hook is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// An upload's stored-digest check or commit.
    Upload,
    /// GC's collection.
    Gc,
    /// `ForgetRepository`.
    Forget,
    /// GC's evidence sweep, between choosing the evidence it deletes and
    /// deleting it. The lock is not held there: [`ArtifactLock::pause`]
    /// only runs the hook.
    EvidenceSweep,
}

/// A hook run each time an [`ArtifactLock`] is taken, while it is held: a
/// test pauses a holder there to race the other.
pub type LockHook = Arc<dyn Fn(Holder) -> BoxFuture<'static, ()> + Send + Sync>;

/// A repository's artifact lock; see the module docs.
#[derive(Default)]
pub struct ArtifactLock {
    lock: tokio::sync::Mutex<()>,
    hook: Mutex<Option<LockHook>>,
}

impl fmt::Debug for ArtifactLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactLock").finish_non_exhaustive()
    }
}

impl ArtifactLock {
    /// Takes the lock for `holder`, then runs the test hook, if any.
    pub async fn acquire(&self, holder: Holder) -> tokio::sync::MutexGuard<'_, ()> {
        let guard = self.lock.lock().await;
        self.pause(holder).await;
        guard
    }

    /// Runs the test hook for `holder`, if any, without taking the lock.
    pub async fn pause(&self, holder: Holder) {
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(holder).await;
        }
    }

    /// Sets or clears the test hook.
    #[doc(hidden)]
    pub fn set_hook(&self, hook: Option<LockHook>) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = hook;
    }
}

/// Every repository's artifact store and lock, in
/// [`crate::AppState::artifacts`].
pub struct Artifacts {
    /// `<state>/repos`.
    repos: PathBuf,
    stores: Mutex<HashMap<RepoId, Arc<dyn ArtifactBytes>>>,
    locks: Mutex<HashMap<RepoId, Arc<ArtifactLock>>>,
}

impl fmt::Debug for Artifacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Artifacts")
            .field("repos", &self.repos)
            .finish_non_exhaustive()
    }
}

impl Artifacts {
    /// The stores of the repositories under `repos` (`<state>/repos`).
    pub fn new(repos: PathBuf) -> Artifacts {
        Artifacts {
            repos,
            stores: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Where `repo`'s artifacts live: `<state>/repos/<id>/artifacts`.
    pub fn dir(&self, repo: RepoId) -> PathBuf {
        self.repos.join(repo.to_string()).join(ARTIFACTS_DIR)
    }

    /// `repo`'s store of bytes.
    pub fn bytes(&self, repo: RepoId) -> Arc<dyn ArtifactBytes> {
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            stores
                .entry(repo)
                .or_insert_with(|| Arc::new(LocalArtifacts::new(self.dir(repo)))),
        )
    }

    /// `repo`'s artifact lock.
    pub fn lock(&self, repo: RepoId) -> Arc<ArtifactLock> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(repo).or_default())
    }

    /// Forgets `repo`'s store, whose files went with its state directory.
    pub fn forget(&self, repo: RepoId) {
        self.stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R3.02 test 2: only 64 lowercase hex digits make a digest, so no other
    // text reaches a path.
    #[test]
    fn digests_are_checked() {
        let good = "0123456789abcdef".repeat(4);
        assert_eq!(Digest::parse(&good).unwrap().as_str(), good);
        for bad in [
            good.to_uppercase(),
            good[..63].to_owned(),
            format!("{good}0"),
            format!("../{}", &good[3..]),
            format!("{}\0", &good[..63]),
            format!("{}/", &good[..63]),
            String::new(),
        ] {
            assert!(Digest::parse(&bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            Digest::of(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
