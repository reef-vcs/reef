//! `reef.api.v1.PolicyService` (R3.07, ADR 0061): evaluates a repository's
//! `reef-policy.toml` over one change.
//!
//! The input is built here, from the change's commit and diff, the
//! evidence on its tree whose signature verifies at this read (with each
//! signer's identity at witness time, and whether a job accepted it fresh),
//! and every provenance record of its change. `reef_policy` does the rest.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::policy_service_server::PolicyService;
use reef_api::v1::{self, EvaluatePolicyRequest, EvaluatePolicyResponse};
use reef_evidence::{Envelope, Statement};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::files;
use reef_lib::jj::log::Describer;
use reef_lib::jj::revset;
use reef_policy::input::{self, Author, Change, File, Input, ProposedPolicy};
use reef_policy::{Decision, Evaluation, RuleKind, Status as RuleStatus};
use tonic::{Request, Response, Status};

use super::reading::{op_id, repo_at, snapshot_first, target, uses_working_copy};
use crate::daemon::AppState;
use crate::db::{EvidenceFilter, ProvenanceFilter};
use crate::error::{Advice, Advise, status};
use crate::model::Timestamp;
use crate::policy::Loaded;

/// The most evidence rows on one tree an evaluation reads, newest first;
/// the rest are counted in `evidence_ignored` (S-94).
pub const MAX_EVIDENCE_READ: usize = 20_000;

/// The most provenance records of one change an evaluation reads.
pub const MAX_PROVENANCE_READ: usize = 10_000;

/// Rows per database page while reading.
const PAGE: usize = 1_000;

/// The largest file whose lines the diff counts (`too_large` past it).
const MAX_FILE_BYTES: usize = 16 << 20;

/// Why an evaluation could not be asked for.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// Both `policy` text and a `policy_revision`.
    #[error("set `policy` or `policy_revision`, not both")]
    SourceConflict,
    /// `policy` text over the file limit.
    #[error("the policy text is {bytes} bytes; a policy is at most {max}")]
    TooLarge {
        /// Its size.
        bytes: usize,
        /// The limit.
        max: usize,
    },
    /// A blocking task failed.
    #[error("policy evaluation failed: {0}")]
    Task(String),
}

impl Classify for PolicyError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::SourceConflict | Self::TooLarge { .. } => ErrorKind::InvalidArgument,
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for PolicyError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::SourceConflict => Some(Advice::hint(
                "Evaluate a policy file's text with `policy`, or the reef-policy.toml at a revision with `policy_revision`, not both.",
            )),
            Self::TooLarge { .. } => Some(Advice::hint(
                "A policy is at most 256 KiB; `reef policy check` reports what a file holds.",
            )),
            Self::Task(_) => None,
        }
    }
}

impl PolicyError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::SourceConflict => (reason::POLICY_SOURCE_CONFLICT, Vec::new()),
            Self::TooLarge { bytes, max } => (
                reason::POLICY_TOO_LARGE,
                vec![("bytes", bytes.to_string()), ("max", max.to_string())],
            ),
            Self::Task(_) => return None,
        })
    }
}

fn task(error: &tokio::task::JoinError) -> Status {
    status(PolicyError::Task(error.to_string()))
}

/// Serves `PolicyService`.
#[derive(Debug)]
pub struct PolicyHandler {
    state: Arc<AppState>,
}

impl PolicyHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> PolicyHandler {
        PolicyHandler { state }
    }
}

/// A policy file read from a tree.
struct PolicyFile {
    blob_id: String,
    bytes: Result<Vec<u8>, String>,
}

/// What the repository thread reads.
struct Read {
    change: Change,
    tree_id: Option<String>,
    proposed: Option<PolicyFile>,
    source_commit: String,
    source: Option<PolicyFile>,
    at_operation: String,
}

