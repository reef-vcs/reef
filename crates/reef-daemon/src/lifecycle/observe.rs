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

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use reef_lib::error::ErrorChain;
use reef_lib::jj::ChangeIdHex;
use reef_lib::jj::lifecycle::on_trunk;
use reef_lib::jj::revset;

use crate::daemon::AppState;
use crate::model::RepoId;

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
    // Most repositories have no proposal: one indexed query, and nothing
    // else.
    let open = state
        .db
        .unlanded_changes(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    if open.is_empty() {
        return Ok(());
    }
    let Some(tip) = state.events.tip(repo) else {
        return Ok(());
    };
    let Some(trunk) = revset::trunk(&tip) else {
        seen.remove(&repo);
        return Ok(());
    };
    let key = trunk.commit_id.to_string();
    if seen.get(&repo) == Some(&key) {
        return Ok(());
    }
    let ids: Vec<ChangeIdHex> = open.iter().filter_map(|id| id.parse().ok()).collect();
    let handle = state
        .manager
        .handle(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    let found = state
        .reads
        .run(&handle, move || async move { on_trunk(&tip, &ids).await })
        .await
        .map_err(|error| ErrorChain(&error).to_string())?
        .map_err(|error| ErrorChain(&error).to_string())?;
    for (change, commit) in found {
        super::record_observed(
            state,
            repo,
            change.as_str(),
            commit.as_str(),
            &trunk.bookmark,
        )
        .await
        .map_err(|error| ErrorChain(&error).to_string())?;
    }
    seen.insert(repo, key);
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
