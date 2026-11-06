//! `reef.api.v1.ArtifactService` (R3.02, ADR 0056): `UploadArtifact`,
//! `GetArtifact` and `ReadArtifact` over the artifact store
//! ([`crate::artifacts`]) and its index in `reef.sqlite3`.
//!
//! An upload is a series of unary calls keyed by digest and size. Each one
//! first checks whether the digest is stored (dedup): a read, and only when
//! a stored row matches does it take the repository's [`ArtifactLock`],
//! check again, mark the artifact used and answer "stored". Then it appends
//! its chunk on the blocking pool without the lock, and, once the bytes
//! received reach the size, hashes them without the lock and commits them
//! under it: the repository checked again, the rename, then the row.
//! Nothing here records an operation.
//!
//! A read streams from the blocking pool through a channel of a few
//! messages. A read of the whole artifact hashes what it sends and ends in
//! `ARTIFACT_DAMAGED` when the bytes no longer match; the daemon then
//! drops them (a tombstone), so the artifact reads as collected until the
//! genuine bytes are uploaded again.
//!
//! [`ArtifactLock`]: crate::artifacts::ArtifactLock

use std::io::{self, Read as _};
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use reef_api::v1::artifact_service_server::ArtifactService;
use reef_api::v1::{
    self, GetArtifactRequest, GetArtifactResponse, ReadArtifactRequest, ReadArtifactResponse,
    UploadArtifactRequest, UploadArtifactResponse,
};
use reef_lib::error::{Classify, ErrorKind};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::artifacts::{AppendError, ArtifactBytes, CommitError, Digest, Holder, MAX_CHUNK};
use crate::daemon::AppState;
use crate::db::{ARTIFACT_DIGEST_LEN, ArtifactRow, DbError, MIN_PREFIX_LEN};
use crate::error::{Advice, Advise, status};
use crate::model::{RepoId, Timestamp};

/// Messages a read buffers ahead of a slow client.
const READ_BUFFER: usize = 4;

type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadArtifactResponse, Status>> + Send>>;

/// Why an artifact request is refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArtifactError {
    /// A digest that is not one: `INVALID_ARTIFACT_DIGEST`.
    #[error("artifact digest {sha256:?} must be {}", if *full { "64 lowercase hex digits".to_owned() } else { format!("{MIN_PREFIX_LEN} to {ARTIFACT_DIGEST_LEN} hex digits") })]
    InvalidDigest {
        /// The text.
        sha256: String,
        /// A full digest was required (an upload).
        full: bool,
    },
    /// A prefix of several artifacts: `AMBIGUOUS_ARTIFACT_DIGEST`.
    #[error("artifact digest {sha256:?} is ambiguous; it matches {}", candidates.join(", "))]
    Ambiguous {
        /// The prefix.
        sha256: String,
        /// Up to ten of the digests it matches.
        candidates: Vec<String>,
    },
    /// A chunk over [`MAX_CHUNK`] or past the size:
    /// `INVALID_ARTIFACT_UPLOAD`.
    #[error("{message}")]
    InvalidUpload {
        /// `data` or `size`.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// A size over `[artifacts] max_object_size`: `ARTIFACT_TOO_LARGE`.
    #[error("the artifact is {size} bytes; this daemon stores at most {max_object_size}")]
    TooLarge {
        /// The declared size.
        size: u64,
        /// The limit.
        max_object_size: u64,
    },
    /// A chunk at another offset than the bytes received:
    /// `ARTIFACT_OFFSET_MISMATCH`.
    #[error("the upload holds {received} bytes; send the chunk that starts there")]
    OffsetMismatch {
        /// The bytes the partial upload holds.
        received: u64,
    },
    /// The complete bytes hash to another digest:
    /// `ARTIFACT_DIGEST_MISMATCH`.
    #[error("the uploaded bytes hash to {actual}, not {sha256}; the partial upload was deleted")]
    DigestMismatch {
        /// The digest declared.
        sha256: String,
        /// The digest of the bytes.
        actual: String,
    },
    /// Nothing stored or named has the digest: `ARTIFACT_NOT_FOUND`.
    #[error("{}", if *named { format!("artifact {sha256} is named by evidence, but its bytes were never stored here") } else { format!("no artifact of the repository has the digest {sha256:?}") })]
    NotFound {
        /// The digest or prefix.
        sha256: String,
        /// Evidence names it (a read of an absent artifact).
        named: bool,
    },
    /// Retention deleted the bytes: `ARTIFACT_COLLECTED`.
    #[error("artifact {sha256} was collected by retention at {collected_at}")]
    Collected {
        /// The digest.
        sha256: String,
        /// When, RFC 3339.
        collected_at: String,
    },
    /// A read starting past the end: `INVALID_ARTIFACT_RANGE`.
    #[error("offset {offset} is beyond the artifact's {size} bytes")]
    InvalidRange {
        /// The offset asked for.
        offset: u64,
        /// The artifact's size.
        size: u64,
    },
    /// The stored bytes are gone or changed: `ARTIFACT_DAMAGED`.
    #[error("{}", match damage {
        Damage::Missing => format!("artifact {sha256} is stored, but its file is missing"),
        Damage::Size { size, actual } => format!("artifact {sha256} is stored as {size} bytes, but its file holds {actual}"),
        Damage::Digest { actual } => format!("artifact {sha256}'s stored bytes hash to {actual}; what was sent is not the artifact"),
    })]
    Damaged {
        /// The digest.
        sha256: String,
        /// What is wrong.
        damage: Damage,
    },
    /// Reading or writing the store failed, as a full disk does.
    #[error("the artifact store")]
    Io(#[from] io::Error),
    /// A blocking task did not finish.
    #[error("an artifact task failed: {0}")]
    Task(String),
}

