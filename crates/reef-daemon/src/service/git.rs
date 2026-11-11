//! `reef.api.v1.GitService` (#258, ADR 0070): publishing a change to a
//! branch on one of the repository's git remotes. The service gates the
//! request, snapshots and describes first, and builds the response;
//! [`crate::publish`] plans, pushes and records. `FetchGitRemote` (#259,
//! ADR 0071) is [`crate::fetch`]'s.

use std::sync::Arc;
use std::time::Instant;

use reef_api::v1::git_service_server::GitService;
use reef_api::v1::{
    self, FetchGitRemoteRequest, FetchGitRemoteResponse, OperationKind, PublishChangeRequest,
    PublishChangeResponse,
};
use reef_lib::jj::push::{self, PushPlan, PushRequest, PushUpdate};
use tonic::{Request, Response, Status};

use super::changes::write_change;
use super::convert::{self, Written};
use super::reading::{snapshot_first, target, uses_working_copy};
use super::written_operation;
use crate::daemon::AppState;
use crate::publish::{self as publishing, Context, PublishError, PublishInput, hosts};
use crate::workspace::Write;

/// Serves `GitService`.
#[derive(Debug)]
pub struct GitHandler {
    state: Arc<AppState>,
}

impl GitHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> GitHandler {
        GitHandler { state }
    }
}

