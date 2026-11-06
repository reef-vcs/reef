//! [`ArtifactBytes`]: the seam between the artifact store's index and where
//! its bytes live (R3.02, ADR 0056 decision 2).
//!
//! A remote implementation (R4.02's S3-compatible blob storage, or a team's
//! own) replaces [`super::LocalArtifacts`], and nothing above the trait
//! changes: the service, the index, retention and the CLI only call it. The
//! methods are blocking, called on the blocking pool as every filesystem
//! step is; a remote implementation may make them async in its own ticket,
//! which changes signatures but not the seam.

use std::io::{self, Read};
use std::time::SystemTime;

use super::Digest;

/// Why an append was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AppendError {
    /// The offset is not the bytes received so far.
    #[error("the upload holds {received} bytes; a chunk must start there")]
    Offset {
        /// The bytes the partial upload holds.
        received: u64,
    },
    /// The chunk reaches past the declared size.
    #[error("the chunk ends at byte {end}, past the artifact's size {size}")]
    Beyond {
        /// Where the chunk ends.
        end: u64,
        /// The declared size.
        size: u64,
    },
    /// Writing failed; the partial upload keeps what was written.
    #[error("write the partial upload")]
    Io(#[from] io::Error),
}

/// Why a commit was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommitError {
    /// The partial upload is not complete: it holds `received` bytes.
    #[error("the upload holds {received} of {size} bytes")]
    Incomplete {
        /// What it holds.
        received: u64,
        /// The declared size.
        size: u64,
    },
    /// The complete bytes hash to another digest; the partial upload was
    /// deleted.
    #[error("the bytes hash to {actual}, not the digest declared")]
    Mismatch {
        /// Their SHA-256.
        actual: String,
    },
    /// Reading, hashing or moving it failed.
    #[error("store the upload")]
    Io(#[from] io::Error),
}

/// Where an artifact's bytes live (R3.02, ADR 0056). Content-addressed by
/// SHA-256. The index (the `artifacts` rows, and which evidence names
/// what) stays in `reef.sqlite3` whichever store holds the bytes. Blocking:
/// called on the blocking pool, as every filesystem step is.
pub trait ArtifactBytes: Send + Sync {
    /// Bytes of a partial upload of (`digest`, `size`) received so far; 0
    /// for none.
    fn received(&self, digest: &Digest, size: u64) -> io::Result<u64>;
    /// Appends `data` to that partial upload at `offset`, which must equal
    /// `received`; the bytes held after. Never creates the repository's own
    /// directory: once the repository is forgotten, an append fails with
    /// [`io::ErrorKind::NotFound`] rather than leave files behind.
    fn append(
        &self,
        digest: &Digest,
        size: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, AppendError>;
    /// Hashes the complete partial upload, made durable first; deletes it
    /// when it is not `digest`. Slow, and called without the
    /// [`super::ArtifactLock`]: a complete partial upload takes no more
    /// appends, so [`ArtifactBytes::commit`] stores what was hashed.
    fn check(&self, digest: &Digest, size: u64) -> Result<(), CommitError>;
    /// Stores a complete partial upload that [`ArtifactBytes::check`] found
    /// to be `digest`, replacing stored bytes of the digest, if any. Quick:
    /// called with the lock held.
    fn commit(&self, digest: &Digest, size: u64) -> Result<(), CommitError>;
    /// Reads stored bytes from `offset`.
    fn open(&self, digest: &Digest, offset: u64) -> io::Result<Box<dyn Read + Send>>;
    /// Whether stored bytes exist, and their length.
    fn stat(&self, digest: &Digest) -> io::Result<Option<u64>>;
    /// Deletes stored bytes; the bytes they had allocated (0 when there
    /// were none).
    fn remove(&self, digest: &Digest) -> io::Result<u64>;
    /// Every stored digest with its length and mtime, for GC's orphan
    /// sweep.
    fn stored(&self) -> io::Result<Vec<(Digest, u64, SystemTime)>>;
    /// Deletes partial uploads last written before `before`: how many, and
    /// their bytes.
    fn sweep_incoming(&self, before: SystemTime) -> io::Result<(u32, u64)>;
}
