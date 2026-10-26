//! What an operation changed (R2.02, ADR 0030): the difference between its
//! view and its parents', as changes created, rewritten, hidden or
//! abandoned, bookmarks moved, and working copies moved.
//!
//! Commits are compared by visibility: those visible after and not before
//! (`A`), and the reverse (`R`), walked from the index without reading a
//! commit. A commit in `A` with a recorded predecessor of its own change
//! rewrote it; other commits in `A` are rewrites of a change visible before
//! or new changes; what is left of `R` was hidden (its change still
//! visible) or abandoned.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::StreamExt as _;
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::index::ChangeIdIndex;
use jj_lib::object_id::{HexPrefix, PrefixResolution};
use jj_lib::op_store::{RefTarget, View};
use jj_lib::operation::Operation;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::{RevsetExpression, walk_revs};

use super::oplog::{OpLogError, load_at};
use super::repo::{ChangeIdHex, CommitIdHex};

/// The default cap on change and bookmark entries.
pub const MAX_EFFECTS: usize = 1000;

/// The remote whose bookmarks are git's own refs, never reported.
const GIT_REMOTE: &str = "git";

/// How an operation changed a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeEffectKind {
    /// A new change.
    Created,
    /// A new commit for an existing change.
    Rewritten,
    /// The change is no longer visible.
    Abandoned,
    /// One of the change's commits is no longer visible; the change still
    /// is.
    Hidden,
}

/// One commit of a change an operation touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEffectInfo {
    /// The change.
    pub change_id: ChangeIdHex,
    /// How it changed.
    pub kind: ChangeEffectKind,
    /// The commit afterwards; `None` when hidden or abandoned.
    pub commit_id: Option<CommitIdHex>,
    /// The commit before; `None` when created, or rewritten from no
    /// single commit.
    pub previous_commit_id: Option<CommitIdHex>,
}

/// One bookmark an operation touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkEffectInfo {
    /// The bookmark.
    pub name: String,
    /// The remote; `None` for a local bookmark.
    pub remote: Option<String>,
    /// Its target afterwards (a conflict's first added commit); `None`
    /// when deleted.
    pub commit_id: Option<CommitIdHex>,
    /// Its target in the first parent's view; `None` when absent there.
    pub previous_commit_id: Option<CommitIdHex>,
    /// It is conflicted afterwards.
    pub conflicted: bool,
}

/// One working copy an operation moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingCopyEffectInfo {
    /// The jj workspace.
    pub workspace: String,
    /// Its working-copy commit afterwards; `None` when forgotten.
    pub commit_id: Option<CommitIdHex>,
    /// Its working-copy commit in the first parent's view; `None` when
    /// added.
    pub previous_commit_id: Option<CommitIdHex>,
}

/// What an operation changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpEffects {
    /// At most the cap, sorted by change id then commit id.
    pub changes: Vec<ChangeEffectInfo>,
    /// Change entries in all.
    pub change_count: usize,
    /// At most the cap: local before remote, by name, remotes by name.
    pub bookmarks: Vec<BookmarkEffectInfo>,
    /// Bookmark entries in all.
    pub bookmark_count: usize,
    /// By jj workspace name.
    pub working_copies: Vec<WorkingCopyEffectInfo>,
    /// The changes of the entries the cap left out of `changes`, sorted and
    /// without repeats: what a filter by change matches beyond the cap.
    pub capped_change_ids: Vec<ChangeIdHex>,
}

impl OpEffects {
    /// Whether the operation touched a change whose hex id starts with
    /// `prefix`, counting entries past the cap.
    pub fn touches(&self, prefix: &str) -> bool {
        self.changes
            .iter()
            .map(|change| &change.change_id)
            .chain(&self.capped_change_ids)
            .any(|change| change.as_str().starts_with(prefix))
    }
}

fn first_added(target: &RefTarget) -> Option<CommitIdHex> {
    target.present_adds().next().map(CommitIdHex::of)
}

