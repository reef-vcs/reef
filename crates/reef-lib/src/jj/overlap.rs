//! Other open changes that change the paths a workspace's change changes
//! (#263, ADR 0079), read from the view a read holds. A path whose content
//! in the other change equals trunk's does not count: that change adds
//! nothing there. Neither does a change whose line already landed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use futures::StreamExt as _;
use jj_lib::backend::{ChangeId, CommitId, TreeId};
use jj_lib::commit::Commit;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_store::OperationId;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::revset::walk_revs;
use jj_lib::rewrite::merge_commit_trees;

use super::diff::DiffError;
use super::oplog::system_time;
use super::repo::{ChangeIdHex, CommitIdHex};
use super::revset::{self, RevisionError, RevsetScope};

/// Changed paths kept per commit; a longer list is cut and the rest is
/// asked of the tree.
pub const MAX_CHANGED_PATHS: usize = 10_000;
/// Open commits read per call, newest first.
pub const MAX_CANDIDATES: usize = 1_000;
/// Overlapping changes listed; the rest are counted.
pub const MAX_OVERLAPS: usize = 10;
/// Shared paths listed per change; the rest are counted.
pub const MAX_LISTED_PATHS: usize = 10;
/// Paths the cache holds in all before it is cleared.
pub const CACHE_BUDGET: usize = 250_000;
/// Trunk commits read to remember the trees they had; past this, no
/// landed line is recognized.
pub const MAX_TRUNK_TREES: usize = 1_000;
/// Landed line heads recognized per read; past this, no tree check is made.
pub const MAX_LANDED_HEADS: usize = 100;
/// Repository operations whose landed heads the cache keeps.
const MAX_LANDED_CACHE: usize = 16;

/// The candidate set: open commits on another line of work than `@`.
const CANDIDATES: &str = "mutable() ~ (::@ | @::)";

/// What one commit changes against its parents merged, in jj's path order
/// (by component, so `a/b` is before `a.rs`), at most a cache's per-commit
/// limit; `cut` when there are more.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChangedPaths {
    paths: Vec<RepoPathBuf>,
    cut: bool,
}

impl ChangedPaths {
    fn is_empty(&self) -> bool {
        !self.cut && self.paths.is_empty()
    }

    fn contains(&self, path: &RepoPath) -> bool {
        self.paths
            .binary_search_by(|stored| stored.as_ref().cmp(path))
            .is_ok()
    }

    /// Whether every path `self` changes is one `cover` changes too. A cut
    /// list does not hold every path it changes, so it is never covered:
    /// the caller keeps the change rather than risk hiding one.
    fn covered_by(&self, cover: &ChangedPaths) -> bool {
        if self.cut {
            return false;
        }
        self.paths.iter().all(|path| cover.contains(path.as_ref()))
    }

    /// The stored paths after `other`'s last stored one when `other` is cut:
    /// the paths `other` must be asked about in its tree.
    fn past_cut<'a>(&'a self, other: &'a ChangedPaths) -> impl Iterator<Item = &'a RepoPath> {
        let last = other.paths.last().filter(|_| other.cut);
        self.paths
            .iter()
            .map(AsRef::as_ref)
            .filter(move |path| last.is_some_and(|last| *path > last.as_ref()))
    }
}

/// The stored paths both lists hold, in path order: the smaller list
/// looked up in the larger.
fn common<'a>(a: &'a ChangedPaths, b: &'a ChangedPaths) -> Vec<&'a RepoPath> {
    let (small, large) = if a.paths.len() <= b.paths.len() {
        (a, b)
    } else {
        (b, a)
    };
    small
        .paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|path| large.contains(path))
        .collect()
}

/// A mutable line tip whose whole tree is a state trunk reached after the
/// line's base on trunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedHead {
    /// The mutable commit.
    pub commit: CommitId,
    /// The trunk commits with that tree, each below or at the line's base.
    pub trunks: Vec<CommitId>,
}

#[derive(Debug, Default)]
struct CacheInner {
    map: HashMap<CommitId, Arc<ChangedPaths>>,
    total: usize,
    landed: HashMap<(OperationId, CommitId), Arc<Vec<LandedHead>>>,
}

/// Changed-path lists by commit id and landed lines by operation and trunk
/// commit, shared by every read. A commit id hashes its tree and parents,
/// and an operation and commit id name an immutable view, so an entry never
/// goes stale: each map is only bounded. The path map is cleared whole when
/// it would pass its budget; the landed map when it holds too many
/// operations. Two reads that miss one entry at once both compute it and
/// store equal values.
#[derive(Debug)]
pub struct PathCache {
    inner: Mutex<CacheInner>,
    per_commit: usize,
    budget: usize,
}

