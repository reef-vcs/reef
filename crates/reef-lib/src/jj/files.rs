//! Files at a commit, read and edited without a working copy (R2.04, ADR
//! 0032): one file's content, a directory's entries, and edits applied to a
//! commit's tree as a new commit.
//!
//! Every path is checked with [`check_path`]. Reads go through the tree,
//! never through a symlink. Edits apply in order to an overlay over the
//! base tree ([`apply`]), all or none; [`apply`] without `write` is the dry
//! run a write makes before it snapshots anything, and with it the overlay
//! is written as a tree. [`Repo::edit_commit`] and [`Repo::new_change_on`]
//! record that tree in one operation.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use bstr::ByteSlice as _;
use futures::{AsyncReadExt as _, StreamExt as _};
use jj_lib::backend::{
    BackendError, CopyId, FileId, MergedTreeValue, MergedTreeValueExt as _, TreeValue,
};
use jj_lib::commit::Commit;
use jj_lib::conflicts::{
    MaterializedTreeValue, materialize_merge_result_to_bytes, materialize_tree_value,
    resolve_file_executable,
};
use jj_lib::matchers::{Matcher, PrefixMatcher, Visit, VisitDirs, VisitFiles};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::{MergedTree, all_merged_tree_entries};
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::tree::TreeMergeExt as _;

use super::diff::{self, DiffError, EntryKind, PathError};
use super::oplog::OpMeta;
use super::repo::{CommitIdHex, OperationIdHex, Repo};
use super::rewrite::{RewriteError, normalize_description};
use crate::error::{Classify, ErrorKind, classify_by_source};
use crate::spec::{SPEC_FILE, Spec};

/// The most bytes one read returns, and what a `limit` of 0 means.
pub const MAX_READ_BYTES: usize = 1 << 20;
/// The largest file a text replacement reads: what `GetDiff` reads up to.
pub const MAX_REPLACE_BYTES: u64 = 16 << 20;
/// The most edits in one request.
pub const MAX_EDITS: usize = 1000;
/// The most content bytes one request's edits may hold in memory: what it
/// sent, and what its replacements read and produced.
pub const MAX_HELD_BYTES: usize = 64 << 20;

/// Path components an edit may not write, compared ignoring ASCII case: a
/// tree holding them cannot be checked out (jj-lib refuses `.git` and
/// `.jj`), and `git~1` is `.git`'s short name on NTFS.
const RESERVED: [&str; 3] = [".git", ".jj", "git~1"];

/// `text` as a repository path: `/`-separated and relative, with no empty,
/// `.` or `..` component and no NUL (R2.02's rules, and not empty). For an
/// edit, a component that is `.git`, `.jj` or `git~1` in any case is
/// refused too; reads may name them.
pub fn check_path(text: &str, for_edit: bool) -> Result<RepoPathBuf, PathError> {
    let invalid = || PathError {
        path: text.to_owned(),
    };
    if text.is_empty() || text.contains('\0') {
        return Err(invalid());
    }
    let mut paths = diff::check_paths(&[text.to_owned()])?;
    if for_edit
        && text.split('/').any(|part| {
            RESERVED
                .iter()
                .any(|reserved| part.eq_ignore_ascii_case(reserved))
        })
    {
        return Err(invalid());
    }
    paths.pop().ok_or_else(invalid)
}

impl EntryKind {
    /// The kind in a message.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Executable => "executable file",
            EntryKind::Symlink => "symlink",
            EntryKind::Directory => "directory",
            EntryKind::Conflict => "conflict",
            EntryKind::Submodule => "submodule",
        }
    }
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `value` is: a directory when every present side is one, so a
/// directory whose contents differ between sides is not a conflict.
fn kind_of(value: &MergedTreeValue) -> Option<EntryKind> {
    if value.is_tree() {
        return Some(EntryKind::Directory);
    }
    diff::entry_kind(value)
}

/// The store id of a resolved file, symlink or submodule, in hex.
fn id_of(value: &MergedTreeValue) -> Option<String> {
    match value.as_normal()? {
        TreeValue::File { id, .. } => Some(id.hex()),
        TreeValue::Symlink(id) => Some(id.hex()),
        TreeValue::GitSubmodule(id) => Some(id.hex()),
        TreeValue::Tree(_) => None,
    }
}

/// One entry of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntryInfo {
    /// Repository-relative.
    pub path: String,
    /// What it is.
    pub kind: EntryKind,
    /// The file's, symlink's or submodule's id in hex; `None` for a
    /// directory or a conflict.
    pub id: Option<String>,
}

/// What [`read_file`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRead {
    /// The entry.
    pub entry: FileEntryInfo,
    /// The bytes from the offset, at most the limit.
    pub content: Vec<u8>,
    /// The whole content's length.
    pub size: u64,
}

/// Failure to read a file or list a directory.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FileError {
    /// Nothing is at the path.
    #[error("{path} does not exist in commit {commit}")]
    NotFound {
        /// The path.
        path: String,
        /// The commit read.
        commit: CommitIdHex,
    },
    /// A directory or submodule where a file was asked for.
    #[error("{path} is a {kind} in commit {commit}, not a file")]
    NotAFile {
        /// The path.
        path: String,
        /// What it is.
        kind: EntryKind,
        /// The commit read.
        commit: CommitIdHex,
    },
    /// Something other than a directory where one was asked for.
    #[error("{path} is a {kind} in commit {commit}, not a directory")]
    NotADirectory {
        /// The path.
        path: String,
        /// What it is.
        kind: EntryKind,
        /// The commit read.
        commit: CommitIdHex,
    },
    /// The path is not a repository path.
    #[error(transparent)]
    Path(#[from] PathError),
    /// The commit, a tree or the file could not be read.
    #[error(transparent)]
    Read(#[from] DiffError),
}

impl From<BackendError> for FileError {
    fn from(error: BackendError) -> FileError {
        FileError::Read(DiffError::Backend(error))
    }
}

impl Classify for FileError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::NotAFile { .. } | Self::NotADirectory { .. } => ErrorKind::Conflict,
            Self::Path(error) => error.kind(),
            Self::Read(error) => error.kind(),
        }
    }
}