/// Commits of `ids`, with their change ids, from `repo`'s index.
async fn change_ids_of(
    repo: &ReadonlyRepo,
    ids: Vec<CommitId>,
) -> Result<HashMap<CommitId, ChangeId>, OpLogError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let revset = RevsetExpression::commits(ids)
        .evaluate(repo)
        .map_err(|error| OpLogError::Commit(error.into_backend_error()))?;
    let mut stream = revset.commit_change_ids();
    let mut found = HashMap::new();
    while let Some(pair) = stream.next().await {
        let (commit, change) =
            pair.map_err(|error| OpLogError::Commit(error.into_backend_error()))?;
        found.insert(commit, change);
    }
    Ok(found)
}

/// `(commit, change)` of every commit visible from `wanted` and not from
/// `unwanted`.
async fn visible_difference(
    repo: &ReadonlyRepo,
    wanted: &[CommitId],
    unwanted: &[CommitId],
) -> Result<Vec<(CommitId, ChangeId)>, OpLogError> {
    let revset = walk_revs(repo, wanted, unwanted)
        .map_err(|error| OpLogError::Commit(error.into_backend_error()))?;
    let mut stream = revset.commit_change_ids();
    let mut found = Vec::new();
    while let Some(pair) = stream.next().await {
        found.push(pair.map_err(|error| OpLogError::Commit(error.into_backend_error()))?);
    }
    Ok(found)
}

async fn has_visible(index: &dyn ChangeIdIndex, change: &ChangeId) -> Result<bool, OpLogError> {
    let prefix = HexPrefix::from_id(change);
    match index
        .resolve_prefix(&prefix)
        .await
        .map_err(|error| OpLogError::Commit(jj_lib::backend::BackendError::Other(error.into())))?
    {
        PrefixResolution::SingleMatch(targets) => {
            Ok(targets.visible_with_offsets().next().is_some())
        }
        PrefixResolution::NoMatch | PrefixResolution::AmbiguousMatch => Ok(false),
    }
}

/// The repository whose index holds every commit of `views`: `head` when
/// it has their heads, else the store loaded at `operation`.
async fn index_holder(
    head: &Arc<ReadonlyRepo>,
    operation: &Operation,
    views: &[&View],
) -> Result<Arc<ReadonlyRepo>, OpLogError> {
    let index = head.index();
    for view in views {
        for id in &view.head_ids {
            let known = index.has_id(id).await.map_err(|error| {
                OpLogError::Commit(jj_lib::backend::BackendError::Other(error.into()))
            })?;
            if !known {
                return load_at(head, operation).await;
            }
        }
    }
    Ok(Arc::clone(head))
}

/// The effects of `operation`, computed over `head`'s store: at most `cap`
/// change and bookmark entries each, with their totals. The root
/// operation's are empty.
pub async fn operation_effects(
    head: &Arc<ReadonlyRepo>,
    operation: &Operation,
    cap: usize,
) -> Result<OpEffects, OpLogError> {
    let parents = operation.parents().await?;
    if parents.is_empty() {
        return Ok(OpEffects::default());
    }
    let after_view = operation.view().await?;
    let after = after_view.store_view();
    let mut before_views = Vec::with_capacity(parents.len());
    for parent in &parents {
        before_views.push(parent.view().await?);
    }
    let befores: Vec<&View> = before_views
        .iter()
        .map(jj_lib::view::View::store_view)
        .collect();
    let mut all_views: Vec<&View> = befores.clone();
    all_views.push(after);
    let repo = index_holder(head, operation, &all_views).await?;

    let after_heads: Vec<CommitId> = after.head_ids.iter().cloned().collect();
    let before_heads: Vec<CommitId> = befores
        .iter()
        .flat_map(|view| view.head_ids.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut changes = change_effects(&repo, operation, &before_heads, &after_heads).await?;
    let change_count = changes.len();
    let mut capped_change_ids: Vec<ChangeIdHex> = changes
        .drain(cap.min(changes.len())..)
        .map(|change| change.change_id)
        .collect();
    capped_change_ids.dedup();

    let mut bookmarks = bookmark_effects(after, &befores);
    let bookmark_count = bookmarks.len();
    bookmarks.truncate(cap);
    Ok(OpEffects {
        changes,
        change_count,
        bookmarks,
        bookmark_count,
        working_copies: working_copy_effects(after, &befores),
        capped_change_ids,
    })
}

/// The commit of `change` in `removed` that `commit` rewrote, following
/// the predecessors this operation recorded through commits it rewrote
/// again in the same transaction (jj records those intermediate commits
/// too), nearest first.
fn predecessor_in<'a>(
    operation: &'a Operation,
    commit: &CommitId,
    change: &ChangeId,
    removed: &HashMap<CommitId, ChangeId>,
) -> Option<&'a CommitId> {
    let mut queue: VecDeque<&CommitId> = operation
        .predecessors_for_commit(commit)
        .unwrap_or_default()
        .iter()
        .collect();
    let mut seen = HashSet::new();
    while let Some(candidate) = queue.pop_front() {
        if !seen.insert(candidate) {
            continue;
        }
        if removed.get(candidate) == Some(change) {
            return Some(candidate);
        }
        if let Some(earlier) = operation.predecessors_for_commit(candidate) {
            queue.extend(earlier);
        }
    }
    None
}

