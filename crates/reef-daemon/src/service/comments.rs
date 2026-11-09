//! `reef.api.v1.CommentService` (R3.12, ADR 0069): comments on changes,
//! and instructions to the session that made them. The service resolves
//! the request; [`crate::comments`] checks, routes and records.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::comment_service_server::CommentService;
use reef_api::v1::{
    self, ClaimInstructionRequest, ClaimInstructionResponse, CreateCommentRequest,
    CreateCommentResponse, GetCommentRequest, GetCommentResponse, ListCommentsRequest,
    ListCommentsResponse, OperationKind, UpdateInstructionRequest, UpdateInstructionResponse,
};
use reef_lib::jj::{ChangeIdHex, CommitIdHex};
use tonic::{Request, Response, Status};

use super::convert::Written;
use super::page::PageRequest;
use super::reading::{Target, bad_token, snapshot_first, target, uses_working_copy};
use super::written_operation;
use crate::comments::messages::{channel_of, comment_message, kind_of, state_of};
use crate::comments::{
    Anchor, Ceilings, ClaimRequest, CommentError, CreateRequest, MAX_REPLIES, UpdateRequest,
};
use crate::daemon::AppState;
use crate::db::{CommentFilter, CommentRow};
use crate::error::status;
use crate::model::Timestamp;

const LIST_COMMENTS: &str = "/reef.api.v1.CommentService/ListComments";

/// Serves `CommentService`.
#[derive(Debug)]
pub struct CommentHandler {
    state: Arc<AppState>,
}

impl CommentHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> CommentHandler {
        CommentHandler { state }
    }
}

/// A timestamp as milliseconds since the Unix epoch.
fn millis(at: &prost_types::Timestamp) -> i64 {
    at.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(at.nanos) / 1_000_000)
}

/// `rows` as messages, each instruction with its target's last sighting and
/// each anchored comment with whether its file changed since.
pub(super) async fn messages(
    state: &AppState,
    target: &Target,
    rows: &[CommentRow],
) -> Vec<v1::Comment> {
    let mut seen: HashMap<String, Option<Timestamp>> = HashMap::new();
    let mut items = Vec::new();
    let mut anchored = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(path) = &row.path
            && let (Ok(change), Ok(commit)) = (
                row.change_id.parse::<ChangeIdHex>(),
                row.commit_id.parse::<CommitIdHex>(),
            )
        {
            items.push((change, commit, path.clone()));
            anchored.push(index);
        }
    }
    let mut outdated = vec![false; rows.len()];
    if !items.is_empty() {
        let found = async {
            let head = state.reads.current(&target.handle).await.ok()?;
            state
                .reads
                .run(&target.handle, move || async move {
                    reef_lib::jj::comments::outdated(&head, &items).await
                })
                .await
                .ok()
        }
        .await;
        if let Some(found) = found {
            for (index, stale) in anchored.into_iter().zip(found) {
                outdated[index] = stale;
            }
        }
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let last_seen = row
                .instruction
                .as_ref()
                .and_then(|columns| columns.target_client_session.as_ref())
                .and_then(|session| {
                    *seen.entry(session.clone()).or_insert_with(|| {
                        state.db.client_session_last_seen(session).ok().flatten()
                    })
                });
            comment_message(row, last_seen, outdated[index])
        })
        .collect()
}

/// The agent and remote token of `request`.
/// A call's agent, and its remote token as `(id, name)`.
type Caller = (Option<String>, Option<(String, String)>);

fn caller<T>(request: &Request<T>) -> Result<Caller, Status> {
    let agent = super::caller_of(request)?;
    let remote = super::remote_of(request)
        .map(|caller| (caller.token_id.clone(), caller.token_name.clone()));
    Ok((agent, remote))
}

