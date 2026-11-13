//! `ChangeService.RebaseChange` (#259, ADR 0071): fetch the destination's
//! branch, refuse a queued change, rebase through `Manager::rewrite`, and
//! report what moved, what conflicts and what the agent does next.

use std::sync::Arc;

use reef_api::v1::{self, OperationKind, RebaseChangeRequest, RebaseChangeResponse};
use reef_lib::jj::conflict::{self, ReadOptions};
use reef_lib::jj::fetch::FetchError;
use reef_lib::jj::lifecycle::{LifecycleState, local_bookmark_exists};
use reef_lib::jj::push::PushError;
use reef_lib::jj::rebase::{self, Action, RebaseMode, RebaseRequest};
use reef_lib::jj::revset::RevisionError;
use reef_lib::jj::{ChangeIdHex, CommitIdHex};
use tonic::{Request, Response, Status};

use super::changes::write_change;
use super::convert::{self, Written};
use super::reading::{Target, target};
use super::written_operation;
use crate::daemon::AppState;
use crate::error::status;
use crate::fetch::{FetchFailure, FetchInput};
use crate::rebase::RebaseRefusal;
use crate::workspace::{RebaseOutcome, Write};

/// The most changes a response lists.
const MAX_LISTED: usize = 100;
/// The most conflicts a response lists.
const MAX_CONFLICTS: usize = 10;
/// The most paths a listed conflict carries.
const MAX_CONFLICT_PATHS: usize = 20;

/// Serves `RebaseChange`.
pub(super) async fn rebase_change(
    state: &Arc<AppState>,
    request: Request<RebaseChangeRequest>,
) -> Result<Response<RebaseChangeResponse>, Status> {
    let agent = super::caller_of(&request)?;
    let remote_caller = super::remote_of(&request).is_some();
    let request = request.into_inner();
    let target = target(state, &request.repo, &request.workspace)?;
    if request.revision.trim().is_empty() {
        return Err(status(RevisionError::Required));
    }
    let mode = match v1::RebaseMode::try_from(request.mode) {
        Ok(v1::RebaseMode::Source) => RebaseMode::Source,
        _ => RebaseMode::Branch,
    };
    let mut notes = Vec::new();

    let (fetched, fetch_operation, fetched_remote) = fetch_first(
        state,
        &target,
        (agent.as_deref(), remote_caller),
        &request,
        &mut notes,
    )
    .await?;

    // A queued change lands exactly the queued commit (R3.13).
    let moving = moving_changes(state, &target, &request, mode).await?;
    for change in &moving {
        let current = crate::lifecycle::state_of_change(state, target.repo.id, change.as_str())
            .map_err(status)?;
        if current == LifecycleState::Queued {
            return Err(status(RebaseRefusal::Queued {
                change: change.to_string(),
            }));
        }
    }

    let write = Write::Rebase {
        revision: request.revision.clone(),
        onto: request.onto.clone(),
        mode,
        expected: request.expected_commit_id.clone(),
        remote: fetched_remote,
    };
    let (rewritten, operation) =
        write_change(state, &target, agent.clone(), write, OperationKind::Rebase).await?;
    let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
    let owner = target
        .workspace
        .as_ref()
        .map(|record| record.owner.as_str());
    let snapshot = written_operation(
        state,
        target.repo.id,
        rewritten.snapshot_op_id.as_deref(),
        Written {
            kind: OperationKind::Snapshot,
            snapshot: Some(crate::snapshot::SnapshotKind::Explicit),
            workspace: workspace_id.as_deref(),
            agent: agent.as_deref().or(owner).unwrap_or_default(),
        },
    )
    .await?;
    let mut change = rewritten.change.as_ref().map(convert::change);
    crate::lifecycle::messages::fill(state, target.repo.id, change.iter_mut(), None).await;
    let Some(outcome) = rewritten.rebase else {
        return Ok(Response::new(RebaseChangeResponse {
            change,
            operation,
            snapshot,
            fetched,
            fetch_operation,
            notes,
            ..RebaseChangeResponse::default()
        }));
    };
    let mut response = report(state, &target, &outcome, notes).await;
    response.change = change;
    response.operation = operation;
    response.snapshot = snapshot;
    response.fetched = fetched;
    response.fetch_operation = fetch_operation;
    Ok(Response::new(response))
}

