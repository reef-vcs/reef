//! Refusing trees jj-lib cannot read (RX.01).
//!
//! jj-lib 0.46 unwraps `RepoPathComponentBuf::new` on every git tree entry
//! it reads (`GitBackend::read_tree`), so an entry whose name is empty or
//! contains `/` panics it. git's porcelain never writes such a name, but a
//! fetched commit can carry one. The panic happens while the git backend's
//! mutex is held, which poisons it: every later read through that backend
//! panics too, for every workspace of the repository, until the daemon
//! restarts. [`Repo::check_tree_names`] walks a commit's trees with gix,
//! outside jj's lock, so the daemon can refuse a commit before anything
//! hands its tree to jj.

use std::collections::HashSet;
use std::path::Path;

use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;

use super::repo::{CommitIdHex, Repo};
use super::workspace::CommitLookupError;
use crate::error::{Classify, ErrorKind};

/// How many known-good tree ids a [`CheckedTrees`] keeps before it starts
/// over: about 30 MiB, more than the trees of any repository Reef is
/// benchmarked on.
const MAX_CHECKED: usize = 500_000;

/// Trees [`Repo::check_tree_names`] found good, so a commit sharing most of
/// its trees with one checked before costs only the trees that changed.
#[derive(Debug, Default)]
pub struct CheckedTrees(HashSet<gix::ObjectId>);

/// A commit [`Repo::check_tree_names`] refused, or could not look up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TreeNameError {
    /// A tree in the commit has an entry jj cannot read.
    #[error("commit {commit} has a tree entry named {name:?}, which jj cannot read")]
    BadName {
        /// The commit, in hex.
        commit: String,
        /// The entry's name, lossily decoded.
        name: String,
    },
    /// The commit could not be read.
    #[error(transparent)]
    Lookup(#[from] CommitLookupError),
}

impl Classify for TreeNameError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::BadName { .. } => ErrorKind::InvalidArgument,
            Self::Lookup(error) => error.kind(),
        }
    }
}

/// Whether jj-lib 0.46 reads a tree entry named `name` without panicking:
/// `RepoPathComponentBuf::new` refuses an empty name or one with `/`.
/// Names it reads but will not check out (`.`, `..`, `.git`, `.jj`) fail
/// the checkout cleanly instead.
fn readable(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/')
}

impl Repo {
    /// Refuses `commit` if any of its trees (every term of a conflicted
    /// one) has an entry jj cannot read; see the module docs. Trees in
    /// `checked` are skipped, and the trees of a commit found good are
    /// added. A tree gix cannot read is left to jj, which fails on it with
    /// an error rather than a panic.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit))]
    pub async fn check_tree_names(
        &self,
        commit: &CommitIdHex,
        checked: &mut CheckedTrees,
    ) -> Result<(), TreeNameError> {
        let found = self.commit_by_id(commit).await?;
        let Ok(backend) = jj_lib::git::get_git_backend(self.repo.store()) else {
            return Ok(());
        };
        let roots = found
            .tree()
            .tree_ids()
            .iter()
            .filter_map(|id| gix::ObjectId::try_from(id.as_bytes()).ok())
            .collect();
        check_trees(&backend.git_repo(), roots, checked, commit.as_str())
    }
}

/// Refuses the git checkout at `root` if its `HEAD` commit's tree has an
/// entry jj cannot read (RX.01), before jj is initialized beside it:
/// `Repo::init_in_git_repo` checks that tree out, and a panic there leaves a
/// half-made `.jj` behind. A checkout gix cannot open, or with an unborn
/// `HEAD`, is left to jj.
pub fn check_git_head_tree_names(root: &Path) -> Result<(), TreeNameError> {
    let Ok(git) = gix::open(root) else {
        return Ok(());
    };
    let Ok(commit) = git.head_commit() else {
        return Ok(());
    };
    let Ok(tree) = commit.tree_id() else {
        return Ok(());
    };
    let label = commit.id.to_string();
    check_trees(
        &git,
        vec![tree.detach()],
        &mut CheckedTrees::default(),
        &label,
    )
}

/// Walks the trees under `roots` that `checked` does not hold, refusing the
/// first entry jj cannot read; adds them to `checked` if all are good.
/// `commit` names what is refused, in the error.
fn check_trees(
    git: &gix::Repository,
    mut pending: Vec<gix::ObjectId>,
    checked: &mut CheckedTrees,
    commit: &str,
) -> Result<(), TreeNameError> {
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if checked.0.contains(&id) || !seen.insert(id) {
            continue;
        }
        let Ok(object) = git.find_object(id) else {
            continue;
        };
        let Ok(tree) = object.try_into_tree() else {
            continue;
        };
        for entry in tree.iter() {
            let Ok(entry) = entry else {
                break;
            };
            if !readable(entry.filename()) {
                let error = TreeNameError::BadName {
                    commit: commit.to_owned(),
                    name: String::from_utf8_lossy(entry.filename()).into_owned(),
                };
                tracing::warn!(error = %error, "refused a commit jj cannot read");
                return Err(error);
            }
            if entry.mode().is_tree() {
                pending.push(entry.object_id());
            }
        }
    }
    if checked.0.len() + seen.len() > MAX_CHECKED {
        checked.0.clear();
    }
    checked.0.extend(seen);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_jj_cannot_read() {
        assert!(readable(b"README") && readable(b"..") && readable(b".git"));
        assert!(!readable(b"") && !readable(b"a/b") && !readable(b"/"));
    }
}
