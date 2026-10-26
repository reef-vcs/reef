//! Changes and the log (R2.02): a commit described as Reef's `Change`, and
//! the log graph of a revset, children first, branches kept together as
//! `jj log` prints them.

use std::collections::HashMap;
use std::time::SystemTime;

use futures::StreamExt as _;
use jj_lib::backend::{CommitId, Signature};
use jj_lib::graph::{GraphEdgeType, TopoGroupedGraph};
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::revset::RevsetContainingFn;

use super::oplog::system_time;
use super::repo::{ChangeIdHex, CommitIdHex};
use super::revset::{self, ParsedRevset, RevisionError, RevsetScope};

/// Who made a commit, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureInfo {
    /// The name.
    pub name: String,
    /// The email address.
    pub email: String,
    /// When, to the millisecond; jj's time zone offset is not kept.
    pub at: SystemTime,
}

impl From<&Signature> for SignatureInfo {
    fn from(signature: &Signature) -> Self {
        SignatureInfo {
            name: signature.name.clone(),
            email: signature.email.clone(),
            at: system_time(signature.timestamp),
        }
    }
}

/// One commit of a change, as `reef.api.v1.Change` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts `reef.api.v1.Change` reports, not states of one machine"
)]
pub struct ChangeInfo {
    /// The change.
    pub change_id: ChangeIdHex,
    /// The commit.
    pub commit_id: CommitIdHex,
    /// Its parents, in jj's order.
    pub parent_commit_ids: Vec<CommitIdHex>,
    /// Their changes, in the same order.
    pub parent_change_ids: Vec<ChangeIdHex>,
    /// The full description.
    pub description: String,
    /// The author.
    pub author: SignatureInfo,
    /// The committer.
    pub committer: SignatureInfo,
    /// The tree, in hex; `None` when the tree is conflicted.
    pub tree_id: Option<String>,
    /// The tree equals its parents' merged tree (the root commit is empty).
    pub empty: bool,
    /// The tree has conflicts.
    pub conflict: bool,
    /// The change has more than one visible commit in the view read.
    pub divergent: bool,
    /// The commit is in `immutable()`.
    pub immutable: bool,
    /// The commit is not visible in the view read.
    pub hidden: bool,
    /// Local bookmarks as `name` and remote ones as `name@remote` (the
    /// `git` remote left out) with this commit among their targets, sorted.
    pub bookmarks: Vec<String>,
    /// Local tags with this commit among their targets, sorted.
    pub tags: Vec<String>,
    /// jj workspaces whose working-copy commit this is, sorted.
    pub workspaces: Vec<String>,
}

/// The remote whose bookmarks are git's own refs in a colocated
/// repository, never reported.
const GIT_REMOTE: &str = "git";

/// Bookmarks, tags and working copies of one view, by commit.
#[derive(Debug, Default)]
struct Decorations {
    bookmarks: HashMap<CommitId, Vec<String>>,
    tags: HashMap<CommitId, Vec<String>>,
    workspaces: HashMap<CommitId, Vec<String>>,
}

impl Decorations {
    fn of(repo: &ReadonlyRepo) -> Decorations {
        let view = repo.view();
        let mut decorations = Decorations::default();
        let add = |map: &mut HashMap<CommitId, Vec<String>>, id: &CommitId, name: String| {
            map.entry(id.clone()).or_default().push(name);
        };
        for (name, target) in view.local_bookmarks() {
            for id in target.present_adds() {
                add(&mut decorations.bookmarks, id, name.as_str().to_owned());
            }
        }
        for (symbol, remote_ref) in view.all_remote_bookmarks() {
            if symbol.remote.as_str() == GIT_REMOTE {
                continue;
            }
            let label = format!("{}@{}", symbol.name.as_str(), symbol.remote.as_str());
            for id in remote_ref.target.present_adds() {
                add(&mut decorations.bookmarks, id, label.clone());
            }
        }
        for (name, target) in view.local_tags() {
            for id in target.present_adds() {
                add(&mut decorations.tags, id, name.as_str().to_owned());
            }
        }
        for (name, id) in view.wc_commit_ids() {
            add(&mut decorations.workspaces, id, name.as_str().to_owned());
        }
        for names in decorations
            .bookmarks
            .values_mut()
            .chain(decorations.tags.values_mut())
            .chain(decorations.workspaces.values_mut())
        {
            names.sort();
            names.dedup();
        }
        decorations
    }

    fn get(map: &HashMap<CommitId, Vec<String>>, id: &CommitId) -> Vec<String> {
        map.get(id).cloned().unwrap_or_default()
    }
}

/// Describes commits of one view as [`ChangeInfo`]s; the view's
/// decorations and `immutable()` are computed once.
pub struct Describer<'a> {
    repo: &'a ReadonlyRepo,
    decorations: Decorations,
    immutable: Box<RevsetContainingFn<'a>>,
}

impl std::fmt::Debug for Describer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Describer").finish_non_exhaustive()
    }
}

impl<'a> Describer<'a> {
    /// A describer over `repo`'s view; `scope` parses `immutable()`.
    pub fn new(repo: &'a ReadonlyRepo, scope: &RevsetScope<'_>) -> Result<Self, RevisionError> {
        let scope = RevsetScope {
            workspace: None,
            ..*scope
        };
        let immutable = revset::parse(repo, "immutable()", &scope)?;
        let immutable = revset::evaluate(repo, &immutable)?.containing_fn();
        Ok(Describer {
            repo,
            decorations: Decorations::of(repo),
            immutable,
        })
    }