/// The rebase's fetch, unless `skip_fetch`; never through the remote
/// listener (S-180), which adds a note. A repository with nowhere to fetch
/// from adds a note instead of failing. Returns the fetch, its operation,
/// and the remote it fetched.
async fn fetch_first(
    state: &Arc<AppState>,
    target: &Target,
    (agent, remote_caller): (Option<&str>, bool),
    request: &RebaseChangeRequest,
    notes: &mut Vec<String>,
) -> Result<(Option<v1::Fetched>, Option<v1::Operation>, Option<String>), Status> {
    if request.skip_fetch {
        return Ok((None, None, None));
    }
    if remote_caller {
        notes.push("not fetched: the remote listener never fetches".to_owned());
        return Ok((None, None, None));
    }
    let Some((remote, branches)) = fetch_target(state, target, &request.onto).await? else {
        return Ok((None, None, None));
    };
    let input = FetchInput {
        repo: target.repo.id,
        handle: target.handle.clone(),
        workspace: target.workspace.clone(),
        agent: agent.map(str::to_owned),
        remote,
        branches,
    };
    match crate::fetch::fetch(state, input).await {
        Ok(recorded) => {
            let operation = super::git::fetch_operation(state, target, agent, &recorded).await?;
            Ok((
                Some(crate::fetch::message(&recorded)),
                operation,
                Some(recorded.remote.clone()),
            ))
        }
        // A repository with nowhere to fetch from rebases onto what it has:
        // nothing could be newer.
        Err(FetchFailure::Fetch(FetchError::Plan(PushError::NoGitBackend))) => {
            notes.push("not fetched: the repository has no git backend".to_owned());
            Ok((None, None, None))
        }
        Err(FetchFailure::Fetch(FetchError::Plan(PushError::RemoteRequired { remotes })))
            if remotes.is_empty() =>
        {
            notes.push("not fetched: the repository has no git remote".to_owned());
            Ok((None, None, None))
        }
        Err(error) => Err(status(error)),
    }
}

