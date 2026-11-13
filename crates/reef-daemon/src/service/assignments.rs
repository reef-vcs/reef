//! `ConflictService`'s resolver assignment RPCs (R3.27, ADR 0068): the
//! handlers parse requests, resolve names and callers, and leave the rules
//! to [`crate::assign`].

use std::sync::Arc;

use reef_api::v1::{
    self, AssignConflictRequest, AssignConflictResponse, ClaimConflictRequest,
    ClaimConflictResponse, GetConflictAssignmentRequest, GetConflictAssignmentResponse,
    ListConflictAssignmentsRequest, ListConflictAssignmentsResponse, ReleaseConflictRequest,
    ReleaseConflictResponse,
};
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::convert::{Written, known};
use super::operations::change_prefix;
use super::page::PageRequest;
use super::reading::{Target, bad_token, target};
use super::written_operation;
use crate::assign::messages::{Names, assignment_message, event_message, state_of};
use crate::assign::{self, AssignError, Caller, Named, Outcome, State};
use crate::daemon::AppState;
use crate::db::AssignmentFilter;
use crate::error::status;
use crate::lifecycle::Actor;
use crate::model::{RepoId, WorkspaceRecord};

const LIST: &str = "/reef.api.v1.ConflictService/ListConflictAssignments";
const GET: &str = "/reef.api.v1.ConflictService/GetConflictAssignment";
/// Assignments per page when the request says 0, and at most.
const LIST_DEFAULT: usize = 50;
const LIST_MAX: usize = 200;
/// Events per page when the request says 0, and at most.
const EVENTS_DEFAULT: usize = 100;
const EVENTS_MAX: usize = 1000;

fn size(page_size: i32, page: &PageRequest, default: usize, max: usize) -> usize {
    if page_size == 0 {
        default
    } else {
        page.size().min(max)
    }
}

/// Who a request's caller is, for the events it records.
fn caller_of(
    state: &AppState,
    agent: Option<String>,
    remote: Option<(String, String)>,
    workspace: Option<&WorkspaceRecord>,
    declared: Option<Actor>,
) -> Caller {
    let (identity_id, _, session_id) =
        crate::lifecycle::identity_for(state, workspace, remote.as_ref());
    let (person_identity_id, _, _) = crate::lifecycle::identity_for(state, None, remote.as_ref());
    let plain = agent.as_deref().map(|marked| {
        reef_lib::jj::oplog::RemoteToken::unmark(marked)
            .0
            .to_owned()
    });
    let actor = if remote.is_some() {
        Actor::Agent
    } else {
        declared.unwrap_or(if plain.is_some() {
            Actor::Agent
        } else {
            Actor::Human
        })
    };
    Caller {
        actor: actor.as_str().to_owned(),
        agent: plain,
        marked_agent: agent,
        identity_id,
        person_identity_id,
        session_id,
        remote,
        workspace: workspace.map(|record| record.id.as_hex()),
    }
}

