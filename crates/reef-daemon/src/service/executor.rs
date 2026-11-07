//! `reef.api.v1.ExecutorService` (R3.05, ADR 0059): the executor
//! protocol's seven RPCs over [`crate::executor::Executor`].
//!
//! A submission resolves its tree here, on the read pool, then queues the
//! job. An update or completion checks the cheap things first (the id, the
//! bounds, the job's existence and owner), verifies the call's envelopes on
//! the blocking pool with R3.01's checks and binds them to the job, and only
//! then takes the one transaction that stores them. Nothing here records an
//! operation (ADR 0029 decision 12, as ADR 0059 amends it).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reef_api::v1::executor_service_server::ExecutorService;
use reef_api::v1::{
    self, CancelJobRequest, CancelJobResponse, ClaimJobRequest, ClaimJobResponse,
    CompleteJobRequest, CompleteJobResponse, GetJobRequest, GetJobResponse, ListJobsRequest,
    ListJobsResponse, SubmitJobRequest, SubmitJobResponse, UpdateJobRequest, UpdateJobResponse,
    submit_job_request,
};
use reef_evidence::Verified;
use reef_lib::jj::files::{self, TreePresence};
use reef_lib::jj::revset;
use tonic::{Request, Response, Status};

use super::convert::timestamp;
use super::evidence::{EvidenceError, full_change_id, messages, result_message};
use super::page::PageRequest;
use super::reading::{bad_token, snapshot_first, target, uses_working_copy};
use crate::daemon::AppState;
use crate::db::{ExecutorColumns, JobCursor, JobEvidenceRow, JobFilter, JobRow, JobState};
use crate::error::status;
use crate::executor::accept::{Call, Incoming, bind};
use crate::executor::submit::{MAX_EXECUTOR_KINDS, MAX_LABELS, kinds, labels};
use crate::executor::{
    Claimant, Completion, DEFAULT_LEASE, ExecutorError, MAX_EVIDENCE_PER_CALL, MAX_LEASE, MAX_WAIT,
    MIN_LEASE, NewJob, Update, full_id, id_prefix,
};
use crate::model::Timestamp;

const LIST_JOBS: &str = "/reef.api.v1.ExecutorService/ListJobs";

/// The longest cancel reason.
const MAX_REASON_BYTES: usize = 256;
/// The longest completion message.
const MAX_MESSAGE_BYTES: usize = 1024;
/// The longest progress phase.
const MAX_PHASE_BYTES: usize = 64;
/// The longest progress unit.
const MAX_UNIT_BYTES: usize = 32;
/// The longest executor version.
const MAX_VERSION_BYTES: usize = 64;

/// Serves `ExecutorService`.
#[derive(Debug)]
pub struct ExecutorHandler {
    state: Arc<AppState>,
}

impl ExecutorHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> ExecutorHandler {
        ExecutorHandler { state }
    }
}

/// A request duration: `None` when unset or zero, `Err` when negative.
fn duration(value: Option<&prost_types::Duration>) -> Result<Option<Duration>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let secs = u64::try_from(value.seconds).map_err(|_| ())?;
    let nanos = u32::try_from(value.nanos).map_err(|_| ())?;
    Ok(Some(Duration::new(secs, nanos)).filter(|d| !d.is_zero()))
}

fn spec_duration(
    field: &'static str,
    value: Option<&prost_types::Duration>,
) -> Result<Option<Duration>, Status> {
    duration(value).map_err(|()| {
        status(ExecutorError::InvalidSpec {
            field,
            message: "a duration is not negative".to_owned(),
        })
    })
}

/// Whether `text` has a control character other than a newline: it would
/// reach a requester's terminal (`reef job show`).
fn has_control(text: &str) -> bool {
    text.chars().any(|c| c.is_control() && c != '\n')
}

fn visible(text: &str, max: usize) -> bool {
    text.len() <= max && text.bytes().all(|b| b.is_ascii_graphic())
}