/// The whole content of `value` at `path` in `tree`, as `GetDiff` reads a
/// side: a file's bytes, a symlink's target, a file conflict with markers,
/// and another conflict as jj describes it.
async fn content_of(
    tree: &MergedTree,
    path: &RepoPath,
    value: MergedTreeValue,
    options: &jj_lib::conflicts::ConflictMaterializeOptions,
) -> Result<Vec<u8>, FileError> {
    let value = materialize_tree_value(tree.store(), path, value, tree.labels()).await?;
    Ok(match value {
        MaterializedTreeValue::Absent
        | MaterializedTreeValue::GitSubmodule(_)
        | MaterializedTreeValue::Tree(_) => Vec::new(),
        MaterializedTreeValue::AccessDenied(source) => {
            return Err(DiffError::AccessDenied {
                path: path.as_internal_file_string().to_owned(),
                source,
            }
            .into());
        }
        MaterializedTreeValue::Symlink { target, .. } => target.into_bytes(),
        MaterializedTreeValue::FileConflict(conflict) => {
            materialize_merge_result_to_bytes(&conflict.contents, &conflict.labels, options).into()
        }
        MaterializedTreeValue::OtherConflict { id, labels } => id.describe(&labels).into_bytes(),
        MaterializedTreeValue::File(mut file) => {
            let mut bytes = Vec::new();
            file.reader
                .read_to_end(&mut bytes)
                .await
                .map_err(|source| BackendError::ReadFile {
                    path: path.to_owned(),
                    id: file.id.clone(),
                    source: source.into(),
                })?;
            bytes
        }
    })
}

async fn commit_of(repo: &ReadonlyRepo, commit: &CommitIdHex) -> Result<Commit, BackendError> {
    repo.store().get_commit_async(&commit.to_id()).await
}

/// The file at `path` in `commit`: at most `limit` bytes (0, or more than
/// [`MAX_READ_BYTES`], is that) from `offset`, and the whole length. An
/// offset at or past the end reads nothing. A directory or submodule is
/// [`FileError::NotAFile`]; a path under a symlink or a file is
/// [`FileError::NotFound`].
pub async fn read_file(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    path: &RepoPath,
    offset: u64,
    limit: u64,
) -> Result<FileRead, FileError> {
    let tree = commit_of(repo, commit).await?.tree();
    let value = tree.path_value(path).await?;
    let text = path.as_internal_file_string().to_owned();
    let Some(kind) = kind_of(&value) else {
        return Err(FileError::NotFound {
            path: text,
            commit: commit.clone(),
        });
    };
    if matches!(kind, EntryKind::Directory | EntryKind::Submodule) {
        return Err(FileError::NotAFile {
            path: text,
            kind,
            commit: commit.clone(),
        });
    }
    let id = id_of(&value);
    let options = diff::materialize_options(repo.settings())?;
    let bytes = content_of(&tree, path, value, &options).await?;
    let limit = match usize::try_from(limit) {
        Ok(0) | Err(_) => MAX_READ_BYTES,
        Ok(limit) => limit.min(MAX_READ_BYTES),
    };
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = start.saturating_add(limit).min(bytes.len());
    Ok(FileRead {
        entry: FileEntryInfo {
            path: text,
            kind,
            id,
        },
        content: bytes[start..end].to_vec(),
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

/// What [`list_files`] lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListOptions {
    /// The directory; the root when `None`.
    pub dir: Option<RepoPathBuf>,
    /// Everything below it that is not a directory, instead of its own
    /// entries.
    pub recursive: bool,
    /// Entries at or before this path, in jj's path order, are skipped.
    pub after: Option<RepoPathBuf>,
    /// Entries per page at most.
    pub limit: usize,
}

/// A page of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePage {
    /// Up to `limit` entries, in path order.
    pub entries: Vec<FileEntryInfo>,
    /// More follow these.
    pub more: bool,
}

/// Matches the paths below `dir` after `after`, and skips every directory
/// that is outside `dir` or sorts wholly before `after` without reading it.
#[derive(Debug)]
struct Below<'a> {
    dir: &'a RepoPath,
    after: Option<&'a RepoPath>,
}

impl Matcher for Below<'_> {
    fn matches(&self, file: &RepoPath) -> bool {
        file.starts_with(self.dir) && self.after.is_none_or(|after| file > after)
    }

    fn visit(&self, dir: &RepoPath) -> Visit {
        if !dir.starts_with(self.dir) && !self.dir.starts_with(dir) {
            return Visit::Nothing;
        }
        // In component order a directory's descendants sort right after it
        // and before its next sibling, so a directory before the cursor that
        // does not hold it holds nothing after it.
        if let Some(after) = self.after
            && dir < after
            && !after.starts_with(dir)
        {
            return Visit::Nothing;
        }
        Visit::Specific {
            dirs: VisitDirs::All,
            files: VisitFiles::All,
        }
    }
}

/// A page of the entries of a directory of `commit`. Not recursive: its
/// own entries, directories among them. Recursive: every entry below it
/// that is not a directory, symlinks not followed. `should_stop` is checked
/// between entries; when it returns true the page ends there, with `more`
/// set.
pub async fn list_files(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    options: &ListOptions,
    should_stop: &dyn Fn() -> bool,
) -> Result<FilePage, FileError> {
    let tree = commit_of(repo, commit).await?.tree();
    let root = RepoPathBuf::root();
    let dir = options.dir.as_deref().unwrap_or(&root);
    if !dir.is_root() {
        let value = tree.path_value(dir).await?;
        let path = dir.as_internal_file_string().to_owned();
        match kind_of(&value) {
            None => {
                return Err(FileError::NotFound {
                    path,
                    commit: commit.clone(),
                });
            }
            Some(EntryKind::Directory) => {}
            Some(kind) => {
                return Err(FileError::NotADirectory {
                    path,
                    kind,
                    commit: commit.clone(),
                });
            }
        }
    }
    let mut page = FilePage {
        entries: Vec::new(),
        more: false,
    };
    let mut push = |path: &RepoPath, value: &MergedTreeValue| -> bool {
        if page.entries.len() == options.limit || should_stop() {
            page.more = true;
            return false;
        }
        if let Some(kind) = kind_of(value) {
            page.entries.push(FileEntryInfo {
                path: path.as_internal_file_string().to_owned(),
                kind,
                id: id_of(value),
            });
        }
        true
    };
    if options.recursive {
        let matcher = Below {
            dir,
            after: options.after.as_deref(),
        };
        for (path, value) in tree.entries_matching(&matcher) {
            if !push(&path, &value?) {
                break;
            }
        }
        return Ok(page);
    }
    let trees = tree.trees().await?;
    let trees = if dir.is_root() {
        trees
    } else {
        // Checked above: every present side is a directory.
        let Some(trees) = trees.sub_tree_recursive(dir).await? else {
            return Ok(page);
        };
        trees
    };
    let names: Vec<RepoPathComponentBuf> = all_merged_tree_entries(&trees)
        .map(|(name, _)| name.to_owned())
        .collect();
    for name in names {
        let path = dir.join(&name);
        if options
            .after
            .as_deref()
            .is_some_and(|after| *path <= *after)
        {
            continue;
        }
        let value = trees.value(&name).cloned();
        if !push(&path, &value) {
            break;
        }
    }
    Ok(page)
}

