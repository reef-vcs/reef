//! What an open proposal depends on (R3.11, ADR 0084).
//!
//! A proposal's dependencies are the changes below it in its stack (R3.10):
//! its ancestors not on its destination, read from jj's parent edges, never
//! stored. A dependency **left review** when it is declined or withdrawn at
//! its current commit: the proposal above it cannot land, and returns to
//! its owner with a `missing_dependency` conflict (derived here on every
//! read), an assignment of kind `dependency` (R3.27), and an instruction.
//! Reef never moves it off the dependency: [`move_off`] says how its owner
//! does.

use std::collections::{HashMap, HashSet};

use reef_lib::error::ErrorChain;
use reef_lib::jj::lifecycle::LifecycleState;
use reef_lib::jj::{ChangeIdHex, land};

use crate::daemon::AppState;
use crate::db::LifecycleRow;
use crate::model::RepoId;

/// One change of a stack, bottom first, as the moves read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    /// The change, in hex.
    pub change: String,
    /// Its visible commit.
    pub commit: String,
    /// Its commit's parents.
    pub parents: Vec<String>,
}

/// A stack member that left review, below a proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Missing {
    /// The member.
    pub change: String,
    /// `declined` or `withdrawn`.
    pub state: LifecycleState,
    /// The transition that took it out of review.
    pub transition: String,
    /// The member above it to move off it (`reef rebase <above> --source`).
    pub above: String,
    /// Where to move it: the change below the missing one, or the
    /// destination's bookmark when it sits on the destination.
    pub onto: String,
}

/// The `reef rebase` arguments that move the changes above `links[at]` off
/// it: past the run of members above it that left review too (`left`), the
/// first member whose parents include the run's top (else the head, the
/// last link), onto `links[at]`'s first parent's change when that is a
/// member, else onto `bookmark`.
pub(crate) fn move_off(
    links: &[Link],
    at: usize,
    bookmark: &str,
    left: impl Fn(&Link) -> bool,
) -> (String, String) {
    let mut top = at;
    let above = loop {
        let next = links[top + 1..]
            .iter()
            .position(|link| link.parents.contains(&links[top].commit))
            .map(|offset| top + 1 + offset);
        match next {
            Some(next) if left(&links[next]) && next + 1 < links.len() => top = next,
            Some(next) => break &links[next],
            None => break links.last().unwrap_or(&links[at]),
        }
    };
    let onto = links[at]
        .parents
        .first()
        .and_then(|parent| links[..at].iter().find(|link| &link.commit == parent))
        .map_or_else(|| bookmark.to_owned(), |link| link.change.clone());
    (above.change.clone(), onto)
}

/// Whether `row` left review at `commit`: declined or withdrawn, and not
/// revised since. The carry follower moves a declined or withdrawn row's
/// commit along a rebase by a rewrite below it, so only its owner's own
/// revision makes the commits differ; callers carry first (R3.11).
pub(crate) fn left_review(row: &LifecycleRow, commit: &str) -> Option<LifecycleState> {
    let state: LifecycleState = row.state.parse().ok()?;
    (matches!(state, LifecycleState::Declined | LifecycleState::Withdrawn)
        && row.commit_id == commit)
        .then_some(state)
}

/// The lowest member below the head (the last link) that left review, with
/// how to move off it: `rows` holds the members' lifecycle rows by change,
/// `still` the declined or withdrawn changes still out of review (at their
/// commit, or rebased below with their own diff).
pub(crate) fn missing(
    links: &[Link],
    rows: &HashMap<String, LifecycleRow>,
    still: &HashSet<String>,
    bookmark: &str,
) -> Option<Missing> {
    let below = links.len().checked_sub(1)?;
    let left_state = |link: &Link| -> Option<LifecycleState> {
        let row = rows.get(&link.change)?;
        let state: LifecycleState = row.state.parse().ok()?;
        (matches!(state, LifecycleState::Declined | LifecycleState::Withdrawn)
            && still.contains(&link.change))
        .then_some(state)
    };
    links[..below].iter().enumerate().find_map(|(at, link)| {
        let state = left_state(link)?;
        let row = rows.get(&link.change)?;
        let (above, onto) = move_off(links, at, bookmark, |link| left_state(link).is_some());
        Some(Missing {
            change: link.change.clone(),
            state,
            transition: row.transition_id.clone(),
            above,
            onto,
        })
    })
}

