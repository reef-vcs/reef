//! `reef.api.v1.EvidenceService` (R3.01, ADR 0055): `AddEvidence`,
//! `GetEvidence` and `ListEvidence` over the `evidence` tables of
//! `reef.sqlite3`.
//!
//! An add checks the envelope with [`reef_evidence::Verified::check`] on
//! the blocking pool, then the subject's tree and the optional revision on
//! the read pool, and only then takes the database for one transaction:
//! the key, the evidence and the link. Every read checks each object's
//! signature again, so a row changed behind the daemon reads as
//! `SIGNATURE_CHECK_INVALID`. Nothing here records an operation.
//!
//! An add also indexes the artifact digests the payload names (R3.02), in
//! its transaction, and every read reports each artifact's state from that
//! index: stored, collected, or absent.
//!
//! Every read also reports `identity` (R3.03, ADR 0057): the operator
//! identity the signing key is certified for, if any, and its status at the
//! object's `added_at`. The page's key ids are looked up in one query, and
//! each certificate is checked again in the blocking job that checks the
//! envelopes ([`crate::identity::read`]).
//!
//! A listing is newest added first; its page token's cursor is the last
//! object's `(added_at, id)`, and it pins the tree a `revision` resolved to
//! on the first page.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::evidence_service_server::EvidenceService;
use reef_api::v1::{
    self, AddEvidenceRequest, AddEvidenceResponse, GetEvidenceRequest, GetEvidenceResponse,
    ListEvidenceRequest, ListEvidenceResponse,
};
use reef_evidence::{Envelope, Index, Invalid, Outcome, Statement, Verified};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::files::{self, TreePresence};
use reef_lib::jj::revset;
use tonic::{Request, Response, Status};

use super::page::PageRequest;
use super::reading::{Target, bad_token, repo_at, snapshot_first, target, uses_working_copy};
use crate::daemon::AppState;
use crate::db::{
    DbError, EVIDENCE_ID_LEN, EvidenceFilter, EvidenceKeyRow, EvidenceLinkRow, EvidenceRow,
    MIN_PREFIX_LEN, StoredEvidence,
};
use crate::error::{Advice, Advise, status};
use crate::identity::read::{ReadKey, key_identity, read_keys, unverified_identity};
use crate::model::Timestamp;

const LIST_EVIDENCE: &str = "/reef.api.v1.EvidenceService/ListEvidence";

/// Objects per page when the request says 0.
const DEFAULT_PAGE_SIZE: usize = 50;
/// The largest page.
const MAX_PAGE_SIZE: usize = 200;
/// The length of a change id in hex.
const CHANGE_ID_LEN: usize = 32;