#[tonic::async_trait]
impl CommentService for CommentHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn create_comment(
        &self,
        request: Request<CreateCommentRequest>,
    ) -> Result<Response<CreateCommentResponse>, Status> {
        let arrived = Instant::now();
        let (agent, remote) = caller(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let mut snapshot = None;
        if !request.skip_snapshot && !request.revision.trim().is_empty() {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot = snapshot_first(state, &target, needed, arrived, agent.clone())
                .await?
                .operation;
        }
        let anchor = request
            .anchor
            .as_ref()
            .filter(|anchor| !anchor.path.is_empty() || anchor.start_line > 0)
            .map(|anchor| Anchor {
                path: anchor.path.clone(),
                old: anchor.side() == v1::LineSide::Old,
                start_line: anchor.start_line,
                end_line: anchor.end_line,
            });
        let ceilings = request.ceilings.as_ref().map(|ceilings| Ceilings {
            max_check_wall_ms: ceilings.max_check_wall_ms,
            max_agent_wall_ms: ceilings.max_agent_wall_ms,
            allow_unknown: ceilings.allow_unknown,
        });
        let outcome = crate::comments::create(
            state,
            CreateRequest {
                repo: target.repo.id,
                workspace: target.workspace.clone(),
                user_email: target.user_email.clone(),
                root: target.repo.root.clone(),
                revision: request.revision.clone(),
                expected_commit: request.expected_commit_id.clone(),
                anchor,
                reply_to: request.reply_to.clone(),
                kind: kind_of(request.kind),
                text: request.text.clone(),
                actor: crate::lifecycle::messages::actor_of(request.actor),
                confirm: request.confirm,
                ceilings,
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
        let operation = written_operation(
            state,
            target.repo.id,
            Some(outcome.op_id.as_str()),
            Written {
                kind: OperationKind::Comment,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        let comment = messages(state, &target, std::slice::from_ref(&outcome.comment))
            .await
            .pop();
        Ok(Response::new(CreateCommentResponse {
            comment,
            operation,
            snapshot,
            confirm_refused: outcome.confirm_refused.unwrap_or_default(),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_comment(
        &self,
        request: Request<GetCommentRequest>,
    ) -> Result<Response<GetCommentResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, "")?;
        let id = request.id.trim();
        let row = state
            .db
            .comment(target.repo.id, id)
            .map_err(status)?
            .ok_or_else(|| status(CommentError::NotFound { id: id.to_owned() }))?;
        let replies = state
            .db
            .comment_replies(target.repo.id, id, MAX_REPLIES)
            .map_err(status)?;
        let mut all = vec![row];
        all.extend(replies);
        let mut messages = messages(state, &target, &all).await;
        let comment = messages.remove(0);
        Ok(Response::new(GetCommentResponse {
            comment: Some(comment),
            replies: messages,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn list_comments(
        &self,
        request: Request<ListCommentsRequest>,
    ) -> Result<Response<ListCommentsResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let page = PageRequest::new(
            LIST_COMMENTS,
            request.page_size,
            &request.page_token,
            &ListCommentsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let mut changes = Vec::new();
        for change in &request.changes {
            changes.push(super::operations::change_prefix(change).ok_or_else(|| {
                status(super::operations::InvalidChangeFilter {
                    change: change.clone(),
                })
            })?);
        }
        let mut kinds = Vec::new();
        for value in &request.kinds {
            let kind = kind_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not a comment kind"))
            })?;
            kinds.push(kind.as_str().to_owned());
        }
        let mut states = Vec::new();
        for value in &request.states {
            let state = state_of(*value).ok_or_else(|| {
                Status::invalid_argument(format!("{value} is not an instruction state"))
            })?;
            states.push(state.as_str().to_owned());
        }
        let filter = CommentFilter {
            changes,
            kinds,
            states,
            client_session: Some(request.client_session.trim().to_owned())
                .filter(|session| !session.is_empty()),
            created_after: request
                .created_after
                .as_ref()
                .map(|after| Timestamp(millis(after))),
        };
        let cursor = match page.cursor() {
            None => None,
            Some(text) => {
                let (at, id) = text.split_once(':').ok_or_else(bad_token)?;
                Some((
                    Timestamp(at.parse::<i64>().map_err(|_| bad_token())?),
                    id.to_owned(),
                ))
            }
        };
        let size = page.size();
        let mut rows = state
            .db
            .comment_page(
                target.repo.id,
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match (more, rows.last()) {
            (true, Some(last)) => page.token_at(&format!("{}:{}", last.created_at.0, last.id), ""),
            _ => String::new(),
        };
        Ok(Response::new(ListCommentsResponse {
            comments: messages(state, &target, &rows).await,
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn update_instruction(
        &self,
        request: Request<UpdateInstructionRequest>,
    ) -> Result<Response<UpdateInstructionResponse>, Status> {
        let (agent, remote) = caller(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let outcome = crate::comments::update(
            state,
            UpdateRequest {
                repo: target.repo.id,
                id: request.id.clone(),
                to: state_of(request.to),
                expected: state_of(request.expected_state),
                summary: request.summary.clone(),
                workspace: target.workspace.clone(),
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
        let operation = written_operation(
            state,
            target.repo.id,
            outcome.op_id.as_ref().map(ToString::to_string).as_deref(),
            Written {
                kind: OperationKind::Comment,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        let comment = match &outcome.comment {
            Some(row) => messages(state, &target, std::slice::from_ref(row))
                .await
                .pop(),
            None => None,
        };
        Ok(Response::new(UpdateInstructionResponse {
            comment,
            operation,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn claim_instruction(
        &self,
        request: Request<ClaimInstructionRequest>,
    ) -> Result<Response<ClaimInstructionResponse>, Status> {
        let (agent, remote) = caller(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let outcome = crate::comments::claim(
            state,
            ClaimRequest {
                repo: target.repo.id,
                workspace: target.workspace.clone(),
                user_email: target.user_email.clone(),
                root: target.repo.root.clone(),
                client_session: request.client_session.clone(),
                change: request.change.clone(),
                comment_id: request.comment_id.clone(),
                take: request.take,
                start: request.start,
                channel: channel_of(request.channel),
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
        let operation = written_operation(
            state,
            target.repo.id,
            outcome.op_id.as_ref().map(ToString::to_string).as_deref(),
            Written {
                kind: OperationKind::Comment,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        let comment = match &outcome.comment {
            Some(row) => messages(state, &target, std::slice::from_ref(row))
                .await
                .pop(),
            None => None,
        };
        Ok(Response::new(ClaimInstructionResponse {
            comment,
            operation,
        }))
    }
}
