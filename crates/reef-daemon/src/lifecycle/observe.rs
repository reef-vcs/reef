//! Observed landings (R3.09): the daemon records a recorded change as
//! landed when its commit reaches `trunk()`, however it got there (a push
//! and a fetch, a bookmark moved by the jj CLI). Until the landing service
//! (R3.13), every landing happens outside Reef.
//!
//! One task, woken by every new operation ([`crate::subscribe::OpEvents`]),
//! keeps the commit `trunk()` resolved to in each repository when it last
//! looked. When it moves, or at start, and the repository has recorded
//! changes not landed, it asks the repository at its tip which of them have
//! a visible commit in `::trunk()`, in chunks of 500, and records each as
//! landed in an operation of its own (cause `observed`, actor daemon).
//!
//! It also follows trunk for workspaces (issue #248): every active Reef
//! workspace whose working-copy commit has landed (is in `::trunk()` or
//! under a tag) gets a new empty change on it, so its next edit never
//! amends a landed commit. For a recorded change, that happens in the
//! operation that records the landing; for any other, in an operation per
//! workspace. The snapshot's own check (`begin_snapshot`'s immutable arm)
//! covers the time before this task acts.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use reef_lib::error::ErrorChain;
use reef_lib::jj::lifecycle::on_trunk;
use reef_lib::jj::revset;
use reef_lib::jj::{ChangeIdHex, CommitIdHex, ReadonlyRepo};

use crate::daemon::AppState;
use crate::model::{RepoId, WorkspaceId};
use crate::workspace::{Write, WriteRequest};

/// How long the task waits after a wake-up, so a burst of operations is
/// looked at once.
const SETTLE: Duration = Duration::from_millis(200);

/// Starts the task; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    let mut wakes = state.events.changes();
    tokio::spawn(async move {
        let mut seen: HashMap<RepoId, String> = HashMap::new();
        loop {
            if let Some(state) = weak.upgrade() {
                look(&state, &mut seen).await;
            } else {
                return;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = wakes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(SETTLE) => {}
            }
        }
    });
}

/// Looks at every open repository once; `seen` holds each one's `trunk()`
/// commit as last looked at.
async fn look(state: &AppState, seen: &mut HashMap<RepoId, String>) {
    for repo in state.repos.ids() {
        if let Err(error) = look_at(state, repo, seen).await {
            tracing::warn!(%repo, error = %error, "cannot look for landed proposals");
        }
    }
}

async fn look_at(
    state: &AppState,
    repo: RepoId,
    seen: &mut HashMap<RepoId, String>,
) -> Result<(), String> {
    // Most repositories have no proposal and no workspace: one indexed
    // query, a map read, and nothing else.
    let open = state
        .db
        .unlanded_changes(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    let active = state.manager.active_in(repo);
    if open.is_empty() && active.is_empty() {
        return Ok(());
    }
    let Some(tip) = state.events.tip(repo) else {
        return Ok(());
    };
    let Some(trunk) = revset::trunk(&tip) else {
        seen.remove(&repo);
        return Ok(());
    };
    // The remote's trunk counts too: a git user's local `main` is rarely
    // fast-forwarded, and a workspace whose commit reached `main@origin`
    // has landed (#259).
    let ahead = revset::trunk_ahead(&tip)
        .await
        .map_err(|error| ErrorChain(&error).to_string())?;
    let key = match &ahead {
        Some(ahead) => format!("{} {}", trunk.commit_id, ahead.commit_id),
        None => trunk.commit_id.to_string(),
    };
    if seen.get(&repo) == Some(&key) {
        return Ok(());
    }
    let ids: Vec<ChangeIdHex> = open.iter().filter_map(|id| id.parse().ok()).collect();
    let handle = state
        .manager
        .handle(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    let (found, mut on_landed) = {
        let tip = Arc::clone(&tip);
        state
            .reads
            .run(&handle, move || async move {
                let found = on_trunk(&tip, &ids)
                    .await
                    .map_err(|error| ErrorChain(&error).to_string())?;
                let on_landed = landed_working_copies(&tip, &active).await?;
                Ok::<_, String>((found, on_landed))
            })
            .await
            .map_err(|error| ErrorChain(&error).to_string())??
    };
    for (change, commit) in found {
        // Workspaces on the landed commit move in the landing's operation.
        let (here, elsewhere): (Vec<_>, Vec<_>) = on_landed
            .into_iter()
            .partition(|(_, landed)| *landed == commit);
        on_landed = elsewhere;
        let names = here
            .into_iter()
            .map(|(id, _)| id.jj_workspace_name())
            .collect();
        super::record_observed(
            state,
            repo,
            change.as_str(),
            commit.as_str(),
            (&trunk.bookmark, names),
        )
        .await
        .map_err(|error| ErrorChain(&error).to_string())?;
    }
    for (id, landed) in on_landed {
        start_on_landed(state, repo, id, &landed).await?;
    }
    seen.insert(repo, key);
    Ok(())
}

/// Of `active`, the workspaces whose working-copy commit in `tip`'s view
/// has landed (is immutable), with that commit.
async fn landed_working_copies(
    tip: &ReadonlyRepo,
    active: &[WorkspaceId],
) -> Result<Vec<(WorkspaceId, CommitIdHex)>, String> {
    let mut found = Vec::new();
    for id in active {
        let Some(commit) = reef_lib::jj::view_wc_commit(tip, &id.jj_workspace_name()) else {
            continue;
        };
        if revset::immutable_head(tip, &commit)
            .await
            .map_err(|error| ErrorChain(&error).to_string())?
            .is_some()
        {
            found.push((*id, commit));
        }
    }
    Ok(found)
}

/// Gives workspace `id`, on the landed commit `landed`, a new empty change
/// on it in an operation of its own (issue #248).
async fn start_on_landed(
    state: &AppState,
    repo: RepoId,
    id: WorkspaceId,
    landed: &CommitIdHex,
) -> Result<(), String> {
    let written = state
        .manager
        .rewrite(WriteRequest {
            repo,
            workspace: None,
            agent: None,
            write: Write::StartOnLanded {
                workspaces: vec![id.jj_workspace_name()],
                transitions: Vec::new(),
                description: format!(
                    "reef: new change on landed {} for workspace {id}",
                    landed.short()
                ),
            },
        })
        .await
        .map_err(|error| ErrorChain(&error).to_string())?;
    if let Some(op_id) = written.op_id {
        tracing::info!(workspace_id = %id, %repo, commit_id = %landed, %op_id, "the workspace's commit has landed; started a new change on it");
    }
    Ok(())
}

/// Looks once, now, at `repo` (tests and start).
#[doc(hidden)]
pub async fn look_now(state: &Weak<AppState>, repo: RepoId) -> Result<(), String> {
    let Some(state) = state.upgrade() else {
        return Ok(());
    };
    look_at(&state, repo, &mut HashMap::new()).await
}