/// What is wrong with a stored artifact's bytes: `ARTIFACT_DAMAGED`'s
/// `damage`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Damage {
    /// The file is gone: `missing`.
    Missing,
    /// The file's length is not the row's size: `size`.
    Size {
        /// The size the index records.
        size: u64,
        /// The file's length.
        actual: u64,
    },
    /// The bytes hash to another digest: `digest`.
    Digest {
        /// Their SHA-256.
        actual: String,
    },
}

impl Classify for ArtifactError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidDigest { .. }
            | Self::Ambiguous { .. }
            | Self::InvalidUpload { .. }
            | Self::TooLarge { .. }
            | Self::DigestMismatch { .. }
            | Self::InvalidRange { .. } => ErrorKind::InvalidArgument,
            Self::OffsetMismatch { .. } => ErrorKind::Conflict,
            Self::NotFound { .. } | Self::Collected { .. } => ErrorKind::NotFound,
            Self::Damaged { .. } | Self::Task(_) => ErrorKind::Internal,
            Self::Io(error) => reef_lib::error::classify_by_source(error),
        }
    }
}

impl Advise for ArtifactError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::OffsetMismatch { received } => Some(Advice::hint(format!(
                "Resume from byte {received}: send the chunk that starts there."
            ))),
            Self::TooLarge { .. } => Some(Advice::hint(
                "Store a smaller artifact, such as a compressed or trimmed log, or raise [artifacts] max_object_size in the daemon's config.toml.",
            )),
            Self::DigestMismatch { .. } => Some(Advice::hint(
                "The bytes changed between hashing and uploading, or the digest is wrong: hash the file again and upload it from byte 0.",
            )),
            Self::NotFound { named: true, .. } => Some(Advice::hint(
                "The evidence records the digest only; upload the same bytes with `reef evidence artifact upload <file>`, or run the check again.",
            )),
            Self::Collected { .. } => Some(Advice::hint(
                "Retention removed the bytes; run the check again, or upload the same bytes with `reef evidence artifact upload <file>`, which stores them again.",
            )),
            Self::Damaged { .. } => Some(Advice::hint(
                "The stored file was changed, truncated or removed behind the daemon; upload the genuine bytes again with `reef evidence artifact upload <file>` to repair it.",
            )),
            _ => None,
        }
    }
}

