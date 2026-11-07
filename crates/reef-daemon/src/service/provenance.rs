//! `reef.api.v1.ProvenanceService` (R3.04, ADR 0058): declaring a
//! workspace's context, and reading provenance records, each checked again
//! at every read ([`crate::provenance::read`]).

use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::provenance_service_server::ProvenanceService;
use reef_api::v1::{
    self, DeclareProvenanceRequest, DeclareProvenanceResponse, GetProvenanceRequest,
    GetProvenanceResponse, ListProvenanceRequest, ListProvenanceResponse,
};
use reef_lib::jj::revset;
use tonic::{Request, Response, Status};

use super::convert::Written;
use super::page::PageRequest;
use super::reading::{bad_token, snapshot_first, target, uses_working_copy};
use super::written_operation;
use crate::daemon::AppState;
use crate::db::{DbError, ProvenanceFilter, StoredProvenance};
use crate::error::status;
use crate::model::{RepoId, Timestamp};
use crate::provenance::session::{For, patch_of};
use crate::provenance::{MAX_CHANGE_IDS, ProvenanceError};
use crate::snapshot::SnapshotKind;

const LIST_PROVENANCE: &str = "/reef.api.v1.ProvenanceService/ListProvenance";

/// A page's size when the request names none.
const DEFAULT_PAGE_SIZE: usize = 100;

/// The largest page.
const MAX_PAGE_SIZE: usize = 1000;

/// The shortest record id prefix.
const MIN_ID_PREFIX: usize = 8;

/// The provenance service.
#[derive(Clone, Debug)]
pub struct ProvenanceHandler {
    state: Arc<AppState>,
}

impl ProvenanceHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> ProvenanceHandler {
        ProvenanceHandler { state }
    }
}

fn task(error: &tokio::task::JoinError) -> Status {
    status(ProvenanceError::Task(error.to_string()))
}

/// `rows` read on the blocking pool.
async fn read(
    state: &AppState,
    rows: Vec<StoredProvenance>,
    with_envelope: bool,
) -> Result<Vec<v1::ProvenanceRecord>, Status> {
    let db = Arc::clone(&state.db);
    tokio::task::spawn_blocking(move || {
        crate::provenance::read::read_records(&db, rows, with_envelope)
    })
    .await
    .map_err(|error| task(&error))?
    .map_err(status)
}

/// The `(recorded_at, id)` cursor of a page token.
fn parse_cursor(cursor: &str) -> Result<(Timestamp, String), Status> {
    let (at, id) = cursor.split_once(':').ok_or_else(bad_token)?;
    Ok((
        Timestamp(at.parse().map_err(|_| bad_token())?),
        id.to_owned(),
    ))
}

/// The record of `repo`'s commit `commit_id`; `PROVENANCE_NOT_FOUND`, with
/// the change, when it has none.
fn by_commit(
    state: &AppState,
    repo: RepoId,
    commit_id: &str,
    change_id: &str,
) -> Result<StoredProvenance, Status> {
    state
        .db
        .provenance_by_commit(repo, commit_id)
        .map_err(status)?
        .ok_or_else(|| {
            status(ProvenanceError::CommitNotFound {
                commit_id: commit_id.to_owned(),
                change_id: change_id.to_owned(),
            })
        })
}

