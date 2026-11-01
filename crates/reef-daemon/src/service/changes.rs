//! `reef.api.v1.ChangeService`: `ListChanges` and `GetChange` (R2.02),
//! `DescribeChange` and `AbandonChange` (R2.03, through
//! [`crate::workspace::Manager::rewrite`]).
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
    AbandonChangeRequest, AbandonChangeResponse, DescribeChangeRequest, DescribeChangeResponse,
    GetChangeRequest, GetChangeResponse, ListChangesRequest, ListChangesResponse, OperationKind,
};
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
        let (entries, more, at) = state
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
                Ok::<_, Status>((entries, found.more, op_id(&repo)))
            })
            .await
            .map_err(status)??;
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
        let (change, at) = state
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
                Ok::<_, Status>((convert::change(&info), op_id(&repo)))
            })
            .await
            .map_err(status)??;
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
        let write = Write::Describe {
            revision: request.revision,
            description: request.description,
            expected: request.expected_commit_id,
        };
        let (rewritten, operation) =
            write_change(&self.state, &target, agent, write, OperationKind::Describe).await?;
        Ok(Response::new(DescribeChangeResponse {
            change: rewritten.change.as_ref().map(convert::change),
            operation,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn abandon_change(
        &self,
        request: Request<AbandonChangeRequest>,
    ) -> Result<Response<AbandonChangeResponse>, Status> {
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let target = target(&self.state, &request.repo, &request.workspace)?;
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
}

/// Runs `write` for `target` and reads back the operation it recorded.
pub(super) async fn write_change(
    state: &AppState,
    target: &super::reading::Target,
    agent: Option<String>,
    write: Write,
    kind: OperationKind,
) -> Result<(crate::workspace::Rewritten, Option<reef_api::v1::Operation>), Status> {
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