/// The stack of each of `heads` (change and destination bookmark): its
/// links bottom first, the head last. A change with no visible commit, a
/// destination that is gone, or a stack too large is left out; `None` when
/// the read failed.
pub(crate) async fn stacks(
    state: &AppState,
    repo: RepoId,
    heads: Vec<(String, String)>,
) -> Option<HashMap<String, Vec<Link>>> {
    if heads.is_empty() {
        return Some(HashMap::new());
    }
    let tip = state.events.tip(repo)?;
    let handle = state.manager.handle(repo).ok()?;
    let read = state
        .reads
        .run(&handle, move || async move {
            let mut found = HashMap::new();
            for (change, bookmark) in heads {
                let Ok(id) = change.parse::<ChangeIdHex>() else {
                    continue;
                };
                let Ok(Some(visible)) = reef_lib::jj::assignment::visible_commit(&tip, &id).await
                else {
                    continue;
                };
                let Ok(destination) = land::destination_head(&tip, &bookmark) else {
                    continue;
                };
                let Ok(members) = land::stack(&tip, &visible.commit, &destination).await else {
                    continue;
                };
                let links: Vec<Link> = members
                    .into_iter()
                    .map(|member| Link {
                        change: member.change.to_string(),
                        commit: member.commit.to_string(),
                        parents: member.parents.iter().map(ToString::to_string).collect(),
                    })
                    .collect();
                found.insert(change, links);
            }
            found
        })
        .await;
    match read {
        Ok(found) => Some(found),
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot read proposals' stacks");
            None
        }
    }
}

/// Of `open`, the proposals with a change of `left` below them still out
/// of review, and those changes, by change id; `None` when the read fails.
async fn above_left(
    state: &AppState,
    repo: RepoId,
    open: &[LifecycleRow],
    left: &[LifecycleRow],
) -> Option<(HashSet<String>, HashSet<String>)> {
    let heads: Vec<ChangeIdHex> = open
        .iter()
        .filter_map(|row| row.change_id.parse().ok())
        .collect();
    let left: Vec<(ChangeIdHex, reef_lib::jj::CommitIdHex)> = left
        .iter()
        .filter_map(|row| Some((row.change_id.parse().ok()?, row.commit_id.parse().ok()?)))
        .collect();
    if heads.is_empty() || left.is_empty() {
        return Some((HashSet::new(), HashSet::new()));
    }
    let tip = state.events.tip(repo)?;
    let handle = state.manager.handle(repo).ok()?;
    let read = state
        .reads
        .run(&handle, move || async move {
            reef_lib::jj::lifecycle::above_any(&tip, &heads, &left).await
        })
        .await;
    let found = match read {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot read proposals above declined changes");
            return None;
        }
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot read proposals above declined changes");
            return None;
        }
    };
    Some((
        found.heads.iter().map(ToString::to_string).collect(),
        found
            .left
            .iter()
            .map(|(change, _)| change.to_string())
            .collect(),
    ))
}

/// The lifecycle rows of every change of `stacks`, by change.
pub(crate) fn rows_of(
    state: &AppState,
    repo: RepoId,
    stacks: &HashMap<String, Vec<Link>>,
) -> HashMap<String, LifecycleRow> {
    let mut rows = HashMap::new();
    for link in stacks.values().flatten() {
        if rows.contains_key(&link.change) {
            continue;
        }
        if let Ok(Some(row)) = state.db.lifecycle_row(repo, &link.change) {
            rows.insert(link.change.clone(), row);
        }
    }
    rows
}

/// Every open proposal of `repo` (of `only`, when given) whose stack holds
/// a member that left review, with the lowest such member, by change; see
/// [`try_held`]. A failed read holds nothing.
pub(crate) async fn held(
    state: &AppState,
    repo: RepoId,
    only: Option<&[String]>,
) -> HashMap<String, (Missing, LifecycleRow)> {
    try_held(state, repo, only).await.unwrap_or_default()
}