/// One edit of one path, as requested; [`check_edits`] checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The path, as given.
    pub path: String,
    /// What to do; `None` changes only the executable bit.
    pub action: Option<EditAction>,
    /// The executable bit to set; `None` keeps it (false for a new file).
    pub executable: Option<bool>,
}

/// What an edit does to its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Replace the whole content, creating the file if needed.
    Content(Vec<u8>),
    /// Replace text inside the file, byte for byte.
    Replace {
        /// The text to find; not empty.
        old: String,
        /// What to put in its place.
        new: String,
        /// Every occurrence; else exactly one.
        all: bool,
    },
    /// Delete the file, symlink, conflict or submodule.
    Delete,
    /// Take the file or symlink at this path, as the earlier edits left it.
    CopyFrom(String),
}

/// What is wrong with one edit.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditProblem {
    /// A path that is not a repository path, or holds a reserved name.
    #[error("{0:?} is not a repository-relative path with `/` separators, or names .git or .jj")]
    InvalidPath(String),
    /// A malformed edit.
    #[error("{0}")]
    Invalid(&'static str),
    /// Nothing is at the path.
    #[error("{0} does not exist")]
    PathNotFound(String),
    /// Something other than a file where the edit needs one.
    #[error("{path} is a {kind}, not a file")]
    NotAFile {
        /// The path.
        path: String,
        /// What it is.
        kind: EntryKind,
    },
    /// A path whose ancestor is not a directory.
    #[error("{path} is a {kind}, not a directory")]
    NotADirectory {
        /// The ancestor.
        path: String,
        /// What it is.
        kind: EntryKind,
    },
    /// The text to replace is not in the file.
    #[error("the text to replace is not in {0}")]
    TextNotFound(String),
    /// The text to replace occurs more than once.
    #[error("the text to replace occurs {occurrences} times in {path}, not once")]
    TextNotUnique {
        /// The path.
        path: String,
        /// How many times, counting overlapping ones.
        occurrences: usize,
    },
    /// A file too large to replace text in.
    #[error("{path} is {size} bytes; text is replaced in files of at most {MAX_REPLACE_BYTES}")]
    FileTooLarge {
        /// The path.
        path: String,
        /// Its size, or the first size known to be over.
        size: u64,
    },
    /// The request's edits hold more content than [`MAX_HELD_BYTES`].
    #[error("the edits hold {0} bytes of content; one request holds at most {MAX_HELD_BYTES}")]
    TooMuchContent(usize),
    /// A path at or under a shared store's mount.
    #[error("{path} is under {mount}, the mount of a shared store in reef.toml")]
    StoreMountPath {
        /// The path.
        path: String,
        /// The mount.
        mount: String,
    },
}

impl EditProblem {
    /// The path the problem is about.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Invalid(_) | Self::TooMuchContent(_) => None,
            Self::InvalidPath(path)
            | Self::PathNotFound(path)
            | Self::TextNotFound(path)
            | Self::NotAFile { path, .. }
            | Self::NotADirectory { path, .. }
            | Self::TextNotUnique { path, .. }
            | Self::FileTooLarge { path, .. }
            | Self::StoreMountPath { path, .. } => Some(path),
        }
    }
}

impl Classify for EditProblem {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidPath(_) | Self::Invalid(_) => ErrorKind::InvalidArgument,
            Self::PathNotFound(_) => ErrorKind::NotFound,
            Self::NotAFile { .. }
            | Self::NotADirectory { .. }
            | Self::TextNotFound(_)
            | Self::TextNotUnique { .. }
            | Self::FileTooLarge { .. }
            | Self::TooMuchContent(_)
            | Self::StoreMountPath { .. } => ErrorKind::Conflict,
        }
    }
}

/// Failure to apply edits or record them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// One edit cannot be applied; nothing was written.
    #[error("edit {index} ({path}): {problem}")]
    Edit {
        /// Its position in the request, from 0.
        index: usize,
        /// Its path, as given.
        path: String,
        /// What is wrong.
        problem: EditProblem,
    },
    /// More than [`MAX_EDITS`] edits.
    #[error("{0} edits in one request; at most {MAX_EDITS} are allowed")]
    TooMany(usize),
    /// A tree or file could not be read or written.
    #[error("failed to read or write the tree")]
    Backend(#[from] BackendError),
    /// The commit could not be checked or written.
    #[error("write the commit")]
    Rewrite(#[from] RewriteError),
}

impl Classify for EditError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Edit { problem, .. } => problem.kind(),
            Self::TooMany(_) => ErrorKind::InvalidArgument,
            Self::Backend(error) => classify_by_source(error),
            Self::Rewrite(error) => error.kind(),
        }
    }
}

fn at(index: usize, edit: &Edit, problem: EditProblem) -> EditError {
    EditError::Edit {
        index,
        path: edit.path.clone(),
        problem,
    }
}

