//! What a workspace's change needs next to be finished (#262, ADR 0080):
//! `Workspace.step`, with the lifecycle and counts it was read from, set by
//! `GetWorkspace` for an active workspace.
//!
//! The facts are read at head, in one job on the repository's read thread:
//! the working-copy commit, how many paths it changes against its parents,
//! how many are conflicted, and whether the repository lands through
//! proposals. The lifecycle is then filled as every read fills it, the
//! automatic service's status (R3.45) says whether the service will land a
//! proposal with no request, and [`step`] decides.

use std::sync::Arc;

use reef_api::v1::{self, ChangeStep, LifecycleState};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::log::Describer;
use reef_lib::jj::revset::{self, RevsetScope};
use reef_lib::jj::{ReadonlyRepo, files};

use super::convert;
use crate::daemon::AppState;
use crate::model::{WorkspaceRecord, WorkspaceState};

/// A workspace's step and what it was read from.
#[derive(Clone, Debug)]
pub(crate) struct Stepped {
    pub(crate) step: ChangeStep,
    pub(crate) lifecycle: v1::ChangeLifecycle,
    pub(crate) changed_file_count: u32,
    pub(crate) conflicted_path_count: u32,
}

/// The facts [`step`] reads, besides the change's lifecycle.
struct Facts {
    change: v1::Change,
    changed: u32,
    conflicted: u32,
    has_policy: bool,
}

/// The step of `record`'s change at head; `None` for a workspace that is
/// not active, or when it cannot be read, which is logged.
pub(crate) async fn of_workspace(
    state: &Arc<AppState>,
    record: &WorkspaceRecord,
) -> Option<Stepped> {
    if record.state != WorkspaceState::Active {
        return None;
    }
    let repo = record.repo_id;
    let id = record.id;
    let warn = |error: &(dyn std::error::Error + 'static)| {
        tracing::warn!(%repo, workspace_id = %id, error = %reef_lib::error::ErrorChain(error), "cannot read the workspace's step");
    };
    let handle = state.manager.handle(repo).ok()?;
    let root = state.db.repository(repo).ok()?.root;
    let head = state
        .reads
        .current(&handle)
        .await
        .inspect_err(|error| warn(error))
        .ok()?;
    let (stop, _guard) = state.reads.stop();
    let user_email = state.settings.user_email().to_owned();
    let workspace = record.id.jj_workspace_name();
    let facts = state
        .reads
        .run(&handle, move || async move {
            let scope = RevsetScope {
                workspace: Some(&workspace),
                user_email: &user_email,
                root: &root,
            };
            read_facts(&head, &scope, &|| stop.requested()).await
        })
        .await
        .inspect_err(|error| warn(error))
        .ok()?
        .inspect_err(|error| warn(error.as_ref()))
        .ok()?;
    let Facts {
        mut change,
        changed,
        conflicted,
        has_policy,
    } = facts;
    crate::lifecycle::messages::fill(state, repo, std::iter::once(&mut change), None).await;
    // `fill` leaves it unset when the rows cannot be read: no step then.
    let lifecycle = change.lifecycle.clone()?;
    let proposals = has_policy
        || lifecycle.recorded
        || state
            .db
            .has_lifecycle(repo)
            .inspect_err(|error| warn(error))
            .unwrap_or(false);
    // R3.28: a conflict that waits for a person is the person's to move.
    let blocked = (change.conflict || conflicted > 0)
        && state
            .db
            .assignment_of_change(repo, &change.change_id)
            .inspect_err(|error| warn(error))
            .ok()
            .flatten()
            .is_some_and(|row| {
                row.closed_at.is_none() && row.state == crate::assign::State::Blocked.as_str()
            });
    let automatic = lifecycle.state() == LifecycleState::Proposed
        && state
            .automatic
            .status(repo, &change.change_id)
            .is_some_and(|status| {
                status.lands_unasked(&lifecycle.destination, &lifecycle.commit_id)
            });
    Some(Stepped {
        step: step(
            &change, &lifecycle, changed, conflicted, proposals, automatic, blocked,
        ),
        lifecycle,
        changed_file_count: changed,
        conflicted_path_count: conflicted,
    })
}

type ReadResult = Result<Facts, Box<dyn std::error::Error + Send + Sync>>;

/// The working copy described, its changed and conflicted path counts, and
/// whether `trunk()`'s tree has `reef-policy.toml`.
async fn read_facts(
    repo: &ReadonlyRepo,
    scope: &RevsetScope<'_>,
    should_stop: &dyn Fn() -> bool,
) -> ReadResult {
    let wc = revset::resolve_revision(repo, "@", scope).await?;
    let info = Describer::new(repo, scope)?.describe(&wc.commit_id).await?;
    let options = DiffOptions {
        format: DiffFormat::Summary,
        context: 0,
        max_lines: 0,
        max_hunk_bytes: 0,
        max_file_bytes: 0,
        paths: Vec::new(),
        after: None,
        limit: 1,
        pairs: false,
        sizes: false,
    };
    let changed = diff::diff_page(
        repo,
        &DiffBase::Parents,
        &wc.commit_id,
        &options,
        should_stop,
    )
    .await?
    .total;
    let (_, conflicted) = diff::conflicted_paths(repo, &wc.commit_id, 1)?;
    let has_policy = match revset::trunk(repo) {
        Some(trunk) => {
            let tree = files::commit_tree(repo, &trunk.commit_id).await?;
            files::tree_file(&tree, reef_policy::POLICY_PATH)
                .await?
                .is_some()
        }
        None => false,
    };
    Ok(Facts {
        change: convert::change(&info),
        changed: u32::try_from(changed).unwrap_or(u32::MAX),
        conflicted: u32::try_from(conflicted).unwrap_or(u32::MAX),
        has_policy,
    })
}

