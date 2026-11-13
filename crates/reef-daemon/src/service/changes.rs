//! `reef.api.v1.ChangeService`: `ListChanges` and `GetChange` (R2.02),
//! `DescribeChange` and `AbandonChange` (R2.03, through
//! [`crate::workspace::Manager::rewrite`]), and the lifecycle's
//! `TransitionChange`, `ListProposals` and `ListTransitions` (R3.09,
//! through [`crate::lifecycle`]), and `RebaseChange` (#259, in
//! `service::rebase`). Every `Change` returned carries its
//! lifecycle.
//!
//! A log page token pins the operation the first page read, and its cursor
//! is `<entries so far>:<last commit id>`: the next page re-evaluates the
//! revset there, skips as many entries, and checks it ended on that
//! commit.

use std::sync::Arc;
use std::time::Instant;

use prost::Message as _;
use reef_api::v1::change_service_server::ChangeService;
use reef_api::v1::{
    self, AbandonChangeRequest, AbandonChangeResponse, DescribeChangeRequest,
    DescribeChangeResponse, GetChangeRequest, GetChangeResponse, ListChangesRequest,
    ListChangesResponse, ListProposalsRequest, ListProposalsResponse, ListTransitionsRequest,
    ListTransitionsResponse, OperationKind, RebaseChangeRequest, RebaseChangeResponse,
    TransitionChangeRequest, TransitionChangeResponse,
};
use reef_lib::jj::lifecycle::LifecycleState;
use reef_lib::jj::log::{self, Describer};
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::convert;
use super::convert::Written;
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{
    bad_token, op_id, positional, repo_at, snapshot_first, target, uses_working_copy,
};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::workspace::{Write, WriteRequest};

const LIST_CHANGES: &str = "/reef.api.v1.ChangeService/ListChanges";
const LIST_PROPOSALS: &str = "/reef.api.v1.ChangeService/ListProposals";
const LIST_TRANSITIONS: &str = "/reef.api.v1.ChangeService/ListTransitions";

/// Serves `ChangeService`.
#[derive(Debug)]
pub struct ChangesHandler {
    state: Arc<AppState>,
}

impl ChangesHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> ChangesHandler {
        ChangesHandler { state }
    }
}