/// The assignment `text` names in `target`'s repository: an id (`asg_`),
/// else a revision naming one change; empty for `empty`.
async fn named(
    state: &AppState,
    target: &Target,
    text: &str,
    empty: Option<Named>,
) -> Result<Named, Status> {
    let text = text.trim();
    if text.is_empty() {
        return empty.ok_or_else(|| status(RevisionError::Required));
    }
    if text.starts_with("asg_") {
        return Ok(Named::Id(text.to_owned()));
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let read_target = target.clone();
    let revision = text.to_owned();
    let resolved = state
        .reads
        .run(&target.handle, move || async move {
            revset::resolve_revision(&head, &revision, &read_target.scope()).await
        })
        .await
        .map_err(status)?
        .map_err(status)?;
    Ok(Named::Change(resolved.change_id.to_string()))
}

/// The response's operation for `outcome`.
async fn operation_of(
    state: &AppState,
    repo: RepoId,
    outcome: &Outcome,
    workspace: Option<&str>,
    agent: &str,
) -> Result<Option<v1::Operation>, Status> {
    let op_id = outcome.op_id.as_ref().map(ToString::to_string);
    written_operation(
        state,
        repo,
        op_id.as_deref(),
        Written {
            kind: v1::OperationKind::ConflictAssignment,
            snapshot: None,
            workspace,
            agent,
        },
    )
    .await
}

pub(super) async fn list(
    state: &Arc<AppState>,
    request: Request<ListConflictAssignmentsRequest>,
) -> Result<Response<ListConflictAssignmentsResponse>, Status> {
    let request = request.into_inner();
    let page = PageRequest::new(
        LIST,
        request.page_size,
        &request.page_token,
        &ListConflictAssignmentsRequest {
            page_size: 0,
            page_token: String::new(),
            ..request.clone()
        },
    )
    .map_err(status)?;
    let limit = size(request.page_size, &page, LIST_DEFAULT, LIST_MAX);
    let offset = match page.cursor() {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| bad_token())?,
        None => 0,
    };
    let target = target(state, &request.repo, &request.workspace)?;
    let repo = target.repo.id;
    assign::look(state, repo).await.map_err(status)?;
    let mut states = Vec::with_capacity(request.states.len());
    for value in &request.states {
        let _: v1::ConflictAssignmentState = known("states", *value).map_err(status)?;
        if let Some(state) = state_of(*value) {
            states.push(state.as_str().to_owned());
        }
    }
    let mut change_prefixes = Vec::with_capacity(request.change_ids.len());
    for change in &request.change_ids {
        let prefix = change_prefix(change).ok_or_else(|| {
            status(super::operations::InvalidChangeFilter {
                change: change.clone(),
            })
        })?;
        change_prefixes.push(prefix);
    }
    let identity_id = match request.identity.trim() {
        "" => None,
        text => Some(
            state
                .db
                .identity(text)
                .map_err(status)?
                .map_or_else(|| text.to_owned(), |row| row.id),
        ),
    };
    let filter = AssignmentFilter {
        states,
        workspace_id: target.workspace.as_ref().map(|record| record.id.as_hex()),
        change_prefixes,
        agent: Some(request.agent.clone()).filter(|agent| !agent.is_empty()),
        identity_id,
    };
    let mut rows = state
        .db
        .list_assignments(repo, &filter, offset, limit + 1)
        .map_err(status)?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let mut names = Names::default();
    let mut assignments = Vec::with_capacity(rows.len());
    for row in &rows {
        let position = if row.state == State::Queued.as_str() {
            state.db.queue_position(repo, row).map_err(status)?
        } else {
            0
        };
        assignments.push(assignment_message(state, &mut names, row, position));
    }
    let next_page_token = if more {
        page.token_at(&(offset + rows.len()).to_string(), "")
    } else {
        String::new()
    };
    Ok(Response::new(ListConflictAssignmentsResponse {
        assignments,
        next_page_token,
    }))
}

pub(super) async fn get(
    state: &Arc<AppState>,
    request: Request<GetConflictAssignmentRequest>,
) -> Result<Response<GetConflictAssignmentResponse>, Status> {
    let request = request.into_inner();
    let page = PageRequest::new(
        GET,
        request.page_size,
        &request.page_token,
        &GetConflictAssignmentRequest {
            page_size: 0,
            page_token: String::new(),
            ..request.clone()
        },
    )
    .map_err(status)?;
    let limit = size(request.page_size, &page, EVENTS_DEFAULT, EVENTS_MAX);
    let after = match page.cursor() {
        Some(cursor) => cursor.parse::<i64>().map_err(|_| bad_token())?,
        None => 0,
    };
    let target = target(state, &request.repo, &request.workspace)?;
    let repo = target.repo.id;
    assign::look(state, repo).await.map_err(status)?;
    let which = named(state, &target, &request.assignment, None).await?;
    let row = match &which {
        Named::Id(id) => state.db.assignment(repo, id).map_err(status)?,
        Named::Change(change) => state
            .db
            .assignment_of_change(repo, change)
            .map_err(status)?,
        Named::QueueHead => None,
    }
    .ok_or_else(|| {
        status(AssignError::NotFound {
            assignment: request.assignment.trim().to_owned(),
        })
    })?;
    let mut events = state
        .db
        .assignment_events(repo, &row.id, after, limit + 1)
        .map_err(status)?;
    let more = events.len() > limit;
    events.truncate(limit);
    let mut names = Names::default();
    let position = if row.state == State::Queued.as_str() {
        state.db.queue_position(repo, &row).map_err(status)?
    } else {
        0
    };
    let next_page_token = match (more, events.last()) {
        (true, Some(last)) => page.token_at(&last.seq.to_string(), ""),
        _ => String::new(),
    };
    Ok(Response::new(GetConflictAssignmentResponse {
        assignment: Some(assignment_message(state, &mut names, &row, position)),
        events: events
            .iter()
            .map(|event| event_message(state, &mut names, event))
            .collect(),
        next_page_token,
    }))
}

