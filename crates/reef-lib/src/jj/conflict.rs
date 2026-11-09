//! Conflict objects (R3.25, ADR 0065): jj's stored conflicts as Reef's own
//! records.
//!
//! jj stores a conflicted commit's tree as a `Merge<TreeId>` with one label
//! per term: *n* sides and *n − 1* bases, interleaved side, base, side, ….
//! [`read_tree`] turns such a tree into a [`ConflictObject`]: every term with
//! its role and label, and every conflicted path with each term's value, an
//! id, a kind, and whether a parent carries the same conflict. It takes a
//! `MergedTree`, not a commit, so a merge Reef has not written yet (R5.01)
//! reads the same way; [`read`] is the commit's case.
//!
//! [`attribute`] says which commit each term came from. A label is free
//! text that any client can write, so it only proposes a commit; a term's
//! source is `Verified` only when the commit holds exactly the term's value
//! at the conflicted paths (S-127). [`hunks`] splits one file into resolved
//! and conflicted hunks with jj's merge options, on request and bounded.
//!
//! Nothing here is stored: every read derives the object from jj again.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use bstr::BString;
use futures::{AsyncReadExt as _, StreamExt as _};
use jj_lib::backend::{BackendError, CommitId, MergedTreeValue, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::files::{MergeOptions, MergeResult, merge_hunks};
use jj_lib::graph::TopoGroupedGraph;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::store::Store;
use sha2::{Digest as _, Sha256};

use super::log::SignatureInfo;
use super::repo::{ChangeIdHex, CommitIdHex};
use super::revset::{self, ParsedRevset, RevisionError};
use crate::error::{Classify, ErrorKind, classify_by_source};

/// Terms a conflict may have and still be reported term by term; a wider
/// one is [`ConflictObject::too_wide`] (S-129).
pub const MAX_TERMS: usize = 64;
/// Conflicted paths a source is checked at, at most.
pub const MAX_CHECKED_PATHS: usize = 200;
/// Candidate commits per conflict, at most.
pub const MAX_CANDIDATES: usize = 32;
/// Predecessors of the conflicted change among the candidates, at most.
pub const MAX_PREDECESSORS: usize = 16;
/// Operations walked back from the head to find predecessors, at most.
pub const MAX_OPERATIONS_WALKED: usize = 1000;
/// Bytes of a label, or of a source's description (its first line), kept.
pub const MAX_TEXT: usize = 1024;
/// Revset entries one listing page looks at, at most.
pub const MAX_SCANNED: usize = 10_000;
/// Commits one label may name.
const MAX_LABEL_COMMITS: usize = 8;
/// Bytes git reads to decide a file is binary: a NUL among them.
const BINARY_PEEK: usize = 8000;
/// Bytes of hunk text one file's hunks may hold, as UTF-8 after invalid
/// sequences are replaced: well under a response's 3 MiB.
pub const MAX_HUNK_TEXT: usize = 2 << 20;
/// Bytes one file's terms may hold together for its hunks.
pub const MAX_HUNK_INPUT: usize = 32 << 20;

/// A conflict could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConflictError {
    /// A commit, tree or file could not be read.
    #[error("failed to read the conflicted trees")]
    Backend(#[from] BackendError),
    /// The commit's tree has no conflict.
    #[error("commit {commit_id} has no conflict")]
    NotConflicted {
        /// The commit.
        commit_id: CommitIdHex,
    },
    /// The predecessors could not be walked.
    #[error("failed to walk the change's predecessors")]
    Predecessors(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The index could not be read.
    #[error("failed to read the commit index")]
    Index(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The revset could not be evaluated.
    #[error("failed to evaluate the revset")]
    Revision(#[source] RevisionError),
    /// A page cursor that is not a repository path.
    #[error("the page cursor is not a repository path")]
    BadCursor,
}

impl Classify for ConflictError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Backend(error) => classify_by_source(error),
            Self::NotConflicted { .. } => ErrorKind::NotFound,
            Self::Predecessors(_) | Self::Index(_) => ErrorKind::Internal,
            Self::Revision(error) => error.kind(),
            Self::BadCursor => ErrorKind::InvalidArgument,
        }
    }
}

/// A term's place in the merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// One of the *n* sides (jj's adds).
    Side,
    /// One of the *n − 1* bases (jj's removes).
    Base,
}

