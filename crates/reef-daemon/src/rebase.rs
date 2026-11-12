//! Rebasing (#259, ADR 0071): the daemon's side of `reef_lib::jj::rebase`.
//! The rebase itself is a [`crate::workspace::Write::Rebase`] through
//! `Manager::rewrite`, which snapshots and follows every workspace it
//! moves; this module holds what the daemon adds: the refusal of a queued
//! change, and each refusal's reason, hint and `next`.

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::rebase::RebaseError;

use crate::error::{Advice, Advise};

/// A rebase the daemon refuses from its own records.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RebaseRefusal {
    /// A change of the set is QUEUED: the landing service lands exactly the
    /// queued commit (R3.13).
    #[error("change {change} is queued to land; a rebase would change what lands")]
    Queued {
        /// The change, in hex.
        change: String,
    },
}

impl Classify for RebaseRefusal {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Queued { .. } => ErrorKind::Conflict,
        }
    }
}

impl Advise for RebaseRefusal {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Queued { change } => Some(Advice::run(
                "The landing service rebases a queued change onto the destination itself; wait for it to land, or withdraw it with `reef proposal withdraw` first.",
                [
                    "reef",
                    "op",
                    "watch",
                    "--kind",
                    "transition",
                    "--change",
                    change.as_str(),
                ],
            )),
        }
    }
}

impl RebaseRefusal {
    /// Its reason and metadata, as `crate::error` reports them.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the shape of every error type's reason(), which crate::error chains"
    )]
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        Some(match self {
            Self::Queued { change } => (
                reef_api::error::CHANGE_QUEUED,
                vec![("change_id", change.clone())],
            ),
        })
    }
}

/// A jj workspace name as the API names it: the Reef workspace id.
fn workspace_id(name: &str) -> &str {
    name.strip_prefix(reef_lib::jj::workspace::REEF_WORKSPACE_PREFIX)
        .unwrap_or(name)
}

/// `error`'s reason and metadata, as `crate::error` reports them; `None`
/// for a failure the source chain names (a revision's), or none.
pub(crate) fn reason(error: &RebaseError) -> Option<(&'static str, Vec<(&'static str, String)>)> {
    use reef_api::error as reason;
    Some(match error {
        RebaseError::Mismatch { expected, actual } => (
            reason::EXPECTED_COMMIT_MISMATCH,
            vec![
                ("expected_commit_id", expected.to_string()),
                ("commit_id", actual.to_string()),
            ],
        ),
        RebaseError::NoDestination => (reason::NO_DESTINATION, Vec::new()),
        RebaseError::OntoDescendant { destination } => (
            reason::REBASE_ONTO_DESCENDANT,
            vec![("commit_id", destination.to_string())],
        ),
        RebaseError::Immutable {
            commit_id,
            change_id,
            head,
        } => (
            reason::IMMUTABLE_REVISION,
            vec![
                ("commit_id", commit_id.to_string()),
                ("change_id", change_id.to_string()),
                ("head", head.clone()),
            ],
        ),
        RebaseError::Hidden(commit_id) => (
            reason::HIDDEN_REVISION,
            vec![("commit_id", commit_id.to_string())],
        ),
        RebaseError::Divergent {
            change_id,
            commit_id,
        } => (
            reason::CHANGE_DIVERGENT,
            vec![
                ("change_id", change_id.to_string()),
                ("commit_id", commit_id.to_string()),
            ],
        ),
        RebaseError::IncludesWorkingCopy {
            commit_id,
            change_id,
            workspace,
        } => (
            reason::INCLUDES_WORKING_COPY,
            vec![
                ("change_id", change_id.to_string()),
                ("commit_id", commit_id.to_string()),
                ("workspace", workspace_id(workspace).to_owned()),
            ],
        ),
        RebaseError::TooLarge { count } => {
            (reason::RANGE_TOO_LARGE, vec![("count", count.to_string())])
        }
        _ => return None,
    })
}

impl Advise for RebaseError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Mismatch { actual, .. } => Some(Advice::hint(format!(
                "The revision now names {actual}; read it again and retry with that id."
            ))),
            Self::NoDestination => Some(Advice::hint(
                "The repository has no trunk() bookmark (`main`, `master` or `trunk`, local or on origin); name the destination with `--onto`.",
            )),
            Self::OntoDescendant { .. } => Some(Advice::hint(
                "The destination is one of the moved commits or above one; rebase onto trunk, or onto a commit outside the change's stack.",
            )),
            Self::Immutable { head, .. } => Some(Advice::hint(format!(
                "That commit is already on {head}; it cannot move. Start new work on top of it with `reef ws move <workspace> --new-on {head}`, or rebase a change above it."
            ))),
            Self::Hidden(_) => Some(Advice::hint(
                "The commit was rewritten since it was read; read the change again and retry with its current commit.",
            )),
            Self::Divergent { change_id, .. } => Some(Advice::run(
                "Resolve the divergence first: abandon the commit you do not want with `reef abandon <commit>`.",
                [
                    "reef".to_owned(),
                    "log".to_owned(),
                    "-r".to_owned(),
                    format!("change_id({change_id})"),
                ],
            )),
            Self::IncludesWorkingCopy { workspace, .. } => Some(Advice::run(
                format!(
                    "Workspace {} is still writing a change below this one; rebasing its workspace moves both, with its files, and is the explicit form. `--source` moves only this change and its descendants.",
                    workspace_id(workspace)
                ),
                ["reef", "rebase", "--workspace", workspace_id(workspace)],
            )),
            Self::TooLarge { .. } => Some(Advice::hint(
                "Rebase a smaller set: name the change with `--source` to move only it and its descendants.",
            )),
            _ => None,
        }
    }
}

/// How many commits `trunk()` has that `commit` lacks, at most 1,000, at
/// the repository's current head (#259's `Workspace.behind_trunk`); `None`
/// without a `trunk()`, for a commit that is not one, or when it cannot be
/// read, which is logged.
pub(crate) async fn behind_trunk(
    state: &crate::daemon::AppState,
    repo: crate::model::RepoId,
    commit: &str,
) -> Option<u32> {
    let commit: reef_lib::jj::CommitIdHex = commit.parse().ok()?;
    let handle = state.manager.handle(repo).ok()?;
    let head = state.reads.current(&handle).await.ok()?;
    let counted = state
        .reads
        .run(&handle, move || async move {
            reef_lib::jj::rebase::behind_trunk(&head, &commit).await
        })
        .await;
    match counted {
        Ok(Ok(count)) => count.map(|(count, _)| count),
        Ok(Err(error)) => {
            tracing::warn!(%repo, error = %reef_lib::error::ErrorChain(&error), "cannot count the commits trunk() is ahead by");
            None
        }
        Err(error) => {
            tracing::warn!(%repo, error = %reef_lib::error::ErrorChain(&error), "cannot count the commits trunk() is ahead by");
            None
        }
    }
}
