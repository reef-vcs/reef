//! `reef.api.v1.LandingService` (R3.13, ADR 0064): landing proposed changes
//! onto a local bookmark, and reading landings. The service resolves the
//! request; [`crate::landing`] judges, queues and lands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reef_api::v1::landing_service_server::LandingService;
use reef_api::v1::{
    self, GetBatchRequest, GetBatchResponse, GetLandingRequest, GetLandingResponse,
    LandChangeRequest, LandChangeResponse, ListBatchesRequest, ListBatchesResponse,
    ListLandingsRequest, ListLandingsResponse, OperationKind,
};
use tonic::{Request, Response, Status};

use super::convert::{self, Written};
use super::page::PageRequest;
use super::reading::{bad_token, snapshot_first, target, uses_working_copy};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::landing::messages::{batch_message, landing_message, outcome_of};
use crate::landing::{LandRequest, LandingError};

const LIST_LANDINGS: &str = "/reef.api.v1.LandingService/ListLandings";
const LIST_BATCHES: &str = "/reef.api.v1.LandingService/ListBatches";

/// Serves `LandingService`.
#[derive(Debug)]
pub struct LandingHandler {
    state: Arc<AppState>,
}

impl LandingHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> LandingHandler {
        LandingHandler { state }
    }
}

/// A timestamp as milliseconds since the Unix epoch.
fn millis(at: &prost_types::Timestamp) -> i64 {
    at.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(at.nanos) / 1_000_000)
}