/// One entry per commit that became or stopped being visible between
/// `before_heads` and `after_heads`, sorted by change id then commit id.
#[expect(
    clippy::too_many_lines,
    reason = "one pass pairing `A` with `R`; split, its state would thread through every part"
)]
async fn change_effects(
    repo: &ReadonlyRepo,
    operation: &Operation,
    before_heads: &[CommitId],
    after_heads: &[CommitId],
) -> Result<Vec<ChangeEffectInfo>, OpLogError> {
    let added = visible_difference(repo, after_heads, before_heads).await?;
    let removed = visible_difference(repo, before_heads, after_heads).await?;
    let readonly = repo.readonly_index();
    let visible_before = readonly
        .change_id_index(&mut before_heads.iter())
        .map_err(|error| OpLogError::Commit(jj_lib::backend::BackendError::Other(error.into())))?;
    let visible_after = readonly
        .change_id_index(&mut after_heads.iter())
        .map_err(|error| OpLogError::Commit(jj_lib::backend::BackendError::Other(error.into())))?;

    // Predecessors that are not in `R` need their change ids from the index.
    let removed_changes: HashMap<CommitId, ChangeId> = removed.iter().cloned().collect();
    let mut outside = Vec::new();
    for (commit, _) in &added {
        for predecessor in operation
            .predecessors_for_commit(commit)
            .unwrap_or_default()
        {
            if !removed_changes.contains_key(predecessor) {
                outside.push(predecessor.clone());
            }
        }
    }
    let outside = change_ids_of(repo, outside).await?;

    let mut consumed: HashSet<CommitId> = HashSet::new();
    let mut effects: Vec<ChangeEffectInfo> = Vec::new();
    let mut by_change_removed: HashMap<&ChangeId, Vec<&CommitId>> = HashMap::new();
    for (commit, change) in &removed {
        by_change_removed.entry(change).or_default().push(commit);
    }
    let mut unpaired = Vec::new();
    for (commit, change) in &added {
        let predecessor =
            predecessor_in(operation, commit, change, &removed_changes).or_else(|| {
                // A predecessor that was not visible before either: a rewrite
                // of a commit another workspace already rewrote.
                operation
                    .predecessors_for_commit(commit)
                    .unwrap_or_default()
                    .iter()
                    .find(|predecessor| outside.get(*predecessor) == Some(change))
            });
        match predecessor {
            Some(predecessor) => {
                consumed.insert(predecessor.clone());
                effects.push(ChangeEffectInfo {
                    change_id: ChangeIdHex::of(change),
                    kind: ChangeEffectKind::Rewritten,
                    commit_id: Some(CommitIdHex::of(commit)),
                    previous_commit_id: Some(CommitIdHex::of(predecessor)),
                });
            }
            None => unpaired.push((commit, change)),
        }
    }
    for (commit, change) in unpaired {
        let effect = if has_visible(&*visible_before, change).await? {
            let left: Vec<&&CommitId> = by_change_removed
                .get(change)
                .map(|commits| commits.iter().filter(|c| !consumed.contains(**c)).collect())
                .unwrap_or_default();
            let previous = match left.as_slice() {
                [only] => {
                    consumed.insert((**only).clone());
                    Some(CommitIdHex::of(only))
                }
                _ => None,
            };
            ChangeEffectInfo {
                change_id: ChangeIdHex::of(change),
                kind: ChangeEffectKind::Rewritten,
                commit_id: Some(CommitIdHex::of(commit)),
                previous_commit_id: previous,
            }
        } else {
            ChangeEffectInfo {
                change_id: ChangeIdHex::of(change),
                kind: ChangeEffectKind::Created,
                commit_id: Some(CommitIdHex::of(commit)),
                previous_commit_id: None,
            }
        };
        effects.push(effect);
    }
    for (commit, change) in &removed {
        if consumed.contains(commit) {
            continue;
        }
        let kind = if has_visible(&*visible_after, change).await? {
            ChangeEffectKind::Hidden
        } else {
            ChangeEffectKind::Abandoned
        };
        effects.push(ChangeEffectInfo {
            change_id: ChangeIdHex::of(change),
            kind,
            commit_id: None,
            previous_commit_id: Some(CommitIdHex::of(commit)),
        });
    }
    effects.sort_by(|a, b| {
        (&a.change_id, &a.commit_id, &a.previous_commit_id).cmp(&(
            &b.change_id,
            &b.commit_id,
            &b.previous_commit_id,
        ))
    });
    Ok(effects)
}