/// One term of the conflicted tree, in jj's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    /// Its index: side 0, base 0, side 1, ….
    pub index: u32,
    /// Side or base.
    pub role: Role,
    /// jj's label, verbatim; empty for none.
    pub label: String,
    /// The label's last parenthesized suffix (`rebase destination`), as
    /// written; empty for none.
    pub origin: String,
}

/// What one term holds at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// Nothing.
    Absent,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// A directory.
    Tree,
    /// A git submodule.
    Submodule,
}

/// One term's value at a conflicted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermValue {
    /// The term's index in the commit's merge.
    pub index: u32,
    /// What it holds.
    pub kind: ValueType,
    /// The object id in hex (blob, symlink, tree or submodule commit);
    /// empty when absent.
    pub id: String,
    /// The executable bit, for a file.
    pub executable: bool,
}

/// How a path conflicts, summarizing its terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// Files on every present term, and the bits agree.
    Content,
    /// Only the executable bit differs.
    Executable,
    /// No base has it; several sides add it.
    AddAdd,
    /// Some side deletes it.
    ModifyDelete,
    /// A file against a symlink, directory or submodule.
    Type,
    /// Anything else (symlinks or submodules against their own kind).
    Other,
}

/// One conflicted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictFile {
    /// The path, repository-relative.
    pub path: String,
    /// SHA-256 of the path and its simplified terms, 64 hex digits: the
    /// same in a child that inherits the conflict, and across edits to
    /// other paths.
    pub id: String,
    /// How it conflicts.
    pub kind: FileKind,
    /// A parent carries the same conflict at this path.
    pub inherited: bool,
    /// Each term's value, in the commit's term order.
    pub terms: Vec<TermValue>,
}

/// One conflicted tree, as Reef reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictObject {
    /// `cfl_` and 32 hex digits: SHA-256 over the change id and each
    /// file's id in path order.
    pub id: String,
    /// The number of sides.
    pub arity: u32,
    /// Every term, in jj's order; empty when [`Self::too_wide`].
    pub terms: Vec<Term>,
    /// The files the read asked for, in path order.
    pub files: Vec<ConflictFile>,
    /// Conflicted paths in all.
    pub files_total: u32,
    /// Conflicted paths no parent carries.
    pub introduced: u32,
    /// Files after the page's last one match the read's filter.
    pub more: bool,
    /// More than [`MAX_TERMS`] terms: files carry paths only, and sources
    /// are not attributed.
    pub too_wide: bool,
    /// The values of every conflicted path, in path order, for
    /// [`attribute`]: at most [`MAX_CHECKED_PATHS`].
    checked: Vec<(RepoPathBuf, MergedTreeValue)>,
}

/// Which files a read returns.
#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    /// Only paths equal to, or under, one of these; empty for every path.
    pub paths: Vec<String>,
    /// Only paths after this one.
    pub after: Option<String>,
    /// At most this many files.
    pub limit: usize,
}

/// Whether `path` is one of `filter` or under one ("" is the root).
fn wanted(path: &str, filter: &[String]) -> bool {
    filter.is_empty()
        || filter.iter().any(|prefix| {
            prefix.is_empty()
                || path == prefix
                || (path.starts_with(prefix.as_str())
                    && path.as_bytes().get(prefix.len()) == Some(&b'/'))
        })
}

/// The conflict of `commit`'s tree in `repo`.
pub async fn read(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    options: &ReadOptions,
) -> Result<ConflictObject, ConflictError> {
    let commit = repo.store().get_commit_async(&commit.to_id()).await?;
    let tree = commit.tree();
    if !tree.has_conflict() {
        return Err(ConflictError::NotConflicted {
            commit_id: CommitIdHex::of(commit.id()),
        });
    }
    let parents = commit.parents().await?;
    let parent_trees: Vec<MergedTree> = parents.iter().map(Commit::tree).collect();
    read_tree(
        &tree,
        &parent_trees,
        &ChangeIdHex::of(commit.change_id()),
        options,
    )
    .await
}