/// `reef-policy.toml` in `tree`, if it has one.
async fn policy_file(tree: &reef_lib::jj::MergedTree) -> Result<Option<PolicyFile>, Status> {
    let Some(file) = files::tree_file(tree, reef_policy::POLICY_PATH)
        .await
        .map_err(status)?
    else {
        return Ok(None);
    };
    let blob_id = hex(&file.file_id);
    if file.file_id.is_empty() {
        return Ok(Some(PolicyFile {
            blob_id,
            bytes: Err(format!(
                "{} is not a regular file ({:?})",
                reef_policy::POLICY_PATH,
                file.kind
            )),
        }));
    }
    let max = reef_policy::format::MAX_FILE_BYTES as u64;
    let bytes = files::read_file_id(tree, reef_policy::POLICY_PATH, &file.file_id, max)
        .await
        .map_err(status)?;
    Ok(Some(PolicyFile {
        blob_id,
        bytes: bytes.map(|bytes| bytes.to_vec()).ok_or_else(|| {
            format!(
                "{} is over {max} bytes; a policy is at most that",
                reef_policy::POLICY_PATH
            )
        }),
    }))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn millis(at: Option<&prost_types::Timestamp>) -> i64 {
    at.map_or(0, |at| {
        at.seconds.saturating_mul(1000) + i64::from(at.nanos) / 1_000_000
    })
}

/// A prost enum's proto name without its prefix, lowercased:
/// `IDENTITY_STATUS_BOUND` is `bound`.
fn word(name: &str, prefix: &str) -> String {
    let rest = name.strip_prefix(prefix).unwrap_or(name);
    if rest == "UNSPECIFIED" {
        String::new()
    } else {
        rest.to_ascii_lowercase()
    }
}

fn key_of(identity: Option<&v1::KeyIdentity>, key_id: &str) -> input::Key {
    let identity = identity.cloned().unwrap_or_default();
    let key = identity.key.unwrap_or_default();
    input::Key {
        id: if key.key_id.is_empty() {
            key_id.to_owned()
        } else {
            key.key_id
        },
        status: word(
            v1::IdentityStatus::try_from(identity.status)
                .unwrap_or_default()
                .as_str_name(),
            "IDENTITY_STATUS_",
        ),
        identity: key.identity,
        identity_id: key.identity_id,
        identity_kind: word(
            v1::IdentityKind::try_from(key.identity_kind)
                .unwrap_or_default()
                .as_str_name(),
            "IDENTITY_KIND_",
        ),
        actor: word(
            v1::SessionActor::try_from(key.actor)
                .unwrap_or_default()
                .as_str_name(),
            "SESSION_ACTOR_",
        ),
        agent: key.agent,
        session: key.session_id,
        presence: word(
            v1::SessionPresence::try_from(key.presence)
                .unwrap_or_default()
                .as_str_name(),
            "SESSION_PRESENCE_",
        ),
        custody: word(
            v1::KeyCustody::try_from(key.custody)
                .unwrap_or_default()
                .as_str_name(),
            "KEY_CUSTODY_",
        ),
        authority: key.authority_key_id,
    }
}

/// The most bytes of annotations, as JSON, an object brings into the
/// input; larger ones are left out (`{}`), so a condition over them fails
/// and the evaluation blocks.
pub const MAX_ANNOTATION_BYTES: usize = 16 * 1024;

/// The annotations of a verified envelope's payload, when within
/// [`MAX_ANNOTATION_BYTES`].
fn annotations(envelope: &[u8]) -> BTreeMap<String, serde_json::Value> {
    let found = all_annotations(envelope);
    let bytes = serde_json::to_vec(&found).map_or(usize::MAX, |json| json.len());
    if bytes > MAX_ANNOTATION_BYTES {
        BTreeMap::new()
    } else {
        found
    }
}

fn all_annotations(envelope: &[u8]) -> BTreeMap<String, serde_json::Value> {
    let statement: Option<Statement> = serde_json::from_slice::<Envelope>(envelope)
        .ok()
        .and_then(|envelope| envelope.payload_bytes())
        .and_then(|payload| serde_json::from_slice(&payload).ok());
    statement
        .and_then(|statement| statement.predicate.annotations)
        .map(|map| map.into_iter().collect())
        .unwrap_or_default()
}

impl PolicyHandler {
    /// The evidence on `tree` whose signature verifies, and how many rows
    /// were left out.
    async fn evidence(
        &self,
        repo: crate::model::RepoId,
        tree: &str,
    ) -> Result<(Vec<input::Evidence>, i64), Status> {
        let state = &self.state;
        let filter = EvidenceFilter {
            tree_id: Some(tree.to_owned()),
            ..EvidenceFilter::default()
        };
        let mut cursor: Option<(Timestamp, String)> = None;
        let mut ignored = 0i64;
        let mut read = 0usize;
        let mut evidence = Vec::new();
        // Page by page, so no more than a page of envelopes is in memory at
        // once (review finding 6).
        loop {
            let page = state
                .db
                .list_evidence(
                    repo,
                    &filter,
                    cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                    PAGE,
                )
                .map_err(status)?;
            let full = page.len() == PAGE;
            if let Some(last) = page.last() {
                cursor = Some((last.row.added_at, last.row.id.clone()));
            }
            let room = MAX_EVIDENCE_READ - read;
            let mut rows = page;
            if rows.len() > room {
                ignored += i64::try_from(rows.len() - room).unwrap_or(i64::MAX);
                rows.truncate(room);
            }
            read += rows.len();
            if !rows.is_empty() {
                let messages =
                    super::evidence::messages(state, repo, rows, true, None, None).await?;
                let (page, invalid) = self.convert(repo, &messages)?;
                evidence.extend(page);
                ignored += invalid;
            }
            if !full {
                break;
            }
        }
        Ok((evidence, ignored))
    }

    /// Evidence messages as the policy's input, and how many were left out
    /// for a signature that does not verify.
    fn convert(
        &self,
        repo: crate::model::RepoId,
        messages: &[v1::Evidence],
    ) -> Result<(Vec<input::Evidence>, i64), Status> {
        let ids: Vec<&str> = messages.iter().map(|m| m.evidence_id.as_str()).collect();
        let jobs = self.state.db.evidence_jobs(repo, &ids).map_err(status)?;
        let mut ignored = 0i64;
        let mut evidence = Vec::with_capacity(messages.len());
        for message in messages {
            if message.signature != v1::SignatureCheck::Valid as i32 {
                ignored += 1;
                continue;
            }
            let (job, fresh) = jobs.get(&message.evidence_id).cloned().unwrap_or_default();
            evidence.push(input::Evidence {
                id: message.evidence_id.clone(),
                kind: message.kind.clone(),
                result: word(
                    v1::EvidenceResult::try_from(message.result)
                        .unwrap_or_default()
                        .as_str_name(),
                    "EVIDENCE_RESULT_",
                ),
                target: message.target.clone(),
                started_at: millis(message.started_at.as_ref()),
                finished_at: millis(message.finished_at.as_ref()),
                added_at: millis(message.added_at.as_ref()),
                added_by_agent: message.added_by_agent.clone(),
                artifacts: message
                    .artifacts
                    .iter()
                    .map(|artifact| input::Artifact {
                        name: artifact.name.clone(),
                        sha256: artifact.sha256.clone(),
                        media_type: artifact.media_type.clone(),
                        state: word(
                            v1::ArtifactState::try_from(artifact.state)
                                .unwrap_or_default()
                                .as_str_name(),
                            "ARTIFACT_STATE_",
                        ),
                        size: i64::try_from(artifact.size).unwrap_or(i64::MAX),
                    })
                    .collect(),
                annotations: annotations(&message.envelope),
                key: key_of(message.identity.as_ref(), &message.key_id),
                fresh,
                job,
                from_self: false,
                attestors: Vec::new(),
            });
        }
        Ok((evidence, ignored))
    }

    /// Every provenance record of `change`, checked again at this read.
    async fn provenance(
        &self,
        repo: crate::model::RepoId,
        change: &str,
    ) -> Result<(Vec<input::Provenance>, i64), Status> {
        let state = &self.state;
        let filter = ProvenanceFilter {
            change_ids: vec![change.to_owned()],
            ..ProvenanceFilter::default()
        };
        let mut rows = Vec::new();
        let mut cursor: Option<(Timestamp, String)> = None;
        loop {
            let page = state
                .db
                .list_provenance(
                    repo,
                    &filter,
                    cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                    PAGE,
                )
                .map_err(status)?;
            let full = page.len() == PAGE;
            if let Some(last) = page.last() {
                cursor = Some((last.row.recorded_at, last.row.id.clone()));
            }
            rows.extend(page);
            if !full || rows.len() > MAX_PROVENANCE_READ {
                break;
            }
        }
        // At least this many were not read: `builtin:input` blocks on them.
        let unread = rows.len().saturating_sub(MAX_PROVENANCE_READ);
        rows.truncate(MAX_PROVENANCE_READ);
        let db = Arc::clone(&state.db);
        let records = tokio::task::spawn_blocking(move || {
            crate::provenance::read::read_records(&db, rows, false)
        })
        .await
        .map_err(|error| task(&error))?
        .map_err(status)?;
        let records = records
            .into_iter()
            .map(|record| {
                let context = record.context.clone().unwrap_or_default();
                input::Provenance {
                    id: record.id.clone(),
                    commit: record.commit_id.clone(),
                    tree: record.tree_id.clone(),
                    workspace: record.workspace_id.clone(),
                    session: record.session_id.clone(),
                    sequence: i64::try_from(record.sequence).unwrap_or(i64::MAX),
                    kind: record.kind.clone(),
                    agent: record.agent.clone(),
                    recorded_at: millis(record.recorded_at.as_ref()),
                    model: context.model,
                    harness: context.harness,
                    task: context.task,
                    instruction_sha256: context.instruction_sha256,
                    client_session: context.client_session,
                    signature: word(
                        v1::SignatureCheck::try_from(record.signature)
                            .unwrap_or_default()
                            .as_str_name(),
                        "SIGNATURE_CHECK_",
                    ),
                    key: key_of(record.identity.as_ref(), &record.key_id),
                }
            })
            .collect();
        Ok((records, i64::try_from(unread).unwrap_or(i64::MAX)))
    }
}

fn file_status(kind: Option<diff::ChangeKind>) -> &'static str {
    match kind {
        Some(diff::ChangeKind::Added) => "added",
        Some(diff::ChangeKind::Deleted) => "deleted",
        Some(diff::ChangeKind::Renamed) => "renamed",
        Some(diff::ChangeKind::Copied) => "copied",
        _ => "modified",
    }
}