/// Checks `edits` without reading anything: their count, their paths, and
/// what no tree can make valid (an edit with no action and no bit, an empty
/// `old` text, the bit with a delete).
pub fn check_edits(edits: &[Edit]) -> Result<(), EditError> {
    if edits.len() > MAX_EDITS {
        return Err(EditError::TooMany(edits.len()));
    }
    for (index, edit) in edits.iter().enumerate() {
        let invalid = |why| Err(at(index, edit, EditProblem::Invalid(why)));
        check_path(&edit.path, true)
            .map_err(|error| at(index, edit, EditProblem::InvalidPath(error.path)))?;
        match &edit.action {
            None if edit.executable.is_none() => {
                return invalid("an edit needs an action, or executable set");
            }
            Some(EditAction::Replace { old, .. }) if old.is_empty() => {
                return invalid("replace needs a non-empty old_text");
            }
            Some(EditAction::Delete) if edit.executable.is_some() => {
                return invalid("executable cannot be set on a delete");
            }
            Some(EditAction::CopyFrom(source)) => {
                check_path(source, true)
                    .map_err(|error| at(index, edit, EditProblem::InvalidPath(error.path)))?;
                if *source == edit.path {
                    return invalid("copy_from names the edit's own path");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// A path's value in the overlay.
#[derive(Clone, Debug)]
enum Pending {
    /// Deleted.
    Absent,
    /// An existing file or symlink.
    Value(TreeValue),
    /// Content this request writes.
    Bytes {
        bytes: Arc<[u8]>,
        executable: bool,
        copy_id: CopyId,
    },
}

/// What a path holds, overlay first.
#[derive(Clone, Debug)]
enum Current {
    Absent,
    Directory,
    Pending(Pending),
    /// A conflict, or a submodule, in the base tree.
    Base(MergedTreeValue),
}

impl Current {
    fn kind(&self) -> Option<EntryKind> {
        match self {
            Current::Absent | Current::Pending(Pending::Absent) => None,
            Current::Directory => Some(EntryKind::Directory),
            Current::Pending(Pending::Value(value)) => {
                diff::entry_kind(&Merge::normal(value.clone()))
            }
            Current::Pending(Pending::Bytes { executable, .. }) => Some(if *executable {
                EntryKind::Executable
            } else {
                EntryKind::File
            }),
            Current::Base(value) => kind_of(value),
        }
    }
}

/// Edits applied so far over a base tree.
struct Overlay<'a> {
    base: &'a MergedTree,
    entries: BTreeMap<RepoPathBuf, Pending>,
    /// The content bytes the entries hold, counted as they are added.
    held: usize,
}

impl Overlay<'_> {
    /// What `path` holds now.
    async fn get(&self, path: &RepoPath) -> Result<Current, BackendError> {
        if let Some(pending) = self.entries.get(path) {
            return Ok(match pending {
                // Deleted, then written below: a directory now.
                Pending::Absent if self.has_below(path).await? => Current::Directory,
                Pending::Absent => Current::Absent,
                pending => Current::Pending(pending.clone()),
            });
        }
        let value = self.base.path_value(path).await?;
        if value.is_tree() || value.is_absent() {
            return Ok(if self.has_below(path).await? {
                Current::Directory
            } else {
                Current::Absent
            });
        }
        Ok(match value.as_normal() {
            Some(value @ (TreeValue::File { .. } | TreeValue::Symlink(_))) => {
                Current::Pending(Pending::Value(value.clone()))
            }
            _ => Current::Base(value),
        })
    }

    /// Whether anything is below `dir`: a present overlay entry, or a base
    /// entry the overlay did not delete. The base is walked only when the
    /// overlay deleted something there.
    async fn has_below(&self, dir: &RepoPath) -> Result<bool, BackendError> {
        let mut deleted = BTreeSet::new();
        for (path, pending) in self
            .entries
            .range::<RepoPath, _>((Bound::Included(dir), Bound::Unbounded))
        {
            if !path.starts_with(dir) {
                break;
            }
            if path.as_ref() == dir {
                continue;
            }
            match pending {
                Pending::Absent => {
                    deleted.insert(path.clone());
                }
                _ => return Ok(true),
            }
        }
        let value = self.base.path_value(dir).await?;
        if !value.is_tree() {
            return Ok(false);
        }
        if deleted.is_empty() {
            return Ok(true);
        }
        let matcher = PrefixMatcher::new([dir]);
        for (path, value) in self.base.entries_matching(&matcher) {
            value?;
            if !deleted.contains(&path) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fails unless every proper ancestor of `path` is absent or a
    /// directory, so no edit writes through a symlink or under a file.
    async fn check_ancestors(&self, path: &RepoPath) -> Result<(), Failure> {
        let mut ancestors: Vec<&RepoPath> = path.ancestors().skip(1).collect();
        ancestors.pop(); // the root
        for ancestor in ancestors.into_iter().rev() {
            let current = self.get(ancestor).await?;
            if let Some(kind) = current.kind()
                && kind != EntryKind::Directory
            {
                return Err(EditProblem::NotADirectory {
                    path: ancestor.as_internal_file_string().to_owned(),
                    kind,
                }
                .into());
            }
        }
        Ok(())
    }

    /// The bytes of the file at `path`, for a replacement.
    async fn bytes_of(&self, path: &RepoPath, current: &Current) -> Result<Arc<[u8]>, Failure> {
        let text = || path.as_internal_file_string().to_owned();
        match current {
            Current::Pending(Pending::Bytes { bytes, .. }) => {
                let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if size > MAX_REPLACE_BYTES {
                    return Err(EditProblem::FileTooLarge { path: text(), size }.into());
                }
                Ok(Arc::clone(bytes))
            }
            Current::Pending(Pending::Value(TreeValue::File { id, .. })) => {
                read_blob(self.base, path, id).await
            }
            Current::Absent | Current::Pending(Pending::Absent) => {
                Err(EditProblem::PathNotFound(text()).into())
            }
            other => Err(not_a_file(path, other).into()),
        }
    }
}

/// Why one edit failed: its own problem, or the store's.
#[derive(Debug)]
enum Failure {
    Problem(EditProblem),
    Backend(BackendError),
}

impl From<EditProblem> for Failure {
    fn from(problem: EditProblem) -> Failure {
        Failure::Problem(problem)
    }
}

impl From<BackendError> for Failure {
    fn from(error: BackendError) -> Failure {
        Failure::Backend(error)
    }
}

impl Failure {
    fn at(self, index: usize, edit: &Edit) -> EditError {
        match self {
            Failure::Problem(problem) => at(index, edit, problem),
            Failure::Backend(error) => EditError::Backend(error),
        }
    }
}

fn not_a_file(path: &RepoPath, current: &Current) -> EditProblem {
    EditProblem::NotAFile {
        path: path.as_internal_file_string().to_owned(),
        kind: current.kind().unwrap_or(EntryKind::File),
    }
}

/// At most [`MAX_REPLACE_BYTES`] of blob `id`.
async fn read_blob(tree: &MergedTree, path: &RepoPath, id: &FileId) -> Result<Arc<[u8]>, Failure> {
    let mut reader = tree.store().read_file(path, id).await?;
    let mut bytes = Vec::new();
    (&mut reader)
        .take(MAX_REPLACE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| BackendError::ReadFile {
            path: path.to_owned(),
            id: id.clone(),
            source: source.into(),
        })?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > MAX_REPLACE_BYTES {
        return Err(EditProblem::FileTooLarge {
            path: path.as_internal_file_string().to_owned(),
            size,
        }
        .into());
    }
    Ok(bytes.into())
}

/// `bytes` with `old` replaced by `new`: exactly one occurrence (counting
/// overlapping ones), or with `all` every non-overlapping one, left to
/// right.
fn replace(
    bytes: &[u8],
    old: &[u8],
    new: &[u8],
    all: bool,
    path: &RepoPath,
) -> Result<Vec<u8>, EditProblem> {
    let text = || path.as_internal_file_string().to_owned();
    // The result is bounded like the input, before it is built: `all` with
    // a long `new` multiplies the file.
    let too_large = |count: usize| {
        let size = bytes.len() - count * old.len() + count.saturating_mul(new.len());
        let size = u64::try_from(size).unwrap_or(u64::MAX);
        (size > MAX_REPLACE_BYTES).then(|| EditProblem::FileTooLarge { path: text(), size })
    };
    if all {
        let count = bytes.find_iter(old).count();
        if count == 0 {
            return Err(EditProblem::TextNotFound(text()));
        }
        if let Some(problem) = too_large(count) {
            return Err(problem);
        }
        return Ok(bytes.replace(old, new));
    }
    let Some(start) = bytes.find(old) else {
        return Err(EditProblem::TextNotFound(text()));
    };
    let occurrences = occurrences(bytes, old);
    if occurrences > 1 {
        return Err(EditProblem::TextNotUnique {
            path: text(),
            occurrences,
        });
    }
    if let Some(problem) = too_large(1) {
        return Err(problem);
    }
    let mut out = Vec::with_capacity(bytes.len() - old.len() + new.len());
    out.extend_from_slice(&bytes[..start]);
    out.extend_from_slice(new);
    out.extend_from_slice(&bytes[start + old.len()..]);
    Ok(out)
}

/// How many times `needle` occurs in `haystack`, overlapping ones counted.
fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut from = 0;
    while from <= haystack.len() {
        let Some(found) = haystack[from..].find(needle) else {
            break;
        };
        count += 1;
        from += found + 1;
    }
    count
}

/// The bit a file written over `current` gets when the edit leaves it
/// unset: the file's own, a conflict's as jj resolves it when it snapshots a
/// resolved conflict (its sides', else its base's when unambiguous), else
/// false.
fn kept_bit(current: &Current) -> bool {
    match current {
        Current::Pending(
            Pending::Bytes { executable, .. } | Pending::Value(TreeValue::File { executable, .. }),
        ) => *executable,
        Current::Base(value) => value
            .to_executable_merge()
            .and_then(|bits| resolve_file_executable(&bits))
            .unwrap_or(false),
        _ => false,
    }
}

fn kept_copy_id(current: &Current) -> CopyId {
    match current {
        Current::Pending(
            Pending::Bytes { copy_id, .. } | Pending::Value(TreeValue::File { copy_id, .. }),
        ) => copy_id.clone(),
        _ => CopyId::placeholder(),
    }
}

/// Applies one edit to `overlay`.
async fn apply_one(overlay: &mut Overlay<'_>, edit: &Edit) -> Result<(), Failure> {
    let path =
        check_path(&edit.path, true).map_err(|error| EditProblem::InvalidPath(error.path))?;
    let current = overlay.get(&path).await?;
    let is_file = matches!(
        current,
        Current::Pending(Pending::Bytes { .. } | Pending::Value(TreeValue::File { .. }))
    );
    let absent = current.kind().is_none();
    let next = match &edit.action {
        None => {
            if absent {
                return Err(EditProblem::PathNotFound(edit.path.clone()).into());
            }
            if !is_file {
                return Err(not_a_file(&path, &current).into());
            }
            let executable = edit.executable.unwrap_or(kept_bit(&current));
            match current {
                Current::Pending(Pending::Bytes { bytes, copy_id, .. }) => Pending::Bytes {
                    bytes,
                    executable,
                    copy_id,
                },
                Current::Pending(Pending::Value(TreeValue::File { id, copy_id, .. })) => {
                    Pending::Value(TreeValue::File {
                        id,
                        executable,
                        copy_id,
                    })
                }
                _ => return Err(not_a_file(&path, &current).into()),
            }
        }
        Some(EditAction::Delete) => {
            if absent {
                return Err(EditProblem::PathNotFound(edit.path.clone()).into());
            }
            if matches!(current, Current::Directory) {
                return Err(not_a_file(&path, &current).into());
            }
            Pending::Absent
        }
        Some(EditAction::Content(bytes)) => {
            writable_over(&path, &current)?;
            overlay.check_ancestors(&path).await?;
            Pending::Bytes {
                bytes: bytes.as_slice().into(),
                executable: edit.executable.unwrap_or(kept_bit(&current)),
                copy_id: kept_copy_id(&current),
            }
        }
        Some(EditAction::Replace { old, new, all }) => {
            if absent {
                return Err(EditProblem::PathNotFound(edit.path.clone()).into());
            }
            if !is_file {
                return Err(not_a_file(&path, &current).into());
            }
            let bytes = overlay.bytes_of(&path, &current).await?;
            let replaced = replace(&bytes, old.as_bytes(), new.as_bytes(), *all, &path)?;
            Pending::Bytes {
                bytes: replaced.into(),
                executable: edit.executable.unwrap_or(kept_bit(&current)),
                copy_id: kept_copy_id(&current),
            }
        }
        Some(EditAction::CopyFrom(source)) => {
            let copied = copied(overlay, source, edit.executable).await?;
            writable_over(&path, &current)?;
            overlay.check_ancestors(&path).await?;
            copied
        }
    };
    if let Pending::Bytes { bytes, .. } = &next {
        overlay.held = overlay.held.saturating_add(bytes.len());
        if overlay.held > MAX_HELD_BYTES {
            return Err(EditProblem::TooMuchContent(overlay.held).into());
        }
    }
    overlay.entries.insert(path, next);
    Ok(())
}

/// What a `copy_from` of `source_text` takes: the file or symlink there,
/// as the overlay holds it, with `bit` overriding a file's executable bit.
async fn copied(
    overlay: &Overlay<'_>,
    source_text: &str,
    bit: Option<bool>,
) -> Result<Pending, Failure> {
    let source =
        check_path(source_text, true).map_err(|error| EditProblem::InvalidPath(error.path))?;
    let from = overlay.get(&source).await?;
    Ok(match from {
        Current::Pending(Pending::Bytes {
            bytes, executable, ..
        }) => Pending::Bytes {
            bytes,
            executable: bit.unwrap_or(executable),
            copy_id: CopyId::placeholder(),
        },
        Current::Pending(Pending::Value(TreeValue::File { id, executable, .. })) => {
            Pending::Value(TreeValue::File {
                id,
                executable: bit.unwrap_or(executable),
                copy_id: CopyId::placeholder(),
            })
        }
        Current::Pending(Pending::Value(TreeValue::Symlink(id))) => {
            if bit.is_some() {
                return Err(
                    EditProblem::Invalid("executable cannot be set on a copied symlink").into(),
                );
            }
            Pending::Value(TreeValue::Symlink(id))
        }
        Current::Absent | Current::Pending(Pending::Absent) => {
            return Err(EditProblem::PathNotFound(source_text.to_owned()).into());
        }
        other => return Err(not_a_file(&source, &other).into()),
    })
}

/// Fails when `current` is something a file cannot replace in place: a
/// symlink, a submodule, or a directory.
fn writable_over(path: &RepoPath, current: &Current) -> Result<(), EditProblem> {
    match current.kind() {
        Some(EntryKind::Symlink | EntryKind::Submodule | EntryKind::Directory) => {
            Err(not_a_file(path, current))
        }
        _ => Ok(()),
    }
}

/// The store mounts `reef.toml` declares in `bytes`; none when it does not
/// parse.
fn mounts_in(bytes: &[u8]) -> Vec<String> {
    Spec::parse_bytes(bytes)
        .map(|spec| spec.stores.into_iter().map(|store| store.mount).collect())
        .unwrap_or_default()
}

/// The mounts of the base tree's `reef.toml` and of the one the edits
/// leave.
async fn mounts(overlay: &Overlay<'_>) -> Result<Vec<String>, BackendError> {
    #[expect(clippy::expect_used, reason = "a constant, valid repository path")]
    let spec = RepoPath::from_internal_string(SPEC_FILE).expect("reef.toml is a valid path");
    // A `reef.toml` too large to read declares nothing a workspace uses: a
    // create fails on it too.
    let read = async |id: &FileId| match read_blob(overlay.base, spec, id).await {
        Ok(bytes) => Ok(mounts_in(&bytes)),
        Err(Failure::Backend(error)) => Err(error),
        Err(Failure::Problem(_)) => Ok(Vec::new()),
    };
    let mut mounts = Vec::new();
    if let Some(TreeValue::File { id, .. }) = overlay.base.path_value(spec).await?.as_normal() {
        mounts.extend(read(id).await?);
    }
    match overlay.entries.get(spec) {
        Some(Pending::Bytes { bytes, .. }) => mounts.extend(mounts_in(bytes)),
        Some(Pending::Value(TreeValue::File { id, .. })) => mounts.extend(read(id).await?),
        _ => {}
    }
    Ok(mounts)
}

/// Whether `path` is `mount` or below it, ignoring ASCII case: on a
/// case-insensitive filesystem `Node_Modules/x` lands in `node_modules`.
fn under_mount(path: &str, mount: &str) -> bool {
    let (path, mount) = (path.as_bytes(), mount.as_bytes());
    path.len() >= mount.len()
        && path[..mount.len()].eq_ignore_ascii_case(mount)
        && (path.len() == mount.len() || path[mount.len()] == b'/')
}

/// Applies `edits` in order over `base`, all or none, each seeing what the
/// earlier ones left. Without `write` nothing is written and the result is
/// `None`: the dry run. With it, the new tree; file contents are written
/// only then, all after every edit applied.
pub async fn apply(
    base: &MergedTree,
    edits: &[Edit],
    write: bool,
) -> Result<Option<MergedTree>, EditError> {
    check_edits(edits)?;
    let mut overlay = Overlay {
        base,
        entries: BTreeMap::new(),
        held: 0,
    };
    for (index, edit) in edits.iter().enumerate() {
        apply_one(&mut overlay, edit)
            .await
            .map_err(|failure| failure.at(index, edit))?;
    }
    let mounts = mounts(&overlay).await?;
    for (index, edit) in edits.iter().enumerate() {
        let path = &edit.path;
        if let Some(mount) = mounts.iter().find(|mount| under_mount(path, mount)) {
            return Err(at(
                index,
                edit,
                EditProblem::StoreMountPath {
                    path: path.clone(),
                    mount: mount.clone(),
                },
            ));
        }
    }
    if !write {
        return Ok(None);
    }
    let store = base.store();
    let mut builder = MergedTreeBuilder::new(base.clone());
    for (path, pending) in overlay.entries {
        let value = match pending {
            Pending::Absent => Merge::absent(),
            Pending::Value(value) => Merge::normal(value),
            Pending::Bytes {
                bytes,
                executable,
                copy_id,
            } => {
                let id = store.write_file(&path, &mut &bytes[..]).await?;
                Merge::normal(TreeValue::File {
                    id,
                    executable,
                    copy_id,
                })
            }
        };
        builder.set_or_remove(path, value);
    }
    Ok(Some(builder.write_tree().await?))
}

/// [`apply`]'s dry run of `edits` against `commit`'s tree: what a write
/// checks before it snapshots anything.
pub async fn dry_run(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    edits: &[Edit],
) -> Result<(), EditError> {
    let tree = commit_of(repo, commit).await?.tree();
    apply(&tree, edits, false).await.map(|_| ())
}

/// What an edit recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    /// The operation.
    pub op_id: OperationIdHex,
    /// The new commit.
    pub commit_id: CommitIdHex,
}

impl Repo {
    /// Applies `edits` to `commit`'s tree and rewrites it with the result,
    /// keeping its change id, in one operation recorded with `meta`, which
    /// this [`Repo`] adopts. `description` set replaces the description
    /// ([`normalize_description`]d). `None`, writing nothing, when neither
    /// the tree nor the description changes. Descendants are rebased onto
    /// the new commit; bookmarks and working copies follow. The author
    /// stays; the committer is this repository's identity, now. The caller
    /// checks the commit with [`super::rewrite::writable`] first.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %commit, edits_count = edits.len()))]
    pub async fn edit_commit(
        &mut self,
        commit: &CommitIdHex,
        edits: &[Edit],
        description: Option<&str>,
        meta: &OpMeta,
    ) -> Result<Option<Edited>, EditError> {
        let description = description.map(normalize_description).transpose()?;
        let old = self.repo.store().get_commit_async(&commit.to_id()).await?;
        let tree = apply(&old.tree(), edits, true)
            .await?
            .unwrap_or_else(|| old.tree());
        if tree.tree_ids() == old.tree_ids()
            && description
                .as_deref()
                .is_none_or(|text| text == old.description())
        {
            return Ok(None);
        }
        let mut tx = self.repo.start_transaction();
        let mut builder = tx.repo_mut().rewrite_commit(&old).set_tree(tree);
        if let Some(text) = description {
            builder = builder.set_description(text);
        }
        let new = builder.write().await?;
        tx.repo_mut().rebase_descendants().await?;
        self.repo = meta.commit(tx).await.map_err(RewriteError::from)?;
        Ok(Some(Edited {
            op_id: OperationIdHex::of(self.repo.op_id()),
            commit_id: CommitIdHex::of(new.id()),
        }))
    }

    /// Creates a new change whose one parent is `parent`, with `edits`
    /// applied to the parent's tree and `description` (empty when `None`),
    /// in one operation recorded with `meta`, which this [`Repo`] adopts.
    /// No working copy moves. The caller checks the parent with
    /// [`super::rewrite::buildable`] first.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), commit_id = %parent, edits_count = edits.len()))]
    pub async fn new_change_on(
        &mut self,
        parent: &CommitIdHex,
        edits: &[Edit],
        description: Option<&str>,
        meta: &OpMeta,
    ) -> Result<Edited, EditError> {
        let description = normalize_description(description.unwrap_or_default())?;
        let parent = self.repo.store().get_commit_async(&parent.to_id()).await?;
        let tree = apply(&parent.tree(), edits, true)
            .await?
            .unwrap_or_else(|| parent.tree());
        let mut tx = self.repo.start_transaction();
        let new = tx
            .repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description(description)
            .write()
            .await?;
        self.repo = meta.commit(tx).await.map_err(RewriteError::from)?;
        Ok(Edited {
            op_id: OperationIdHex::of(self.repo.op_id()),
            commit_id: CommitIdHex::of(new.id()),
        })
    }
}

// The symbol index's reads (R2.14, ADR 0042): a tree's files with their
// ids, the files two trees differ in, and a blob by id, so the index keys
// entries by path and content without materializing anything.

/// A tree, as the symbol index holds one across reads.
pub use jj_lib::merged_tree::MergedTree as Tree;

fn repo_path(text: &str) -> Result<RepoPathBuf, FileError> {
    RepoPathBuf::from_internal_string(text).map_err(|_| {
        FileError::Path(PathError {
            path: text.to_owned(),
        })
    })
}

/// A file of a tree, for the symbol index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// Repository-relative.
    pub path: String,
    /// What it is: a file, an executable, a symlink, a submodule or a
    /// conflict. Never a directory.
    pub kind: EntryKind,
    /// The content id's bytes for a file or an executable; empty otherwise.
    pub file_id: Vec<u8>,
}

impl TreeFile {
    fn of(path: &RepoPath, value: &MergedTreeValue) -> Option<Self> {
        let kind = kind_of(value)?;
        if kind == EntryKind::Directory {
            return None;
        }
        let file_id = match value.as_normal() {
            Some(TreeValue::File { id, .. }) => id.as_bytes().to_vec(),
            _ => Vec::new(),
        };
        Some(Self {
            path: path.as_internal_file_string().to_owned(),
            kind,
            file_id,
        })
    }

    /// A file or an executable: something with content to read.
    pub fn is_file(&self) -> bool {
        matches!(self.kind, EntryKind::File | EntryKind::Executable)
    }
}

/// Matches the files whose path `filter` accepts, at or under `dir` (the
/// root when empty), and visits only the directories that can hold them.
struct Filtered<'a>(&'a (dyn Fn(&str) -> bool + Sync), &'a str);

impl std::fmt::Debug for Filtered<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Filtered")
    }
}