#[tonic::async_trait]
impl GitService for GitHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn fetch_git_remote(
        &self,
        request: Request<FetchGitRemoteRequest>,
    ) -> Result<Response<FetchGitRemoteResponse>, Status> {
        let agent = super::caller_of(&request)?;
        if super::remote_of(&request).is_some() {
            return Err(crate::error::status(
                crate::fetch::FetchFailure::RemoteDenied,
            ));
        }
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let recorded = crate::fetch::fetch(
            state,
            crate::fetch::FetchInput {
                repo: target.repo.id,
                handle: target.handle.clone(),
                workspace: target.workspace.clone(),
                agent: agent.clone(),
                remote: request.git_remote.clone(),
                branches: request.branches.clone(),
            },
        )
        .await
        .map_err(crate::error::status)?;
        let operation = fetch_operation(state, &target, agent.as_deref(), &recorded).await?;
        Ok(Response::new(FetchGitRemoteResponse {
            fetched: Some(crate::fetch::message(&recorded)),
            operation,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn publish_change(
        &self,
        request: Request<PublishChangeRequest>,
    ) -> Result<Response<PublishChangeResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let remote_caller = super::remote_of(&request).is_some();
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let context = Context::new(state, target.repo.root.clone(), &request.revision);
        let fail = |error: PublishError| publishing::status(&error, &context);
        let config = &state.config;

        // The gate.
        if remote_caller {
            return Err(fail(PublishError::RemoteDenied));
        }
        if !config.file.publish.enabled {
            return Err(fail(PublishError::Disabled));
        }
        if config.identity_is_default() {
            return Err(fail(PublishError::IdentityUnset {
                email: config.identity.email.clone(),
            }));
        }

        // The snapshot and the describe, which a dry run skips: it writes
        // nothing.
        let mut snapshot = None;
        if !request.skip_snapshot && !request.dry_run && !request.revision.trim().is_empty() {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot = snapshot_first(state, &target, needed, arrived, agent.clone())
                .await?
                .operation;
        }
        let describing = !request.description.trim().is_empty() || request.signoff;
        let mut revision = request.revision.clone();
        let mut expected = request.expected_commit_id.clone();
        let mut describe = None;
        let mut dry_description = None;
        if describing && request.dry_run {
            if !request.description.trim().is_empty() {
                dry_description = Some(request.description.clone());
            }
        } else if describing {
            let write = Write::Describe {
                revision: request.revision.clone(),
                description: request.description.clone(),
                expected: request.expected_commit_id.clone(),
                signoff: request.signoff.then(|| config.identity.clone()),
                reset_author: false,
            };
            let (rewritten, operation) = write_change(
                state,
                &target,
                agent.clone(),
                write,
                OperationKind::Describe,
            )
            .await?;
            describe = operation;
            // The plan resolves the described commit by id: a describe
            // hides the commit a commit-id revision named.
            if let Some(info) = &rewritten.change {
                revision = info.commit_id.to_string();
                expected = info.commit_id.to_string();
            }
        }

        let publish_config = &config.file.publish;
        let expected = if expected.trim().is_empty() {
            None
        } else {
            Some(expected.trim().parse().map_err(|_| {
                crate::error::status(reef_lib::jj::rewrite::RewriteError::InvalidCommitId(
                    expected.clone(),
                ))
            })?)
        };
        let input = PublishInput {
            repo: target.repo.id,
            handle: target.handle.clone(),
            workspace: target.workspace.clone(),
            user_email: target.user_email.clone(),
            root: target.repo.root.clone(),
            request: PushRequest {
                revision,
                expected,
                git_remote: request.git_remote.clone(),
                branch: request.branch.clone(),
                branch_prefix: publish_config.branch_prefix.clone(),
                allowed_remotes: publish_config.remotes.clone(),
                force: request.force,
                own_workspace: target.jj_workspace.clone(),
                max_commits: push::MAX_RANGE,
                description: dry_description,
            },
            agent: agent.clone(),
            dry_run: request.dry_run,
            timeout: publish_config.timeout,
        };
        let outcome = publishing::publish(state, input).await.map_err(fail)?;

        let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
        let owner = target
            .workspace
            .as_ref()
            .map(|record| record.owner.as_str());
        let op_id = outcome
            .recorded
            .as_ref()
            .and_then(|recorded| recorded.op_id.as_ref())
            .map(ToString::to_string);
        let operation = written_operation(
            state,
            target.repo.id,
            op_id.as_deref(),
            Written {
                kind: OperationKind::Publish,
                snapshot: None,
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;

        let plan = &outcome.plan;
        let mut notes = Vec::new();
        if plan.update == PushUpdate::UpToDate {
            notes.push(format!(
                "{}/{} already holds this commit; nothing new was pushed.",
                plan.remote, plan.branch
            ));
        }
        if outcome
            .recorded
            .as_ref()
            .is_some_and(|recorded| recorded.local_left)
        {
            notes.push(format!(
                "The change was rewritten during the push, so the local bookmark {} was left where it was; publish again to send the new commit.",
                plan.branch
            ));
        }
        if outcome
            .recorded
            .as_ref()
            .is_some_and(|recorded| recorded.local_moved)
        {
            notes.push(format!(
                "The local bookmark {} moved during the push, so it was left where it was moved to.",
                plan.branch
            ));
        }
        let lists = hosts::HostLists {
            github: &publish_config.github_hosts,
            gitlab: &publish_config.gitlab_hosts,
        };
        let pull_request = plan
            .push_url
            .as_deref()
            .and_then(|url| {
                hosts::hint(
                    url,
                    &plan.branch,
                    plan.base.as_deref(),
                    &plan.description,
                    lists,
                )
            })
            .filter(|hint| hint.host_kind() != v1::GitHostKind::Unspecified);
        let publication = publication(
            plan,
            pull_request,
            outcome
                .pushed
                .map(|pushed| pushed.remote_messages)
                .unwrap_or_default(),
            outcome.published_rows,
            request.dry_run,
        );
        let mut change = outcome.info.as_ref().map(convert::change);
        crate::lifecycle::messages::fill(state, target.repo.id, change.iter_mut(), None);
        Ok(Response::new(PublishChangeResponse {
            publication: Some(publication),
            operation,
            snapshot,
            describe,
            change,
            notes,
        }))
    }
}

/// The v1 update.
fn update(update: PushUpdate) -> v1::PublicationUpdate {
    match update {
        PushUpdate::Created => v1::PublicationUpdate::Created,
        PushUpdate::FastForward => v1::PublicationUpdate::FastForward,
        PushUpdate::Rewritten => v1::PublicationUpdate::Rewritten,
        PushUpdate::Forced => v1::PublicationUpdate::Forced,
        PushUpdate::UpToDate => v1::PublicationUpdate::UpToDate,
    }
}

/// `plan` as a `Publication`.
fn publication(
    plan: &PushPlan,
    pull_request: Option<v1::PullRequestHint>,
    remote_messages: Vec<String>,
    published_rows: u64,
    dry_run: bool,
) -> v1::Publication {
    v1::Publication {
        git_remote: plan.remote.clone(),
        branch: plan.branch.clone(),
        change_id: plan.change.to_string(),
        commit_id: plan.commit.to_string(),
        previous_commit_id: plan
            .previous
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        update: update(plan.update).into(),
        commit_ids: plan
            .range
            .iter()
            .take(push::MAX_LISTED)
            .map(|commit| commit.commit.to_string())
            .collect(),
        commit_count: u32::try_from(plan.range.len()).unwrap_or(u32::MAX),
        change_ids: plan
            .change_ids()
            .iter()
            .take(push::MAX_LISTED)
            .map(ToString::to_string)
            .collect(),
        pull_request,
        remote_messages,
        local_snapshots_published: u32::try_from(published_rows).unwrap_or(u32::MAX),
        dry_run,
    }
}

/// The fetch's operation, read back; `None` when it recorded none.
pub(super) async fn fetch_operation(
    state: &AppState,
    target: &super::reading::Target,
    agent: Option<&str>,
    recorded: &reef_lib::jj::fetch::RecordedFetch,
) -> Result<Option<v1::Operation>, Status> {
    let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
    let owner = target
        .workspace
        .as_ref()
        .map(|record| record.owner.as_str());
    let op_id = recorded.op_id.as_ref().map(ToString::to_string);
    written_operation(
        state,
        target.repo.id,
        op_id.as_deref(),
        Written {
            kind: OperationKind::Fetch,
            snapshot: None,
            workspace: workspace_id.as_deref(),
            agent: agent.or(owner).unwrap_or_default(),
        },
    )
    .await
}