/// Why an evidence request is refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EvidenceError {
    /// The envelope is not evidence: `INVALID_EVIDENCE`, or
    /// `SIGNATURE_INVALID` for a signature that does not verify.
    #[error(transparent)]
    Invalid(#[from] Invalid),
    /// The subject's tree is not in the repository: `TREE_NOT_FOUND`.
    #[error("{}", if *not_a_tree {
        format!("object {tree_id} is in the repository, but it is not a tree")
    } else {
        format!("the repository holds no tree {tree_id}")
    })]
    TreeNotFound {
        /// The subject's tree id.
        tree_id: String,
        /// An object has the id, but it is a blob or a commit.
        not_a_tree: bool,
    },
    /// A `tree_id` filter that is not one: `INVALID_TREE_ID`.
    #[error("tree_id {tree_id:?} is not {len} lowercase hex digits")]
    InvalidTreeId {
        /// The text.
        tree_id: String,
        /// The repository's object id length in hex.
        len: usize,
    },
    /// The revision's tree is not the subject: `EVIDENCE_TREE_MISMATCH`.
    #[error(
        "the evidence is about tree {tree_id}, but commit {commit_id} has tree {commit_tree_id}"
    )]
    TreeMismatch {
        /// The subject's tree.
        tree_id: String,
        /// The commit the revision resolved to.
        commit_id: String,
        /// Its tree.
        commit_tree_id: String,
    },
    /// The revision's tree is conflicted: `CONFLICTED_REVISION`.
    #[error("commit {commit_id} is conflicted, so it has no single tree for evidence to be about")]
    Conflicted {
        /// The commit.
        commit_id: String,
    },
    /// A `change_id` filter that is not a full change id:
    /// `INVALID_CHANGE_ID`.
    #[error("change_id {change:?} is not a full change id in hex or reverse hex")]
    InvalidChangeId {
        /// The text.
        change: String,
    },
    /// A `key_ids` filter value that is not a full key id: 64 lowercase hex
    /// digits.
    #[error("key id {key_id:?} is not a full key id (64 lowercase hex digits)")]
    InvalidKeyId {
        /// The text.
        key_id: String,
    },
    /// An evidence id that is not 4 to 64 hex digits: `INVALID_EVIDENCE_ID`.
    #[error("evidence id {evidence_id:?} must be {MIN_PREFIX_LEN} to {EVIDENCE_ID_LEN} hex digits")]
    InvalidId {
        /// The text.
        evidence_id: String,
    },
    /// A prefix of several objects: `AMBIGUOUS_EVIDENCE_ID`.
    #[error("evidence id {evidence_id:?} is ambiguous; it matches {}", candidates.join(", "))]
    Ambiguous {
        /// The prefix.
        evidence_id: String,
        /// Up to ten of the ids it matches.
        candidates: Vec<String>,
    },
    /// No object has the id: `EVIDENCE_NOT_FOUND`.
    #[error("no evidence in the repository has the id {evidence_id:?}")]
    NotFound {
        /// The id or prefix.
        evidence_id: String,
    },
    /// Fields of a listing that do not go together, or a value out of its
    /// enum.
    #[error("{0}")]
    Request(String),
    /// A check on the blocking pool did not finish.
    #[error("checking evidence failed: {0}")]
    Task(String),
}

impl Classify for EvidenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Invalid(_)
            | Self::InvalidTreeId { .. }
            | Self::InvalidChangeId { .. }
            | Self::InvalidKeyId { .. }
            | Self::InvalidId { .. }
            | Self::Ambiguous { .. }
            | Self::Request(_) => ErrorKind::InvalidArgument,
            Self::TreeNotFound { .. } | Self::NotFound { .. } => ErrorKind::NotFound,
            Self::TreeMismatch { .. } | Self::Conflicted { .. } => ErrorKind::Conflict,
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for EvidenceError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Invalid(invalid) => Some(Advice::hint(invalid.hint())),
            Self::TreeNotFound { .. } => Some(Advice::hint(
                "Evidence is about a tree of this repository: sign the tree id of a commit in it, as `git rev-parse <commit>^{tree}` prints it.",
            )),
            Self::TreeMismatch { commit_tree_id, .. } => Some(Advice::hint(format!(
                "The revision has moved on to tree {commit_tree_id}; sign a claim about that tree, or add this one without a revision."
            ))),
            Self::Conflicted { .. } => Some(Advice::hint(
                "Resolve the conflict first; a conflicted change has no tree to test.",
            )),
            Self::NotFound { .. } => Some(Advice::run(
                "List the repository's evidence to find its id; evidence that is not on live work is deleted after [evidence] retention (`reef config show`).",
                ["reef", "evidence", "list"],
            )),
            Self::InvalidKeyId { .. } => Some(Advice::hint(
                "Filter by a full key id, 64 hex digits: `reef evidence show <id>` prints an object's key, and `reef evidence key generate` printed yours.",
            )),
            _ => None,
        }
    }
}