impl ArtifactError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::InvalidDigest { sha256, .. } => (
                reason::INVALID_ARTIFACT_DIGEST,
                vec![("sha256", sha256.clone())],
            ),
            Self::Ambiguous { sha256, candidates } => (
                reason::AMBIGUOUS_ARTIFACT_DIGEST,
                vec![
                    ("sha256", sha256.clone()),
                    ("sha256s", candidates.join(",")),
                ],
            ),
            Self::InvalidUpload { field, .. } => (
                reason::INVALID_ARTIFACT_UPLOAD,
                vec![("field", (*field).to_owned())],
            ),
            Self::TooLarge {
                size,
                max_object_size,
            } => (
                reason::ARTIFACT_TOO_LARGE,
                vec![
                    ("size", size.to_string()),
                    ("max_object_size", max_object_size.to_string()),
                ],
            ),
            Self::OffsetMismatch { received } => (
                reason::ARTIFACT_OFFSET_MISMATCH,
                vec![("received", received.to_string())],
            ),
            Self::DigestMismatch { sha256, actual } => (
                reason::ARTIFACT_DIGEST_MISMATCH,
                vec![
                    ("sha256", sha256.clone()),
                    ("actual_sha256", actual.clone()),
                ],
            ),
            Self::NotFound { sha256, .. } => {
                (reason::ARTIFACT_NOT_FOUND, vec![("sha256", sha256.clone())])
            }
            Self::Collected {
                sha256,
                collected_at,
            } => (
                reason::ARTIFACT_COLLECTED,
                vec![
                    ("sha256", sha256.clone()),
                    ("collected_at", collected_at.clone()),
                ],
            ),
            Self::InvalidRange { offset, size } => (
                reason::INVALID_ARTIFACT_RANGE,
                vec![("offset", offset.to_string()), ("size", size.to_string())],
            ),
            Self::Damaged { sha256, damage } => {
                let mut metadata = vec![("sha256", sha256.clone())];
                match damage {
                    Damage::Missing => metadata.push(("damage", "missing".to_owned())),
                    Damage::Size { actual, .. } => {
                        metadata.push(("damage", "size".to_owned()));
                        metadata.push(("actual_size", actual.to_string()));
                    }
                    Damage::Digest { actual } => {
                        metadata.push(("damage", "digest".to_owned()));
                        metadata.push(("actual_sha256", actual.clone()));
                    }
                }
                (reason::ARTIFACT_DAMAGED, metadata)
            }
            Self::Io(_) | Self::Task(_) => return None,
        })
    }
}

fn task(error: &tokio::task::JoinError) -> Status {
    status(ArtifactError::Task(error.to_string()))
}

/// Runs `f` on the blocking pool.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| task(&error))
}

/// An artifact row as the API reports it, with the evidence naming it.
pub(crate) fn artifact_message(
    row: &ArtifactRow,
    evidence: Option<(u32, Vec<String>)>,
) -> v1::Artifact {
    let (evidence_count, evidence_ids) = evidence.unwrap_or_default();
    let ts = |at: Timestamp| Some(super::convert::timestamp(at));
    v1::Artifact {
        sha256: row.sha256.clone(),
        state: if row.is_stored() {
            v1::ArtifactState::Stored
        } else {
            v1::ArtifactState::Collected
        }
        .into(),
        size: row.size,
        stored_at: ts(row.stored_at),
        used_at: ts(row.used_at),
        collected_at: row.collected_at.and_then(ts),
        added_by_agent: row.added_by_agent.clone().unwrap_or_default(),
        remote_token_id: row.remote_token_id.clone().unwrap_or_default(),
        evidence_count,
        evidence_ids,
    }
}

/// Serves `ArtifactService`.
#[derive(Debug)]
pub struct ArtifactsHandler {
    state: Arc<AppState>,
}