/// The conflict of `tree`, whose parents' trees are `parents` (each one's
/// own tree, so a merge's conflict is its own and a child's is inherited),
/// for the change `change_id`. `tree` must be conflicted.
pub async fn read_tree(
    tree: &MergedTree,
    parents: &[MergedTree],
    change_id: &ChangeIdHex,
    options: &ReadOptions,
) -> Result<ConflictObject, ConflictError> {
    let ids = tree.tree_ids();
    let arity = ids.num_sides();
    let count = ids.as_slice().len();
    let too_wide = count > MAX_TERMS;
    let labels = tree.labels().as_slice();
    let terms = if too_wide {
        Vec::new()
    } else {
        (0..count)
            .map(|index| {
                let full = labels.get(index).map_or("", String::as_str);
                let label = bounded(full, MAX_TEXT);
                Term {
                    index: to_u32(index),
                    role: if index % 2 == 0 {
                        Role::Side
                    } else {
                        Role::Base
                    },
                    origin: bounded(origin(full), MAX_TEXT),
                    label,
                }
            })
            .collect()
    };
    let mut conflict_digest = Sha256::new();
    conflict_digest.update(change_id.as_str().as_bytes());
    conflict_digest.update([0]);
    let mut files = Vec::new();
    let mut checked = Vec::new();
    let mut total = 0usize;
    let mut introduced = 0usize;
    let mut more = false;
    let after = match options.after.as_deref() {
        Some(after) => {
            Some(RepoPathBuf::from_internal_string(after).map_err(|_| ConflictError::BadCursor)?)
        }
        None => None,
    };
    let mut stream = tree.conflicts();
    while let Some((path, value)) = stream.next().await {
        let value = value?;
        let text = path.as_internal_file_string().to_owned();
        let simplified = value.simplify();
        let id = file_id(&text, &simplified);
        conflict_digest.update(id.as_bytes());
        total += 1;
        let inherited = inherited_from(parents, &path, &id).await?;
        if !inherited {
            introduced += 1;
        }
        if checked.len() < MAX_CHECKED_PATHS {
            checked.push((path.clone(), value.clone()));
        }
        // jj's path order (`/` before every other byte), the stream's.
        let past = after
            .as_ref()
            .is_none_or(|after| path.as_ref() > after.as_ref());
        if !past || !wanted(&text, &options.paths) {
            continue;
        }
        if files.len() >= options.limit {
            more = true;
            continue;
        }
        files.push(ConflictFile {
            kind: file_kind(&simplified),
            terms: if too_wide {
                Vec::new()
            } else {
                term_values(&value)
            },
            path: text,
            id,
            inherited,
        });
    }
    let digest = conflict_digest.finalize();
    Ok(ConflictObject {
        id: format!("cfl_{}", &hex(&digest)[..32]),
        arity: to_u32(arity),
        terms,
        files,
        files_total: to_u32(total),
        introduced: to_u32(introduced),
        more,
        too_wide,
        checked,
    })
}

/// One conflicted commit of a listing.
#[derive(Clone, Debug)]
pub struct Listed {
    /// Its position in the revset's log order, from 1: a page token's
    /// `skip` that resumes after it.
    pub position: usize,
    /// Its commit.
    pub commit_id: CommitIdHex,
    /// Its conflict, with its first files.
    pub object: ConflictObject,
}

/// A page of [`list`].
#[derive(Clone, Debug, Default)]
pub struct ListPage {
    /// The conflicts, in the revset's log order.
    pub conflicts: Vec<Listed>,
    /// More of the revset is left to look at.
    pub more: bool,
    /// The position and commit of the last entry looked at in full: where
    /// the next page resumes when `more`.
    pub resume: Option<(usize, CommitIdHex)>,
    /// The commit at position `skip`, for checking a page token.
    pub last_skipped: Option<CommitIdHex>,
}