/// [`held`], or `None` when a read failed, so the assignment follower
/// closes nothing on a failure (R3.11). A repository with no declined or
/// withdrawn change costs one query; otherwise one index read decides which
/// proposals sit above one, and only their stacks are read.
pub(crate) async fn try_held(
    state: &AppState,
    repo: RepoId,
    only: Option<&[String]>,
) -> Option<HashMap<String, (Missing, LifecycleRow)>> {
    let filter = |states: &[LifecycleState]| crate::db::LifecycleFilter {
        states: states
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect(),
        changes: only.map(<[String]>::to_vec).unwrap_or_default(),
        agents: Vec::new(),
    };
    let left = crate::db::LifecycleFilter {
        changes: Vec::new(),
        ..filter(&[LifecycleState::Declined, LifecycleState::Withdrawn])
    };
    let left = match state.db.lifecycle_page(repo, &left, None, 1000) {
        Ok(rows) if rows.is_empty() => return Some(HashMap::new()),
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot read declined proposals");
            return None;
        }
    };
    let open = match state.db.lifecycle_page(
        repo,
        &filter(&[LifecycleState::Proposed, LifecycleState::Queued]),
        None,
        1000,
    ) {
        Ok(open) => open,
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot read open proposals");
            return None;
        }
    };
    let open: Vec<LifecycleRow> = open
        .into_iter()
        .filter(|row| only.is_none_or(|only| only.contains(&row.change_id)))
        .collect();
    // Only proposals above a change still at the commit it left review at
    // need their stack read: most withdrawn changes were revised or are
    // gone.
    let (above, still) = above_left(state, repo, &open, &left).await?;
    let open: Vec<LifecycleRow> = open
        .into_iter()
        .filter(|row| above.contains(&row.change_id))
        .collect();
    let heads = open
        .iter()
        .filter_map(|row| Some((row.change_id.clone(), row.destination.clone()?)))
        .collect();
    let stacks = stacks(state, repo, heads).await?;
    let rows = rows_of(state, repo, &stacks);
    Some(
        open.into_iter()
            .filter_map(|row| {
                let links = stacks.get(&row.change_id)?;
                let bookmark = row.destination.clone().unwrap_or_default();
                let missing = missing(links, &rows, &still, &bookmark)?;
                Some((row.change_id.clone(), (missing, row)))
            })
            .collect(),
    )
}

/// Each open proposal of `rows`' dependencies: the changes below it in its
/// stack, bottom first, at most 31, with their lifecycles.
pub(crate) async fn dependencies(
    state: &AppState,
    repo: RepoId,
    rows: &[LifecycleRow],
) -> HashMap<String, Vec<reef_api::v1::ProposalDependency>> {
    let heads = rows
        .iter()
        .filter(|row| {
            row.state == LifecycleState::Proposed.as_str()
                || row.state == LifecycleState::Queued.as_str()
        })
        .filter_map(|row| Some((row.change_id.clone(), row.destination.clone()?)))
        .collect();
    let stacks = stacks(state, repo, heads).await.unwrap_or_default();
    let lifecycles = rows_of(state, repo, &stacks);
    stacks
        .into_iter()
        .map(|(head, links)| {
            let below = links.len().saturating_sub(1);
            let entries = links[..below]
                .iter()
                .take(31)
                .map(|link| {
                    let row = lifecycles.get(&link.change);
                    let lifecycle = row.map_or_else(
                        || reef_api::v1::ChangeLifecycle {
                            state: reef_api::v1::LifecycleState::Draft.into(),
                            visible: true,
                            ..reef_api::v1::ChangeLifecycle::default()
                        },
                        |row| {
                            let last = state.db.transition(repo, &row.transition_id).ok().flatten();
                            super::messages::lifecycle_message(
                                row,
                                last.as_ref(),
                                Some(link.commit.as_str()),
                            )
                        },
                    );
                    reef_api::v1::ProposalDependency {
                        change_id: link.change.clone(),
                        lifecycle: Some(lifecycle),
                        proposed_by: row
                            .and_then(|row| row.proposed_by_agent.clone())
                            .unwrap_or_default(),
                    }
                })
                .collect();
            (head, entries)
        })
        .collect()
}

