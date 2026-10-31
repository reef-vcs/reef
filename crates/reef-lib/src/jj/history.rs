//! What the daemon's history index (R2.16, ADR 0044) reads of a change at
//! the repository's tip: its visible commits, whether `trunk()` has
//! reached it, and its subject and author; and which changes a move of
//! `trunk()` landed.

use std::collections::HashSet;

use futures::StreamExt as _;
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::walk_revs;

use super::oplog::OpLogError;
use super::repo::{ChangeIdHex, CommitIdHex};

/// The most bytes of a description's first line [`change_states`] keeps.
pub const MAX_SUBJECT: usize = 200;

/// One change as `repo`'s view shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeState {
    /// Its visible commits; empty when abandoned or undone, more than one
    /// when divergent.
    pub visible: Vec<CommitIdHex>,
    /// One of its visible commits is an ancestor of `trunk`, or it.
    pub landed: bool,
    /// The first line of the commit's description, at most
    /// [`MAX_SUBJECT`] bytes; empty when the commit cannot be read.
    pub subject: String,
    /// The email of the commit's author; empty when it cannot be read.
    pub author_email: String,
    /// When the commit was committed, in milliseconds since the epoch; 0
    /// when it cannot be read.
    pub committed_at: i64,
}

/// The state of each `(change, commit)` in `repo`'s view, in order:
/// `commit` is the commit the caller last saw for the change, read for its
/// subject and author when the change has no visible commit (an abandoned
/// commit stays in the store for the undo window). `trunk` is
/// `trunk()`'s commit, if any.
pub async fn change_states(
    repo: &ReadonlyRepo,
    changes: &[(ChangeIdHex, CommitIdHex)],
    trunk: Option<&CommitIdHex>,
) -> Result<Vec<ChangeState>, OpLogError> {
    let trunk = trunk.map(CommitIdHex::to_id);
    let mut states = Vec::with_capacity(changes.len());
    for (change, commit) in changes {
        let mut state = ChangeState::default();
        if let Some(change) = ChangeId::try_from_hex(change.as_str()) {
            let targets = repo.resolve_change_id(&change).await.map_err(|error| {
                OpLogError::Commit(jj_lib::backend::BackendError::Other(error.into()))
            })?;
            if let Some(targets) = targets {
                for (_, id) in targets.visible_with_offsets() {
                    state.visible.push(CommitIdHex::of(id));
                    if let Some(trunk) = &trunk
                        && !state.landed
                    {
                        state.landed =
                            repo.index().is_ancestor(id, trunk).await.map_err(|error| {
                                OpLogError::Commit(jj_lib::backend::BackendError::Other(
                                    error.into(),
                                ))
                            })?;
                    }
                }
            }
        }
        let read = state.visible.first().unwrap_or(commit).to_id();
        if let Ok(read) = repo.store().get_commit_async(&read).await {
            state.subject = subject(read.description());
            state.author_email.clone_from(&read.author().email);
            state.committed_at = read.committer().timestamp.timestamp.0;
        }
        states.push(state);
    }
    Ok(states)
}

/// The first line of `description`, cut at a character boundary to at most
/// [`MAX_SUBJECT`] bytes.
fn subject(description: &str) -> String {
    let line = description.lines().next().unwrap_or("");
    let mut end = line.len().min(MAX_SUBJECT);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

/// The change ids of the commits in `::to ~ ::from` (every ancestor of
/// `to` when `from` is `None`), without repeats, at most `cap`; and whether
/// there were more.
pub async fn change_ids_between(
    repo: &ReadonlyRepo,
    from: Option<&CommitIdHex>,
    to: &CommitIdHex,
    cap: usize,
) -> Result<(Vec<ChangeIdHex>, bool), OpLogError> {
    let wanted: Vec<CommitId> = vec![to.to_id()];
    let unwanted: Vec<CommitId> = from.map(CommitIdHex::to_id).into_iter().collect();
    let revset = walk_revs(repo, &wanted, &unwanted)
        .map_err(|error| OpLogError::Commit(error.into_backend_error()))?;
    let mut stream = revset.commit_change_ids();
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    while let Some(pair) = stream.next().await {
        let (_, change) = pair.map_err(|error| OpLogError::Commit(error.into_backend_error()))?;
        if seen.insert(change.clone()) {
            if ids.len() == cap {
                return Ok((ids, true));
            }
            ids.push(ChangeIdHex::of(&change));
        }
    }
    Ok((ids, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_the_first_line_cut_at_a_boundary() {
        assert_eq!(subject("fix the parser\n\nmore"), "fix the parser");
        assert_eq!(subject(""), "");
        let long = "é".repeat(150);
        let cut = subject(&long);
        assert!(cut.len() <= MAX_SUBJECT && cut.len() >= MAX_SUBJECT - 1);
        assert!(long.starts_with(&cut));
    }
}