/// Up to `limit` conflicted commits of `parsed` after its first `skip`
/// entries, in the revset's log order (as `ListChanges` returns it), each
/// with its first `files` files; with `introduced_only`, commits whose
/// every conflicted path is inherited are left out. A page looks at no
/// more than [`MAX_SCANNED`] entries, and ends early when `should_stop`
/// returns true; either way `more` is set, and the next page may hold
/// none.
pub async fn list(
    repo: &ReadonlyRepo,
    parsed: &ParsedRevset,
    skip: usize,
    limit: usize,
    files: usize,
    introduced_only: bool,
    should_stop: &dyn Fn() -> bool,
) -> Result<ListPage, ConflictError> {
    let evaluated = revset::evaluate(repo, parsed).map_err(ConflictError::Revision)?;
    let grouped = TopoGroupedGraph::new(evaluated.stream_graph(), |id: &CommitId| id);
    let mut stream = std::pin::pin!(grouped.stream());
    let mut page = ListPage::default();
    let mut position = 0usize;
    let mut scanned = 0usize;
    let options = ReadOptions {
        limit: files,
        ..ReadOptions::default()
    };
    while let Some(node) = stream.next().await {
        let (id, _) =
            node.map_err(|error| ConflictError::Revision(RevisionError::Evaluation(error)))?;
        position += 1;
        if position <= skip {
            page.last_skipped = Some(CommitIdHex::of(&id));
            continue;
        }
        if scanned >= MAX_SCANNED || should_stop() {
            page.more = true;
            break;
        }
        scanned += 1;
        let commit_id = CommitIdHex::of(&id);
        let commit = repo.store().get_commit_async(&id).await?;
        if commit.has_conflict() {
            if page.conflicts.len() == limit {
                page.more = true;
                break;
            }
            let object = read(repo, &commit_id, &options).await?;
            if !introduced_only || object.introduced > 0 {
                page.conflicts.push(Listed {
                    position,
                    commit_id: commit_id.clone(),
                    object,
                });
            }
        }
        page.resume = Some((position, commit_id));
    }
    Ok(page)
}

/// Whether one of `parents` has the conflict `id` at `path`.
async fn inherited_from(
    parents: &[MergedTree],
    path: &RepoPath,
    id: &str,
) -> Result<bool, ConflictError> {
    for parent in parents {
        if !parent.has_conflict() {
            continue;
        }
        let value = parent.path_value(path).await?;
        if value.is_resolved() {
            continue;
        }
        if file_id(path.as_internal_file_string(), &value.simplify()) == id {
            return Ok(true);
        }
    }
    Ok(false)
}

/// SHA-256 over `path` and `simplified`'s terms, in hex.
fn file_id(path: &str, simplified: &MergedTreeValue) -> String {
    let mut digest = Sha256::new();
    digest.update(path.as_bytes());
    digest.update([0]);
    for term in simplified {
        let (kind, id, executable) = describe(term.as_ref());
        digest.update([kind as u8, u8::from(executable)]);
        digest.update(id.as_bytes());
        digest.update([0]);
    }
    hex(&digest.finalize())
}

/// A term's type, id and executable bit.
fn describe(value: Option<&TreeValue>) -> (ValueType, String, bool) {
    match value {
        None => (ValueType::Absent, String::new(), false),
        Some(TreeValue::File { id, executable, .. }) => (ValueType::File, id.hex(), *executable),
        Some(TreeValue::Symlink(id)) => (ValueType::Symlink, id.hex(), false),
        Some(TreeValue::Tree(id)) => (ValueType::Tree, id.hex(), false),
        Some(TreeValue::GitSubmodule(id)) => (ValueType::Submodule, id.hex(), false),
    }
}

fn term_values(value: &MergedTreeValue) -> Vec<TermValue> {
    value
        .iter()
        .enumerate()
        .map(|(index, term)| {
            let (kind, id, executable) = describe(term.as_ref());
            TermValue {
                index: to_u32(index),
                kind,
                id,
                executable,
            }
        })
        .collect()
}

/// How `simplified` conflicts.
fn file_kind(simplified: &MergedTreeValue) -> FileKind {
    let present: Vec<&TreeValue> = simplified.iter().flatten().collect();
    let files = present
        .iter()
        .filter(|value| matches!(value, TreeValue::File { .. }))
        .count();
    if files > 0 && files < present.len() {
        return FileKind::Type;
    }
    let kinds: HashSet<std::mem::Discriminant<TreeValue>> = present
        .iter()
        .map(|value| std::mem::discriminant(*value))
        .collect();
    if kinds.len() > 1 {
        return FileKind::Type;
    }
    if simplified.adds().any(Option::is_none) {
        return FileKind::ModifyDelete;
    }
    if files == 0 {
        return FileKind::Other;
    }
    if simplified.removes().all(Option::is_none) {
        return FileKind::AddAdd;
    }
    let contents = simplified.map(|value| match value {
        Some(TreeValue::File { id, .. }) => Some(id.clone()),
        _ => None,
    });
    if contents.simplify().is_resolved() {
        FileKind::Executable
    } else {
        FileKind::Content
    }
}