impl Default for PathCache {
    fn default() -> Self {
        PathCache::with_limits(MAX_CHANGED_PATHS, CACHE_BUDGET)
    }
}

impl PathCache {
    /// A cache keeping `per_commit` paths per commit and `budget` in all.
    pub fn with_limits(per_commit: usize, budget: usize) -> PathCache {
        PathCache {
            inner: Mutex::new(CacheInner::default()),
            per_commit,
            budget,
        }
    }

    /// How many paths it holds.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total
    }

    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    async fn paths(
        &self,
        repo: &ReadonlyRepo,
        commit: &Commit,
    ) -> Result<Arc<ChangedPaths>, DiffError> {
        if let Some(hit) = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(commit.id())
        {
            return Ok(Arc::clone(hit));
        }
        let parents = commit.parents().await?;
        let from = merge_commit_trees(repo, &parents).await?;
        let to = commit.tree();
        let mut stream = from.diff(&to, &EverythingMatcher);
        let mut found = ChangedPaths::default();
        while let Some(entry) = stream.next().await {
            if found.paths.len() == self.per_commit {
                found.cut = true;
                break;
            }
            found.paths.push(entry.path);
        }
        let found = Arc::new(found);
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let size = found.paths.len().max(1);
        if inner.total + size > self.budget {
            inner.map.clear();
            inner.total = 0;
        }
        if inner
            .map
            .insert(commit.id().clone(), Arc::clone(&found))
            .is_none()
        {
            inner.total += size;
        }
        Ok(found)
    }

    /// The landed line tips at `repo`'s operation, keyed by the operation
    /// and trunk commit: both are immutable, so an entry never goes stale,
    /// and the map is only bounded. A read `should_stop` cut is returned
    /// but not stored.
    async fn landed(
        &self,
        repo: &ReadonlyRepo,
        scope: &RevsetScope<'_>,
        trunk: &CommitIdHex,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<Arc<Vec<LandedHead>>, OverlapError> {
        let key = (repo.op_id().clone(), trunk.to_id());
        if let Some(hit) = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .landed
            .get(&key)
        {
            return Ok(Arc::clone(hit));
        }
        let (heads, complete) = landed_heads(repo, scope, trunk, should_stop).await?;
        let heads = Arc::new(heads);
        if complete {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if inner.landed.len() == MAX_LANDED_CACHE {
                inner.landed.clear();
            }
            inner.landed.insert(key, Arc::clone(&heads));
        }
        Ok(heads)
    }
}

/// Which old changes count: a change written before `since` counts only
/// when a working copy on it passes `always`.
pub struct Policy<'a> {
    /// Changes written before this count only through `always`.
    pub since: SystemTime,
    /// jj workspace names whose changes count however old: Reef's active
    /// workspaces.
    pub always: &'a dyn Fn(&str) -> bool,
}

/// One overlapping change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// The change.
    pub change_id: ChangeIdHex,
    /// Its newest visible commit.
    pub commit_id: CommitIdHex,
    /// That commit's committer time.
    pub written_at: SystemTime,
    /// Its author's email.
    pub author_email: String,
    /// The jj workspaces whose working copy is on the change, sorted.
    pub working_copies: Vec<String>,
    /// The shared paths, in path order, [`MAX_LISTED_PATHS`] at most.
    pub paths: Vec<String>,
    /// Such paths, in all.
    pub path_count: usize,
}

/// What [`find`] found: [`MAX_OVERLAPS`] changes at most, newest written
/// first, and how many in all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The changes listed.
    pub overlaps: Vec<Overlap>,
    /// Overlapping changes, in all.
    pub count: usize,
}