fn source_message(
    revision: &str,
    commit: &str,
    file: Option<&PolicyFile>,
    from_request: Option<&[u8]>,
    evaluation: &Evaluation,
) -> v1::PolicySource {
    match from_request {
        Some(_) => v1::PolicySource {
            sha256: evaluation.sha256.clone(),
            version: evaluation.version,
            from_request: true,
            ..v1::PolicySource::default()
        },
        None => v1::PolicySource {
            revision: revision.to_owned(),
            commit_id: commit.to_owned(),
            path: reef_policy::POLICY_PATH.to_owned(),
            blob_id: file.map(|file| file.blob_id.clone()).unwrap_or_default(),
            sha256: evaluation.sha256.clone(),
            version: evaluation.version,
            from_request: false,
        },
    }
}

/// `evaluation` as the API's message.
pub(crate) fn evaluation_message(evaluation: &Evaluation) -> v1::PolicyEvaluation {
    let need = |need: &reef_policy::Need| v1::PolicyEvidenceNeed {
        kind: need.kind.clone(),
        target: need.target.clone(),
        attestors: need.attestors.clone(),
        evidence_ids: need.evidence_ids.clone(),
    };
    let count = |n: i64| u32::try_from(n.max(0)).unwrap_or(u32::MAX);
    v1::PolicyEvaluation {
        decision: match evaluation.decision {
            Decision::Allow => v1::PolicyDecision::Allow,
            Decision::Block => v1::PolicyDecision::Block,
            Decision::NoPolicy => v1::PolicyDecision::NoPolicy,
            Decision::InvalidPolicy => v1::PolicyDecision::InvalidPolicy,
        }
        .into(),
        diagnostics: evaluation
            .diagnostics
            .iter()
            .map(|d| v1::PolicyDiagnostic {
                rule: d.rule.clone(),
                field: d.field.clone(),
                line: d.line,
                column: d.column,
                message: d.message.clone(),
            })
            .collect(),
        rules: evaluation
            .rules
            .iter()
            .map(|rule| v1::PolicyRuleResult {
                name: rule.name.clone(),
                kind: match rule.kind {
                    RuleKind::Builtin => v1::PolicyRuleKind::Builtin,
                    RuleKind::Require => v1::PolicyRuleKind::Require,
                    RuleKind::Limits => v1::PolicyRuleKind::Limits,
                    RuleKind::Forbid => v1::PolicyRuleKind::Forbid,
                    RuleKind::Signoff => v1::PolicyRuleKind::Signoff,
                    RuleKind::Rule => v1::PolicyRuleKind::Rule,
                }
                .into(),
                status: match rule.status {
                    RuleStatus::Satisfied => v1::PolicyRuleStatus::Satisfied,
                    RuleStatus::Unsatisfied => v1::PolicyRuleStatus::Unsatisfied,
                    RuleStatus::NotApplicable => v1::PolicyRuleStatus::NotApplicable,
                    RuleStatus::Error => v1::PolicyRuleStatus::Error,
                }
                .into(),
                message: rule.message.clone(),
                paths: rule.paths.clone(),
                paths_total: count(rule.paths_total),
                missing: rule.missing.iter().map(need).collect(),
                failed: rule.failed.iter().map(need).collect(),
                signoff: rule.signoff.as_ref().map(|s| v1::PolicySignoff {
                    groups: s.groups.clone(),
                    identities: s.identities.clone(),
                    count: count(s.count),
                    have: count(s.have),
                    signed_by: s.signed_by.clone(),
                    presence: s.presence.clone(),
                    kinds: s.kinds.clone(),
                }),
                evidence_ids: rule.evidence_ids.clone(),
            })
            .collect(),
        input: Some(v1::PolicyInputSummary {
            paths: count(evaluation.input.paths),
            lines_added: u64::try_from(evaluation.input.lines_added.max(0)).unwrap_or(0),
            lines_removed: u64::try_from(evaluation.input.lines_removed.max(0)).unwrap_or(0),
            evidence: count(evaluation.input.evidence),
            evidence_ignored: count(evaluation.input.evidence_ignored),
            provenance: count(evaluation.input.provenance),
            conflicted: evaluation.input.conflicted,
        }),
        cost: evaluation.cost,
        ..v1::PolicyEvaluation::default()
    }
}