impl Matcher for Filtered<'_> {
    fn matches(&self, file: &RepoPath) -> bool {
        let path = file.as_internal_file_string();
        under(path, self.1) && (self.0)(path)
    }

    fn visit(&self, dir: &RepoPath) -> Visit {
        let dir = dir.as_internal_file_string();
        if !under(dir, self.1) && !under(self.1, dir) {
            return Visit::Nothing;
        }
        Visit::Specific {
            dirs: VisitDirs::All,
            files: VisitFiles::All,
        }
    }
}

/// `commit`'s tree.
pub async fn commit_tree(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
) -> Result<MergedTree, FileError> {
    Ok(commit_of(repo, commit).await?.tree())
}

/// `commit`'s tree and its first parent's, when it has a parent other than
/// the root.
pub async fn commit_and_parent_trees(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
) -> Result<(MergedTree, Option<MergedTree>), FileError> {
    let commit = commit_of(repo, commit).await?;
    let root = repo.store().root_commit_id();
    let parent = match commit.parent_ids().first() {
        Some(id) if id != root => Some(repo.store().get_commit_async(id).await?.tree()),
        _ => None,
    };
    Ok((commit.tree(), parent))
}

/// Every workspace's working-copy commit in `repo`'s view, deduplicated.
pub fn working_copy_commits(repo: &ReadonlyRepo) -> Vec<CommitIdHex> {
    let ids: BTreeSet<CommitIdHex> = repo
        .view()
        .wc_commit_ids()
        .values()
        .map(CommitIdHex::of)
        .collect();
    ids.into_iter().collect()
}