/// The label's last parenthesized suffix, without the parentheses.
pub fn origin(label: &str) -> &str {
    let Some(stripped) = label.strip_suffix(')') else {
        return "";
    };
    match stripped.rfind(" (") {
        Some(start) => &stripped[start + 2..],
        None => "",
    }
}

/// The `(change, commit)` prefixes a label names, in jj's form: a change
/// prefix in reverse hex (`k`–`z`) and a commit prefix in hex, one pair
/// before a quoted subject, or several pairs joined by `", "`.
pub fn label_commits(label: &str) -> Vec<(String, String)> {
    let mut rest = label;
    let suffix = origin(label);
    if !suffix.is_empty() {
        rest = &rest[..rest.len() - suffix.len() - 3];
    }
    let groups: Vec<&str> = match rest.find(" \"") {
        Some(quote) => vec![&rest[..quote]],
        None => rest.split(", ").collect(),
    };
    groups
        .into_iter()
        .take(MAX_LABEL_COMMITS)
        .filter_map(|group| {
            let (change, commit) = group.trim().split_once(' ')?;
            let change_ok = !change.is_empty() && change.bytes().all(|b| matches!(b, b'k'..=b'z'));
            let commit_ok = commit.len() >= 4
                && commit.len() <= 40
                && commit
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            (change_ok && commit_ok).then(|| (change.to_owned(), commit.to_owned()))
        })
        .collect()
}

/// How a term's source was decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// The commit holds exactly the term's value at every checked
    /// conflicted path.
    Verified,
    /// The term's label names the commit, and the content does not
    /// confirm it: declared, not checked.
    Label,
}

/// The commit a term came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The commit.
    pub commit_id: CommitIdHex,
    /// Its change.
    pub change_id: ChangeIdHex,
    /// How it was decided.
    pub attribution: Attribution,
    /// Its description's first line, at most [`MAX_TEXT`] bytes.
    pub description: String,
    /// Its author.
    pub author: SignatureInfo,
    /// When it was committed.
    pub committed_at: SystemTime,
}

