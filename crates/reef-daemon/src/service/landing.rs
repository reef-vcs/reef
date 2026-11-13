//! `reef.api.v1.LandingService` (R3.13, ADR 0064): landing proposed changes
//! onto a local bookmark, and reading landings. The service resolves the
//! request; [`crate::landing`] judges, queues and lands.

use std::sync::Arc;

use futures::FutureExt as _;
use std::time::{Duration, Instant};

use reef_api::v1::landing_service_server::LandingService;
use reef_api::v1::{
    self, GetBatchRequest, GetBatchResponse, GetLandingRequest, GetLandingResponse,
    LandChangeRequest, LandChangeResponse, ListBatchesRequest, ListBatchesResponse,
    ListLandingsRequest, ListLandingsResponse, ListPushesRequest, ListPushesResponse,
    OperationKind, PushLandedRequest, PushLandedResponse,
};
use tonic::{Request, Response, Status};

use super::convert::{self, Written};
use super::page::PageRequest;
use super::reading::{bad_token, snapshot_first, target, uses_working_copy};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::landing::messages::{
    batch_message, destination_push_message, fill_pushes, landing_message, landing_push, outcome_of,
};
use crate::landing::{LandRequest, LandingError};

const LIST_LANDINGS: &str = "/reef.api.v1.LandingService/ListLandings";
const LIST_BATCHES: &str = "/reef.api.v1.LandingService/ListBatches";
const LIST_PUSHES: &str = "/reef.api.v1.LandingService/ListPushes";

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
        crate::lifecycle::messages::fill(state, target.repo.id, change.iter_mut(), None).await;
        let mut landing = landing_message(&outcome.landing, true);
        // A landing onto a pushing destination waits, within its own wait,
        // for the push that carries it (R3.17); its outcome is the
        // landing's either way.
        if outcome.landing.row.outcome == "landed" {
            let deadline = tokio::time::Instant::from_std(arrived + wait);
            match crate::landing::push::wait_for_push(
                state,
                target.repo.id,
                &outcome.landing.row.id,
                deadline,
            )
            .await
            {
                Ok(Some((row, push))) => landing.push = landing_push(&row, Some(&push)),
                Ok(None) => {}
                // The landing landed: its outcome is the answer.
                Err(error) => {
                    tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read a landing's push");
                }
            }
        }
        Ok(Response::new(LandChangeResponse {
            landing: Some(landing),
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
        let mut message = landing_message(&landing, true);
        fill_pushes(state, [(&landing.row, &mut message)]);
        Ok(Response::new(GetLandingResponse {
            landing: Some(message),
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
        let mut landings = rows
            .iter()
            .map(|landing| landing_message(landing, false))
            .collect::<Vec<v1::Landing>>();
        fill_pushes(
            state,
            rows.iter()
                .map(|landing| &landing.row)
                .zip(landings.iter_mut()),
        );
        Ok(Response::new(ListLandingsResponse {
            landings,
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

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_pushes(
        &self,
        request: Request<ListPushesRequest>,
    ) -> Result<Response<ListPushesResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_PUSHES,
            request.page_size,
            &request.page_token,
            &ListPushesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let after = page.cursor().map(str::to_owned);
        let size = page.size();
        let mut rows: Vec<_> = state
            .db
            .destination_pushes(Some(target.repo.id))
            .map_err(status)?
            .into_iter()
            .filter(|row| {
                after
                    .as_deref()
                    .is_none_or(|after| row.destination.as_str() > after)
            })
            .take(size + 1)
            .collect();
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last()) {
            (true, Some(last)) => page.token_at(&last.destination, ""),
            _ => String::new(),
        };
        Ok(Response::new(ListPushesResponse {
            pushes: rows.iter().map(destination_push_message).collect(),
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn push_landed(
        &self,
        request: Request<PushLandedRequest>,
    ) -> Result<Response<PushLandedResponse>, Status> {
        if super::remote_of(&request).is_some() {
            let context = crate::publish::Context::default();
            return Err(crate::publish::status(
                &crate::publish::PublishError::RemoteDenied,
                &context,
            ));
        }
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, "")?;
        let repo = target.repo.id;
        // Git's refs first (the destination may be a git branch jj has not
        // imported yet), then the destination: the one named, else
        // `trunk()`'s bookmark; whether it exists; and `trunk()`'s name.
        let named = request.destination.trim().to_owned();
        let (destination, exists, trunk) = target
            .handle
            .call_async(move |head| {
                async move {
                    crate::workspace::refresh_for_resolve(
                        head,
                        &crate::workspace::import_meta(None, Some(crate::landing::BATCH_AGENT)),
                    )
                    .await?;
                    let repo = head.refresh().await?;
                    let trunk = reef_lib::jj::revset::trunk(&repo).map(|trunk| trunk.bookmark);
                    let destination = if named.is_empty() {
                        trunk.clone().unwrap_or_default()
                    } else {
                        named
                    };
                    let exists = !destination.is_empty()
                        && reef_lib::jj::lifecycle::local_bookmark_exists(&repo, &destination);
                    Ok::<_, crate::workspace::ManagerError>((destination, exists, trunk))
                }
                .boxed_local()
            })
            .await
            .map_err(status)?
            .map_err(status)?;
        if !exists {
            let name = if destination.is_empty() {
                "trunk()".to_owned()
            } else {
                destination
            };
            return Err(crate::landing::push::not_found(&name));
        }
        // Only a landing destination is pushed: one with a push state, a
        // landing, or `trunk()`'s bookmark; never any branch that happens to
        // carry the policy file.
        let known = state
            .db
            .destination_push(repo, &destination)
            .map_err(status)?
            .is_some()
            || state.db.has_landings(repo, &destination).map_err(status)?
            || trunk.as_deref() == Some(destination.as_str());
        if !known {
            return Err(crate::landing::push::not_pushed(&destination));
        }
        let before = state
            .db
            .destination_push(repo, &destination)
            .map_err(status)?
            .and_then(|row| row.operation_id);
        let row = if request.wait {
            let deadline = tokio::time::Instant::now()
                + state.config.file.publish.timeout
                + Duration::from_secs(30);
            crate::landing::push::push_now(state, repo, &destination, deadline)
                .await
                .map_err(status)?
        } else {
            crate::landing::push::wake(
                state,
                repo,
                &destination,
                crate::landing::push::Trigger::Request,
            );
            crate::landing::push::current(state, repo, &destination)
                .await
                .map_err(status)?
        };
        let Some(row) = row else {
            return Err(crate::landing::push::not_pushed(&destination));
        };
        let operation = match row.operation_id.as_deref() {
            Some(id) if before.as_deref() != Some(id) => {
                written_operation(
                    state,
                    repo,
                    Some(id),
                    Written {
                        kind: OperationKind::Publish,
                        snapshot: None,
                        workspace: None,
                        agent: agent.as_deref().unwrap_or(crate::landing::BATCH_AGENT),
                    },
                )
                .await?
            }
            _ => None,
        };
        Ok(Response::new(PushLandedResponse {
            push: Some(destination_push_message(&row)),
            operation,
        }))
    }
}
