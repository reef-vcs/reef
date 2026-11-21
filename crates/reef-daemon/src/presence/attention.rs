//! `ATTENTION` entries (R3.33, #145): one per active workspace whose
//! attention is set, made to match the rows whenever a writer of them wakes
//! the task (`Db::attention_changed`), at start, and every [`TICK`].
//! Reconciling is idempotent, so a missed or doubled wake costs at most one
//! tick. No snapshot call site changes: the writers wake it through `Db`.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use reef_api::v1::{self, PresenceEndReason, PresenceKind};

use super::{DaemonKey, PresenceHub, Report, STATE_TTL};
use crate::daemon::AppState;
use crate::model::{RepoId, WorkspaceId, WorkspaceState};

/// The backstop between wakes.
const TICK: Duration = Duration::from_secs(30);
/// Writes come in runs (a move, then its snapshot): wait this long after a
/// wake before reading.
const SETTLE: Duration = Duration::from_millis(100);

/// An active workspace that needs attention, as its row says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Needing {
    pub workspace: WorkspaceId,
    pub repo: RepoId,
    pub change_id: String,
    pub owner: String,
    pub attention: v1::Attention,
}

/// One step that makes the entries match the rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Publish(Needing),
    End(WorkspaceId),
}

/// The steps from `live` (each published workspace's attention) to `rows`.
pub(crate) fn reconcile(rows: &[Needing], live: &HashMap<WorkspaceId, i32>) -> Vec<Step> {
    let mut steps: Vec<Step> = rows
        .iter()
        .filter(|row| live.get(&row.workspace) != Some(&i32::from(row.attention)))
        .cloned()
        .map(Step::Publish)
        .collect();
    let mut ended: Vec<WorkspaceId> = live
        .keys()
        .filter(|workspace| !rows.iter().any(|row| row.workspace == **workspace))
        .copied()
        .collect();
    ended.sort();
    steps.extend(ended.into_iter().map(Step::End));
    steps
}

/// Applies `steps` to `hub`.
pub(crate) fn apply(hub: &PresenceHub, steps: Vec<Step>) {
    for step in steps {
        match step {
            Step::Publish(row) => {
                hub.publish_daemon(DaemonKey::Attention(row.workspace), |_| Report {
                    agent: row.owner,
                    repo: Some(row.repo),
                    workspace: Some(row.workspace),
                    change_id: row.change_id,
                    kind: PresenceKind::Attention,
                    attention: row.attention,
                    ttl: STATE_TTL,
                    ..Report::default()
                });
            }
            Step::End(workspace) => {
                hub.end_daemon(DaemonKey::Attention(workspace), PresenceEndReason::Cleared);
            }
        }
    }
}

/// The active workspaces that need attention.
fn needing(state: &AppState) -> Option<Vec<Needing>> {
    let rows = match state.db.workspaces(None, &[WorkspaceState::Active]) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read workspaces' attention");
            return None;
        }
    };
    Some(
        rows.into_iter()
            .filter(|row| row.attention.is_some())
            .map(|row| Needing {
                workspace: row.id,
                repo: row.repo_id,
                change_id: row.change_id.clone(),
                owner: row.owner.clone(),
                attention: crate::service::convert::attention(row.attention),
            })
            .collect(),
    )
}

/// One reconcile against the current rows.
pub(crate) fn sync(state: &AppState) {
    if let Some(rows) = needing(state) {
        apply(
            &state.presence,
            reconcile(&rows, &state.presence.attention_entries()),
        );
    }
}

/// Starts the task; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let weak: Weak<AppState> = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            sync(&state);
            let woken = async {
                state.db.attention_changed().notified().await;
                tokio::time::sleep(SETTLE).await;
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = woken => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u8, attention: v1::Attention) -> Needing {
        Needing {
            workspace: format!("{n:016x}").parse().unwrap(),
            repo: "aaaa000000000001".parse().unwrap(),
            change_id: String::new(),
            owner: "claude@a".to_owned(),
            attention,
        }
    }

    #[test]
    fn publishes_what_changed_and_ends_what_is_gone() {
        let a = row(1, v1::Attention::WatchFailed);
        let b = row(2, v1::Attention::Divergent);
        let gone = row(3, v1::Attention::Divergent).workspace;
        let live = HashMap::from([
            (a.workspace, i32::from(v1::Attention::WatchFailed)),
            (b.workspace, i32::from(v1::Attention::StoreMissing)),
            (gone, i32::from(v1::Attention::Divergent)),
        ]);
        assert_eq!(
            reconcile(&[a, b.clone()], &live),
            [Step::Publish(b), Step::End(gone)]
        );
    }

    #[test]
    fn nothing_to_do_twice() {
        let a = row(1, v1::Attention::WatchFailed);
        let live = HashMap::from([(a.workspace, i32::from(a.attention))]);
        assert_eq!(reconcile(&[a], &live), Vec::<Step>::new());
    }
}