/// A job as the API reports it. `identities` names linked identities by
/// id; `count` is how many evidence objects it accepted; `coverage` is
/// `GetJob`'s.
fn job_message(
    job: &JobRow,
    identities: &HashMap<String, String>,
    count: u32,
    coverage: Vec<v1::JobCoverage>,
) -> v1::Job {
    use crate::executor::presence as m;
    let executor = job.executor.as_ref().map(|executor| v1::JobExecutor {
        info: Some(v1::ExecutorInfo {
            name: executor.name.clone(),
            version: executor.version.clone(),
            kinds: executor.kinds.clone(),
            labels: executor.labels.clone(),
        }),
        protocol_version: executor.protocol,
        agent: executor.agent.clone().unwrap_or_default(),
        remote_token_id: executor.token.clone().unwrap_or_default(),
        identity: executor
            .identity_id
            .as_ref()
            .and_then(|id| identities.get(id).cloned())
            .unwrap_or_default(),
    });
    let time = |at: Option<Timestamp>| at.map(timestamp);
    let ms = |value: i64| Duration::from_millis(u64::try_from(value).unwrap_or(0));
    v1::Job {
        job_id: job.id.clone(),
        repo_id: job.repo_id.as_hex(),
        spec: Some(v1::JobSpec {
            tree_id: job.tree_id.clone(),
            kinds: job.kinds.clone(),
            targets: job.targets.clone(),
            budget: Some(v1::Budget {
                wall_time: Some(super::convert::proto_duration(ms(job.wall_time_ms))),
                cpu_time: job
                    .cpu_time_ms
                    .map(|cpu| super::convert::proto_duration(ms(cpu))),
            }),
            requirements: job.requirements.clone(),
            priority: job.priority,
            commit_id: job.commit_id.clone().unwrap_or_default(),
            change_id: job.change_id.clone().unwrap_or_default(),
        }),
        state: m::state_message(job.state).into(),
        outcome: m::outcome_message(job.outcome.as_deref()).into(),
        end_reason: m::end_reason_message(job.end_reason.as_deref()).into(),
        protocol_version: job.protocol,
        attempt: job.attempt,
        max_attempts: job.max_attempts,
        requested_by_agent: job.requested_by_agent.clone().unwrap_or_default(),
        requested_by_token: job.requested_by_token.clone().unwrap_or_default(),
        submitted_at: Some(timestamp(job.submitted_at)),
        queue_expires_at: (job.state == JobState::Queued).then(|| timestamp(job.queue_expires_at)),
        claimed_at: time(job.claimed_at),
        lease_expires_at: time(job.lease_expires_at),
        deadline_at: time(job.deadline_at),
        finished_at: time(job.finished_at),
        executor,
        progress: m::progress_message(job.progress.as_deref()),
        usage: (job.usage_wall_ms.is_some() || job.usage_cpu_ms.is_some()).then(|| v1::JobUsage {
            wall_time_ms: job
                .usage_wall_ms
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0),
            cpu_time_ms: job
                .usage_cpu_ms
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0),
        }),
        elapsed_ms: u64::try_from(job.elapsed_ms).unwrap_or(0),
        message: job.message.clone().unwrap_or_default(),
        evidence_count: count,
        coverage,
    }
}

/// The names of the identities `jobs`' executors are linked to, by id.
fn identity_names(state: &AppState, jobs: &[&JobRow]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for id in jobs
        .iter()
        .filter_map(|job| job.executor.as_ref()?.identity_id.clone())
    {
        if names.contains_key(&id) {
            continue;
        }
        if let Ok(Some(row)) = state.db.identity(&id) {
            names.insert(id, row.name);
        }
    }
    names
}

/// A job's message with its evidence count, without coverage.
fn summary(state: &AppState, job: &JobRow) -> Result<v1::Job, Status> {
    let counts = state
        .db
        .job_evidence_counts(&[(job.repo_id, job.id.clone())])
        .map_err(status)?;
    let identities = identity_names(state, &[job]);
    Ok(job_message(
        job,
        &identities,
        counts.get(&job.id).copied().unwrap_or(0),
        Vec::new(),
    ))
}

/// One accepted object as the API reports it.
fn accepted_message(row: &JobEvidenceRow, result: v1::EvidenceResult) -> v1::JobEvidence {
    v1::JobEvidence {
        evidence_id: row.evidence_id.clone(),
        kind: row.kind.clone(),
        target: row.target.clone().unwrap_or_default(),
        result: result.into(),
        attempt: row.attempt,
        created: row.created,
        fresh: row.fresh,
        accepted_at: Some(timestamp(row.accepted_at)),
    }
}