pub(super) async fn claim(
    state: &Arc<AppState>,
    request: Request<ClaimConflictRequest>,
) -> Result<Response<ClaimConflictResponse>, Status> {
    let agent = super::caller_of(&request)?;
    let remote_caller = super::remote_of(&request).cloned();
    let remote = remote_caller
        .as_ref()
        .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
    let request = request.into_inner();
    if request.workspace.trim().is_empty() {
        return Err(status(AssignError::NoLiveSession {
            workspace: String::new(),
        }));
    }
    let target = target(state, &request.repo, &request.workspace)?;
    let Some(workspace) = target.workspace.clone() else {
        return Err(status(AssignError::NoLiveSession {
            workspace: request.workspace.clone(),
        }));
    };
    crate::remote::rules::check_owned(remote_caller.as_ref(), &workspace)?;
    let which = named(state, &target, &request.assignment, Some(Named::QueueHead)).await?;
    let caller = caller_of(state, agent.clone(), remote, Some(&workspace), None);
    let outcome = assign::claim(
        state,
        &workspace,
        &which,
        request.expected_conflict_id.trim(),
        &caller,
    )
    .await
    .map_err(status)?;
    let workspace_id = workspace.id.as_hex();
    let operation = operation_of(
        state,
        target.repo.id,
        &outcome,
        Some(&workspace_id),
        agent.as_deref().unwrap_or(workspace.owner.as_str()),
    )
    .await?;
    let change = change_of(state, &target, &outcome.row.change_id).await;
    let mut names = Names::default();
    Ok(Response::new(ClaimConflictResponse {
        assignment: Some(assignment_message(state, &mut names, &outcome.row, 0)),
        change,
        operation,
    }))
}

/// The change `change_id` at the head, as `GetChange` gives it; `None`
/// when it has no visible commit or cannot be read.
async fn change_of(state: &AppState, target: &Target, change_id: &str) -> Option<v1::Change> {
    let Ok(change) = change_id.parse::<reef_lib::jj::ChangeIdHex>() else {
        return None;
    };
    let head = state.reads.current(&target.handle).await.ok()?;
    let read_target = target.clone();
    let info = state
        .reads
        .run(&target.handle, move || async move {
            let visible = reef_lib::jj::assignment::visible_commit(&head, &change)
                .await
                .ok()??;
            let describer = reef_lib::jj::log::Describer::new(&head, &read_target.scope()).ok()?;
            describer.describe(&visible.commit).await.ok()
        })
        .await
        .ok()??;
    let mut change = super::convert::change(&info);
    crate::lifecycle::messages::fill(state, target.repo.id, std::iter::once(&mut change), None)
        .await;
    Some(change)
}