/// A tree's identity: its tree ids in hex, joined by `+` when it is
/// conflicted. Two commits with one key hold the same files.
pub fn tree_key(tree: &MergedTree) -> String {
    tree.tree_ids()
        .iter()
        .map(ObjectId::hex)
        .collect::<Vec<_>>()
        .join("+")
}

/// Whether `path` is `dir` or below it, by whole components; every path is
/// under the root, `""`.
fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Every entry of `tree` that is not a directory and whose path `filter`
/// accepts, in path order.
pub fn walk_files(
    tree: &MergedTree,
    filter: &(dyn Fn(&str) -> bool + Sync),
) -> Result<Vec<TreeFile>, FileError> {
    walk_files_in(tree, "", filter)
}

/// [`walk_files`] at or under `dir`, a file or a directory (the root when
/// empty), reading only the trees on the way to it and below it.
pub fn walk_files_in(
    tree: &MergedTree,
    dir: &str,
    filter: &(dyn Fn(&str) -> bool + Sync),
) -> Result<Vec<TreeFile>, FileError> {
    let mut files = Vec::new();
    for (path, value) in tree.entries_matching(&Filtered(filter, dir)) {
        files.extend(TreeFile::of(&path, &value?));
    }
    Ok(files)
}

/// A path whose entry differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative.
    pub path: String,
    /// Its entry in the old tree; `None` when absent or a directory.
    pub before: Option<TreeFile>,
    /// Its entry in the new tree; `None` when absent or a directory.
    pub after: Option<TreeFile>,
}