/// The source of each of `object`'s terms (`None` when nothing resolved or
/// matched), for `commit`, whose tree it is.
///
/// The commits linked to `commit` are its parents, its change's
/// predecessors and their parents ([`MAX_PREDECESSORS`], walking at most
/// [`MAX_OPERATIONS_WALKED`] operations); a commit a term's label names
/// counts as linked when it is one of those or an ancestor of a parent. A
/// term's source is `Verified` by the first linked candidate, its own
/// label's first, that holds the term's value at every checked path and
/// has it present at one at least (S-127). Otherwise a commit its label
/// names is its source as `Label`. [`MAX_CANDIDATES`] in all.
pub async fn attribute(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    object: &ConflictObject,
) -> Result<Vec<Option<Source>>, ConflictError> {
    if object.too_wide {
        return Ok(Vec::new());
    }
    let store = repo.store();
    let commit = store.get_commit_async(&commit.to_id()).await?;
    let parents = commit.parents().await?;
    let linked = linked_commits(repo, &commit, &parents).await?;
    // Each term's label commits, and whether each is linked.
    let mut candidates: Vec<(Commit, bool)> = linked.into_iter().map(|c| (c, true)).collect();
    let mut named: Vec<Vec<usize>> = Vec::with_capacity(object.terms.len());
    for term in &object.terms {
        let mut mine = Vec::new();
        for (change, prefix) in label_commits(&term.label) {
            let Some(found) = resolve_label_commit(repo, &change, &prefix).await? else {
                continue;
            };
            if found.id() == commit.id() {
                continue;
            }
            let index = match candidates.iter().position(|(c, _)| c.id() == found.id()) {
                Some(index) => index,
                None if candidates.len() < MAX_CANDIDATES => {
                    let mut is_linked = false;
                    for parent in &parents {
                        if repo
                            .index()
                            .is_ancestor(found.id(), parent.id())
                            .await
                            .map_err(|error| ConflictError::Index(Box::new(error)))?
                        {
                            is_linked = true;
                            break;
                        }
                    }
                    candidates.push((found, is_linked));
                    candidates.len() - 1
                }
                None => continue,
            };
            mine.push(index);
        }
        named.push(mine);
    }
    // Each candidate's values at the checked paths, read once.
    let mut values: HashMap<usize, Vec<MergedTreeValue>> = HashMap::new();
    let mut sources = Vec::with_capacity(object.terms.len());
    for (term_index, term) in object.terms.iter().enumerate() {
        let index = usize::try_from(term.index).unwrap_or(usize::MAX);
        let present = object
            .checked
            .iter()
            .any(|(_, value)| value.as_slice().get(index).is_some_and(Option::is_some));
        let mine = &named[term_index];
        let order: Vec<usize> = mine
            .iter()
            .copied()
            .chain((0..candidates.len()).filter(|index| !mine.contains(index)))
            .filter(|candidate| candidates[*candidate].1)
            .collect();
        let mut found = None;
        for candidate in order.into_iter().filter(|_| present) {
            if let std::collections::hash_map::Entry::Vacant(entry) = values.entry(candidate) {
                let tree = candidates[candidate].0.tree();
                let mut at = Vec::with_capacity(object.checked.len());
                for (path, _) in &object.checked {
                    at.push(tree.path_value(path).await?);
                }
                entry.insert(at);
            }
            let at = &values[&candidate];
            let matches = object.checked.iter().zip(at).all(|((_, value), theirs)| {
                theirs
                    .as_resolved()
                    .is_some_and(|theirs| value.as_slice().get(index) == Some(theirs))
            });
            if matches {
                found = Some((candidate, Attribution::Verified));
                break;
            }
        }
        if found.is_none()
            && let Some(first) = mine.first()
        {
            found = Some((*first, Attribution::Label));
        }
        sources.push(found.map(|(candidate, attribution)| {
            let commit = &candidates[candidate].0;
            Source {
                commit_id: CommitIdHex::of(commit.id()),
                change_id: ChangeIdHex::of(commit.change_id()),
                attribution,
                description: bounded(commit.description().lines().next().unwrap_or(""), MAX_TEXT),
                author: SignatureInfo::from(commit.author()),
                committed_at: SignatureInfo::from(commit.committer()).at,
            }
        }));
    }
    Ok(sources)
}

/// The commits linked to `commit`, in the order they are tried: its
/// `parents`, its change's predecessors, and their parents.
async fn linked_commits(
    repo: &ReadonlyRepo,
    commit: &Commit,
    parents: &[Commit],
) -> Result<Vec<Commit>, ConflictError> {
    let mut linked: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    seen.insert(commit.id().clone());
    for parent in parents {
        if seen.insert(parent.id().clone()) {
            linked.push(parent.clone());
        }
    }
    let predecessors = predecessors_of(repo, commit.id()).await?;
    for predecessor in &predecessors {
        if linked.len() < MAX_CANDIDATES && seen.insert(predecessor.id().clone()) {
            linked.push(predecessor.clone());
        }
    }
    for predecessor in &predecessors {
        for parent in predecessor.parents().await? {
            if linked.len() < MAX_CANDIDATES && seen.insert(parent.id().clone()) {
                linked.push(parent);
            }
        }
    }
    Ok(linked)
}

