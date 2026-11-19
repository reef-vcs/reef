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
use crate::db::{AssignmentRow, DbError};
use crate::model::{RepoId, WorkspaceRecord, WorkspaceState};

/// A workspace's step and what it was read from.
#[derive(Clone, Debug)]
pub(crate) struct Stepped {
    pub(crate) step: ChangeStep,
    pub(crate) lifecycle: v1::ChangeLifecycle,
    pub(crate) changed_file_count: u32,
    pub(crate) conflicted_path_count: u32,
    /// With step `AWAIT_HELP`, the help request it waits on (R3.32); with
    /// step `ASK`, the session's answered help request about the blocked
    /// conflict, asked since it last became blocked or changed.
    pub(crate) help_request_id: Option<String>,
    /// With step `ASK`, the blocked conflict's assignment (R3.28).
    pub(crate) conflict_assignment_id: Option<String>,
    /// With step `ASK`, why it is blocked: `ASK` or `ATTEMPTS`.
    pub(crate) conflict_block_reason: Option<v1::ConflictAssignmentReason>,
}

/// What a person holds a change for, as [`step`] ranks it: a help request
/// the session waits on outranks every row but done, and a blocked
/// conflict only the conflict row. So an agent that asked about its
/// blocked conflict waits for the answer, then sees `ask` again, with the
/// answered help request named, until a person allows it. A person's
/// allow or take withdraws a help request still open about it
/// (`comments::after_unblocked`), so the agent then resolves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Waits {
    /// The change's open conflict assignment is blocked (R3.28): its tier
    /// is `ask` with no go-ahead, or its attempts are used.
    pub(crate) conflict: bool,
    /// The workspace's session asked a help request on the change that is
    /// open, or whose answer it has not taken (R3.32).
    pub(crate) help: bool,
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
#[expect(
    clippy::too_many_lines,
    reason = "reads each fact the step table takes, in order"
)]
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
    let blocked = if change.conflict || conflicted > 0 {
        blocked_assignment(state, repo, &change.change_id)
            .inspect_err(|error| warn(error))
            .ok()
            .flatten()
    } else {
        None
    };
    let automatic = lifecycle.state() == LifecycleState::Proposed
        && state
            .automatic
            .status(repo, &change.change_id)
            .is_some_and(|status| {
                status.lands_unasked(&lifecycle.destination, &lifecycle.commit_id)
            });
    let help_request_id = awaited_help_request(state, record, &change.change_id)
        .inspect_err(|error| warn(error))
        .ok()
        .flatten();
    let next = step(
        &change,
        &lifecycle,
        changed,
        conflicted,
        proposals,
        automatic,
        Waits {
            conflict: blocked.is_some(),
            help: help_request_id.is_some(),
        },
    );
    let blocked = blocked.filter(|_| next == ChangeStep::Ask);
    let help_request_id = match (next, &blocked) {
        (ChangeStep::AwaitHelp, _) => help_request_id,
        (ChangeStep::Ask, Some(row)) => answered_help_request(state, record, row)
            .inspect_err(|error| warn(error))
            .ok()
            .flatten(),
        _ => None,
    };
    Some(Stepped {
        help_request_id,
        conflict_block_reason: blocked
            .as_ref()
            .map(|row| crate::assign::messages::reason_message(&row.reason)),
        conflict_assignment_id: blocked.map(|row| row.id),
        step: next,
        lifecycle,
        changed_file_count: changed,
        conflicted_path_count: conflicted,
    })
}

/// The id of a help request `record`'s session asked on `change`, open or
/// with an answer no session took yet (R3.32). Another session's help
/// request leaves this workspace's step alone.
fn awaited_help_request(
    state: &AppState,
    record: &WorkspaceRecord,
    change: &str,
) -> Result<Option<String>, DbError> {
    let Some(session) = state.db.live_provenance_session(&record.id.as_hex())? else {
        return Ok(None);
    };
    Ok(state
        .db
        .awaited_help_request(
            record.repo_id,
            change,
            session.declared.client_session.as_deref(),
            &session.session_id,
        )?
        .map(|row| row.id))
}