/// A job's coverage: one entry per requested pair, in the spec's order,
/// each with the latest accepted object of exactly that kind and target,
/// its signature checked again.
async fn coverage(
    state: &AppState,
    job: &JobRow,
    rows: &[JobEvidenceRow],
) -> Result<Vec<v1::JobCoverage>, Status> {
    let targets: Vec<Option<String>> = if job.targets.is_empty() {
        vec![None]
    } else {
        job.targets.iter().cloned().map(Some).collect()
    };
    let mut latest: BTreeMap<(String, Option<String>), &JobEvidenceRow> = BTreeMap::new();
    for row in rows {
        let key = (row.kind.clone(), row.target.clone());
        if latest.get(&key).is_none_or(|have| have.seq <= row.seq) {
            latest.insert(key, row);
        }
    }
    let mut stored = Vec::new();
    for row in latest.values() {
        if let Some(found) = state
            .db
            .evidence_by_id(job.repo_id, &row.evidence_id)
            .map_err(status)?
        {
            stored.push(found);
        }
    }
    let read: HashMap<String, v1::Evidence> =
        messages(state, job.repo_id, stored, false, None, None)
            .await?
            .into_iter()
            .map(|evidence| (evidence.evidence_id.clone(), evidence))
            .collect();
    let mut entries = Vec::new();
    for kind in &job.kinds {
        for target in &targets {
            let mut entry = v1::JobCoverage {
                kind: kind.clone(),
                target: target.clone().unwrap_or_default(),
                ..v1::JobCoverage::default()
            };
            if let Some(row) = latest.get(&(kind.clone(), target.clone()))
                && let Some(evidence) = read.get(&row.evidence_id)
            {
                entry.evidence_id.clone_from(&row.evidence_id);
                entry.result = evidence.result;
                entry.fresh = row.fresh;
                entry.signature = evidence.signature;
            }
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// The job a read or a cancel names: a full id or a prefix.
fn resolve(state: &AppState, repo: &str, text: &str) -> Result<JobRow, Status> {
    let repo = if repo.trim().is_empty() {
        None
    } else {
        Some(state.manager.resolve_repo(repo).map_err(status)?.id)
    };
    if let Ok(id) = full_id(text) {
        return match state.db.job(&id).map_err(status)? {
            Some(job) if repo.is_none_or(|repo| repo == job.repo_id) => Ok(job),
            _ => Err(status(ExecutorError::NotFound { job_id: id })),
        };
    }
    let prefix = id_prefix(text).map_err(status)?;
    match state.db.job_by_prefix(repo, &prefix) {
        Ok(job) => Ok(job),
        Err(crate::db::DbError::NotFound { .. }) => Err(status(ExecutorError::NotFound {
            job_id: format!("job_{prefix}"),
        })),
        Err(crate::db::DbError::Ambiguous { candidates, .. }) => {
            Err(status(ExecutorError::Ambiguous {
                job_id: format!("job_{prefix}"),
                candidates,
            }))
        }
        Err(error) => Err(status(error)),
    }
}

/// The progress an update sends, checked, as the JSON the job keeps.
fn progress_json(progress: Option<&v1::JobProgress>) -> Result<Option<String>, Status> {
    let Some(progress) = progress else {
        return Ok(None);
    };
    if !visible(&progress.phase, MAX_PHASE_BYTES) || !visible(&progress.unit, MAX_UNIT_BYTES) {
        return Err(status(ExecutorError::InvalidUpdate {
            field: "progress",
            message: format!(
                "phase is at most {MAX_PHASE_BYTES} and unit at most {MAX_UNIT_BYTES} bytes of visible ASCII"
            ),
        }));
    }
    Ok(Some(
        serde_json::json!({
            "phase": progress.phase,
            "done": progress.done,
            "total": progress.total,
            "unit": progress.unit,
        })
        .to_string(),
    ))
}

impl ExecutorHandler {
    /// Verifies and binds a call's envelopes to `job`, on the blocking pool.
    async fn verify(
        &self,
        job: &JobRow,
        evidence: Vec<v1::SignedEvidence>,
        agent: Option<String>,
        remote: Option<String>,
    ) -> Result<Vec<Incoming>, Status> {
        if evidence.len() > MAX_EVIDENCE_PER_CALL {
            return Err(status(ExecutorError::InvalidUpdate {
                field: "evidence",
                message: format!(
                    "{} envelopes; at most {MAX_EVIDENCE_PER_CALL} per call",
                    evidence.len()
                ),
            }));
        }
        let verified = tokio::task::spawn_blocking(move || {
            evidence
                .iter()
                .enumerate()
                .map(|(index, signed)| {
                    Verified::check(&signed.envelope, &signed.public_key).map_err(|invalid| {
                        ExecutorError::Evidence {
                            index,
                            source: EvidenceError::Invalid(invalid),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|error| status(ExecutorError::Task(error.to_string())))?
        .map_err(status)?;
        bind(job, &verified).map_err(status)?;
        Ok(verified
            .into_iter()
            .map(|verified| Incoming {
                verified,
                agent: agent.clone(),
                remote_token_id: remote.clone(),
            })
            .collect())
    }

    /// An update or a completion, after its own checks.
    async fn accept(
        &self,
        id: &str,
        token: &str,
        caller: Option<String>,
        agent: Option<String>,
        evidence: Vec<v1::SignedEvidence>,
        call: Call,
    ) -> Result<(v1::Job, Vec<v1::JobEvidence>), Status> {
        let state = &self.state;
        let executor = Arc::clone(&state.executor);
        let job = executor
            .check_claim(id, token, caller.as_deref())
            .map_err(status)?;
        let incoming = self.verify(&job, evidence, agent, caller.clone()).await?;
        let results: HashMap<String, v1::EvidenceResult> = incoming
            .iter()
            .map(|object| {
                (
                    object.verified.id.clone(),
                    result_message(object.verified.index.result.as_str()),
                )
            })
            .collect();
        let (id, token) = (id.to_owned(), token.to_owned());
        let accepted = tokio::task::spawn_blocking(move || {
            executor.accept(&id, &token, caller.as_deref(), &incoming, &call)
        })
        .await
        .map_err(|error| status(ExecutorError::Task(error.to_string())))?
        .map_err(status)?;
        let job = summary(state, &accepted.job)?;
        let evidence = accepted
            .evidence
            .iter()
            .map(|row| {
                accepted_message(
                    row,
                    results
                        .get(&row.evidence_id)
                        .copied()
                        .unwrap_or(v1::EvidenceResult::Unspecified),
                )
            })
            .collect();
        Ok((job, evidence))
    }
}

#[tonic::async_trait]
impl ExecutorService for ExecutorHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let token = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let (tree_id, commit_id, change_id) = match &request.source {
            Some(submit_job_request::Source::Revision(revision)) if !revision.trim().is_empty() => {
                let needed = uses_working_copy(state, &target, vec![revision.clone()]).await?;
                snapshot_first(state, &target, needed, Instant::now(), agent.clone()).await?;
                let head = state.reads.current(&target.handle).await.map_err(status)?;
                let read_target = target.clone();
                let revision = revision.clone();
                state
                    .reads
                    .run(&target.handle, move || async move {
                        let target = read_target;
                        let resolved = revset::resolve_revision(&head, &revision, &target.scope())
                            .await
                            .map_err(status)?;
                        let commit_id = resolved.commit_id.to_string();
                        let (change, tree) = files::commit_tree_id(&head, &resolved.commit_id)
                            .await
                            .map_err(status)?;
                        let tree = tree.ok_or_else(|| {
                            status(ExecutorError::Tree(EvidenceError::Conflicted {
                                commit_id: commit_id.clone(),
                            }))
                        })?;
                        Ok::<_, Status>((tree, Some(commit_id), Some(change.to_string())))
                    })
                    .await
                    .map_err(status)??
            }
            Some(submit_job_request::Source::TreeId(tree)) if !tree.trim().is_empty() => {
                let tree = tree.trim().to_owned();
                let head = state.reads.current(&target.handle).await.map_err(status)?;
                let check = tree.clone();
                let presence = state
                    .reads
                    .run(&target.handle, move || async move {
                        let len = files::tree_id_hex_len(&head);
                        if check.len() != len
                            || !check
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                        {
                            return Ok(None);
                        }
                        files::tree_presence(&head, &check)
                            .await
                            .map(Some)
                            .map_err(status)
                    })
                    .await
                    .map_err(status)??;
                match presence {
                    None => {
                        return Err(status(ExecutorError::InvalidSpec {
                            field: "tree_id",
                            message: format!("{tree:?} is not a lowercase hex object id"),
                        }));
                    }
                    Some(TreePresence::Present) => {}
                    Some(other) => {
                        return Err(status(ExecutorError::Tree(EvidenceError::TreeNotFound {
                            tree_id: tree,
                            not_a_tree: other == TreePresence::NotATree,
                        })));
                    }
                }
                (tree, None, None)
            }
            _ => {
                return Err(status(ExecutorError::InvalidSpec {
                    field: "tree_id",
                    message: "name the tree to check: a revision or a tree_id".to_owned(),
                }));
            }
        };
        let budget = request.budget.unwrap_or_default();
        let job = NewJob {
            repo_id: target.repo.id,
            tree_id,
            commit_id,
            change_id,
            kinds: request.kinds,
            targets: request.targets,
            requirements: request.requirements,
            wall_time: spec_duration("wall_time", budget.wall_time.as_ref())?,
            cpu_time: spec_duration("cpu_time", budget.cpu_time.as_ref())?,
            priority: request.priority,
            queue_timeout: spec_duration("queue_timeout", request.queue_timeout.as_ref())?,
            duplicate: request.duplicate,
            agent,
            token,
        };
        let executor = Arc::clone(&state.executor);
        let (row, created) = tokio::task::spawn_blocking(move || executor.submit(&job))
            .await
            .map_err(|error| status(ExecutorError::Task(error.to_string())))?
            .map_err(status)?;
        Ok(Response::new(SubmitJobResponse {
            job: Some(summary(state, &row)?),
            created,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let job = resolve(state, &request.repo, &request.job_id)?;
        let rows = state.executor.evidence_of(&job).map_err(status)?;
        let entries = coverage(state, &job, &rows).await?;
        let identities = identity_names(state, &[&job]);
        let count = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        Ok(Response::new(GetJobResponse {
            job: Some(job_message(&job, &identities, count, entries)),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let filters = ListJobsRequest {
            page_size: 0,
            page_token: String::new(),
            ..request.clone()
        };
        let page = PageRequest::new(LIST_JOBS, request.page_size, &request.page_token, &filters)
            .map_err(status)?;
        let repo = if request.repo.trim().is_empty() {
            None
        } else {
            Some(
                state
                    .manager
                    .resolve_repo(&request.repo)
                    .map_err(status)?
                    .id,
            )
        };
        let mut states = Vec::new();
        for value in &request.states {
            let state = match v1::JobState::try_from(*value) {
                Ok(v1::JobState::Queued) => JobState::Queued,
                Ok(v1::JobState::Running) => JobState::Running,
                Ok(v1::JobState::Completed) => JobState::Completed,
                Ok(v1::JobState::Cancelled) => JobState::Cancelled,
                Ok(v1::JobState::Expired) => JobState::Expired,
                _ => {
                    return Err(Status::invalid_argument(format!(
                        "{value} is not a job state"
                    )));
                }
            };
            states.push(state);
        }
        let change_id = if request.change_id.trim().is_empty() {
            None
        } else {
            Some(full_change_id(&request.change_id)?)
        };
        let filter = JobFilter {
            repo,
            states,
            tree_id: Some(request.tree_id.trim().to_owned()).filter(|tree| !tree.is_empty()),
            change_id,
            executor: Some(request.executor.trim().to_owned()).filter(|name| !name.is_empty()),
            since: request.since.map(|since| {
                Timestamp(
                    since
                        .seconds
                        .saturating_mul(1000)
                        .saturating_add(i64::from(since.nanos) / 1_000_000),
                )
            }),
        };
        let cursor = match page.cursor() {
            None => None,
            Some(text) => Some(parse_cursor(text, request.queue_order)?),
        };
        let size = page.size();
        let mut jobs = state
            .db
            .list_jobs(&filter, request.queue_order, cursor.as_ref(), size + 1)
            .map_err(status)?;
        let more = jobs.len() > size;
        jobs.truncate(size);
        let next_page_token = match (more, jobs.last()) {
            (true, Some(last)) => page.token_at(&cursor_text(last, request.queue_order), ""),
            _ => String::new(),
        };
        let counts = state
            .db
            .job_evidence_counts(
                &jobs
                    .iter()
                    .map(|job| (job.repo_id, job.id.clone()))
                    .collect::<Vec<_>>(),
            )
            .map_err(status)?;
        let identities = identity_names(state, &jobs.iter().collect::<Vec<_>>());
        Ok(Response::new(ListJobsResponse {
            jobs: jobs
                .iter()
                .map(|job| {
                    job_message(
                        job,
                        &identities,
                        counts.get(&job.id).copied().unwrap_or(0),
                        Vec::new(),
                    )
                })
                .collect(),
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let owner = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let state = &self.state;
        if request.reason.len() > MAX_REASON_BYTES || has_control(&request.reason) {
            return Err(status(ExecutorError::InvalidUpdate {
                field: "reason",
                message: format!(
                    "{} bytes; at most {MAX_REASON_BYTES}, with no control characters",
                    request.reason.len()
                ),
            }));
        }
        let job = resolve(state, &request.repo, &request.job_id)?;
        let reason = Some(request.reason.trim().to_owned()).filter(|reason| !reason.is_empty());
        let executor = Arc::clone(&state.executor);
        let id = job.id.clone();
        let job = tokio::task::spawn_blocking(move || {
            executor.cancel(&id, owner.as_deref(), agent.as_deref(), reason.as_deref())
        })
        .await
        .map_err(|error| status(ExecutorError::Task(error.to_string())))?
        .map_err(status)?;
        Ok(Response::new(CancelJobResponse {
            job: Some(summary(state, &job)?),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn claim_job(
        &self,
        request: Request<ClaimJobRequest>,
    ) -> Result<Response<ClaimJobResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let channel = super::identity::channel_of(&self.state, &request);
        let request = request.into_inner();
        let state = &self.state;
        let (token, identity_id) = match channel {
            crate::identity::issue::Channel::Socket => (None, None),
            crate::identity::issue::Channel::Remote { linked: None, .. } => {
                return Err(status(ExecutorError::TokenNotLinked));
            }
            crate::identity::issue::Channel::Remote {
                token_id,
                linked: Some(identity),
                ..
            } => (Some(token_id), Some(identity)),
        };
        let invalid = |field: &'static str, message: String| {
            status(ExecutorError::InvalidExecutor { field, message })
        };
        let info = request.executor.unwrap_or_default();
        let name = info.name.trim().to_owned();
        if let Some(problem) = reef_evidence::session::agent_problem(&name) {
            return Err(invalid("name", problem));
        }
        if !visible(&info.version, MAX_VERSION_BYTES) {
            return Err(invalid(
                "version",
                format!("at most {MAX_VERSION_BYTES} bytes of visible ASCII"),
            ));
        }
        let declared_kinds =
            kinds(&info.kinds, MAX_EXECUTOR_KINDS).map_err(|message| invalid("kinds", message))?;
        let declared_labels =
            labels(&info.labels, MAX_LABELS).map_err(|message| invalid("labels", message))?;
        if request.protocol_version == 0 {
            return Err(invalid(
                "protocol_version",
                "name the highest protocol version the executor speaks, at least 1".to_owned(),
            ));
        }
        let wait = duration(request.wait.as_ref())
            .map_err(|()| invalid("wait", "not negative".to_owned()))?
            .unwrap_or(Duration::ZERO);
        if wait > MAX_WAIT {
            return Err(invalid("wait", "at most 60 seconds".to_owned()));
        }
        let lease = duration(request.lease.as_ref())
            .map_err(|()| invalid("lease", "not negative".to_owned()))?
            .unwrap_or(DEFAULT_LEASE);
        if !(MIN_LEASE..=MAX_LEASE).contains(&lease) {
            return Err(invalid("lease", "from 15 seconds to 10 minutes".to_owned()));
        }
        let mut repos = Vec::new();
        for repo in &request.repos {
            repos.push(state.manager.resolve_repo(repo).map_err(status)?.id);
        }
        let claimant = Claimant {
            columns: ExecutorColumns {
                name,
                version: info.version.clone(),
                labels: declared_labels,
                kinds: declared_kinds,
                protocol: request.protocol_version,
                agent,
                token,
                identity_id,
            },
            repos,
            lease,
        };
        let claimed = state
            .executor
            .claim(&claimant, wait, &state.shutdown)
            .await
            .map_err(status)?;
        let (job, claim_token) = match claimed {
            Some((job, token)) => (Some(summary(state, &job)?), token),
            None => (None, String::new()),
        };
        Ok(Response::new(ClaimJobResponse {
            job,
            claim_token,
            protocol_version: reef_api::EXECUTOR_PROTOCOL,
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn update_job(
        &self,
        request: Request<UpdateJobRequest>,
    ) -> Result<Response<UpdateJobResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let caller = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let id = full_id(&request.job_id).map_err(status)?;
        let progress = progress_json(request.progress.as_ref())?;
        let (job, accepted) = self
            .accept(
                &id,
                &request.claim_token,
                caller,
                agent,
                request.evidence,
                Call::Update(Update {
                    progress,
                    release: request.release,
                }),
            )
            .await?;
        Ok(Response::new(UpdateJobResponse {
            job: Some(job),
            accepted,
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn complete_job(
        &self,
        request: Request<CompleteJobRequest>,
    ) -> Result<Response<CompleteJobResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let caller = super::remote_of(&request).map(|caller| caller.token_id.clone());
        let request = request.into_inner();
        let id = full_id(&request.job_id).map_err(status)?;
        let outcome = match v1::JobOutcome::try_from(request.outcome) {
            Ok(v1::JobOutcome::Done) => "done",
            Ok(v1::JobOutcome::BudgetExhausted) => "budget_exhausted",
            Ok(v1::JobOutcome::Failed) => "failed",
            _ => {
                return Err(status(ExecutorError::InvalidUpdate {
                    field: "outcome",
                    message: "name the outcome: DONE, BUDGET_EXHAUSTED or FAILED".to_owned(),
                }));
            }
        };
        if request.message.len() > MAX_MESSAGE_BYTES || has_control(&request.message) {
            return Err(status(ExecutorError::InvalidUpdate {
                field: "message",
                message: format!(
                    "{} bytes; at most {MAX_MESSAGE_BYTES}, with no control characters but newlines",
                    request.message.len()
                ),
            }));
        }
        let usage = request.usage.unwrap_or_default();
        let to_ms = |value: u64| -> Result<Option<i64>, Status> {
            if value == 0 {
                return Ok(None);
            }
            i64::try_from(value).map(Some).map_err(|_| {
                status(ExecutorError::InvalidUpdate {
                    field: "usage",
                    message: "too large".to_owned(),
                })
            })
        };
        let completion = Completion {
            outcome,
            usage_wall_ms: to_ms(usage.wall_time_ms)?,
            usage_cpu_ms: to_ms(usage.cpu_time_ms)?,
            message: Some(request.message.trim().to_owned()).filter(|m| !m.is_empty()),
        };
        let (job, accepted) = self
            .accept(
                &id,
                &request.claim_token,
                caller,
                agent,
                request.evidence,
                Call::Complete(completion),
            )
            .await?;
        Ok(Response::new(CompleteJobResponse {
            job: Some(job),
            accepted,
        }))
    }
}

/// The page cursor after `job`.
fn cursor_text(job: &JobRow, queue_order: bool) -> String {
    if queue_order {
        format!("{}:{}:{}", job.priority, job.submitted_at.0, job.id)
    } else {
        format!("{}:{}", job.submitted_at.0, job.id)
    }
}

/// A page token's cursor.
fn parse_cursor(text: &str, queue_order: bool) -> Result<JobCursor, Status> {
    let mut parts = text.splitn(3, ':');
    if queue_order {
        let priority = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(bad_token)?;
        let at = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(bad_token)?;
        let id = parts.next().ok_or_else(bad_token)?;
        Ok(JobCursor::Queue(priority, Timestamp(at), id.to_owned()))
    } else {
        let at = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(bad_token)?;
        let id = parts.next().ok_or_else(bad_token)?;
        Ok(JobCursor::Newest(Timestamp(at), id.to_owned()))
    }
}