    /// The commit `id` as a [`ChangeInfo`].
    pub async fn describe(&self, id: &CommitIdHex) -> Result<ChangeInfo, RevisionError> {
        self.describe_id(&id.to_id()).await
    }

    async fn describe_id(&self, id: &CommitId) -> Result<ChangeInfo, RevisionError> {
        let repo = self.repo;
        let store = repo.store();
        let commit = store.get_commit_async(id).await?;
        let mut parent_change_ids = Vec::with_capacity(commit.parent_ids().len());
        for parent in commit.parent_ids() {
            if parent == store.root_commit_id() {
                parent_change_ids.push(ChangeIdHex::of(store.root_change_id()));
            } else {
                let parent = store.get_commit_async(parent).await?;
                parent_change_ids.push(ChangeIdHex::of(parent.change_id()));
            }
        }
        let is_root = id == store.root_commit_id();
        let empty = is_root || commit.is_empty(repo).await?;
        let targets = repo
            .resolve_change_id(commit.change_id())
            .await
            .map_err(|error| RevisionError::Other(error.into()))?;
        let (divergent, hidden) = match &targets {
            Some(targets) => (targets.is_divergent(), !targets.has_visible(id)),
            None => (false, !is_root),
        };
        let immutable = (self.immutable)(id)
            .await
            .map_err(RevisionError::Evaluation)?;
        Ok(ChangeInfo {
            change_id: ChangeIdHex::of(commit.change_id()),
            commit_id: CommitIdHex::of(id),
            parent_commit_ids: commit.parent_ids().iter().map(CommitIdHex::of).collect(),
            parent_change_ids,
            description: commit.description().to_owned(),
            author: commit.author().into(),
            committer: commit.committer().into(),
            tree_id: commit.tree_ids().as_resolved().map(ObjectId::hex),
            empty,
            conflict: commit.has_conflict(),
            divergent,
            immutable,
            hidden,
            bookmarks: Decorations::get(&self.decorations.bookmarks, id),
            tags: Decorations::get(&self.decorations.tags, id),
            workspaces: Decorations::get(&self.decorations.workspaces, id),
        })
    }
}

/// How a log entry reaches a parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// A direct parent, itself in the log.
    Direct,
    /// Reached through commits the log leaves out.
    Indirect,
    /// Outside the revset entirely.
    Missing,
}

/// One entry of the log: a commit and its edges to parents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogNode {
    /// The commit.
    pub commit_id: CommitIdHex,
    /// Its edges, in jj's order.
    pub edges: Vec<(CommitIdHex, EdgeKind)>,
}

/// A page of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    /// Up to `limit` entries after the skipped ones.
    pub nodes: Vec<LogNode>,
    /// Entries follow these.
    pub more: bool,
    /// The last entry skipped, to check a page token against.
    pub last_skipped: Option<CommitIdHex>,
}

/// The log of `parsed`: `skip` entries skipped, then up to `limit`, in
/// `jj log`'s order. `should_stop` is checked between entries; when it
/// returns true the page ends there, with `more` set.
pub async fn log_page(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
    skip: usize,
    limit: usize,
    should_stop: &dyn Fn() -> bool,
) -> Result<LogPage, RevisionError> {
    let revset = revset::evaluate(repo, parsed)?;
    let grouped = TopoGroupedGraph::new(revset.stream_graph(), |id: &CommitId| id);
    let mut stream = std::pin::pin!(grouped.stream());
    let mut page = LogPage {
        nodes: Vec::new(),
        more: false,
        last_skipped: None,
    };
    let mut seen = 0;
    while let Some(node) = stream.next().await {
        let (id, edges) = node.map_err(RevisionError::Evaluation)?;
        if seen < skip {
            seen += 1;
            page.last_skipped = Some(CommitIdHex::of(&id));
            continue;
        }
        if page.nodes.len() == limit || should_stop() {
            page.more = true;
            break;
        }
        page.nodes.push(LogNode {
            commit_id: CommitIdHex::of(&id),
            edges: edges
                .into_iter()
                .map(|edge| {
                    let kind = match edge.edge_type {
                        GraphEdgeType::Direct => EdgeKind::Direct,
                        GraphEdgeType::Indirect => EdgeKind::Indirect,
                        GraphEdgeType::Missing => EdgeKind::Missing,
                    };
                    (CommitIdHex::of(&edge.target), kind)
                })
                .collect(),
        });
    }
    Ok(page)
}

/// Up to `limit` commits of `parsed` in the revset's order (children
/// first), and how many it has in all.
pub async fn commits(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
    limit: usize,
) -> Result<(Vec<CommitIdHex>, usize), RevisionError> {
    let revset = revset::evaluate(repo, parsed)?;
    let mut stream = revset.stream();
    let mut ids = Vec::new();
    let mut total = 0;
    while let Some(id) = stream.next().await {
        let id = id.map_err(RevisionError::Evaluation)?;
        if ids.len() < limit {
            ids.push(CommitIdHex::of(&id));
        }
        total += 1;
    }
    Ok((ids, total))
}

/// The change ids of `parsed`'s commits in the revset's order, without
/// repeats, up to `limit`, and how many distinct changes it has in all.
pub async fn change_ids(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
    limit: usize,
) -> Result<(Vec<ChangeIdHex>, usize), RevisionError> {
    let revset = revset::evaluate(repo, parsed)?;
    let mut stream = revset.commit_change_ids();
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    while let Some(pair) = stream.next().await {
        let (_, change) = pair.map_err(RevisionError::Evaluation)?;
        if seen.insert(change.clone()) && ids.len() < limit {
            ids.push(ChangeIdHex::of(&change));
        }
    }
    Ok((ids, seen.len()))
}