impl EvidenceError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::Invalid(Invalid::Signature { key_id }) => {
                (reason::SIGNATURE_INVALID, vec![("key_id", key_id.clone())])
            }
            Self::Invalid(invalid) => {
                let mut metadata = vec![("check", invalid.check().unwrap_or_default().to_owned())];
                if let Some(field) = invalid.field() {
                    metadata.push(("field", field.to_owned()));
                }
                (reason::INVALID_EVIDENCE, metadata)
            }
            Self::TreeNotFound { tree_id, .. } => {
                (reason::TREE_NOT_FOUND, vec![("tree_id", tree_id.clone())])
            }
            Self::InvalidTreeId { tree_id, .. } => {
                (reason::INVALID_TREE_ID, vec![("tree_id", tree_id.clone())])
            }
            Self::TreeMismatch {
                tree_id,
                commit_id,
                commit_tree_id,
            } => (
                reason::EVIDENCE_TREE_MISMATCH,
                vec![
                    ("tree_id", tree_id.clone()),
                    ("commit_id", commit_id.clone()),
                    ("commit_tree_id", commit_tree_id.clone()),
                ],
            ),
            Self::Conflicted { commit_id } => (
                reason::CONFLICTED_REVISION,
                vec![("commit_id", commit_id.clone())],
            ),
            Self::InvalidChangeId { change } => {
                (reason::INVALID_CHANGE_ID, vec![("change", change.clone())])
            }
            Self::InvalidId { evidence_id } => (
                reason::INVALID_EVIDENCE_ID,
                vec![("evidence_id", evidence_id.clone())],
            ),
            Self::Ambiguous {
                evidence_id,
                candidates,
            } => (
                reason::AMBIGUOUS_EVIDENCE_ID,
                vec![
                    ("evidence_id", evidence_id.clone()),
                    ("evidence_ids", candidates.join(",")),
                ],
            ),
            Self::NotFound { evidence_id } => (
                reason::EVIDENCE_NOT_FOUND,
                vec![("evidence_id", evidence_id.clone())],
            ),
            Self::Request(_) | Self::Task(_) | Self::InvalidKeyId { .. } => return None,
        })
    }
}

/// The rows an add of `verified` writes, as `AddEvidence` and an executor
/// job's acceptance (R3.05) write them: the key, the evidence row, and the
/// digests of the artifacts its payload names.
pub(crate) fn evidence_rows(
    verified: &Verified,
    repo_id: crate::model::RepoId,
    agent: Option<String>,
    remote_token_id: Option<String>,
    now: Timestamp,
) -> (EvidenceKeyRow, EvidenceRow, Vec<String>) {
    let key = EvidenceKeyRow {
        id: verified.key_id().to_string(),
        algorithm: verified.key.algorithm().to_owned(),
        public_key: verified.key.spki().to_vec(),
        first_seen_at: now,
    };
    let index = &verified.index;
    let row = EvidenceRow {
        repo_id,
        id: verified.id.clone(),
        envelope: verified.canonical.clone(),
        format: reef_evidence::FORMAT,
        tree_id: index.tree_id.clone(),
        kind: index.kind.clone(),
        result: index.result.as_str().to_owned(),
        target: index.target.clone(),
        key_id: key.id.clone(),
        started_at: Timestamp(index.started_at_ms),
        finished_at: Timestamp(index.finished_at_ms),
        added_at: now,
        added_by_agent: agent,
        remote_token_id,
    };
    let digests = index
        .artifacts
        .iter()
        .map(|artifact| artifact.sha256.clone())
        .collect();
    (key, row, digests)
}

fn task(error: &tokio::task::JoinError) -> Status {
    status(EvidenceError::Task(error.to_string()))
}

/// Serves `EvidenceService`.
#[derive(Debug)]
pub struct EvidenceHandler {
    state: Arc<AppState>,
}

impl EvidenceHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> EvidenceHandler {
        EvidenceHandler { state }
    }
}

/// `outcome` in the API.
pub(crate) fn result_message(outcome: &str) -> v1::EvidenceResult {
    match Outcome::parse(outcome) {
        Some(Outcome::Pass) => v1::EvidenceResult::Pass,
        Some(Outcome::Fail) => v1::EvidenceResult::Fail,
        Some(Outcome::Error) => v1::EvidenceResult::Error,
        None => v1::EvidenceResult::Unspecified,
    }
}