/// Failure to compute overlaps.
#[derive(Debug, thiserror::Error)]
pub enum OverlapError {
    /// The candidate revset or `@` could not be read.
    #[error(transparent)]
    Revision(#[from] RevisionError),
    /// A tree could not be read.
    #[error(transparent)]
    Diff(#[from] DiffError),
}

impl From<jj_lib::backend::BackendError> for OverlapError {
    fn from(error: jj_lib::backend::BackendError) -> Self {
        OverlapError::Diff(DiffError::from(error))
    }
}

/// A candidate commit and what is known of it before any diff.
struct Candidate {
    commit: Commit,
    written_at: SystemTime,
    working_copies: Vec<String>,
}

/// The other open changes that change paths `scope.workspace`'s `@`
/// changes, in `repo`'s view. Nothing is diffed when there is no
/// candidate. `should_stop` is checked between candidates.
pub async fn find(
    repo: &ReadonlyRepo,
    scope: &RevsetScope<'_>,
    policy: &Policy<'_>,
    cache: &PathCache,
    should_stop: &dyn Fn() -> bool,
) -> Result<Found, OverlapError> {
    let wc = revset::resolve_revision(repo, "@", scope).await?;
    let store = repo.store();
    let mine = store.get_commit_async(&wc.commit_id.to_id()).await?;
    let mut found = Found::default();
    // A fresh workspace changes nothing: no candidate is read.
    if mine.is_empty(repo.index()).await? {
        return Ok(found);
    }
    let trunk = revset::landed_trunk(repo)
        .await
        .map_err(|error| RevisionError::Other(error.into()))?;
    let candidates = candidates(
        repo,
        scope,
        mine.change_id(),
        trunk.as_ref().map(|trunk| &trunk.commit_id),
        policy,
        should_stop,
    )
    .await?;
    if candidates.is_empty() {
        return Ok(found);
    }
    let my_paths = cache.paths(repo, &mine).await?;
    if my_paths.is_empty() {
        return Ok(found);
    }
    let trunk_tree = match &trunk {
        Some(trunk) => Some(
            store
                .get_commit_async(&trunk.commit_id.to_id())
                .await?
                .tree(),
        ),
        None => None,
    };
    // Read the landed lines only once a candidate shares a path: a status
    // with no overlap pays nothing for them.
    let mut landed: Option<Arc<Vec<LandedHead>>> = None;
    for candidate in candidates {
        if should_stop() {
            break;
        }
        let theirs = cache.paths(repo, &candidate.commit).await?;
        if theirs.is_empty() {
            continue;
        }
        let listed = found.overlaps.len() < MAX_OVERLAPS;
        let shared = shared_paths(
            repo,
            (&mine, &my_paths),
            (&candidate.commit, &theirs),
            trunk_tree.as_ref(),
            !listed,
        )
        .await?;
        if shared.is_empty() {
            continue;
        }
        if let Some(trunk) = &trunk {
            let heads = if let Some(heads) = &landed {
                Arc::clone(heads)
            } else {
                let heads = cache
                    .landed(repo, scope, &trunk.commit_id, should_stop)
                    .await?;
                landed = Some(Arc::clone(&heads));
                heads
            };
            if candidate_landed(repo, cache, &candidate.commit, &theirs, &heads).await? {
                continue;
            }
        }
        found.count += 1;
        if listed {
            found.overlaps.push(Overlap {
                change_id: ChangeIdHex::of(candidate.commit.change_id()),
                commit_id: CommitIdHex::of(candidate.commit.id()),
                written_at: candidate.written_at,
                author_email: candidate.commit.author().email.clone(),
                working_copies: candidate.working_copies,
                path_count: shared.len(),
                paths: shared
                    .iter()
                    .take(MAX_LISTED_PATHS)
                    .map(|path| path.as_internal_file_string().to_owned())
                    .collect(),
            });
        }
    }
    Ok(found)
}

/// The trees trunk's newest commits had, by tree, with every trunk commit
/// that had one. The second value is false when `should_stop` cut the walk,
/// so the result must not be cached.
async fn trunk_trees(
    repo: &ReadonlyRepo,
    trunk: &CommitIdHex,
    should_stop: &dyn Fn() -> bool,
) -> Result<(HashMap<TreeId, Vec<CommitId>>, bool), OverlapError> {
    let wanted = [trunk.to_id()];
    let revset = walk_revs(repo, &wanted, &[])
        .map_err(|error| OverlapError::Revision(RevisionError::Other(error.into())))?;
    let store = repo.store();
    let mut trees: HashMap<TreeId, Vec<CommitId>> = HashMap::new();
    let mut stream = revset.stream();
    let mut read = 0;
    let mut complete = true;
    while let Some(id) = stream.next().await {
        if should_stop() {
            complete = false;
            break;
        }
        if read == MAX_TRUNK_TREES {
            break;
        }
        let id = id.map_err(RevisionError::Evaluation)?;
        let commit = store.get_commit_async(&id).await?;
        read += 1;
        if let Some(tree) = commit.tree_ids().as_resolved() {
            trees.entry(tree.clone()).or_default().push(id);
        }
    }
    Ok((trees, complete))
}

/// Mutable line tips whose whole tree is a state trunk reached after the
/// line's base on trunk, a squash merge's source among them. The base is
/// the merge-base of the tip and trunk, so only a trunk commit below or at
/// the base can name a landed line: a clock cannot. The second value is
/// false when `should_stop` cut the read, so the result must not be cached.
async fn landed_heads(
    repo: &ReadonlyRepo,
    scope: &RevsetScope<'_>,
    trunk: &CommitIdHex,
    should_stop: &dyn Fn() -> bool,
) -> Result<(Vec<LandedHead>, bool), OverlapError> {
    let (trees, mut complete) = trunk_trees(repo, trunk, should_stop).await?;
    if trees.is_empty() {
        return Ok((Vec::new(), complete));
    }
    let trunk_id = trunk.to_id();
    let text = format!("mutable() ~ ::{trunk}");
    let parsed = revset::parse(repo, &text, scope)?;
    let evaluated = revset::evaluate(repo, &parsed)?;
    let store = repo.store();
    let mut heads: Vec<LandedHead> = Vec::new();
    let mut stream = evaluated.stream();
    let mut read = 0;
    while let Some(id) = stream.next().await {
        if should_stop() {
            complete = false;
            break;
        }
        if read == MAX_CANDIDATES || heads.len() == MAX_LANDED_HEADS {
            break;
        }
        let id = id.map_err(RevisionError::Evaluation)?;
        read += 1;
        let commit = store.get_commit_async(&id).await?;
        let Some(tree) = commit.tree_ids().as_resolved() else {
            continue;
        };
        let Some(matches) = trees.get(tree) else {
            continue;
        };
        let bases = repo
            .index()
            .common_ancestors(
                std::slice::from_ref(commit.id()),
                std::slice::from_ref(&trunk_id),
            )
            .await
            .map_err(|error| RevisionError::Other(error.into()))?;
        let mut trunks = Vec::new();
        for found in matches {
            for base in &bases {
                if repo
                    .index()
                    .is_ancestor(base, found)
                    .await
                    .map_err(|error| RevisionError::Other(error.into()))?
                {
                    trunks.push(found.clone());
                    break;
                }
            }
        }
        if !trunks.is_empty() {
            heads.push(LandedHead {
                commit: commit.id().clone(),
                trunks,
            });
        }
    }
    Ok((heads, complete))
}

/// Whether `commit`'s line landed: it is a landed tip itself, whose whole
/// tree is a trunk state after its base, or it is below one and the trunk
/// commit with the tip's tree changed every path `paths` changes. A change
/// below a tip on a path trunk's matching commit did not change stays live.
async fn candidate_landed(
    repo: &ReadonlyRepo,
    cache: &PathCache,
    commit: &Commit,
    paths: &ChangedPaths,
    heads: &[LandedHead],
) -> Result<bool, OverlapError> {
    for head in heads {
        if commit.id() == &head.commit {
            return Ok(true);
        }
        if !repo
            .index()
            .is_ancestor(commit.id(), &head.commit)
            .await
            .map_err(|error| RevisionError::Other(error.into()))?
        {
            continue;
        }
        for trunk in &head.trunks {
            let trunk = repo.store().get_commit_async(trunk).await?;
            let cover = cache.paths(repo, &trunk).await?;
            if paths.covered_by(&cover) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// [`CANDIDATES`] less `::trunk` (landed commits `mutable()` holds when
/// trunk is `main@origin`, see [`revset::landed_trunk`]): working copies first, then
/// the rest, [`MAX_CANDIDATES`] at most; one commit per change (its
/// newest), not on `mine`, old ones only through `policy.always`; newest
/// written first, ties by commit id.
async fn candidates(
    repo: &ReadonlyRepo,
    scope: &RevsetScope<'_>,
    mine: &ChangeId,
    trunk: Option<&CommitIdHex>,
    policy: &Policy<'_>,
    should_stop: &dyn Fn() -> bool,
) -> Result<Vec<Candidate>, OverlapError> {
    let open = match trunk {
        Some(trunk) => format!("({CANDIDATES}) ~ ::{trunk}"),
        None => CANDIDATES.to_owned(),
    };
    // Working copies first, so the cap never cuts an active workspace's
    // change however many commits a fetch brought.
    let mut ids: Vec<CommitId> = Vec::new();
    for text in [format!("working_copies() & ({open})"), open] {
        let parsed = revset::parse(repo, &text, scope)?;
        let evaluated = revset::evaluate(repo, &parsed)?;
        let mut stream = evaluated.stream();
        while let Some(id) = stream.next().await {
            if should_stop() || ids.len() == MAX_CANDIDATES {
                break;
            }
            let id = id.map_err(RevisionError::Evaluation)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let store = repo.store();
    let mut copies: HashMap<ChangeId, Vec<String>> = HashMap::new();
    for (name, id) in repo.view().wc_commit_ids() {
        let commit = store.get_commit_async(id).await?;
        copies
            .entry(commit.change_id().clone())
            .or_default()
            .push(name.as_str().to_owned());
    }
    let mut newest: HashMap<ChangeId, Candidate> = HashMap::new();
    for id in ids {
        let commit = store.get_commit_async(&id).await?;
        if commit.change_id() == mine {
            continue;
        }
        let written_at = system_time(commit.committer().timestamp);
        let mut working_copies = copies.get(commit.change_id()).cloned().unwrap_or_default();
        working_copies.sort();
        if written_at < policy.since && !working_copies.iter().any(|name| (policy.always)(name)) {
            continue;
        }
        let newer = newest
            .get(commit.change_id())
            .is_none_or(|held| (written_at, commit.id()) > (held.written_at, held.commit.id()));
        if newer {
            newest.insert(
                commit.change_id().clone(),
                Candidate {
                    commit,
                    written_at,
                    working_copies,
                },
            );
        }
    }
    let mut candidates: Vec<Candidate> = newest.into_values().collect();
    candidates.sort_by(|a, b| (b.written_at, b.commit.id()).cmp(&(a.written_at, a.commit.id())));
    Ok(candidates)
}

/// The paths both commits change, in path order, less those where
/// `theirs`' content equals `trunk`'s or is conflicted. Paths past a cut
/// list are asked of that commit's tree. With `first_only`, stops at the
/// first.
async fn shared_paths(
    repo: &ReadonlyRepo,
    mine: (&Commit, &ChangedPaths),
    theirs: (&Commit, &ChangedPaths),
    trunk: Option<&MergedTree>,
    first_only: bool,
) -> Result<Vec<RepoPathBuf>, OverlapError> {
    let mut shared: Vec<RepoPathBuf> = common(mine.1, theirs.1)
        .into_iter()
        .map(RepoPath::to_owned)
        .collect();
    for (asked, of, other) in [(theirs, mine.1, theirs.1), (mine, theirs.1, mine.1)] {
        let past: Vec<&RepoPath> = of.past_cut(other).collect();
        if past.is_empty() {
            continue;
        }
        let parents = asked.0.parents().await?;
        let before = merge_commit_trees(repo, &parents).await?;
        let after = asked.0.tree();
        for path in past {
            if before.path_value(path).await? != after.path_value(path).await? {
                shared.push(path.to_owned());
            }
        }
    }
    shared.sort();
    shared.dedup();
    let their_tree = theirs.0.tree();
    let mut kept = Vec::new();
    for path in shared {
        let their_value = their_tree.path_value(&path).await?;
        // A conflicted path cannot be landed as it is; status reports the
        // conflict itself, so an overlap there is not a live collision.
        if !their_value.is_resolved() {
            continue;
        }
        if let Some(trunk) = trunk
            && their_value == trunk.path_value(&path).await?
        {
            continue;
        }
        kept.push(path);
        if first_only {
            break;
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str], cut: bool) -> ChangedPaths {
        let mut paths: Vec<RepoPathBuf> = list
            .iter()
            .map(|path| RepoPathBuf::from_internal_string(*path).unwrap())
            .collect();
        paths.sort();
        ChangedPaths { paths, cut }
    }

    fn names(list: &[&RepoPath]) -> Vec<String> {
        list.iter()
            .map(|path| path.as_internal_file_string().to_owned())
            .collect()
    }

    #[test]
    fn common_matches_in_jj_path_order() {
        // `a/b` sorts before `a.rs` by component, not as strings.
        let mine = paths(&["a.rs", "a/b", "z"], false);
        let theirs = paths(&["a/b", "a.rs", "b", "c", "d"], false);
        assert_eq!(names(&common(&mine, &theirs)), ["a/b", "a.rs"]);
        assert_eq!(names(&common(&theirs, &mine)), ["a/b", "a.rs"]);
    }

    #[test]
    fn past_cut_is_only_after_a_cut_lists_last_path() {
        let cut = paths(&["b", "d"], true);
        let whole = paths(&["a", "d", "e", "f/g"], false);
        assert_eq!(
            names(&whole.past_cut(&cut).collect::<Vec<_>>()),
            ["e", "f/g"]
        );
        // A whole list is never asked about.
        assert!(cut.past_cut(&whole).next().is_none());
    }

    #[test]
    fn a_cut_list_is_not_empty() {
        assert!(paths(&[], false).is_empty());
        assert!(!paths(&[], true).is_empty());
    }
}