impl ArtifactsHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> ArtifactsHandler {
        ArtifactsHandler { state }
    }

    /// `row` with the evidence naming it.
    fn message(&self, row: &ArtifactRow) -> Result<v1::Artifact, Status> {
        let evidence = self
            .state
            .db
            .artifact_evidence(row.repo_id, &row.sha256)
            .map_err(status)?;
        Ok(artifact_message(row, Some(evidence)))
    }

    /// The answer for a stored digest: `size` received, the record, and
    /// `created`.
    fn stored(&self, row: &ArtifactRow, created: bool) -> Result<UploadArtifactResponse, Status> {
        Ok(UploadArtifactResponse {
            received: row.size,
            artifact: Some(self.message(row)?),
            created,
        })
    }

    /// The row of `digest` when its bytes are stored, marked used; `None`
    /// otherwise. A read first: only when a stored row matches is the lock
    /// taken and the row checked again ([`ArtifactsHandler::stored_held`]),
    /// so the answer "stored" is only given under the lock, never for bytes
    /// GC is deleting.
    async fn stored_now(
        &self,
        repo: RepoId,
        digest: &Digest,
    ) -> Result<Option<ArtifactRow>, Status> {
        let row = self
            .state
            .db
            .artifact(repo, digest.as_str())
            .map_err(status)?;
        if !row.is_some_and(|row| row.is_stored()) {
            return Ok(None);
        }
        let lock = self.state.artifacts.lock(repo);
        let _held = lock.acquire(Holder::Upload).await;
        self.stored_held(repo, digest).await
    }

    /// [`ArtifactsHandler::stored_now`] with the lock held by the caller.
    /// A stored row whose file is gone, or whose file's length is not the
    /// row's size (damaged, or changed behind the daemon), is not stored:
    /// the upload goes on and repairs it. Writes (marks it used) only when
    /// it is stored.
    async fn stored_held(
        &self,
        repo: RepoId,
        digest: &Digest,
    ) -> Result<Option<ArtifactRow>, Status> {
        let Some(row) = self
            .state
            .db
            .artifact(repo, digest.as_str())
            .map_err(status)?
            .filter(ArtifactRow::is_stored)
        else {
            return Ok(None);
        };
        let (bytes, key) = (self.state.artifacts.bytes(repo), digest.clone());
        match blocking(move || bytes.stat(&key)).await? {
            Ok(Some(length)) if length == row.size => {}
            Ok(_) => return Ok(None),
            Err(error) => return Err(status(ArtifactError::Io(error))),
        }
        self.state
            .db
            .touch_artifact(repo, digest.as_str(), Timestamp::now())
            .map_err(status)
    }

    /// An I/O error of `repo`'s store; `REPOSITORY_NOT_FOUND` when it is a
    /// missing directory because the repository was forgotten meanwhile.
    fn io_status(&self, repo: RepoId, error: io::Error) -> Status {
        if error.kind() == io::ErrorKind::NotFound
            && let Err(gone) = self.state.db.repository(repo)
        {
            return status(gone);
        }
        status(ArtifactError::Io(error))
    }

    /// Where an upload stands when its partial upload is not complete, or
    /// is gone: stored by another call meanwhile, or `received` bytes.
    async fn standing(
        &self,
        repo: RepoId,
        digest: &Digest,
        received: u64,
    ) -> Result<UploadArtifactResponse, Status> {
        if let Some(row) = self.stored_now(repo, digest).await? {
            return self.stored(&row, false);
        }
        Ok(UploadArtifactResponse {
            received,
            artifact: None,
            created: false,
        })
    }

    /// Commits a complete partial upload: hashes it without the lock, then,
    /// holding it, checks that the repository is still registered and that
    /// no other call stored the digest meanwhile (which makes this one a
    /// dedup), stores the bytes and records the row.
    #[expect(clippy::too_many_arguments, reason = "the upload's fields")]
    async fn commit(
        &self,
        repo: RepoId,
        digest: &Digest,
        size: u64,
        bytes: Arc<dyn ArtifactBytes>,
        agent: Option<String>,
        remote_token_id: Option<String>,
        received: u64,
    ) -> Result<UploadArtifactResponse, Status> {
        let (checking, key) = (Arc::clone(&bytes), digest.clone());
        match blocking(move || checking.check(&key, size)).await? {
            Ok(()) => {}
            Err(CommitError::Mismatch { actual }) => {
                return Err(status(ArtifactError::DigestMismatch {
                    sha256: digest.to_string(),
                    actual,
                }));
            }
            Err(CommitError::Incomplete { received, .. }) => {
                // Another call's commit moved it, or it shrank: say where it
                // stands.
                return self.standing(repo, digest, received).await;
            }
            Err(CommitError::Io(error)) => return Err(self.io_status(repo, error)),
        }
        let lock = self.state.artifacts.lock(repo);
        let _held = lock.acquire(Holder::Upload).await;
        // `ForgetRepository` holds the lock while it deletes: after it, the
        // repository is gone and nothing is stored.
        self.state.db.repository(repo).map_err(status)?;
        if let Some(row) = self.stored_held(repo, digest).await? {
            return self.stored(&row, false);
        }
        let (committing, key) = (Arc::clone(&bytes), digest.clone());
        match blocking(move || committing.commit(&key, size)).await? {
            Ok(()) => {}
            Err(CommitError::Mismatch { actual }) => {
                return Err(status(ArtifactError::DigestMismatch {
                    sha256: digest.to_string(),
                    actual,
                }));
            }
            Err(CommitError::Incomplete { received, .. }) => {
                return Ok(UploadArtifactResponse {
                    received,
                    artifact: None,
                    created: false,
                });
            }
            Err(CommitError::Io(error)) => return Err(self.io_status(repo, error)),
        }
        let now = Timestamp::now();
        let row = self
            .state
            .db
            .store_artifact(&ArtifactRow {
                repo_id: repo,
                sha256: digest.to_string(),
                size,
                stored_at: now,
                used_at: now,
                collected_at: None,
                added_by_agent: agent,
                remote_token_id,
            })
            .map_err(status)?;
        tracing::info!(
            sha256 = %digest,
            size_bytes = size,
            received_bytes = received,
            "artifact stored"
        );
        self.stored(&row, true)
    }

    /// The error for `row`, stored, whose file is not there: collected,
    /// when GC made it a tombstone since it was read, otherwise damaged.
    fn gone(&self, row: ArtifactRow) -> Status {
        match self.state.db.artifact(row.repo_id, &row.sha256) {
            Ok(Some(ArtifactRow {
                collected_at: Some(collected_at),
                ..
            })) => status(ArtifactError::Collected {
                sha256: row.sha256,
                collected_at: collected_at.to_rfc3339(),
            }),
            Ok(_) => status(ArtifactError::Damaged {
                sha256: row.sha256,
                damage: Damage::Missing,
            }),
            Err(error) => status(error),
        }
    }

    /// The row a get or read names: `text` is a full digest or a prefix.
    /// `Ok(None)` for a full digest no row has.
    fn lookup(&self, repo: RepoId, text: &str) -> Result<Option<ArtifactRow>, Status> {
        let text = text.trim();
        let valid = (MIN_PREFIX_LEN..=ARTIFACT_DIGEST_LEN).contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(status(ArtifactError::InvalidDigest {
                sha256: text.to_owned(),
                full: false,
            }));
        }
        let prefix = text.to_ascii_lowercase();
        if prefix.len() == ARTIFACT_DIGEST_LEN {
            return self.state.db.artifact(repo, &prefix).map_err(status);
        }
        match self.state.db.artifact_by_prefix(repo, &prefix) {
            Ok(row) => Ok(Some(row)),
            Err(DbError::NotFound { .. }) => Err(status(ArtifactError::NotFound {
                sha256: prefix,
                named: false,
            })),
            Err(DbError::Ambiguous { candidates, .. }) => Err(status(ArtifactError::Ambiguous {
                sha256: prefix,
                candidates,
            })),
            Err(error) => Err(status(error)),
        }
    }
}