/// `missing` as a message, with the transition that took it out.
pub(crate) fn missing_message(
    state: &AppState,
    repo: RepoId,
    missing: &Missing,
) -> reef_api::v1::MissingDependency {
    let state_value = super::messages::state_message(missing.state);
    reef_api::v1::MissingDependency {
        change_id: missing.change.clone(),
        state: state_value.into(),
        transition: state
            .db
            .transition(repo, &missing.transition)
            .ok()
            .flatten()
            .as_ref()
            .map(super::messages::transition_message),
        move_change_id: missing.above.clone(),
        move_onto: missing.onto.clone(),
    }
}

/// A `missing_dependency` conflict's id: `cfl_` and 32 hex digits of
/// SHA-256 over the kind, the dependent, the missing change, the transition
/// that took it out and the change to move, so a later decline, or a move
/// that left another parent on it (a merge), is a new conflict.
pub(crate) fn conflict_id(dependent: &str, missing: &Missing) -> String {
    let digest = reef_policy::sha256_hex(
        format!(
            "dependency\0{dependent}\0{}\0{}\0{}",
            missing.change, missing.transition, missing.above
        )
        .as_bytes(),
    );
    format!("cfl_{}", &digest[..32])
}

/// Tells the session that proposed `dependent` that `missing` below it
/// left review (R3.11, decision 5), with a fixed template over ids (S-246).
pub(crate) async fn tell_missing(
    state: &std::sync::Arc<AppState>,
    repo: RepoId,
    dependent: &str,
    missing: &Missing,
) {
    use crate::landing::automatic::short;
    use crate::landing::instruct::safe_id;
    let text = format!(
        "Change {dep} below change {id} was {state}, so {id} cannot land on it. Move {id} off it (reef rebase {above} --source --onto {onto}), or withdraw it (reef proposal withdraw {id}).",
        dep = short(&missing.change),
        id = short(dependent),
        state = missing.state,
        above = short(&missing.above),
        onto = safe_id(&if missing.onto.len() == 32
            && missing.onto.chars().all(|c| c.is_ascii_hexdigit())
        {
            short(&missing.onto)
        } else {
            missing.onto.clone()
        }),
    );
    crate::landing::instruct::write(
        state,
        repo,
        dependent,
        text,
        String::new(),
        super::carry::AGENT,
        "missing dependency",
    )
    .await;
}

/// Tells the session that proposed `dependent` that a rewrite of `below`
/// left it conflicted (R3.11, decision 5).
pub(crate) async fn tell_conflicted(
    state: &std::sync::Arc<AppState>,
    repo: RepoId,
    dependent: &str,
    below: &str,
) {
    use crate::landing::automatic::short;
    let text = format!(
        "Change {dep} below change {id} changed, and {id} no longer applies on it. Resolve the conflict (reef conflict show -r {id}), then propose it again (reef propose {id}).",
        dep = short(below),
        id = short(dependent),
    );
    crate::landing::instruct::write(
        state,
        repo,
        dependent,
        text,
        String::new(),
        super::carry::AGENT,
        "dependency conflict",
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(change: &str, parents: &[&str]) -> Link {
        Link {
            change: change.to_owned(),
            commit: format!("c-{change}"),
            parents: parents.iter().map(|parent| format!("c-{parent}")).collect(),
        }
    }

    #[test]
    fn moves_the_member_above_onto_the_one_below() {
        let links = [link("a", &["trunk"]), link("b", &["a"]), link("c", &["b"])];
        let none = |_: &Link| false;
        assert_eq!(move_off(&links, 1, "main", none), ("c".into(), "a".into()));
        assert_eq!(
            move_off(&links, 0, "main", none),
            ("b".into(), "main".into())
        );
    }

    #[test]
    fn moves_past_members_that_left_review_too() {
        let links = [
            link("a", &["trunk"]),
            link("b", &["a"]),
            link("c", &["b"]),
            link("d", &["c"]),
        ];
        let b_left = |link: &Link| link.change == "b";
        assert_eq!(
            move_off(&links, 0, "main", b_left),
            ("c".into(), "main".into())
        );
    }
}