#[tonic::async_trait]
impl ProvenanceService for ProvenanceHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn declare_provenance(
        &self,
        request: Request<DeclareProvenanceRequest>,
    ) -> Result<Response<DeclareProvenanceResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let remote = super::remote_of(&request).cloned();
        let request = request.into_inner();
        let state = &self.state;
        if !state.provenance.enabled() {
            return Err(status(ProvenanceError::Disabled));
        }
        let patch = patch_of(&request.declaration.unwrap_or_default(), true).map_err(status)?;
        let manager = &state.manager;
        let record = manager
            .resolve_workspace(&request.repo, &request.workspace)
            .map_err(status)?;
        crate::remote::rules::check_owned(remote.as_ref(), &record)?;
        // Snapshot or not, only a live workspace's context changes.
        if record.state != crate::model::WorkspaceState::Active {
            return Err(status(crate::workspace::ManagerError::NotActive {
                id: record.id,
                repo: record.repo_id,
                state: record.state,
            }));
        }
        let span = tracing::Span::current();
        span.record("repo", tracing::field::display(record.repo_id));
        span.record("workspace_id", tracing::field::display(record.id));
        // Edits on disk were made under the old context: record them first.
        let operation = if request.no_snapshot {
            None
        } else {
            let snapshot = manager
                .snapshot(record.id, SnapshotKind::Explicit, "", agent.clone())
                .await
                .map_err(status)?;
            let op_id = snapshot.op_id.to_string();
            let workspace_id = record.id.as_hex();
            written_operation(
                state,
                record.repo_id,
                snapshot.changed.then_some(op_id.as_str()),
                Written {
                    kind: v1::OperationKind::Snapshot,
                    snapshot: Some(SnapshotKind::Explicit),
                    workspace: Some(&workspace_id),
                    agent: agent.as_deref().unwrap_or(&record.owner),
                },
            )
            .await?
        };
        let provenance = Arc::clone(&state.provenance);
        let session = tokio::task::spawn_blocking(move || {
            let row = provenance.declare(
                For {
                    repo_id: record.repo_id,
                    workspace_id: record.id,
                    owner: &record.owner,
                    remote_token_id: record.remote_token_id.as_deref(),
                },
                &patch,
                agent.as_deref(),
            )?;
            let records = provenance
                .db
                .live_provenance_sessions(&[row.workspace_id.as_str()])?
                .remove(&row.workspace_id)
                .map_or(0, |(_, records)| records);
            Ok::<_, ProvenanceError>(provenance.session_message(&row, records)?)
        })
        .await
        .map_err(|error| task(&error))?
        .map_err(status)?;
        Ok(Response::new(DeclareProvenanceResponse {
            session: Some(session),
            operation,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_provenance(
        &self,
        request: Request<GetProvenanceRequest>,
    ) -> Result<Response<GetProvenanceResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let repo = target.repo.id;
        let stored = match request.target {
            Some(v1::get_provenance_request::Target::Id(id)) => {
                let text = id.trim().to_ascii_lowercase();
                let valid = (MIN_ID_PREFIX..=64).contains(&text.len())
                    && text.bytes().all(|b| b.is_ascii_hexdigit());
                if !valid {
                    return Err(status(ProvenanceError::IdNotFound { id: text }));
                }
                match state.db.provenance_by_prefix(repo, &text) {
                    Ok(stored) => stored,
                    Err(DbError::NotFound { .. }) => {
                        return Err(status(ProvenanceError::IdNotFound { id: text }));
                    }
                    Err(DbError::Ambiguous { candidates, .. }) => {
                        return Err(status(ProvenanceError::Ambiguous {
                            id: text,
                            candidates,
                        }));
                    }
                    Err(error) => return Err(status(error)),
                }
            }
            Some(v1::get_provenance_request::Target::CommitId(commit_id)) => {
                by_commit(state, repo, &commit_id.trim().to_ascii_lowercase(), "")?
            }
            target_revision => {
                let revision = match target_revision {
                    Some(v1::get_provenance_request::Target::Revision(revision)) => revision,
                    _ => "@".to_owned(),
                };
                let needed = uses_working_copy(state, &target, vec![revision.clone()]).await?;
                snapshot_first(state, &target, needed, Instant::now(), agent).await?;
                let head = state.reads.current(&target.handle).await.map_err(status)?;
                let scope_target = target.clone();
                let resolved = state
                    .reads
                    .run(&target.handle, move || async move {
                        revset::resolve_revision(&head, &revision, &scope_target.scope())
                            .await
                            .map_err(status)
                    })
                    .await
                    .map_err(status)??;
                by_commit(
                    state,
                    repo,
                    resolved.commit_id.as_str(),
                    resolved.change_id.as_str(),
                )?
            }
        };
        let mut records = read(state, vec![stored], true).await?;
        Ok(Response::new(GetProvenanceResponse {
            record: records.pop(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_provenance(
        &self,
        request: Request<ListProvenanceRequest>,
    ) -> Result<Response<ListProvenanceResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_PROVENANCE,
            request.page_size,
            &request.page_token,
            &ListProvenanceRequest {
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
        if request.change_ids.len() > MAX_CHANGE_IDS {
            return Err(status(ProvenanceError::Invalid {
                field: "change_ids",
                message: format!(
                    "{} change ids; a listing filters on at most {MAX_CHANGE_IDS}",
                    request.change_ids.len()
                ),
            }));
        }
        let change_ids: Vec<String> = request
            .change_ids
            .iter()
            .map(|id| id.trim().to_owned())
            .collect();
        if let Some(bad) = change_ids.iter().find(|id| {
            id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }) {
            return Err(status(ProvenanceError::Invalid {
                field: "change_ids",
                message: format!("{bad:?} is not a change id: 32 lowercase hex digits"),
            }));
        }
        let state = &self.state;
        let repo = state.manager.resolve_repo(&request.repo).map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(repo.id));
        let filter = ProvenanceFilter {
            change_ids,
            workspace_id: Some(request.workspace_id.trim().to_owned()).filter(|id| !id.is_empty()),
            session_id: Some(request.session_id.trim().to_owned()).filter(|id| !id.is_empty()),
            since: request.since.as_ref().map(|at| {
                let millis = i128::from(at.seconds) * 1000 + i128::from(at.nanos) / 1_000_000;
                Timestamp(i64::try_from(millis).unwrap_or(i64::MAX))
            }),
        };
        let cursor = page.cursor().map(parse_cursor).transpose()?;
        let mut found = state
            .db
            .list_provenance(
                repo.id,
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = found.len() > size;
        found.truncate(size);
        let next_page_token = match found.last() {
            Some(last) if more => {
                page.token_at(&format!("{}:{}", last.row.recorded_at.0, last.row.id), "")
            }
            _ => String::new(),
        };
        let records = read(state, found, false).await?;
        Ok(Response::new(ListProvenanceResponse {
            records,
            next_page_token,
        }))
    }
}