/// Reads `length` bytes from `reader`, at `offset` of `digest`, on the
/// blocking pool, sending messages of at most [`MAX_CHUNK`] on `tx`, the
/// first with `artifact`. With `whole`, hashes what it sent: the digest of
/// what was sent when that is not `digest`, for the caller's
/// `ARTIFACT_DAMAGED`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "moved into the blocking task"
)]
fn stream_bytes(
    mut reader: Box<dyn io::Read + Send>,
    digest: Digest,
    artifact: v1::Artifact,
    offset: u64,
    length: u64,
    whole: bool,
    tx: &mpsc::Sender<Result<ReadArtifactResponse, Status>>,
) -> Option<String> {
    let mut context = whole.then(|| ring::digest::Context::new(&ring::digest::SHA256));
    let mut first = Some(artifact);
    let mut at = offset;
    let mut left = length;
    let mut buffer = vec![0u8; MAX_CHUNK];
    loop {
        let want = usize::try_from(left).map_or(MAX_CHUNK, |left| left.min(MAX_CHUNK));
        let mut filled = 0;
        while filled < want {
            match reader.read(&mut buffer[filled..want]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = tx.blocking_send(Err(status(ArtifactError::Io(error))));
                    return None;
                }
            }
        }
        let data = buffer[..filled].to_vec();
        if let Some(context) = context.as_mut() {
            context.update(&data);
        }
        let message = ReadArtifactResponse {
            artifact: first.take(),
            offset: at,
            data,
        };
        let sent = u64::try_from(filled).unwrap_or(u64::MAX);
        let short = filled < want;
        // A message with data, or the first one, which carries the record.
        if (sent > 0 || message.artifact.is_some()) && tx.blocking_send(Ok(message)).is_err() {
            return None;
        }
        at += sent;
        left -= sent;
        if left == 0 || short {
            break;
        }
    }
    let context = context?;
    let actual = hex(context.finish().as_ref());
    (actual != digest.as_str() || left > 0).then_some(actual)
}