#[tonic::async_trait]
impl LandingService for LandingHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn land_change(
        &self,
        request: Request<LandChangeRequest>,
    ) -> Result<Response<LandChangeResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let remote = super::remote_of(&request)
            .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
        let request = request.into_inner();
        let state = &self.state;
        let wait = Duration::from_millis(u64::from(request.wait_ms));
        if wait > crate::landing::MAX_WAIT {
            return Err(status(LandingError::InvalidWait));
        }
        let target = target(state, &request.repo, &request.workspace)?;
        let mut snapshot = None;
        if !request.skip_snapshot && !request.revision.trim().is_empty() {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot = snapshot_first(state, &target, needed, arrived, agent.clone())
                .await?
                .operation;
        }
        let outcome = crate::landing::land_change(
            state,
            LandRequest {
                repo: target.repo.id,
                workspace: target.workspace.clone(),
                user_email: target.user_email.clone(),
                root: target.repo.root.clone(),
                revision: request.revision.clone(),
                expected_commit: request.expected_commit_id.clone(),
                wait,
                actor: crate::lifecycle::messages::actor_of(request.actor),
                agent: agent.clone(),
                remote,
            },
        )
        .await
        .map_err(status)?;
        let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
        let owner = target
            .workspace
            .as_ref()
            .map(|record| record.owner.as_str());
        let enqueue = written_operation(
            state,
            target.repo.id,
            outcome.enqueue.as_ref().map(ToString::to_string).as_deref(),
            Written {
                kind: OperationKind::Transition,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        let finished = outcome.landing.row.outcome != "queued" && outcome.enqueue.is_some();
        let operation = if finished {
            let kind = if outcome.landing.row.outcome == "landed" {
                OperationKind::Land
            } else {
                OperationKind::Transition
            };
            written_operation(
                state,
                target.repo.id,
                outcome.landing.row.operation_id.as_deref(),
                Written {
                    kind,
                    snapshot: None,
                    workspace: None,
                    agent: agent.as_deref().or(owner).unwrap_or_default(),
                },
            )
            .await?
        } else {
            None
        };
        let mut change = outcome.change.as_ref().map(convert::change);
        crate::lifecycle::messages::fill(state, target.repo.id, change.iter_mut(), None);
        Ok(Response::new(LandChangeResponse {
            landing: Some(landing_message(&outcome.landing, true)),
            enqueue,
            operation,
            snapshot,
            change,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_landing(
        &self,
        request: Request<GetLandingRequest>,
    ) -> Result<Response<GetLandingResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let id = request.id.trim();
        let change = request.change.trim();
        if id.is_empty() == change.is_empty() {
            return Err(status(LandingError::Selector));
        }
        let target = target(state, &request.repo, "")?;
        let found = if id.is_empty() {
            let prefix = super::operations::change_prefix(change).ok_or_else(|| {
                status(super::operations::InvalidChangeFilter {
                    change: change.to_owned(),
                })
            })?;
            state
                .db
                .latest_landing_of(target.repo.id, &prefix)
                .map_err(status)?
        } else {
            state.db.landing(target.repo.id, id).map_err(status)?
        };
        let Some(landing) = found else {
            let what = if id.is_empty() { change } else { id };
            return Err(status(LandingError::NotFound(what.to_owned())));
        };
        let policy = if request.include_policy {
            match landing.row.policy_sha256.as_deref() {
                Some(sha) => state
                    .db
                    .landing_policy(target.repo.id, sha)
                    .map_err(status)?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default(),
                None => String::new(),
            }
        } else {
            String::new()
        };
        Ok(Response::new(GetLandingResponse {
            landing: Some(landing_message(&landing, true)),
            policy,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_landings(
        &self,
        request: Request<ListLandingsRequest>,
    ) -> Result<Response<ListLandingsResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_LANDINGS,
            request.page_size,
            &request.page_token,
            &ListLandingsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let mut outcomes = Vec::new();
        for value in &request.outcomes {
            let outcome = outcome_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not a landing outcome"))
            })?;
            outcomes.push(outcome.to_owned());
        }
        let mut changes = Vec::new();
        for change in &request.changes {
            changes.push(super::operations::change_prefix(change).ok_or_else(|| {
                status(super::operations::InvalidChangeFilter {
                    change: change.clone(),
                })
            })?);
        }
        let filter = crate::db::LandingFilter {
            destinations: request.destinations.clone(),
            outcomes,
            changes,
            finished_after: request
                .finished_after
                .as_ref()
                .map(|after| crate::model::Timestamp(millis(after))),
        };
        let cursor = match page.cursor() {
            None => None,
            Some(text) => {
                let (at, id) = text.split_once(':').ok_or_else(bad_token)?;
                Some((
                    crate::model::Timestamp(at.parse::<i64>().map_err(|_| bad_token())?),
                    id.to_owned(),
                ))
            }
        };
        let size = page.size();
        let mut rows = state
            .db
            .landing_page(
                target.repo.id,
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last()) {
            (true, Some(last)) => {
                page.token_at(&format!("{}:{}", last.row.created_at.0, last.row.id), "")
            }
            _ => String::new(),
        };
        Ok(Response::new(ListLandingsResponse {
            landings: rows
                .iter()
                .map(|landing| landing_message(landing, false))
                .collect::<Vec<v1::Landing>>(),
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_batch(
        &self,
        request: Request<GetBatchRequest>,
    ) -> Result<Response<GetBatchResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let id = request.id.trim();
        let change = request.change.trim();
        if id.is_empty() == change.is_empty() {
            return Err(status(LandingError::Selector));
        }
        let target = target(state, &request.repo, "")?;
        let found = if id.is_empty() {
            let prefix = super::operations::change_prefix(change).ok_or_else(|| {
                status(super::operations::InvalidChangeFilter {
                    change: change.to_owned(),
                })
            })?;
            state
                .db
                .latest_batch_of(target.repo.id, &prefix)
                .map_err(status)?
        } else {
            state.db.batch(target.repo.id, id).map_err(status)?
        };
        let Some(batch) = found else {
            let what = if id.is_empty() { change } else { id };
            return Err(status(LandingError::BatchNotFound(what.to_owned())));
        };
        Ok(Response::new(GetBatchResponse {
            batch: Some(batch_message(&batch)),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_batches(
        &self,
        request: Request<ListBatchesRequest>,
    ) -> Result<Response<ListBatchesResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_BATCHES,
            request.page_size,
            &request.page_token,
            &ListBatchesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let filter = crate::db::BatchFilter {
            destinations: request.destinations.clone(),
            open_only: request.open_only,
            finished_after: request
                .finished_after
                .as_ref()
                .map(|after| crate::model::Timestamp(millis(after))),
        };
        let cursor = match page.cursor() {
            None => None,
            Some(text) => {
                let (at, id) = text.split_once(':').ok_or_else(bad_token)?;
                Some((
                    crate::model::Timestamp(at.parse::<i64>().map_err(|_| bad_token())?),
                    id.to_owned(),
                ))
            }
        };
        let size = page.size();
        let mut rows = state
            .db
            .batch_page(
                target.repo.id,
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last()) {
            (true, Some(last)) => {
                page.token_at(&format!("{}:{}", last.row.created_at.0, last.row.id), "")
            }
            _ => String::new(),
        };
        Ok(Response::new(ListBatchesResponse {
            batches: rows.iter().map(batch_message).collect(),
            next_page_token,
        }))
    }
}