/// The id of a help request with reason CONFLICT that `record`'s session
/// asked about `blocked` since it last became blocked or its conflict last
/// changed, and that is answered: with step ASK, asking again is no use.
fn answered_help_request(
    state: &AppState,
    record: &WorkspaceRecord,
    blocked: &AssignmentRow,
) -> Result<Option<String>, DbError> {
    let Some(session) = state.db.live_provenance_session(&record.id.as_hex())? else {
        return Ok(None);
    };
    Ok(state
        .db
        .answered_conflict_help_request(
            record.repo_id,
            &blocked.change_id,
            &blocked.id,
            blocked.conflict_changed_at,
            session.declared.client_session.as_deref(),
            &session.session_id,
        )?
        .map(|row| row.id))
}

/// `change`'s open conflict assignment when it is blocked (R3.28): its id
/// is what `reef ask --conflict` names.
fn blocked_assignment(
    state: &AppState,
    repo: RepoId,
    change: &str,
) -> Result<Option<AssignmentRow>, DbError> {
    Ok(state.db.assignment_of_change(repo, change)?.filter(|row| {
        row.closed_at.is_none() && row.state == crate::assign::State::Blocked.as_str()
    }))
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
/// `waits`, what a person holds the change for.
pub(crate) fn step(
    change: &v1::Change,
    lifecycle: &v1::ChangeLifecycle,
    changed: u32,
    conflicted: u32,
    proposals: bool,
    automatic: bool,
    waits: Waits,
) -> ChangeStep {
    let final_state = matches!(
        lifecycle.state(),
        LifecycleState::Landed | LifecycleState::Superseded
    );
    if waits.help && !change.immutable && !final_state {
        return ChangeStep::AwaitHelp;
    }
    if change.conflict || conflicted > 0 {
        return if waits.conflict {
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
                    change,
                    &lifecycle,
                    changed,
                    conflicted,
                    proposals,
                    automatic,
                    Waits::default()
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
        let blocked = Waits {
            conflict: true,
            help: false,
        };
        assert_eq!(
            step(&conflicted, &draft, 3, 1, true, false, blocked),
            ChangeStep::Ask
        );
        assert_eq!(
            step(&described, &draft, 3, 0, true, false, blocked),
            ChangeStep::Propose
        );
    }

    /// A session that asked about its blocked conflict waits for the answer
    /// (R3.32 over R3.28); once it takes the answer, the conflict asks again
    /// until a person allows it.
    #[test]
    fn an_asked_help_request_outranks_a_blocked_conflict() {
        let draft = lifecycle(LifecycleState::Draft, false);
        let conflicted = v1::Change {
            conflict: true,
            ..change("Fix the parser\n", false)
        };
        let both = Waits {
            conflict: true,
            help: true,
        };
        assert_eq!(
            step(&conflicted, &draft, 3, 1, true, false, both),
            ChangeStep::AwaitHelp
        );
        let answered = Waits {
            help: false,
            ..both
        };
        assert_eq!(
            step(&conflicted, &draft, 3, 1, true, false, answered),
            ChangeStep::Ask
        );
    }

    /// An open help request (R3.32) outranks every row but done: a
    /// conflicted, proposed or draft change waits for the answer; a landed
    /// or immutable one is done.
    #[test]
    fn help_outranks_every_row_but_done() {
        use ChangeStep as Step;
        use LifecycleState as State;
        let described = change("Fix the parser", false);
        let conflicted = v1::Change {
            conflict: true,
            ..described.clone()
        };
        let immutable = v1::Change {
            immutable: true,
            ..described.clone()
        };
        let at = |state| lifecycle(state, false);
        for (change, lifecycle, expected) in [
            (&conflicted, at(State::Draft), Step::AwaitHelp),
            (&described, at(State::Proposed), Step::AwaitHelp),
            (&described, at(State::Queued), Step::AwaitHelp),
            (&described, at(State::Landed), Step::Done),
            (&described, at(State::Superseded), Step::Done),
            (&immutable, at(State::Draft), Step::Done),
        ] {
            assert_eq!(
                step(
                    change,
                    &lifecycle,
                    3,
                    0,
                    true,
                    true,
                    Waits {
                        conflict: false,
                        help: true
                    }
                ),
                expected,
                "{change:?} {lifecycle:?}"
            );
        }
    }
}