/// The statement in a stored envelope, without checking it: what a row
/// whose signature no longer verifies still shows.
fn unchecked_statement(envelope: &[u8]) -> Option<Statement> {
    let envelope: Envelope = serde_json::from_slice(envelope).ok()?;
    serde_json::from_slice(&envelope.payload_bytes()?).ok()
}

/// What a read reports of `stored`, its signature checked again, the index
/// its payload gives (`None` when the envelope does not decode), the key id
/// it reports, and that key id again only when the signature is valid: the
/// one an identity may be looked up by (review finding 1).
///
/// Every field comes from the payload: from the verified one when the
/// signature checks out, else from the payload as stored, else from the
/// row's columns. The columns are an index written at insert; a row is
/// `SIGNATURE_CHECK_VALID` only when its id, its envelope, its key and
/// every index column agree with the verified payload, so a column changed
/// behind the daemon reads as invalid, with the payload's own values.
fn evidence_message(
    stored: &StoredEvidence,
    change_ids: Vec<String>,
    with_envelope: bool,
    current: Option<&BTreeSet<String>>,
) -> (v1::Evidence, Option<Index>, String, Option<String>) {
    let row = &stored.row;
    let checked = stored
        .public_key
        .as_deref()
        .and_then(|key| Verified::check(&row.envelope, key).ok());
    let valid = checked.as_ref().is_some_and(|verified| {
        verified.id == row.id
            && verified.canonical == row.envelope
            && verified.key_id().as_str() == row.key_id
            && row.format == reef_evidence::FORMAT
            && columns_match(&verified.index, &stored.row)
    });
    let key_id = checked.as_ref().map_or_else(
        || row.key_id.clone(),
        |verified| verified.key_id().to_string(),
    );
    let (statement, index) = if let Some(verified) = checked {
        (Some(verified.statement), Some(verified.index))
    } else {
        let statement = unchecked_statement(&row.envelope);
        let index = statement
            .as_ref()
            .and_then(|statement| reef_evidence::verify::index_of(statement).ok());
        (statement, index)
    };
    let artifacts = statement
        .as_ref()
        .map(|statement| {
            statement
                .predicate
                .artifacts
                .iter()
                .map(|artifact| v1::EvidenceArtifact {
                    name: artifact.name.clone().unwrap_or_default(),
                    sha256: artifact.digest.get("sha256").cloned().unwrap_or_default(),
                    uri: artifact.uri.clone().unwrap_or_default(),
                    media_type: artifact.media_type.clone().unwrap_or_default(),
                    state: v1::ArtifactState::Absent.into(),
                    size: 0,
                    collected_at: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let predicate_type = statement
        .map(|statement| statement.predicate_type)
        .unwrap_or_default();
    let (tree_id, kind, result, target, started_at, finished_at) = match &index {
        Some(index) => (
            index.tree_id.clone(),
            index.kind.clone(),
            result_message(index.result.as_str()),
            index.target.clone(),
            Timestamp(index.started_at_ms),
            Timestamp(index.finished_at_ms),
        ),
        None => (
            row.tree_id.clone(),
            row.kind.clone(),
            result_message(&row.result),
            row.target.clone(),
            row.started_at,
            row.finished_at,
        ),
    };
    let now = current.is_some_and(|trees| trees.contains(&tree_id));
    let message = v1::Evidence {
        evidence_id: row.id.clone(),
        tree_id,
        kind,
        result: result.into(),
        target: target.unwrap_or_default(),
        key_id: key_id.clone(),
        started_at: Some(super::convert::timestamp(started_at)),
        finished_at: Some(super::convert::timestamp(finished_at)),
        added_at: Some(super::convert::timestamp(row.added_at)),
        added_by_agent: row.added_by_agent.clone().unwrap_or_default(),
        artifacts,
        change_ids,
        signature: if valid {
            v1::SignatureCheck::Valid
        } else {
            v1::SignatureCheck::Invalid
        }
        .into(),
        predicate_type,
        envelope: if with_envelope {
            row.envelope.clone()
        } else {
            Vec::new()
        },
        current: now,
        remote_token_id: row.remote_token_id.clone().unwrap_or_default(),
        identity: None,
    };
    let verified_key = valid.then(|| key_id.clone());
    (message, index, key_id, verified_key)
}

/// Whether the row's index columns are what the verified payload says.
fn columns_match(index: &Index, row: &EvidenceRow) -> bool {
    index.tree_id == row.tree_id
        && index.kind == row.kind
        && index.result.as_str() == row.result
        && index.target == row.target
        && index.started_at_ms == row.started_at.0
        && index.finished_at_ms == row.finished_at.0
}

/// Whether an object whose payload says `index`, signed by `key_id`,
/// matches the listing's `filter`. SQL filtered on the index columns; a row
/// whose columns were changed behind the daemon could match there and not
/// here.
fn payload_matches(filter: &EvidenceFilter, index: &Index, key_id: &str) -> bool {
    let any =
        |wanted: &[String], value: &str| wanted.is_empty() || wanted.iter().any(|w| w == value);
    filter
        .tree_id
        .as_ref()
        .is_none_or(|tree| *tree == index.tree_id)
        && any(&filter.kinds, &index.kind)
        && any(&filter.results, index.result.as_str())
        && filter
            .target
            .as_ref()
            .is_none_or(|target| Some(target) == index.target.as_ref())
        && any(&filter.key_ids, key_id)
}

/// [`evidence_message`] of each of `found` on the blocking pool, with its
/// links and its key's identity (R3.03); `current` says, for a listing by
/// change, which trees are the change's now. With `filter`, an object whose
/// payload does not match it is left out, and logged: its index columns
/// were changed; and one whose certificate names none of the filter's
/// identities is left out too.
pub(crate) async fn messages(
    state: &AppState,
    repo: crate::model::RepoId,
    found: Vec<StoredEvidence>,
    with_envelope: bool,
    current: Option<BTreeSet<String>>,
    filter: Option<EvidenceFilter>,
) -> Result<Vec<v1::Evidence>, Status> {
    let ids: Vec<&str> = found.iter().map(|f| f.row.id.as_str()).collect();
    let mut links = state.db.links_of(repo, &ids).map_err(status)?;
    let with_links: Vec<(StoredEvidence, Vec<String>)> = found
        .into_iter()
        .map(|stored| {
            let changes = links.remove(&stored.row.id).unwrap_or_default();
            (stored, changes)
        })
        .collect();
    let key_ids: BTreeSet<&str> = with_links
        .iter()
        .map(|(stored, _)| stored.row.key_id.as_str())
        .collect();
    let key_rows = state
        .db
        .session_keys_by_id(&key_ids.into_iter().collect::<Vec<_>>())
        .map_err(status)?;
    let db = Arc::clone(&state.db);
    let mut messages: Vec<v1::Evidence> = tokio::task::spawn_blocking(move || {
        let keys: HashMap<String, ReadKey> = read_keys(&db, key_rows)
            .map_err(status)?
            .into_iter()
            .map(|read| (read.row.key_id.clone(), read))
            .collect();
        Ok::<_, Status>(
            with_links
                .into_iter()
                .filter_map(|(stored, changes)| {
                    let (mut message, index, key_id, verified_key) =
                        evidence_message(&stored, changes, with_envelope, current.as_ref());
                    // Identity only for a key whose signature verified here: a
                    // `key_id` column changed behind the daemon must not borrow
                    // another key's certificate.
                    let key = verified_key.as_ref().and_then(|id| keys.get(id));
                    message.identity = Some(match verified_key {
                        Some(_) => key_identity(key, stored.row.added_at),
                        None => unverified_identity(stored.row.added_at),
                    });
                    if let (Some(filter), Some(index)) = (&filter, &index)
                        && !payload_matches(filter, index, &key_id)
                    {
                        tracing::warn!(
                            evidence_id = %message.evidence_id,
                            "a stored evidence row's index columns differ from its payload; left out of a filtered listing"
                        );
                        return None;
                    }
                    if let Some(filter) = &filter
                        && !filter.identities.is_empty()
                        && !key
                            .and_then(ReadKey::identity_name)
                            .is_some_and(|name| filter.identities.iter().any(|n| n == name))
                    {
                        return None;
                    }
                    Some(message)
                })
                .collect(),
        )
    })
    .await
    .map_err(|error| task(&error))??;
    artifact_states(state, repo, &mut messages)?;
    Ok(messages)
}

/// Sets each artifact's state, size and collection time from the artifact
/// store's index (R3.02): one `IN` query for the page.
fn artifact_states(
    state: &AppState,
    repo: crate::model::RepoId,
    messages: &mut [v1::Evidence],
) -> Result<(), Status> {
    let digests: BTreeSet<&str> = messages
        .iter()
        .flat_map(|message| message.artifacts.iter().map(|a| a.sha256.as_str()))
        .collect();
    if digests.is_empty() {
        return Ok(());
    }
    let digests: Vec<&str> = digests.into_iter().collect();
    let states = state.db.artifact_states(repo, &digests).map_err(status)?;
    for artifact in messages.iter_mut().flat_map(|m| m.artifacts.iter_mut()) {
        if let Some(found) = states.get(&artifact.sha256) {
            artifact.size = found.size;
            match found.collected_at {
                None => artifact.set_state(v1::ArtifactState::Stored),
                Some(at) => {
                    artifact.set_state(v1::ArtifactState::Collected);
                    artifact.collected_at = Some(super::convert::timestamp(at));
                }
            }
        }
    }
    Ok(())
}

/// A full change id in hex, from hex or reverse hex.
pub(crate) fn full_change_id(text: &str) -> Result<String, Status> {
    let invalid = || {
        status(EvidenceError::InvalidChangeId {
            change: text.to_owned(),
        })
    };
    let text = text.trim();
    if text.len() != CHANGE_ID_LEN {
        return Err(invalid());
    }
    super::operations::change_prefix(text).ok_or_else(invalid)
}

fn result_filter(results: &[i32]) -> Result<Vec<String>, Status> {
    results
        .iter()
        .map(|&value| match v1::EvidenceResult::try_from(value) {
            Ok(v1::EvidenceResult::Pass) => Ok(Outcome::Pass.as_str().to_owned()),
            Ok(v1::EvidenceResult::Fail) => Ok(Outcome::Fail.as_str().to_owned()),
            Ok(v1::EvidenceResult::Error) => Ok(Outcome::Error.as_str().to_owned()),
            _ => Err(status(EvidenceError::Request(format!(
                "{value} is not an evidence result"
            )))),
        })
        .collect()
}

/// The `(added_at, id)` cursor of a page token.
fn parse_cursor(cursor: &str) -> Result<(Timestamp, String), Status> {
    let (at, id) = cursor.split_once(':').ok_or_else(bad_token)?;
    Ok((
        Timestamp(at.parse().map_err(|_| bad_token())?),
        id.to_owned(),
    ))
}

/// The tree `revision` resolves to, snapshotting first when it uses `@`
/// (unless `skip_snapshot` or `at_operation`); `None` when conflicted.
async fn revision_tree(
    state: &AppState,
    target: &Target,
    revision: &str,
    at_operation: &str,
    skip_snapshot: bool,
    agent: Option<String>,
) -> Result<Option<String>, Status> {
    if at_operation.is_empty() && !skip_snapshot {
        let needed = uses_working_copy(state, target, vec![revision.to_owned()]).await?;
        snapshot_first(state, target, needed, Instant::now(), agent).await?;
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let reads = Arc::clone(&state.reads);
    let read_target = target.clone();
    let revision = revision.to_owned();
    let at_operation = at_operation.to_owned();
    state
        .reads
        .run(&target.handle, move || async move {
            let target = read_target;
            let repo = repo_at(&reads, target.repo.id, &head, None, &at_operation).await?;
            let resolved = revset::resolve_revision(&repo, &revision, &target.scope())
                .await
                .map_err(status)?;
            let (_, tree) = files::commit_tree_id(&repo, &resolved.commit_id)
                .await
                .map_err(status)?;
            Ok::<_, Status>(tree)
        })
        .await
        .map_err(status)?
}

#[tonic::async_trait]
impl EvidenceService for EvidenceHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn add_evidence(
        &self,
        request: Request<AddEvidenceRequest>,
    ) -> Result<Response<AddEvidenceResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let remote_token_id = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let AddEvidenceRequest {
            envelope,
            public_key,
            revision,
            ..
        } = request;
        let verified = tokio::task::spawn_blocking(move || Verified::check(&envelope, &public_key))
            .await
            .map_err(|error| task(&error))?
            .map_err(|invalid| status(EvidenceError::from(invalid)))?;
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let tree = verified.index.tree_id.clone();
        let read_target = target.clone();
        let link = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                match files::tree_presence(&head, &tree).await.map_err(status)? {
                    TreePresence::Present => {}
                    presence => {
                        return Err(status(EvidenceError::TreeNotFound {
                            tree_id: tree,
                            not_a_tree: presence == TreePresence::NotATree,
                        }));
                    }
                }
                if revision.trim().is_empty() {
                    return Ok(None);
                }
                let resolved = revset::resolve_revision(&head, &revision, &target.scope())
                    .await
                    .map_err(status)?;
                let commit_id = resolved.commit_id.to_string();
                let (change, commit_tree) = files::commit_tree_id(&head, &resolved.commit_id)
                    .await
                    .map_err(status)?;
                match commit_tree {
                    None => Err(status(EvidenceError::Conflicted { commit_id })),
                    Some(commit_tree_id) if commit_tree_id != tree => {
                        Err(status(EvidenceError::TreeMismatch {
                            tree_id: tree,
                            commit_id,
                            commit_tree_id,
                        }))
                    }
                    Some(_) => Ok(Some((change.to_string(), commit_id))),
                }
            })
            .await
            .map_err(status)??;
        let now = Timestamp::now();
        let repo_id = target.repo.id;
        let (key, row, digests) = evidence_rows(&verified, repo_id, agent, remote_token_id, now);
        let link = link.map(|(change_id, commit_id)| EvidenceLinkRow {
            change_id,
            evidence_id: verified.id.clone(),
            commit_id,
            linked_at: now,
        });
        // One transaction writes and reads it back: a GC deleting old
        // evidence meanwhile either finds this row or never sees it.
        let (inserted, stored) = state
            .db
            .add_evidence(&key, &row, link.as_ref(), &digests)
            .map_err(status)?;
        tracing::info!(
            evidence_id = %verified.id,
            key_id = %key.id,
            created = inserted.created,
            artifact_count = digests.len(),
            "evidence added"
        );
        let mut evidence = messages(state, repo_id, vec![stored], false, None, None).await?;
        Ok(Response::new(AddEvidenceResponse {
            evidence: evidence.pop(),
            created: inserted.created,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_evidence(
        &self,
        request: Request<GetEvidenceRequest>,
    ) -> Result<Response<GetEvidenceResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let repo = state.manager.resolve_repo(&request.repo).map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(repo.id));
        let text = request.evidence_id.trim();
        let valid = (MIN_PREFIX_LEN..=EVIDENCE_ID_LEN).contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(status(EvidenceError::InvalidId {
                evidence_id: request.evidence_id.clone(),
            }));
        }
        let prefix = text.to_ascii_lowercase();
        let stored = match state.db.evidence_by_prefix(repo.id, &prefix) {
            Ok(stored) => stored,
            Err(DbError::NotFound { .. }) => {
                return Err(status(EvidenceError::NotFound {
                    evidence_id: prefix,
                }));
            }
            Err(DbError::Ambiguous { candidates, .. }) => {
                return Err(status(EvidenceError::Ambiguous {
                    evidence_id: prefix,
                    candidates,
                }));
            }
            Err(error) => return Err(status(error)),
        };
        let mut evidence = messages(state, repo.id, vec![stored], true, None, None).await?;
        Ok(Response::new(GetEvidenceResponse {
            evidence: evidence.pop(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_evidence(
        &self,
        request: Request<ListEvidenceRequest>,
    ) -> Result<Response<ListEvidenceResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_EVIDENCE,
            request.page_size,
            &request.page_token,
            &ListEvidenceRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let size = if request.page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page.size().min(MAX_PAGE_SIZE)
        };
        let revision = request.revision.trim();
        let tree_id = request.tree_id.trim();
        if !revision.is_empty() && !tree_id.is_empty() {
            return Err(status(EvidenceError::Request(
                "set revision or tree_id, not both".into(),
            )));
        }
        let results = result_filter(&request.results)?;
        if let Some(key_id) = request
            .key_ids
            .iter()
            .find(|key_id| !is_lower_hex(key_id, reef_evidence::keys::KEY_ID_LEN))
        {
            return Err(status(EvidenceError::InvalidKeyId {
                key_id: key_id.clone(),
            }));
        }
        let change_id = match request.change_id.trim() {
            "" => None,
            text => Some(full_change_id(text)?),
        };
        let cursor = page.cursor().map(parse_cursor).transpose()?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let repo_id = target.repo.id;
        // The tree to list: the token's, the revision's, or the request's.
        let mut pinned = String::new();
        let tree = if let Some(tree) = page.pinned() {
            pinned = tree.to_owned();
            Some(tree.to_owned())
        } else if !revision.is_empty() {
            match revision_tree(
                state,
                &target,
                revision,
                &request.at_operation,
                request.skip_snapshot,
                agent,
            )
            .await?
            {
                Some(tree) => {
                    pinned.clone_from(&tree);
                    Some(tree)
                }
                // A conflicted tree has no evidence.
                None => {
                    return Ok(Response::new(ListEvidenceResponse::default()));
                }
            }
        } else if tree_id.is_empty() {
            None
        } else {
            let head = state.reads.loaded(&target.handle).await.map_err(status)?;
            let len = files::tree_id_hex_len(&head);
            if !is_lower_hex(tree_id, len) {
                return Err(status(EvidenceError::InvalidTreeId {
                    tree_id: tree_id.to_owned(),
                    len,
                }));
            }
            Some(tree_id.to_owned())
        };
        // The change's trees now, for `current`.
        let current = match &change_id {
            None => None,
            Some(change) => {
                let head = state.reads.current(&target.handle).await.map_err(status)?;
                let reads = Arc::clone(&state.reads);
                let change = change.clone();
                let at_operation = request.at_operation.clone();
                let trees = state
                    .reads
                    .run(&target.handle, move || async move {
                        let repo = repo_at(&reads, repo_id, &head, None, &at_operation).await?;
                        files::change_tree_ids(&repo, &change).await.map_err(status)
                    })
                    .await
                    .map_err(status)??;
                Some(trees.into_iter().flatten().collect::<BTreeSet<String>>())
            }
        };
        let filter = EvidenceFilter {
            tree_id: tree,
            change_id,
            kinds: request.kinds.clone(),
            results,
            target: Some(request.target.clone()).filter(|target| !target.is_empty()),
            key_ids: request.key_ids.clone(),
            identities: request
                .identities
                .iter()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect(),
            job_id: if request.job_id.trim().is_empty() {
                None
            } else {
                Some(crate::executor::full_id(&request.job_id).map_err(status)?)
            },
        };
        let mut found = state
            .db
            .list_evidence(
                repo_id,
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = found.len() > size;
        found.truncate(size);
        let next_page_token = match found.last() {
            Some(last) if more => {
                page.token_at(&format!("{}:{}", last.row.added_at.0, last.row.id), &pinned)
            }
            _ => String::new(),
        };
        let evidence = messages(state, repo_id, found, false, current, Some(filter)).await?;
        Ok(Response::new(ListEvidenceResponse {
            evidence,
            next_page_token,
        }))
    }
}

/// Whether `text` is `len` lowercase hex digits.
fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