/// Up to [`MAX_PREDECESSORS`] earlier commits of `start`'s change, newest
/// first, from the operations that rewrote it, walking at most
/// [`MAX_OPERATIONS_WALKED`] operations back from the head.
async fn predecessors_of(
    repo: &ReadonlyRepo,
    start: &CommitId,
) -> Result<Vec<Commit>, ConflictError> {
    let mut to_visit: Vec<CommitId> = vec![start.clone()];
    let mut found: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    seen.insert(start.clone());
    let mut operations = std::pin::pin!(jj_lib::op_walk::walk_ancestors(std::slice::from_ref(
        repo.operation()
    )));
    let mut walked = 0usize;
    while !to_visit.is_empty() && found.len() < MAX_PREDECESSORS && walked < MAX_OPERATIONS_WALKED {
        let Some(operation) = operations.next().await else {
            break;
        };
        let operation = operation.map_err(|error| ConflictError::Predecessors(Box::new(error)))?;
        walked += 1;
        let mut next = Vec::with_capacity(to_visit.len());
        for id in std::mem::take(&mut to_visit) {
            let Some(earlier) = operation.predecessors_for_commit(&id) else {
                next.push(id);
                continue;
            };
            for earlier in earlier {
                if found.len() >= MAX_PREDECESSORS || !seen.insert(earlier.clone()) {
                    continue;
                }
                match repo.store().get_commit_async(earlier).await {
                    Ok(commit) => found.push(commit),
                    Err(BackendError::ObjectNotFound { .. }) => continue,
                    Err(error) => return Err(error.into()),
                }
                next.push(earlier.clone());
            }
        }
        to_visit = next;
    }
    Ok(found)
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The commit a label's `(change, commit)` prefixes name, when the commit
/// prefix resolves to one indexed commit whose change starts with the
/// change prefix.
async fn resolve_label_commit(
    repo: &ReadonlyRepo,
    change: &str,
    commit: &str,
) -> Result<Option<Commit>, ConflictError> {
    let Some(prefix) = HexPrefix::try_from_hex(commit) else {
        return Ok(None);
    };
    let resolution = repo
        .index()
        .resolve_commit_id_prefix(&prefix)
        .await
        .map_err(|error| ConflictError::Index(Box::new(error)))?;
    let PrefixResolution::SingleMatch(id) = resolution else {
        return Ok(None);
    };
    let found = match repo.store().get_commit_async(&id).await {
        Ok(found) => found,
        Err(BackendError::ObjectNotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(found
        .change_id()
        .reverse_hex()
        .starts_with(change)
        .then_some(found))
}

/// One hunk's text on one term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkTerm {
    /// The term's index in the commit's merge.
    pub index: u32,
    /// Its text, UTF-8 with invalid sequences replaced.
    pub text: String,
    /// Its first line, from 1, in that term's file as the hunks rebuild it
    /// (the term with the other terms' non-conflicting changes applied, as
    /// jj's markers show it).
    pub start_line: u32,
    /// Its lines.
    pub line_count: u32,
}

/// One hunk of a conflicted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Every term agrees here.
    pub resolved: bool,
    /// The agreed text, for a resolved hunk.
    pub text: String,
    /// Each term's text, for a conflicted hunk, one per term of the file's
    /// simplified merge.
    pub terms: Vec<HunkTerm>,
}

/// A file's hunks, or why it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileHunks {
    /// The file, resolved and conflicted hunks in order.
    Hunks(Vec<Hunk>),
    /// A term has a NUL in its first 8,000 bytes.
    Binary,
    /// A term is over the size limit, or the hunks' text would be.
    TooLarge,
    /// Not a conflict between files.
    NotFiles,
}