/// A ref's target in every view, keyed by (remote, name) with `None` for
/// local bookmarks.
fn bookmarks_of(view: &View) -> BTreeMap<(Option<&str>, &str), &RefTarget> {
    let mut refs = BTreeMap::new();
    for (name, target) in &view.local_bookmarks {
        refs.insert((None, name.as_str()), target);
    }
    for (remote, remote_view) in &view.remote_views {
        if remote.as_str() == GIT_REMOTE {
            continue;
        }
        for (name, remote_ref) in &remote_view.bookmarks {
            refs.insert((Some(remote.as_str()), name.as_str()), &remote_ref.target);
        }
    }
    refs
}

fn bookmark_effects(after: &View, befores: &[&View]) -> Vec<BookmarkEffectInfo> {
    let absent = RefTarget::absent();
    let after_refs = bookmarks_of(after);
    let before_refs: Vec<_> = befores.iter().map(|view| bookmarks_of(view)).collect();
    let keys: BTreeSet<(Option<&str>, &str)> = after_refs
        .keys()
        .chain(before_refs.iter().flat_map(BTreeMap::keys))
        .copied()
        .collect();
    let mut effects = Vec::new();
    for key in keys {
        let now = after_refs.get(&key).copied().unwrap_or(&absent);
        let differs = before_refs
            .iter()
            .all(|refs| refs.get(&key).copied().unwrap_or(&absent) != now);
        if !differs {
            continue;
        }
        let previous = before_refs[0].get(&key).copied().unwrap_or(&absent);
        let (remote, name) = key;
        effects.push(BookmarkEffectInfo {
            name: name.to_owned(),
            remote: remote.map(str::to_owned),
            commit_id: first_added(now),
            previous_commit_id: first_added(previous),
            conflicted: !now.is_resolved(),
        });
    }
    // Local (`None`) sorts before every remote, then by name.
    effects.sort_by(|a, b| (&a.remote, &a.name).cmp(&(&b.remote, &b.name)));
    effects
}

fn working_copy_effects(after: &View, befores: &[&View]) -> Vec<WorkingCopyEffectInfo> {
    let names: BTreeSet<&str> = after
        .wc_commit_ids
        .keys()
        .chain(befores.iter().flat_map(|view| view.wc_commit_ids.keys()))
        .map(|name| name.as_str())
        .collect();
    let mut effects = Vec::new();
    for name in names {
        let get = |view: &View| {
            view.wc_commit_ids
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, id)| id.clone())
        };
        let now = get(after);
        if befores.iter().all(|view| get(view) != now) {
            effects.push(WorkingCopyEffectInfo {
                workspace: name.to_owned(),
                commit_id: now.as_ref().map(CommitIdHex::of),
                previous_commit_id: get(befores[0]).as_ref().map(CommitIdHex::of),
            });
        }
    }
    effects
}