/// After a whole read found `digest`'s bytes damaged: hashes the file again
/// without the lock and, when it is still not the digest, takes the lock
/// and, if the row is still the one stored at `stored_at` (no upload
/// repaired it meanwhile), makes it a tombstone and deletes the file. The
/// artifact then reads as collected, and the next upload of the genuine
/// bytes stores them again. Deletes only bytes that fail their own digest.
async fn quarantine(
    state: &AppState,
    repo: RepoId,
    bytes: Arc<dyn ArtifactBytes>,
    digest: Digest,
    stored_at: Timestamp,
) {
    let (hashing, key) = (Arc::clone(&bytes), digest.clone());
    let actual = match tokio::task::spawn_blocking(move || {
        reef_evidence::sha256_hex_of(hashing.open(&key, 0)?)
    })
    .await
    {
        Ok(Ok(actual)) => actual,
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "cannot check a damaged artifact again");
            return;
        }
        Err(error) => {
            tracing::warn!(error = %error, "checking a damaged artifact again failed");
            return;
        }
    };
    if actual == digest.as_str() {
        return;
    }
    let lock = state.artifacts.lock(repo);
    let _held = lock.acquire(Holder::Upload).await;
    // The row first, as GC does: the index never names bytes that are gone.
    match state
        .db
        .tombstone_artifact(repo, digest.as_str(), Some(stored_at), Timestamp::now())
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(error = %error, "cannot drop a damaged artifact");
            return;
        }
    }
    let key = digest.clone();
    match tokio::task::spawn_blocking(move || bytes.remove(&key)).await {
        Ok(Ok(_)) => tracing::warn!(
            sha256 = %digest,
            actual_sha256 = %actual,
            "a stored artifact's bytes no longer match its digest; dropped, so it reads as collected until an upload repairs it"
        ),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, sha256 = %digest, "cannot delete a damaged artifact's file");
        }
        Err(error) => tracing::warn!(error = %error, "deleting a damaged artifact failed"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[tonic::async_trait]
impl ArtifactService for ArtifactsHandler {
    type ReadArtifactStream = ReadStream;

    #[tracing::instrument(skip_all, fields(repo))]
    async fn upload_artifact(
        &self,
        request: Request<UploadArtifactRequest>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let remote_token_id = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let state = &self.state;
        let repo = state
            .manager
            .resolve_repo(&request.repo)
            .map_err(status)?
            .id;
        tracing::Span::current().record("repo", tracing::field::display(repo));
        let Some(digest) = Digest::parse(request.sha256.trim()) else {
            return Err(status(ArtifactError::InvalidDigest {
                sha256: request.sha256.clone(),
                full: true,
            }));
        };
        let (size, offset, data) = (request.size, request.offset, request.data);
        if data.len() > MAX_CHUNK {
            return Err(status(ArtifactError::InvalidUpload {
                field: "data",
                message: format!(
                    "a chunk is at most {MAX_CHUNK} bytes; this one is {}",
                    data.len()
                ),
            }));
        }
        let end = offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        if end > size {
            return Err(status(ArtifactError::InvalidUpload {
                field: "size",
                message: format!("the chunk ends at byte {end}, past the artifact's size {size}"),
            }));
        }
        // Dedup: stored bytes are never sent again, whatever the limit is
        // now.
        if let Some(row) = self.stored_now(repo, &digest).await? {
            return Ok(Response::new(self.stored(&row, false)?));
        }
        let max_object_size = state.config.file.artifacts.max_object_size.0;
        if size > max_object_size {
            return Err(status(ArtifactError::TooLarge {
                size,
                max_object_size,
            }));
        }
        let bytes = state.artifacts.bytes(repo);
        let probe = offset == 0 && data.is_empty();
        let received = {
            let (bytes, digest) = (Arc::clone(&bytes), digest.clone());
            if probe {
                blocking(move || bytes.received(&digest, size).map_err(AppendError::from)).await?
            } else {
                blocking(move || bytes.append(&digest, size, offset, &data)).await?
            }
        };
        let received = match received {
            Ok(received) => received,
            Err(AppendError::Offset { received }) => {
                // Another client may have finished it meanwhile.
                if let Some(row) = self.stored_now(repo, &digest).await? {
                    return Ok(Response::new(self.stored(&row, false)?));
                }
                if received != size {
                    return Err(status(ArtifactError::OffsetMismatch { received }));
                }
                // Complete, but not committed (a commit that failed, or a
                // restart between the last append and its commit).
                received
            }
            Err(AppendError::Beyond { end, size }) => {
                return Err(status(ArtifactError::InvalidUpload {
                    field: "size",
                    message: format!(
                        "the chunk ends at byte {end}, past the artifact's size {size}"
                    ),
                }));
            }
            Err(AppendError::Io(error)) => return Err(self.io_status(repo, error)),
        };
        if received < size {
            return Ok(Response::new(UploadArtifactResponse {
                received,
                artifact: None,
                created: false,
            }));
        }
        let response = self
            .commit(repo, &digest, size, bytes, agent, remote_token_id, received)
            .await?;
        Ok(Response::new(response))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_artifact(
        &self,
        request: Request<GetArtifactRequest>,
    ) -> Result<Response<GetArtifactResponse>, Status> {
        let request = request.into_inner();
        let repo = self
            .state
            .manager
            .resolve_repo(&request.repo)
            .map_err(status)?
            .id;
        tracing::Span::current().record("repo", tracing::field::display(repo));
        let artifact = if let Some(row) = self.lookup(repo, &request.sha256)? {
            self.message(&row)?
        } else {
            // A full digest with no row: absent, when evidence names it.
            let sha256 = request.sha256.trim().to_ascii_lowercase();
            let (evidence_count, evidence_ids) = self
                .state
                .db
                .artifact_evidence(repo, &sha256)
                .map_err(status)?;
            if evidence_count == 0 {
                return Err(status(ArtifactError::NotFound {
                    sha256,
                    named: false,
                }));
            }
            v1::Artifact {
                sha256,
                state: v1::ArtifactState::Absent.into(),
                evidence_count,
                evidence_ids,
                ..v1::Artifact::default()
            }
        };
        Ok(Response::new(GetArtifactResponse {
            artifact: Some(artifact),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn read_artifact(
        &self,
        request: Request<ReadArtifactRequest>,
    ) -> Result<Response<Self::ReadArtifactStream>, Status> {
        let request = request.into_inner();
        let repo = self
            .state
            .manager
            .resolve_repo(&request.repo)
            .map_err(status)?
            .id;
        tracing::Span::current().record("repo", tracing::field::display(repo));
        let Some(row) = self.lookup(repo, &request.sha256)? else {
            let sha256 = request.sha256.trim().to_ascii_lowercase();
            let (named, _) = self
                .state
                .db
                .artifact_evidence(repo, &sha256)
                .map_err(status)?;
            return Err(status(ArtifactError::NotFound {
                sha256,
                named: named > 0,
            }));
        };
        if let Some(collected_at) = row.collected_at {
            return Err(status(ArtifactError::Collected {
                sha256: row.sha256,
                collected_at: collected_at.to_rfc3339(),
            }));
        }
        if request.offset > row.size {
            return Err(status(ArtifactError::InvalidRange {
                offset: request.offset,
                size: row.size,
            }));
        }
        let Some(digest) = Digest::parse(&row.sha256) else {
            return Err(status(ArtifactError::NotFound {
                sha256: row.sha256,
                named: false,
            }));
        };
        let rest = row.size - request.offset;
        let length = if request.length == 0 {
            rest
        } else {
            request.length.min(rest)
        };
        let whole = request.offset == 0 && length == row.size;
        let bytes = self.state.artifacts.bytes(repo);
        // A missing file, or one of another length than the row's, is an
        // error before any data.
        let present = {
            let (bytes, digest) = (Arc::clone(&bytes), digest.clone());
            blocking(move || bytes.stat(&digest)).await?
        };
        match present {
            Ok(Some(length)) if length == row.size => {}
            Ok(Some(actual)) => {
                return Err(status(ArtifactError::Damaged {
                    sha256: row.sha256,
                    damage: Damage::Size {
                        size: row.size,
                        actual,
                    },
                }));
            }
            Ok(None) => return Err(self.gone(row)),
            Err(error) => return Err(status(ArtifactError::Io(error))),
        }
        let offset = request.offset;
        let reader = {
            let (bytes, digest) = (Arc::clone(&bytes), digest.clone());
            blocking(move || bytes.open(&digest, offset)).await?
        };
        let reader = match reader {
            Ok(reader) => reader,
            // GC collected it since the stat.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(self.gone(row)),
            Err(error) => return Err(status(ArtifactError::Io(error))),
        };
        let artifact = self.message(&row)?;
        let (tx, rx) = mpsc::channel(READ_BUFFER);
        let state = Arc::clone(&self.state);
        let stored_at = row.stored_at;
        tokio::spawn(async move {
            let (key, sender) = (digest.clone(), tx.clone());
            let damaged = tokio::task::spawn_blocking(move || {
                stream_bytes(reader, key, artifact, offset, length, whole, &sender)
            })
            .await;
            if let Ok(Some(actual)) = damaged {
                let sha256 = digest.to_string();
                quarantine(&state, repo, bytes, digest, stored_at).await;
                let _ = tx
                    .send(Err(status(ArtifactError::Damaged {
                        sha256,
                        damage: Damage::Digest { actual },
                    })))
                    .await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_carry_their_metadata() {
        let damaged = ArtifactError::Damaged {
            sha256: "ab".into(),
            damage: Damage::Digest {
                actual: "cd".into(),
            },
        };
        let (reason, metadata) = damaged.reason().unwrap();
        assert_eq!(reason, reef_api::error::ARTIFACT_DAMAGED);
        assert_eq!(
            metadata,
            [
                ("sha256", "ab".to_owned()),
                ("damage", "digest".to_owned()),
                ("actual_sha256", "cd".to_owned())
            ]
        );
        assert_eq!(damaged.kind(), ErrorKind::Internal);
        let short = ArtifactError::Damaged {
            sha256: "ab".into(),
            damage: Damage::Size {
                size: 10,
                actual: 9,
            },
        };
        assert_eq!(
            short.reason().unwrap().1,
            [
                ("sha256", "ab".to_owned()),
                ("damage", "size".to_owned()),
                ("actual_size", "9".to_owned())
            ]
        );
        let mismatch = ArtifactError::OffsetMismatch { received: 7 };
        assert_eq!(mismatch.kind(), ErrorKind::Conflict);
        assert_eq!(mismatch.advice().unwrap().next.len(), 0);
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
    }
}