/// The hunks of `path` in `tree`, with `options` (jj's), reading at most
/// `max_file_bytes` of each term.
pub async fn hunks(
    tree: &MergedTree,
    path: &str,
    options: &MergeOptions,
    max_file_bytes: usize,
) -> Result<FileHunks, ConflictError> {
    let Ok(path) = RepoPathBuf::from_internal_string(path) else {
        return Ok(FileHunks::NotFiles);
    };
    let value = tree.path_value(&path).await?;
    let indexes: Merge<usize> = Merge::from_vec((0..value.as_slice().len()).collect::<Vec<_>>());
    let (indexes, simplified) = indexes
        .zip(value.clone())
        .simplify_by(|(_, value)| value)
        .unzip();
    let store: &Store = tree.store();
    let mut contents: Vec<BString> = Vec::with_capacity(simplified.as_slice().len());
    let mut input = 0usize;
    for term in &simplified {
        match term {
            None => contents.push(BString::default()),
            Some(TreeValue::File { id, .. }) => {
                let mut reader = store.read_file(&path, id).await?;
                let mut bytes = Vec::new();
                let limit = u64::try_from(max_file_bytes).unwrap_or(u64::MAX) + 1;
                (&mut reader)
                    .take(limit)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| BackendError::ReadFile {
                        path: path.clone(),
                        id: id.clone(),
                        source: error.into(),
                    })?;
                input += bytes.len();
                if bytes.len() > max_file_bytes || input > MAX_HUNK_INPUT {
                    return Ok(FileHunks::TooLarge);
                }
                if bytes[..bytes.len().min(BINARY_PEEK)].contains(&0) {
                    return Ok(FileHunks::Binary);
                }
                contents.push(BString::from(bytes));
            }
            Some(_) => return Ok(FileHunks::NotFiles),
        }
    }
    let contents = Merge::from_vec(contents);
    let pieces: Vec<Merge<BString>> = match merge_hunks(&contents, options) {
        MergeResult::Resolved(text) => vec![Merge::resolved(text)],
        MergeResult::Conflict(pieces) => pieces,
    };
    let indexes: Vec<u32> = indexes.iter().map(|&index| to_u32(index)).collect();
    let mut lines = vec![0u32; indexes.len()];
    let mut out = Vec::with_capacity(pieces.len());
    let mut size = 0usize;
    for piece in pieces {
        if let Some(text) = piece.as_resolved() {
            let count = line_count(text);
            for line in &mut lines {
                *line = line.saturating_add(count);
            }
            let text = String::from_utf8_lossy(text).into_owned();
            size += text.len();
            out.push(Hunk {
                resolved: true,
                text,
                terms: Vec::new(),
            });
        } else {
            let mut terms = Vec::with_capacity(indexes.len());
            for (position, text) in piece.iter().enumerate() {
                let count = line_count(text);
                let start = lines.get(position).copied().unwrap_or(0);
                let text_lossy = String::from_utf8_lossy(text).into_owned();
                size += text_lossy.len();
                terms.push(HunkTerm {
                    index: indexes.get(position).copied().unwrap_or(0),
                    text: text_lossy,
                    start_line: start.saturating_add(1),
                    line_count: count,
                });
                if let Some(line) = lines.get_mut(position) {
                    *line = line.saturating_add(count);
                }
            }
            out.push(Hunk {
                resolved: false,
                text: String::new(),
                terms,
            });
        }
        if size > MAX_HUNK_TEXT {
            return Ok(FileHunks::TooLarge);
        }
    }
    Ok(FileHunks::Hunks(out))
}

fn line_count(text: &[u8]) -> u32 {
    to_u32(text.split_inclusive(|b| *b == b'\n').count())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(origin("abc (rebase destination)"), "rebase destination");
        assert_eq!(
            origin("kmn 1a2b \"x (y)\" (rebased revision)"),
            "rebased revision"
        );
        assert_eq!(origin("plain"), "");
        assert_eq!(origin(""), "");
    }

    #[test]
    fn labels() {
        assert_eq!(
            label_commits("kmnopqrs 1a2b3c4d \"subject, with comma\" (rebased revision)"),
            [("kmnopqrs".to_owned(), "1a2b3c4d".to_owned())]
        );
        assert_eq!(
            label_commits("kmnopqrs 1a2b3c4d, ttuuvvww 0f0f0f0f (rebase destination)"),
            [
                ("kmnopqrs".to_owned(), "1a2b3c4d".to_owned()),
                ("ttuuvvww".to_owned(), "0f0f0f0f".to_owned())
            ]
        );
        assert_eq!(
            label_commits("kmnopqrs 1a2b3c4d"),
            [("kmnopqrs".to_owned(), "1a2b3c4d".to_owned())]
        );
        assert_eq!(label_commits(""), Vec::<(String, String)>::new());
        assert_eq!(label_commits("left"), Vec::<(String, String)>::new());
        assert!(
            label_commits("abc 1a2b3c4d").is_empty(),
            "a is not reverse hex"
        );
        assert_eq!(label_commits("kmn xyz"), Vec::<(String, String)>::new());
    }

    #[test]
    fn filters() {
        assert!(wanted("a/b", &[]));
        assert!(wanted("a/b", &["a".into()]));
        assert!(wanted("a", &["a".into()]));
        assert!(!wanted("ab", &["a".into()]));
        assert!(wanted("x", &[String::new()]));
    }

    #[test]
    fn lines() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
    }
}