pub(super) async fn release(
    state: &Arc<AppState>,
    request: Request<ReleaseConflictRequest>,
) -> Result<Response<ReleaseConflictResponse>, Status> {
    let agent = super::caller_of(&request)?;
    let remote_caller = super::remote_of(&request).cloned();
    let remote = remote_caller
        .as_ref()
        .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
    let request = request.into_inner();
    let target = target(state, &request.repo, &request.workspace)?;
    if let Some(workspace) = &target.workspace {
        crate::remote::rules::check_owned(remote_caller.as_ref(), workspace)?;
    }
    let which = match (request.assignment.trim().is_empty(), &target.workspace) {
        // The one assignment the workspace's session owns.
        (true, Some(workspace)) => {
            let (owned, count) = state
                .db
                .assignments_owned_by(&workspace.id.as_hex(), 20)
                .map_err(status)?;
            match (owned.as_slice(), count) {
                ([one], 1) => Named::Id(one.id.clone()),
                _ => {
                    return Err(status(AssignError::WhichAssignment {
                        owned: owned.iter().map(|row| row.id.clone()).collect(),
                    }));
                }
            }
        }
        _ => named(state, &target, &request.assignment, None).await?,
    };
    let caller = caller_of(
        state,
        agent.clone(),
        remote,
        target.workspace.as_ref(),
        None,
    );
    let outcome = assign::release(
        state,
        target.repo.id,
        target.workspace.as_ref(),
        &which,
        &request.reason,
        &caller,
    )
    .await
    .map_err(status)?;
    let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
    let owner = target
        .workspace
        .as_ref()
        .map(|record| record.owner.as_str());
    let operation = operation_of(
        state,
        target.repo.id,
        &outcome,
        workspace_id.as_deref(),
        agent.as_deref().or(owner).unwrap_or_default(),
    )
    .await?;
    let mut names = Names::default();
    Ok(Response::new(ReleaseConflictResponse {
        assignment: Some(assignment_message(state, &mut names, &outcome.row, 0)),
        operation,
    }))
}

pub(super) async fn assign(
    state: &Arc<AppState>,
    request: Request<AssignConflictRequest>,
) -> Result<Response<AssignConflictResponse>, Status> {
    let agent = super::caller_of(&request)?;
    let remote_caller = super::remote_of(&request).cloned();
    let remote = remote_caller
        .as_ref()
        .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
    let request = request.into_inner();
    let target = target(state, &request.repo, &request.caller_workspace)?;
    // A remote caller names only its own workspace as the one it works in
    // (the event's session and the operation's workspace come from it).
    if let Some(workspace) = &target.workspace {
        crate::remote::rules::check_owned(remote_caller.as_ref(), workspace)?;
    }
    let declared = crate::lifecycle::messages::actor_of(request.actor);
    let caller = caller_of(
        state,
        agent.clone(),
        remote,
        target.workspace.as_ref(),
        declared,
    );
    let to = match &request.target {
        Some(v1::assign_conflict_request::Target::Workspace(text)) => {
            let record = state
                .manager
                .resolve_workspace(&target.repo.id.as_hex(), text)
                .map_err(status)?;
            assign::Target::Workspace(Box::new(record))
        }
        Some(v1::assign_conflict_request::Target::Queue(true)) => assign::Target::Queue,
        Some(v1::assign_conflict_request::Target::Take(true)) => assign::Target::Take,
        _ => assign::Target::Priority,
    };
    let which = named(state, &target, &request.assignment, None).await?;
    let outcome = assign::assign(
        state,
        target.repo.id,
        assign::AssignRequest {
            named: &which,
            expected: request.expected_conflict_id.trim(),
            target: &to,
            priority: request.priority,
            reason: &request.reason,
        },
        &caller,
    )
    .await
    .map_err(status)?;
    let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
    let owner = target
        .workspace
        .as_ref()
        .map(|record| record.owner.as_str());
    let operation = operation_of(
        state,
        target.repo.id,
        &outcome,
        workspace_id.as_deref(),
        agent.as_deref().or(owner).unwrap_or_default(),
    )
    .await?;
    let mut names = Names::default();
    Ok(Response::new(AssignConflictResponse {
        assignment: Some(assignment_message(state, &mut names, &outcome.row, 0)),
        operation,
    }))
}

/// Sets `Conflict.assignment` on each of `conflicts` from the open
/// assignments of their changes, as they are now (R3.27).
pub(super) fn fill(state: &AppState, repo: RepoId, conflicts: &mut [v1::Conflict]) {
    let changes: Vec<String> = conflicts
        .iter()
        .filter_map(|conflict| conflict.change.as_ref().map(|c| c.change_id.clone()))
        .collect();
    let rows = match state.db.open_assignments_of_changes(repo, &changes) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read conflict assignments");
            return;
        }
    };
    let mut names = Names::default();
    for conflict in conflicts {
        let Some(change) = conflict.change.as_ref().map(|c| c.change_id.as_str()) else {
            continue;
        };
        if let Some(row) = rows
            .iter()
            .find(|row| row.change_id == change && row.kind == assign::KIND_TREE)
        {
            conflict.assignment = Some(assignment_message(state, &mut names, row, 0));
        }
    }
}