/// What to fetch for `onto`: `(remote, branches)`, where empty means the
/// plan's choice; `None` to fetch nothing, for a destination that names no
/// bookmark (a commit, a revset).
async fn fetch_target(
    state: &AppState,
    target: &Target,
    onto: &str,
) -> Result<Option<(String, Vec<String>)>, Status> {
    let onto = onto.trim();
    if onto.is_empty() {
        return Ok(Some((String::new(), Vec::new())));
    }
    if let Some((name, remote)) = onto.split_once('@')
        && !name.is_empty()
        && !remote.is_empty()
    {
        return Ok(Some((remote.to_owned(), vec![name.to_owned()])));
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let name = onto.to_owned();
    let exists = state
        .reads
        .run(&target.handle, move || async move {
            local_bookmark_exists(&head, &name)
        })
        .await
        .map_err(status)?;
    Ok(exists.then(|| (String::new(), vec![onto.to_owned()])))
}

/// The changes the rebase would move, as planned now; empty when the plan
/// fails (the write reports why).
async fn moving_changes(
    state: &AppState,
    target: &Target,
    request: &RebaseChangeRequest,
    mode: RebaseMode,
) -> Result<Vec<ChangeIdHex>, Status> {
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let read_target = target.clone();
    let (revision, onto) = (request.revision.clone(), request.onto.clone());
    state
        .reads
        .run(&target.handle, move || async move {
            let target = read_target;
            let request = RebaseRequest {
                revision: &revision,
                onto: (!onto.trim().is_empty()).then_some(onto.as_str()),
                mode,
                expected: None,
                remote: None,
            };
            let Ok(plan) = rebase::plan(&head, &request, &target.scope()).await else {
                return Vec::new();
            };
            let Action::Move { moved, .. } = plan.action else {
                return Vec::new();
            };
            let mut changes = Vec::with_capacity(moved.len());
            for commit in &moved {
                if let Ok(Some(change)) = reef_lib::jj::land::visible_change(&head, commit).await {
                    changes.push(change);
                }
            }
            changes
        })
        .await
        .map_err(status)
}

/// The response's report of `outcome`: what moved, the conflicts it
/// introduced, and the notes.
async fn report(
    state: &AppState,
    target: &Target,
    outcome: &RebaseOutcome,
    mut notes: Vec<String>,
) -> RebaseChangeResponse {
    let destination = &outcome.plan.destination;
    if let Some(note) = &destination.note {
        notes.push(note.clone());
    }
    let changes = &outcome.rebased.changes;
    let workspaces_on = |commit: &CommitIdHex| -> Vec<String> {
        outcome
            .working_copies
            .iter()
            .filter(|(_, wc)| wc == commit)
            .filter_map(|(name, _)| {
                name.strip_prefix(reef_lib::jj::workspace::REEF_WORKSPACE_PREFIX)
                    .map(str::to_owned)
            })
            .collect()
    };
    let rebased: Vec<v1::RebasedChange> = changes
        .iter()
        .take(MAX_LISTED)
        .map(|change| v1::RebasedChange {
            change_id: change.change_id.to_string(),
            old_commit_id: change.old_commit_id.to_string(),
            new_commit_id: change.new_commit_id.to_string(),
            conflicted: change.conflicted,
            became_empty: change.became_empty,
            workspace_ids: workspaces_on(&change.new_commit_id),
        })
        .collect();
    for change in changes.iter().filter(|change| change.became_empty) {
        notes.push(format!(
            "change {} is now empty: {} already has its edits (a squash merge?); abandon it with `reef abandon`, or keep working in it",
            short(&change.change_id),
            destination.name
        ));
    }
    for change in changes {
        if crate::lifecycle::state_of_change(state, target.repo.id, change.change_id.as_str())
            .is_ok_and(|current| current == LifecycleState::Proposed)
        {
            notes.push(format!(
                "change {} was proposed: its proposal is amended; propose it again (`reef propose`) once the rebased tree is ready",
                short(&change.change_id)
            ));
        }
    }
    let introduced: Vec<&rebase::RebasedChange> = changes
        .iter()
        .filter(|change| change.introduces_conflict)
        .collect();
    let inherited = changes
        .iter()
        .filter(|change| change.conflicted && !change.introduces_conflict)
        .count();
    let conflicts = conflict_summaries(state, target, &introduced).await;
    RebaseChangeResponse {
        onto: destination.name.clone(),
        onto_commit_id: destination.commit_id.to_string(),
        rebased,
        rebased_count: u32::try_from(changes.len()).unwrap_or(u32::MAX),
        conflicts,
        conflict_count: u32::try_from(introduced.len()).unwrap_or(u32::MAX),
        inherited_conflict_count: u32::try_from(inherited).unwrap_or(u32::MAX),
        up_to_date: matches!(outcome.plan.action, Action::UpToDate),
        started_new_change: matches!(outcome.plan.action, Action::StartOn { .. }),
        notes,
        ..RebaseChangeResponse::default()
    }
}

/// The first conflicted paths of each of `introduced`, at most
/// [`MAX_CONFLICTS`]; a conflict that cannot be read is listed without
/// paths.
async fn conflict_summaries(
    state: &AppState,
    target: &Target,
    introduced: &[&rebase::RebasedChange],
) -> Vec<v1::RebaseConflict> {
    let wanted: Vec<(ChangeIdHex, CommitIdHex)> = introduced
        .iter()
        .take(MAX_CONFLICTS)
        .map(|change| (change.change_id.clone(), change.new_commit_id.clone()))
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let Ok(head) = state.reads.current(&target.handle).await else {
        return wanted
            .iter()
            .map(|(change, commit)| unread(change, commit))
            .collect();
    };
    let read = state
        .reads
        .run(&target.handle, move || async move {
            let options = ReadOptions {
                limit: MAX_CONFLICT_PATHS,
                ..ReadOptions::default()
            };
            let mut out = Vec::with_capacity(wanted.len());
            for (change, commit) in &wanted {
                out.push(match conflict::read(&head, commit, &options).await {
                    Ok(object) => v1::RebaseConflict {
                        change_id: change.to_string(),
                        commit_id: commit.to_string(),
                        paths: object.files.iter().map(|file| file.path.clone()).collect(),
                        paths_total: object.files_total,
                    },
                    Err(_) => unread(change, commit),
                });
            }
            out
        })
        .await;
    read.unwrap_or_default()
}

fn unread(change: &ChangeIdHex, commit: &CommitIdHex) -> v1::RebaseConflict {
    v1::RebaseConflict {
        change_id: change.to_string(),
        commit_id: commit.to_string(),
        ..v1::RebaseConflict::default()
    }
}

/// A change id as people read it: 12 letters of jj's reverse hex.
fn short(change: &ChangeIdHex) -> String {
    change.reverse_hex().chars().take(12).collect()
}
