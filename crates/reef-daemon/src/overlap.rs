//! Overlaps (#263, ADR 0079): other open changes that change the paths a
//! workspace's change changes, named in `GetStatus` and `GetWorkspace`.
//! `reef_lib::jj::overlap::find` reads them from the view a read holds;
//! this module applies `[overlaps] max_age`, names the workspaces on each
//! change from the database, and builds the messages. Best effort: a
//! failure is logged and leaves the list empty, never failing the read.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use reef_api::v1;
use reef_lib::jj::ReadonlyRepo;
use reef_lib::jj::overlap::{self, Found, Policy};
use reef_lib::jj::revset::RevsetScope;

use crate::daemon::AppState;
use crate::model::{RepoId, WorkspaceId, WorkspaceRecord, WorkspaceState};
use crate::service::convert::system_timestamp;

/// The Reef workspace a jj workspace name is, if it is one.
fn reef_workspace(name: &str) -> Option<WorkspaceId> {
    name.strip_prefix("reef-")?.parse().ok()
}

/// `repo_id`'s active workspaces; none when they cannot be read.
fn active(state: &AppState, repo_id: RepoId) -> Vec<WorkspaceRecord> {
    state
        .db
        .workspaces(Some(repo_id), &[WorkspaceState::Active])
        .unwrap_or_else(|error| {
            tracing::warn!(repo = %repo_id, error = %reef_lib::error::ErrorChain(&error), "cannot read the active workspaces for overlaps");
            Vec::new()
        })
}

/// The overlaps of `scope.workspace`'s `@` in `repo`, inside a read job at
/// head. Active Reef workspaces' changes count however old; others only
/// within `[overlaps] max_age`.
pub(crate) async fn find(
    state: &AppState,
    repo_id: RepoId,
    repo: &ReadonlyRepo,
    scope: &RevsetScope<'_>,
    should_stop: &dyn Fn() -> bool,
) -> Found {
    let since = SystemTime::now()
        .checked_sub(state.config.file.overlaps.max_age)
        .unwrap_or(UNIX_EPOCH);
    // A `reef-<id>` working copy whose workspace is no longer active (a
    // forced release whose forget failed) does not count as active.
    let live: HashSet<WorkspaceId> = active(state, repo_id).iter().map(|row| row.id).collect();
    let is_active_workspace =
        |name: &str| reef_workspace(name).is_some_and(|id| live.contains(&id));
    let policy = Policy {
        since,
        always: &is_active_workspace,
    };
    overlap::find(repo, scope, &policy, &state.overlaps, should_stop)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read the workspace's overlaps");
            Found::default()
        })
}

/// `found` as messages, each named by a Reef workspace on its change: an
/// active one whose working copy is on the change in the view read, else
/// the newest one ever on it; else by the commit's author.
pub(crate) fn messages(state: &AppState, repo: RepoId, found: Found) -> (Vec<v1::Overlap>, u32) {
    let count = u32::try_from(found.count).unwrap_or(u32::MAX);
    if found.overlaps.is_empty() {
        return (Vec::new(), count);
    }
    let ids: Vec<&str> = found
        .overlaps
        .iter()
        .map(|found| found.change_id.as_str())
        .collect();
    let rows = state
        .db
        .workspaces_on_changes(repo, &ids)
        .unwrap_or_else(|error| {
            tracing::warn!(%repo, error = %reef_lib::error::ErrorChain(&error), "cannot name the workspaces on overlapping changes");
            Vec::new()
        });
    let active = active(state, repo);
    let messages = found
        .overlaps
        .into_iter()
        .map(|found| {
            // Looked up by id: a live row's change id is as of its last
            // snapshot, and may lag the view.
            let live = found
                .working_copies
                .iter()
                .filter_map(|name| reef_workspace(name))
                .find_map(|id| active.iter().find(|row| row.id == id));
            let row = live.or_else(|| {
                rows.iter()
                    .find(|row| row.change_id == found.change_id.as_str())
            });
            let (workspace_id, owner) = match row {
                Some(row) => (row.id.as_hex(), row.owner.clone()),
                None => (String::new(), found.author_email.clone()),
            };
            v1::Overlap {
                change_id: found.change_id.to_string(),
                commit_id: found.commit_id.to_string(),
                workspace_id,
                owner,
                active: live.is_some(),
                path_count: u32::try_from(found.path_count).unwrap_or(u32::MAX),
                paths: found.paths,
                written_at: Some(system_timestamp(found.written_at)),
            }
        })
        .collect();
    (messages, count)
}

/// `record`'s overlaps at the repository's head; empty for a workspace
/// that is not active.
pub(crate) async fn of_workspace(
    state: &Arc<AppState>,
    record: &WorkspaceRecord,
) -> (Vec<v1::Overlap>, u32) {
    if record.state != WorkspaceState::Active {
        return (Vec::new(), 0);
    }
    let repo = record.repo_id;
    let found = async {
        let handle = state.manager.handle(repo).ok()?;
        let root = state.db.repository(repo).ok()?.root;
        let head = state
            .reads
            .current(&handle)
            .await
            .inspect_err(|error| tracing::warn!(%repo, error = %reef_lib::error::ErrorChain(error), "cannot read the workspace's overlaps"))
            .ok()?;
        let (stop, _guard) = state.reads.stop();
        let job_state = Arc::clone(state);
        let workspace = record.id.jj_workspace_name();
        state
            .reads
            .run(&handle, move || async move {
                let user_email = job_state.settings.user_email().to_owned();
                let scope = RevsetScope {
                    workspace: Some(&workspace),
                    user_email: &user_email,
                    root: &root,
                };
                find(&job_state, repo, &head, &scope, &|| stop.requested()).await
            })
            .await
            .inspect_err(|error| tracing::warn!(%repo, error = %reef_lib::error::ErrorChain(error), "cannot read the workspace's overlaps"))
            .ok()
    }
    .await
    .unwrap_or_default();
    messages(state, repo, found)
}