#[tonic::async_trait]
impl ChangeService for ChangesHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn rebase_change(
        &self,
        request: Request<RebaseChangeRequest>,
    ) -> Result<Response<RebaseChangeResponse>, Status> {
        super::rebase::rebase_change(&self.state, request).await
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_changes(
        &self,
        request: Request<ListChangesRequest>,
    ) -> Result<Response<ListChangesResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_CHANGES,
            request.page_size,
            &request.page_token,
            &ListChangesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let revset_text = if request.revset.trim().is_empty() {
            revset::DEFAULT_LOG.to_owned()
        } else {
            request.revset.clone()
        };
        let (skip, expected) = match page.cursor() {
            Some(cursor) if page.pinned().is_some() => positional(cursor)?,
            Some(_) => return Err(bad_token()),
            None => (0, String::new()),
        };
        // Only a first page at head snapshots: the default set holds every
        // working copy, so an empty revset counts as using `@`.
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = request.revset.trim().is_empty()
                || uses_working_copy(state, &target, vec![request.revset.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let limit = page.size();
        let pinned = page.pinned().map(str::to_owned);
        let at_operation = request.at_operation.clone();
        let read_target = target.clone();
        let entries = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let repo = repo_at(
                    &reads,
                    target.repo.id,
                    &head,
                    pinned.as_deref(),
                    &at_operation,
                )
                .await?;
                let parsed = revset::parse(&repo, &revset_text, &target.scope()).map_err(status)?;
                let found = log::log_page(&repo, &parsed, skip, limit, &|| stop.requested())
                    .await
                    .map_err(status)?;
                if skip > 0
                    && found
                        .last_skipped
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref()
                        != Some(expected.as_str())
                {
                    return Err(bad_token());
                }
                let describer = Describer::new(&repo, &target.scope()).map_err(status)?;
                let mut entries = Vec::with_capacity(found.nodes.len());
                for node in &found.nodes {
                    let info = describer.describe(&node.commit_id).await.map_err(status)?;
                    entries.push(convert::log_entry(&info, node));
                }
                Ok::<_, Status>((entries, found.more, op_id(&repo), op_end(&repo)))
            })
            .await
            .map_err(status)??;
        let (mut entries, more, at, ended) = (entries.0, entries.1, entries.2, entries.3);
        let as_of = (!request.at_operation.is_empty() || page.pinned().is_some())
            .then(|| (ended, at.clone()));
        crate::lifecycle::messages::fill(
            state,
            target.repo.id,
            entries.iter_mut().filter_map(|entry| entry.change.as_mut()),
            as_of,
        )
        .await;
        let mut kept = Vec::with_capacity(entries.len());
        let mut bytes = 0;
        let mut trimmed = false;
        for entry in entries {
            let size = entry.encoded_len();
            if !kept.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            kept.push(entry);
        }
        let next_page_token = match kept.last() {
            Some(last) if more || trimmed => {
                let commit = last
                    .change
                    .as_ref()
                    .map(|change| change.commit_id.clone())
                    .unwrap_or_default();
                page.token_at(&format!("{}:{commit}", skip + kept.len()), &at)
            }
            _ => String::new(),
        };
        Ok(Response::new(ListChangesResponse {
            entries: kept,
            next_page_token,
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_change(
        &self,
        request: Request<GetChangeRequest>,
    ) -> Result<Response<GetChangeResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        if request.revision.trim().is_empty() {
            return Err(status(RevisionError::Required));
        }
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        if request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let read_target = target.clone();
        let at_past = !request.at_operation.is_empty();
        let change = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let repo =
                    repo_at(&reads, target.repo.id, &head, None, &request.at_operation).await?;
                let resolved = revset::resolve_revision(&repo, &request.revision, &target.scope())
                    .await
                    .map_err(status)?;
                let describer = Describer::new(&repo, &target.scope()).map_err(status)?;
                let info = describer
                    .describe(&resolved.commit_id)
                    .await
                    .map_err(status)?;
                Ok::<_, Status>((convert::change(&info), op_id(&repo), op_end(&repo)))
            })
            .await
            .map_err(status)??;
        let (mut change, at, ended) = change;
        let as_of = at_past.then(|| (ended, at.clone()));
        crate::lifecycle::messages::fill(
            state,
            target.repo.id,
            std::iter::once(&mut change),
            as_of,
        )
        .await;
        Ok(Response::new(GetChangeResponse {
            change: Some(change),
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn describe_change(
        &self,
        request: Request<DescribeChangeRequest>,
    ) -> Result<Response<DescribeChangeResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let target = target(&self.state, &request.repo, &request.workspace)?;
        if (request.signoff || request.reset_author) && self.state.config.identity_is_default() {
            let context = crate::publish::Context::new(
                &self.state,
                target.repo.root.clone(),
                &request.revision,
            );
            return Err(crate::publish::status(
                &crate::publish::PublishError::IdentityUnset {
                    email: self.state.config.identity.email.clone(),
                },
                &context,
            ));
        }
        let write = Write::Describe {
            revision: request.revision,
            description: request.description,
            expected: request.expected_commit_id,
            signoff: request.signoff.then(|| self.state.config.identity.clone()),
            reset_author: request.reset_author,
        };
        let (rewritten, operation) =
            write_change(&self.state, &target, agent, write, OperationKind::Describe).await?;
        let mut change = rewritten.change.as_ref().map(convert::change);
        crate::lifecycle::messages::fill(&self.state, target.repo.id, change.iter_mut(), None)
            .await;
        Ok(Response::new(DescribeChangeResponse { change, operation }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn abandon_change(
        &self,
        request: Request<AbandonChangeRequest>,
    ) -> Result<Response<AbandonChangeResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let target = target(&self.state, &request.repo, &request.workspace)?;
        check_abandon(&self.state, &target, &request.revision).await?;
        let write = Write::Abandon {
            revision: request.revision,
            expected: request.expected_commit_id,
        };
        let (rewritten, operation) =
            write_change(&self.state, &target, agent, write, OperationKind::Abandon).await?;
        Ok(Response::new(AbandonChangeResponse {
            abandoned_commit_id: rewritten
                .abandoned
                .map(|id| id.to_string())
                .unwrap_or_default(),
            operation,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn transition_change(
        &self,
        request: Request<TransitionChangeRequest>,
    ) -> Result<Response<TransitionChangeResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let remote = super::remote_of(&request)
            .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
        let request = request.into_inner();
        let state = &self.state;
        let messages = &crate::lifecycle::messages::state_of;
        let to = messages(request.to)
            .ok_or_else(|| status(crate::lifecycle::LifecycleError::TargetRequired))?;
        let target = target(state, &request.repo, &request.workspace)?;
        let mut snapshot = None;
        if !request.skip_snapshot && !request.revision.trim().is_empty() {
            let mut texts = vec![request.revision.clone()];
            if !request.superseded_by.trim().is_empty() {
                texts.push(request.superseded_by.clone());
            }
            let needed = uses_working_copy(state, &target, texts).await?;
            snapshot = snapshot_first(state, &target, needed, arrived, agent.clone())
                .await?
                .operation;
        }
        let outcome = crate::lifecycle::transition(
            state,
            crate::lifecycle::TransitionRequest {
                repo: target.repo.id,
                workspace: target.workspace.clone(),
                user_email: target.user_email.clone(),
                root: target.repo.root.clone(),
                revision: request.revision.clone(),
                to,
                expected_state: messages(request.expected_state),
                expected_commit: request.expected_commit_id.clone(),
                reason: request.reason.clone(),
                superseded_by: request.superseded_by.clone(),
                destination: request.destination.clone(),
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
        let op_id = outcome.op_id.as_ref().map(ToString::to_string);
        let operation = written_operation(
            state,
            target.repo.id,
            op_id.as_deref(),
            Written {
                kind: OperationKind::Transition,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        let mut change = outcome.change.as_ref().map(convert::change);
        crate::lifecycle::messages::fill(state, target.repo.id, change.iter_mut(), None).await;
        let lifecycle = match &change {
            Some(change) => change.lifecycle.clone(),
            // A recorded change with no visible commit.
            None => outcome.lifecycle.as_ref().map(|row| {
                let last = outcome.transition.as_ref();
                crate::lifecycle::messages::lifecycle_message(row, last, None)
            }),
        };
        Ok(Response::new(TransitionChangeResponse {
            lifecycle,
            transition: outcome
                .transition
                .as_ref()
                .map(crate::lifecycle::messages::transition_message),
            operation,
            snapshot,
            change,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_proposals(
        &self,
        request: Request<ListProposalsRequest>,
    ) -> Result<Response<ListProposalsResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_PROPOSALS,
            request.page_size,
            &request.page_token,
            &ListProposalsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let mut states = Vec::new();
        for value in &request.states {
            let state = crate::lifecycle::messages::state_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not a lifecycle state"))
            })?;
            states.push(state.as_str().to_owned());
        }
        if states.is_empty() {
            states = vec![
                LifecycleState::Proposed.as_str().to_owned(),
                LifecycleState::Queued.as_str().to_owned(),
            ];
        }
        let filter = crate::db::LifecycleFilter {
            states,
            changes: change_prefixes(&request.changes)?,
            agents: request.agents.clone(),
        };
        let after = match page.cursor() {
            None => None,
            Some(text) => {
                let (at, change) = text.split_once(':').ok_or_else(bad_token)?;
                Some((
                    crate::model::Timestamp(at.parse().map_err(|_| bad_token())?),
                    change.to_owned(),
                ))
            }
        };
        let size = page.size();
        // Rows read here and changes read after must agree (#293).
        crate::lifecycle::messages::await_pending(state, target.repo.id, None).await;
        let mut rows = crate::lifecycle::proposals(state, target.repo.id, &filter, after, size + 1)
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last()) {
            (true, Some(last)) => {
                page.token_at(&format!("{}:{}", last.updated_at.0, last.change_id), "")
            }
            _ => String::new(),
        };
        let changes = changes_at_head(state, &target, &rows).await?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let last = state
                .db
                .transition(target.repo.id, &row.transition_id)
                .map_err(status)?;
            let change = changes.get(&row.change_id).cloned();
            let lifecycle = crate::lifecycle::messages::lifecycle_message(
                row,
                last.as_ref(),
                change
                    .as_ref()
                    .map(|change: &v1::Change| change.commit_id.as_str()),
            );
            let change = change.map(|mut change| {
                change.lifecycle = Some(lifecycle.clone());
                change
            });
            entries.push(v1::ProposalEntry {
                change_id: row.change_id.clone(),
                lifecycle: Some(lifecycle),
                change,
                proposed_by: row.proposed_by_agent.clone().unwrap_or_default(),
            });
        }
        Ok(Response::new(ListProposalsResponse {
            entries,
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_transitions(
        &self,
        request: Request<ListTransitionsRequest>,
    ) -> Result<Response<ListTransitionsResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_TRANSITIONS,
            request.page_size,
            &request.page_token,
            &ListTransitionsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let mut to = Vec::new();
        for value in &request.to {
            let state = crate::lifecycle::messages::state_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not a lifecycle state"))
            })?;
            to.push(state.as_str().to_owned());
        }
        let mut causes = Vec::new();
        for value in &request.causes {
            let cause = crate::lifecycle::messages::cause_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not a transition cause"))
            })?;
            causes.push(cause.as_str().to_owned());
        }
        let filter = crate::db::TransitionFilter {
            changes: change_prefixes(&request.changes)?,
            to,
            causes,
            recorded_after: request.recorded_after.map(|after| {
                crate::model::Timestamp(
                    after
                        .seconds
                        .saturating_mul(1000)
                        .saturating_add(i64::from(after.nanos) / 1_000_000),
                )
            }),
        };
        let before = match page.cursor() {
            None => None,
            Some(text) => Some(text.parse::<i64>().map_err(|_| bad_token())?),
        };
        let size = page.size();
        let mut rows = state
            .db
            .transition_page(target.repo.id, &filter, before, size + 1)
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last().and_then(|row| row.seq)) {
            (true, Some(seq)) => page.token_at(&seq.to_string(), ""),
            _ => String::new(),
        };
        Ok(Response::new(ListTransitionsResponse {
            transitions: rows
                .iter()
                .map(crate::lifecycle::messages::transition_message)
                .collect(),
            next_page_token,
        }))
    }
}

/// The end of the operation `repo` is loaded at, for a lifecycle as of it.
fn op_end(repo: &reef_lib::jj::ReadonlyRepo) -> crate::model::Timestamp {
    crate::model::Timestamp::from(reef_lib::jj::oplog::entry(repo.operation()).end)
}

/// Change id prefixes in hex or reverse hex, as hex.
fn change_prefixes(changes: &[String]) -> Result<Vec<String>, Status> {
    changes
        .iter()
        .map(|change| {
            super::operations::change_prefix(change).ok_or_else(|| {
                status(super::operations::InvalidChangeFilter {
                    change: change.clone(),
                })
            })
        })
        .collect()
}

/// The visible changes of `rows` at head, by change id; a change with no
/// visible commit is left out.
async fn changes_at_head(
    state: &AppState,
    target: &super::reading::Target,
    rows: &[crate::db::LifecycleRow],
) -> Result<std::collections::HashMap<String, v1::Change>, Status> {
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let ids: Vec<String> = rows.iter().map(|row| row.change_id.clone()).collect();
    let read_target = target.clone();
    state
        .reads
        .run(&target.handle, move || async move {
            let target = read_target;
            let describer = Describer::new(&head, &target.scope()).map_err(status)?;
            let mut found = std::collections::HashMap::new();
            for id in ids {
                let Ok(resolved) = revset::resolve_revision(
                    &head,
                    &format!("latest(change_id({}))", reverse(&id)),
                    &target.scope(),
                )
                .await
                else {
                    continue;
                };
                let info = describer
                    .describe(&resolved.commit_id)
                    .await
                    .map_err(status)?;
                found.insert(id, convert::change(&info));
            }
            Ok::<_, Status>(found)
        })
        .await
        .map_err(status)?
}

/// A hex change id in jj's reverse hex.
fn reverse(hex: &str) -> String {
    reef_client::json::reverse_hex(hex)
}

/// `AbandonChange`'s refusal of a change in review (R3.09): resolves the
/// revision at head and checks its lifecycle. A revision that does not
/// resolve is left to the abandon, which reports it.
async fn check_abandon(
    state: &AppState,
    target: &super::reading::Target,
    revision: &str,
) -> Result<(), Status> {
    if revision.trim().is_empty() {
        return Ok(());
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let read_target = target.clone();
    let revision = revision.to_owned();
    let found = state
        .reads
        .run(&target.handle, move || async move {
            let target = read_target;
            let resolved = revset::resolve_revision(&head, &revision, &target.scope())
                .await
                .ok()?;
            let info = Describer::new(&head, &target.scope())
                .ok()?
                .describe(&resolved.commit_id)
                .await
                .ok()?;
            Some((info.change_id.to_string(), info.divergent))
        })
        .await
        .map_err(status)?;
    if let Some((change, divergent)) = found {
        crate::lifecycle::check_abandon(state, target.repo.id, &change, divergent)
            .map_err(status)?;
    }
    Ok(())
}

/// Runs `write` for `target` and reads back the operation it recorded.
pub(super) async fn write_change(
    state: &AppState,
    target: &super::reading::Target,
    agent: Option<String>,
    write: Write,
    kind: OperationKind,
) -> Result<(crate::workspace::Rewritten, Option<v1::Operation>), Status> {
    let rewritten = state
        .manager
        .rewrite(WriteRequest {
            repo: target.repo.id,
            workspace: target.workspace.as_ref().map(|record| record.id),
            agent: agent.clone(),
            write,
        })
        .await
        .map_err(status)?;
    let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
    let owner = target
        .workspace
        .as_ref()
        .map(|record| record.owner.as_str());
    let operation = written_operation(
        state,
        target.repo.id,
        rewritten.op_id.as_deref(),
        Written {
            kind,
            snapshot: None,
            workspace: workspace_id.as_deref(),
            agent: agent.as_deref().or(owner).unwrap_or_default(),
        },
    )
    .await?;
    Ok((rewritten, operation))
}