/// `bytes` evaluated over `input` through the daemon's cache.
fn evaluate(state: &AppState, bytes: Option<&[u8]>, input: &Input) -> Evaluation {
    match bytes {
        None => Evaluation::no_policy(input),
        Some(bytes) => match &*state.policy.load(bytes) {
            Loaded::Policy(policy) => policy.evaluate(input),
            Loaded::Invalid(diagnostics) => Evaluation::invalid(bytes, diagnostics.clone(), input),
        },
    }
}

#[tonic::async_trait]
impl PolicyService for PolicyHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    #[allow(clippy::too_many_lines)]
    async fn evaluate_policy(
        &self,
        request: Request<EvaluatePolicyRequest>,
    ) -> Result<Response<EvaluatePolicyResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        if !request.policy.is_empty() && !request.policy_revision.trim().is_empty() {
            return Err(status(PolicyError::SourceConflict));
        }
        let max = reef_policy::format::MAX_FILE_BYTES;
        if request.policy.len() > max {
            return Err(status(PolicyError::TooLarge {
                bytes: request.policy.len(),
                max,
            }));
        }
        let state = &self.state;
        let _turn = state
            .policy
            .turns
            .acquire()
            .await
            .map_err(|error| status(PolicyError::Task(error.to_string())))?;
        let target = target(state, &request.repo, &request.workspace)?;
        let revision = match request.revision.trim() {
            "" if target.workspace.is_some() => "@".to_owned(),
            text => text.to_owned(),
        };
        let policy_revision = match request.policy_revision.trim() {
            "" => "trunk()".to_owned(),
            text => text.to_owned(),
        };
        let from_request = !request.policy.is_empty();
        if request.at_operation.is_empty() && !request.skip_snapshot {
            let mut texts = vec![revision.clone()];
            if !from_request {
                texts.push(policy_revision.clone());
            }
            let needed = uses_working_copy(state, &target, texts).await?;
            snapshot_first(state, &target, needed, Instant::now(), agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let read_target = target.clone();
        let at_operation = request.at_operation.clone();
        let wanted_revision = revision.clone();
        let wanted_policy = policy_revision.clone();
        let read = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let scope = target.scope();
                let repo = repo_at(&reads, target.repo.id, &head, None, &at_operation).await?;
                let resolved = revset::resolve_revision(&repo, &wanted_revision, &scope)
                    .await
                    .map_err(status)?;
                let info = Describer::new(&repo, &scope)
                    .map_err(status)?
                    .describe(&resolved.commit_id)
                    .await
                    .map_err(status)?;
                // The diff against the merged parents, every page, with sizes.
                let mut files = Vec::new();
                let mut after = None;
                loop {
                    // One page of everything, up to one past the cap:
                    // `diff_page` streams the whole diff on every call, so
                    // paging would be quadratic (review finding 8).
                    let options = DiffOptions {
                        format: DiffFormat::Stat,
                        context: 0,
                        max_lines: usize::MAX,
                        max_hunk_bytes: usize::MAX,
                        max_file_bytes: MAX_FILE_BYTES,
                        paths: Vec::new(),
                        after,
                        limit: reef_policy::MAX_PATHS + 1,
                        pairs: false,
                        sizes: true,
                    };
                    let page = diff::diff_page(
                        &repo,
                        &DiffBase::Parents,
                        &resolved.commit_id,
                        &options,
                        &|| false,
                    )
                    .await
                    .map_err(status)?;
                    after = page
                        .files
                        .last()
                        .map(|file| files::check_path(&file.path, false))
                        .transpose()
                        .map_err(status)?;
                    let more = page.more;
                    files.extend(page.files);
                    if !more || files.len() > reef_policy::MAX_PATHS {
                        break;
                    }
                }
                let commit_tree = files::commit_tree(&repo, &resolved.commit_id)
                    .await
                    .map_err(status)?;
                // The evaluator matches the name ASCII case-insensitively;
                // so does this (review finding 11).
                let touches_policy = files.iter().any(|file| {
                    file.path.eq_ignore_ascii_case(reef_policy::POLICY_PATH)
                        || file
                            .old_path
                            .as_deref()
                            .is_some_and(|old| old.eq_ignore_ascii_case(reef_policy::POLICY_PATH))
                });
                let proposed = if touches_policy {
                    policy_file(&commit_tree).await?
                } else {
                    None
                };
                let (source_commit, source) = if from_request {
                    (String::new(), None)
                } else {
                    let resolved = revset::resolve_revision(&repo, &wanted_policy, &scope)
                        .await
                        .map_err(status)?;
                    let tree = files::commit_tree(&repo, &resolved.commit_id)
                        .await
                        .map_err(status)?;
                    (
                        resolved.commit_id.as_str().to_owned(),
                        policy_file(&tree).await?,
                    )
                };
                let mut paths: Vec<String> = files
                    .iter()
                    .flat_map(|file| {
                        std::iter::once(file.path.clone()).chain(file.old_path.clone())
                    })
                    .collect();
                paths.sort();
                paths.dedup();
                let lines_added = files.iter().map(|f| i64::from(f.lines_added)).sum();
                let lines_removed = files.iter().map(|f| i64::from(f.lines_removed)).sum();
                let change = Change {
                    id: info.change_id.as_str().to_owned(),
                    commit: info.commit_id.as_str().to_owned(),
                    tree: info.tree_id.clone().unwrap_or_default(),
                    parents: info
                        .parent_commit_ids
                        .iter()
                        .map(|id| id.as_str().to_owned())
                        .collect(),
                    description: info.description.clone(),
                    author: Author {
                        name: info.author.name.clone(),
                        email: info.author.email.clone(),
                    },
                    empty: info.empty,
                    conflicted: info.conflict,
                    paths,
                    files: files
                        .iter()
                        .map(|file| File {
                            path: file.path.clone(),
                            old_path: file.old_path.clone().unwrap_or_default(),
                            status: file_status(file.kind).to_owned(),
                            binary: file.binary,
                            too_large: file.too_large,
                            lines_added: if file.binary || file.too_large {
                                0
                            } else {
                                i64::from(file.lines_added)
                            },
                            lines_removed: if file.binary || file.too_large {
                                0
                            } else {
                                i64::from(file.lines_removed)
                            },
                            size: i64::try_from(file.new_size).unwrap_or(i64::MAX),
                        })
                        .collect(),
                    lines_added,
                    lines_removed,
                };
                Ok::<_, Status>(Read {
                    change,
                    tree_id: info.tree_id,
                    proposed,
                    source_commit,
                    source,
                    at_operation: op_id(&repo),
                })
            })
            .await
            .map_err(status)??;
        let repo_id = target.repo.id;
        let (evidence, ignored) = match &read.tree_id {
            Some(tree) => self.evidence(repo_id, tree).await?,
            None => (Vec::new(), 0),
        };
        let (provenance, provenance_ignored) = self.provenance(repo_id, &read.change.id).await?;
        let state = Arc::clone(&self.state);
        let request_policy = request.policy.clone();
        let include_input = request.include_input;
        let Read {
            change,
            tree_id,
            proposed,
            source_commit,
            source,
            at_operation,
        } = read;
        let evaluation = tokio::task::spawn_blocking(move || {
            let proposed_policy = match &proposed {
                None => ProposedPolicy::default(),
                Some(file) => match &file.bytes {
                    Ok(bytes) => ProposedPolicy {
                        present: true,
                        sha256: reef_policy::sha256_hex(bytes),
                        diagnostics: match &*state.policy.load(bytes) {
                            Loaded::Policy(_) => Vec::new(),
                            Loaded::Invalid(diagnostics) => diagnostics.clone(),
                        },
                    },
                    Err(problem) => ProposedPolicy {
                        present: true,
                        sha256: String::new(),
                        diagnostics: vec![reef_policy::Diagnostic {
                            message: problem.clone(),
                            ..reef_policy::Diagnostic::default()
                        }],
                    },
                },
            };
            let input = Input {
                change,
                evidence,
                provenance,
                evidence_ignored: ignored,
                provenance_ignored,
                proposed_policy,
            };
            let (bytes, problem): (Option<&[u8]>, Option<String>) = if from_request {
                (Some(request_policy.as_slice()), None)
            } else {
                match &source {
                    None => (None, None),
                    Some(file) => match &file.bytes {
                        Ok(bytes) => (Some(bytes.as_slice()), None),
                        Err(problem) => (None, Some(problem.clone())),
                    },
                }
            };
            let mut evaluation = match problem {
                Some(problem) => Evaluation::invalid(
                    &[],
                    vec![reef_policy::Diagnostic {
                        message: problem,
                        ..reef_policy::Diagnostic::default()
                    }],
                    &input,
                ),
                None => evaluate(&state, bytes, &input),
            };
            if evaluation.decision == Decision::InvalidPolicy
                && source.as_ref().is_some_and(|f| f.bytes.is_err())
            {
                evaluation.sha256.clear();
            }
            let mut message = evaluation_message(&evaluation);
            message.source = Some(source_message(
                &policy_revision,
                &source_commit,
                source.as_ref(),
                from_request.then_some(request_policy.as_slice()),
                &evaluation,
            ));
            input.change.id.clone_into(&mut message.change_id);
            input.change.commit.clone_into(&mut message.commit_id);
            message.tree_id = tree_id.unwrap_or_default();
            message.at_operation = at_operation;
            if include_input {
                message.input_json = serde_json::to_string(&input).unwrap_or_default();
            }
            message
        })
        .await
        .map_err(|error| task(&error))?;
        Ok(Response::new(EvaluatePolicyResponse {
            evaluation: Some(evaluation),
        }))
    }
}