/// The step table (ADR 0080, `docs/proposals.md`): the first row that
/// matches. `proposals` is whether the repository lands through
/// proposals; `automatic`, whether the automatic service will land a
/// proposal with no request (R3.45): its destination says
/// `[landing] automatic = true` and the service has not given up on it;
/// `blocked`, whether the change's conflict waits for a person (R3.28).
pub(crate) fn step(
    change: &v1::Change,
    lifecycle: &v1::ChangeLifecycle,
    changed: u32,
    conflicted: u32,
    proposals: bool,
    automatic: bool,
    blocked: bool,
) -> ChangeStep {
    if change.conflict || conflicted > 0 {
        return if blocked {
            ChangeStep::Ask
        } else {
            ChangeStep::Resolve
        };
    }
    match lifecycle.state() {
        LifecycleState::Queued => return ChangeStep::Wait,
        LifecycleState::Proposed if lifecycle.amended => return ChangeStep::Propose,
        LifecycleState::Proposed if automatic => return ChangeStep::Wait,
        LifecycleState::Proposed => return ChangeStep::Land,
        LifecycleState::Declined => return ChangeStep::Revise,
        LifecycleState::Landed | LifecycleState::Superseded => return ChangeStep::Done,
        LifecycleState::Unspecified | LifecycleState::Draft | LifecycleState::Withdrawn => {}
    }
    if change.immutable {
        ChangeStep::Done
    } else if change.empty || changed == 0 {
        ChangeStep::Work
    } else if change.description.trim().is_empty() {
        ChangeStep::Describe
    } else if proposals {
        ChangeStep::Propose
    } else {
        ChangeStep::Publish
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(description: &str, empty: bool) -> v1::Change {
        v1::Change {
            description: description.to_owned(),
            empty,
            ..v1::Change::default()
        }
    }

    fn lifecycle(state: LifecycleState, amended: bool) -> v1::ChangeLifecycle {
        v1::ChangeLifecycle {
            state: state.into(),
            recorded: state != LifecycleState::Draft,
            amended,
            ..v1::ChangeLifecycle::default()
        }
    }

    /// One row per line of the step table.
    #[test]
    fn each_row_of_the_table() {
        use ChangeStep as Step;
        use LifecycleState as State;
        let at = |state| lifecycle(state, false);
        let draft = at(State::Draft);
        let amended = lifecycle(State::Proposed, true);
        let described = change("Fix the parser\n", false);
        let blank = change("  \n", false);
        let empty = change("", true);
        let conflicted = v1::Change {
            conflict: true,
            ..described.clone()
        };
        let landed = v1::Change {
            immutable: true,
            ..described.clone()
        };
        // (change, lifecycle, changed, conflicted, proposals, automatic, step)
        let rows = [
            (&conflicted, draft.clone(), 3, 1, true, false, Step::Resolve),
            (&described, draft.clone(), 3, 2, false, false, Step::Resolve),
            (&described, at(State::Queued), 3, 0, true, false, Step::Wait),
            (
                &described,
                amended.clone(),
                3,
                0,
                true,
                false,
                Step::Propose,
            ),
            (&described, amended, 3, 0, true, true, Step::Propose),
            (
                &described,
                at(State::Proposed),
                3,
                0,
                true,
                true,
                Step::Wait,
            ),
            (
                &described,
                at(State::Proposed),
                3,
                0,
                true,
                false,
                Step::Land,
            ),
            (
                &described,
                at(State::Declined),
                3,
                0,
                true,
                false,
                Step::Revise,
            ),
            (&described, at(State::Landed), 3, 0, true, false, Step::Done),
            (
                &described,
                at(State::Superseded),
                3,
                0,
                true,
                false,
                Step::Done,
            ),
            (&landed, draft.clone(), 3, 0, false, false, Step::Done),
            (&empty, draft.clone(), 0, 0, true, false, Step::Work),
            (&blank, draft.clone(), 3, 0, true, false, Step::Describe),
            (&described, draft.clone(), 3, 0, true, false, Step::Propose),
            (
                &described,
                at(State::Withdrawn),
                3,
                0,
                true,
                false,
                Step::Propose,
            ),
            (&described, draft, 3, 0, false, false, Step::Publish),
        ];
        for (change, lifecycle, changed, conflicted, proposals, automatic, expected) in rows {
            assert_eq!(
                step(
                    change, &lifecycle, changed, conflicted, proposals, automatic, false
                ),
                expected,
                "{change:?} {lifecycle:?} {changed} {conflicted} {proposals} {automatic}"
            );
        }
    }

    /// R3.28: a conflict that waits for a person asks, and only a conflict.
    #[test]
    fn a_blocked_conflict_asks() {
        let draft = lifecycle(LifecycleState::Draft, false);
        let described = change("Fix the parser\n", false);
        let conflicted = v1::Change {
            conflict: true,
            ..described.clone()
        };
        assert_eq!(
            step(&conflicted, &draft, 3, 1, true, false, true),
            ChangeStep::Ask
        );
        assert_eq!(
            step(&described, &draft, 3, 0, true, false, true),
            ChangeStep::Propose
        );
    }
}