/// The paths `filter` accepts whose entry differs from `from` to `to`, in
/// path order.
pub async fn changed_files(
    from: &MergedTree,
    to: &MergedTree,
    filter: &(dyn Fn(&str) -> bool + Sync),
) -> Result<Vec<ChangedFile>, FileError> {
    let matcher = Filtered(filter, "");
    let mut stream = from.diff_stream(to, &matcher);
    let mut changed = Vec::new();
    while let Some(entry) = stream.next().await {
        let values = entry.values.map_err(FileError::from)?;
        changed.push(ChangedFile {
            path: entry.path.as_internal_file_string().to_owned(),
            before: TreeFile::of(&entry.path, &values.before),
            after: TreeFile::of(&entry.path, &values.after),
        });
    }
    Ok(changed)
}

/// The entry at `path` in `tree`, when it is not absent or a directory.
pub async fn tree_file(tree: &MergedTree, path: &str) -> Result<Option<TreeFile>, FileError> {
    let path = repo_path(path)?;
    Ok(TreeFile::of(&path, &tree.path_value(&path).await?))
}

/// The content of file `id` at `path`, or `None` when it is larger than
/// `max` bytes, which are then not read past `max + 1`.
pub async fn read_file_id(
    tree: &MergedTree,
    path: &str,
    id: &[u8],
    max: u64,
) -> Result<Option<Arc<[u8]>>, FileError> {
    let path = &repo_path(path)?;
    let id = FileId::new(id.to_vec());
    let mut reader = tree.store().read_file(path, &id).await?;
    let mut bytes = Vec::new();
    (&mut reader)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| BackendError::ReadFile {
            path: path.to_owned(),
            id: id.clone(),
            source: source.into(),
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Ok(None);
    }
    Ok(Some(bytes.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RepoPathBuf {
        RepoPathBuf::from_internal_string(text).unwrap()
    }

    #[test]
    fn paths_follow_the_rules() {
        for bad in ["", "..", ".", "a//b", "/a", "a/", "a\0b", "a/../b"] {
            assert!(check_path(bad, false).is_err(), "{bad:?}");
            assert!(check_path(bad, true).is_err(), "{bad:?}");
        }
        for reserved in [".git/config", "x/.JJ/y", ".Git", "GIT~1/x", ".jj"] {
            assert!(check_path(reserved, true).is_err(), "{reserved:?}");
            assert!(check_path(reserved, false).is_ok(), "{reserved:?}");
        }
        for fine in [".gitignore", ".github/x", "a.git", "src/main.rs"] {
            assert_eq!(check_path(fine, true).unwrap(), path(fine));
        }
    }

    #[test]
    fn occurrences_count_overlaps() {
        assert_eq!(occurrences(b"aaa", b"aa"), 2);
        assert_eq!(occurrences(b"abcabc", b"bc"), 2);
        assert_eq!(occurrences(b"abc", b"x"), 0);
        let p = path("f");
        assert_eq!(
            replace(b"one two", b"two", b"2", false, &p).unwrap(),
            b"one 2"
        );
        assert_eq!(replace(b"aaaa", b"aa", b"b", true, &p).unwrap(), b"bb");
        assert!(matches!(
            replace(b"aaa", b"aa", b"b", false, &p),
            Err(EditProblem::TextNotUnique { occurrences: 2, .. })
        ));
        assert!(matches!(
            replace(b"abc", b"x", b"y", true, &p),
            Err(EditProblem::TextNotFound(_))
        ));
    }

    #[test]
    fn mounts_match_ignoring_case() {
        assert!(under_mount("node_modules", "node_modules"));
        assert!(under_mount("node_modules/x", "node_modules"));
        assert!(under_mount("Node_Modules/x", "node_modules"));
        assert!(!under_mount("node_modules_x", "node_modules"));
        assert!(!under_mount("node", "node_modules"));
        let p = path("f");
        let big = vec![b'a'; 1 << 20];
        let new = "b".repeat(2 << 20);
        assert!(matches!(
            replace(&big, b"a", new.as_bytes(), true, &p),
            Err(EditProblem::FileTooLarge { .. })
        ));
    }

    #[test]
    fn the_cursor_skips_whole_directories() {
        let dir = RepoPathBuf::root();
        let after = path("b/c");
        let below = Below {
            dir: &dir,
            after: Some(&after),
        };
        assert!(below.visit(&path("a")).is_nothing());
        assert!(!below.visit(&path("b")).is_nothing());
        assert!(!below.visit(&path("c")).is_nothing());
        assert!(!below.matches(&path("b/c")));
        assert!(below.matches(&path("b/d")));
        assert!(below.matches(&path("b.rs")));
        assert!(!below.matches(&path("a.rs")));
        // `a/x` sorts before `a.rs`, so `a` is wholly before a cursor at
        // `a.rs`.
        let after = path("a.rs");
        let below = Below {
            dir: &dir,
            after: Some(&after),
        };
        assert!(below.visit(&path("a")).is_nothing());
        let sub = path("src");
        let below = Below {
            dir: &sub,
            after: None,
        };
        assert!(below.visit(&path("docs")).is_nothing());
        assert!(!below.visit(&path("src/x")).is_nothing());
        assert!(!below.matches(&path("srcx")));
        assert!(below.matches(&path("src/a")));
    }

    #[test]
    fn edits_are_checked_before_reading() {
        let edit = |path: &str, action, executable| Edit {
            path: path.into(),
            action,
            executable,
        };
        let index_of = |edits: &[Edit]| match check_edits(edits) {
            Err(EditError::Edit { index, .. }) => Some(index),
            _ => None,
        };
        assert!(check_edits(&[edit("a", Some(EditAction::Delete), None)]).is_ok());
        assert_eq!(
            index_of(&[
                edit("a", Some(EditAction::Delete), None),
                edit("b", None, None)
            ]),
            Some(1)
        );
        assert_eq!(
            index_of(&[edit("a", Some(EditAction::Delete), Some(true))]),
            Some(0)
        );
        let replace = EditAction::Replace {
            old: String::new(),
            new: "x".into(),
            all: false,
        };
        assert_eq!(index_of(&[edit("a", Some(replace), None)]), Some(0));
        assert_eq!(
            index_of(&[edit("a", Some(EditAction::CopyFrom("a".into())), None)]),
            Some(0)
        );
        assert_eq!(
            index_of(&[edit("a", Some(EditAction::CopyFrom(".git/x".into())), None)]),
            Some(0)
        );
        let many = vec![edit("a", Some(EditAction::Delete), None); MAX_EDITS + 1];
        assert!(matches!(check_edits(&many), Err(EditError::TooMany(_))));
    }
}
