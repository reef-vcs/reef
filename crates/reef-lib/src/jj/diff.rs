//! Diffs between two commits (R2.02): the files that differ in path order,
//! with renames and copies from the backend's copy records, and per file a
//! summary, line counts, or unified hunks.

use bstr::BStr;
use futures::{AsyncReadExt as _, StreamExt as _};
use jj_lib::backend::{
    BackendError, CommitId, MergedTreeValue, MergedTreeValueExt as _, TreeValue,
};
use jj_lib::commit::Commit;
use jj_lib::conflicts::{
    ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedTreeValue,
    materialize_merge_result_to_bytes, materialize_tree_value,
};
use jj_lib::copies::{CopyOperation, CopyRecords};
use jj_lib::diff::DiffHunkKind;
use jj_lib::diff_presentation::unified::{DiffLineType, unified_diff_hunks};
use jj_lib::diff_presentation::{LineCompareMode, diff_by_line};
use jj_lib::matchers::{EverythingMatcher, Matcher, PrefixMatcher};
use jj_lib::merge::Diff;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::tree_merge::MergeOptions;

use super::repo::CommitIdHex;
use crate::error::{Classify, ErrorKind, classify_by_source};

/// Bytes git reads to decide a file is binary: a NUL among them.
const BINARY_PEEK: usize = 8000;
/// Renames and copies are detected only when at most this many files
/// differ: git's default `diff.renameLimit`.
pub const RENAME_LIMIT: usize = 1000;

/// How much of each file's diff to compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffFormat {
    /// Paths and kinds only.
    Summary,
    /// Paths, kinds, and lines added and removed.
    Stat,
    /// Everything in [`DiffFormat::Stat`], and the hunks.
    Hunks,
}

/// What a path holds on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// An executable file.
    Executable,
    /// A symbolic link.
    Symlink,
    /// A directory.
    Directory,
    /// An unresolved conflict.
    Conflict,
    /// A git submodule.
    Submodule,
}

/// How a path differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Absent before.
    Added,
    /// Present on both sides.
    Modified,
    /// Absent after.
    Deleted,
    /// Moved from `old_path`.
    Renamed,
    /// Copied from `old_path`.
    Copied,
}

/// One hunk of a unified diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkInfo {
    /// The first old line, from 1; the line before for an empty side.
    pub old_start: u32,
    /// Old lines in the hunk.
    pub old_lines: u32,
    /// The first new line, from 1; the line before for an empty side.
    pub new_start: u32,
    /// New lines in the hunk.
    pub new_lines: u32,
    /// Each line prefixed `' '`, `'-'` or `'+'` and newline-terminated.
    pub lines: String,
}

/// One file's diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts `reef.api.v1.FileDiff` reports, not states of one machine"
)]
pub struct FileDiffInfo {
    /// The path after (the new path of a rename), repository-relative.
    pub path: String,
    /// How it differs.
    pub kind: Option<ChangeKind>,
    /// The path before, for a rename or copy.
    pub old_path: Option<String>,
    /// What it held before; `None` when absent.
    pub old_type: Option<EntryKind>,
    /// What it holds after; `None` when absent.
    pub new_type: Option<EntryKind>,
    /// A side is binary.
    pub binary: bool,
    /// A side is conflicted.
    pub conflict: bool,
    /// Lines added (stat and hunks).
    pub lines_added: u32,
    /// Lines removed (stat and hunks).
    pub lines_removed: u32,
    /// The hunks (hunks).
    pub hunks: Vec<HunkInfo>,
    /// The hunks were cut.
    pub truncated: bool,
    /// The old side's last line, in a hunk, has no newline.
    pub old_no_final_newline: bool,
    /// The new side's last line, in a hunk, has no newline.
    pub new_no_final_newline: bool,
    /// A side is larger than the read limit.
    pub too_large: bool,
    /// A side is not valid UTF-8.
    pub invalid_utf8: bool,
}

/// What a diff compares against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffBase {
    /// The new side's parents, merged as `jj diff -r` does.
    Parents,
    /// One commit.
    Commit(CommitIdHex),
}

/// How a diff page is computed.
#[derive(Clone, Debug)]
pub struct DiffOptions {
    /// How much per file.
    pub format: DiffFormat,
    /// Lines of context around each hunk.
    pub context: usize,
    /// Hunk lines per file at most.
    pub max_lines: usize,
    /// Bytes of hunk lines per file at most.
    pub max_hunk_bytes: usize,
    /// A side larger than this is not read: `too_large`.
    pub max_file_bytes: usize,
    /// Path prefixes; empty for every path.
    pub paths: Vec<RepoPathBuf>,
    /// Files whose path is this or before it, in jj's path order (by
    /// component, so `a/b` is before `a.rs`), are skipped.
    pub after: Option<RepoPathBuf>,
    /// Files per page at most.
    pub limit: usize,
}

/// A page of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPage {
    /// Up to `limit` files after `after`.
    pub files: Vec<FileDiffInfo>,
    /// Files that differ, over every page.
    pub total: usize,
    /// More files follow these.
    pub more: bool,
    /// The old side's commits.
    pub from: Vec<CommitIdHex>,
    /// The new side's commit.
    pub to: CommitIdHex,
}

/// A request path that is not a repository-relative `/`-separated path.
#[derive(Debug, thiserror::Error)]
#[error("{path:?} is not a repository-relative path with `/` separators")]
pub struct PathError {
    /// The path as given.
    pub path: String,
}

impl Classify for PathError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// Failure to compute a diff.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiffError {
    /// A commit, tree or file could not be read.
    #[error("failed to read the trees being compared")]
    Backend(#[from] BackendError),
    /// A file could not be read (jj's access denied).
    #[error("cannot read {path}")]
    AccessDenied {
        /// The path.
        path: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// jj's merge options could not be read from the settings.
    #[error("failed to read the merge settings")]
    Settings(#[source] jj_lib::config::ConfigGetError),
}

impl Classify for DiffError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Backend(error) => classify_by_source(error),
            Self::AccessDenied { .. } => ErrorKind::PermissionDenied,
            Self::Settings(_) => ErrorKind::Internal,
        }
    }
}

/// `paths` as repository paths: `/`-separated, relative, without `.`, `..`
/// or empty components; an empty string is the root.
pub fn check_paths(paths: &[String]) -> Result<Vec<RepoPathBuf>, PathError> {
    paths
        .iter()
        .map(|path| {
            let invalid = || PathError { path: path.clone() };
            if !path.is_empty()
                && path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(invalid());
            }
            RepoPathBuf::from_internal_string(path.as_str()).map_err(|_| invalid())
        })
        .collect()
}

/// What `value` holds; `None` when absent.
pub(crate) fn entry_kind(value: &MergedTreeValue) -> Option<EntryKind> {
    if value.is_absent() {
        return None;
    }
    let Some(resolved) = value.as_resolved() else {
        return Some(EntryKind::Conflict);
    };
    Some(match resolved.as_ref()? {
        TreeValue::File {
            executable: true, ..
        } => EntryKind::Executable,
        TreeValue::File { .. } => EntryKind::File,
        TreeValue::Symlink(_) => EntryKind::Symlink,
        TreeValue::Tree(_) => EntryKind::Directory,
        TreeValue::GitSubmodule(_) => EntryKind::Submodule,
    })
}

/// One side's content, as far as it was read.
struct Side {
    bytes: Vec<u8>,
    binary: bool,
    too_large: bool,
}

/// Reads one side: at most `peek` bytes when only the binary check is
/// needed, else up to `max + 1` to tell a too-large file.
async fn read_side(
    tree: &MergedTree,
    path: &RepoPath,
    value: MergedTreeValue,
    read: Option<usize>,
    materialize: &ConflictMaterializeOptions,
) -> Result<Side, DiffError> {
    let store = tree.store();
    let value = materialize_tree_value(store, path, value, tree.labels()).await?;
    let (bytes, file) = match value {
        MaterializedTreeValue::Absent
        | MaterializedTreeValue::GitSubmodule(_)
        | MaterializedTreeValue::Tree(_) => (Vec::new(), false),
        MaterializedTreeValue::AccessDenied(source) => {
            return Err(DiffError::AccessDenied {
                path: path.as_internal_file_string().to_owned(),
                source,
            });
        }
        MaterializedTreeValue::Symlink { target, .. } => (target.into_bytes(), false),
        MaterializedTreeValue::FileConflict(conflict) => (
            materialize_merge_result_to_bytes(&conflict.contents, &conflict.labels, materialize)
                .into(),
            false,
        ),
        MaterializedTreeValue::OtherConflict { id, labels } => {
            (id.describe(&labels).into_bytes(), false)
        }
        MaterializedTreeValue::File(file) => {
            let limit = read.unwrap_or(BINARY_PEEK);
            let mut bytes = Vec::new();
            file.reader
                .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
                .read_to_end(&mut bytes)
                .await
                .map_err(|source| BackendError::ReadFile {
                    path: path.to_owned(),
                    id: file.id.clone(),
                    source: source.into(),
                })?;
            (bytes, true)
        }
    };
    let binary = file && bytes[..bytes.len().min(BINARY_PEEK)].contains(&0);
    let too_large = read.is_some_and(|max| bytes.len() > max);
    Ok(Side {
        bytes,
        binary,
        too_large,
    })
}

/// How a conflicted side is written out: jj's diff-style markers, with the
/// merge options from `settings`. `ReadFile` uses the same (R2.04).
pub(crate) fn materialize_options(
    settings: &jj_lib::settings::UserSettings,
) -> Result<ConflictMaterializeOptions, DiffError> {
    Ok(ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Diff,
        marker_len: None,
        merge: MergeOptions::from_settings(settings).map_err(DiffError::Settings)?,
    })
}

fn lines_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.split_inclusive(|b| *b == b'\n').count()).unwrap_or(u32::MAX)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A hunk's start as git writes it: from 1, or the line before an empty
/// side.
fn start_of(range: &std::ops::Range<usize>) -> u32 {
    if range.is_empty() {
        to_u32(range.start)
    } else {
        to_u32(range.start + 1)
    }
}

/// Fills `diff`'s counts, and with `hunks` its hunks, from both sides.
fn compare(diff: &mut FileDiffInfo, old: &[u8], new: &[u8], options: &DiffOptions) {
    if options.format == DiffFormat::Stat {
        for hunk in diff_by_line([old, new], &LineCompareMode::Exact).hunks() {
            if hunk.kind == DiffHunkKind::Different {
                diff.lines_removed += lines_of(hunk.contents[0]);
                diff.lines_added += lines_of(hunk.contents[1]);
            }
        }
        return;
    }
    let hunks = unified_diff_hunks(
        Diff::new(BStr::new(old), BStr::new(new)),
        options.context,
        LineCompareMode::Exact,
    );
    let old_last = lines_of(old);
    let new_last = lines_of(new);
    let old_open = !old.is_empty() && !old.ends_with(b"\n");
    let new_open = !new.is_empty() && !new.ends_with(b"\n");
    let (mut lines_left, mut bytes_left) = (options.max_lines, options.max_hunk_bytes);
    for hunk in hunks {
        let mut kept = HunkInfo {
            old_start: start_of(&hunk.left_line_range),
            old_lines: 0,
            new_start: start_of(&hunk.right_line_range),
            new_lines: 0,
            lines: String::new(),
        };
        let mut old_line = to_u32(hunk.left_line_range.start);
        let mut new_line = to_u32(hunk.right_line_range.start);
        for (kind, tokens) in &hunk.lines {
            let (prefix, on_old, on_new) = match kind {
                DiffLineType::Context => (' ', true, true),
                DiffLineType::Removed => {
                    diff.lines_removed += 1;
                    ('-', true, false)
                }
                DiffLineType::Added => {
                    diff.lines_added += 1;
                    ('+', false, true)
                }
            };
            if diff.truncated {
                continue;
            }
            let bytes: Vec<u8> = tokens
                .iter()
                .flat_map(|(_, part)| part.iter().copied())
                .collect();
            let mut text = String::with_capacity(bytes.len() + 2);
            text.push(prefix);
            text.push_str(&String::from_utf8_lossy(&bytes));
            if !text.ends_with('\n') {
                text.push('\n');
            }
            if lines_left == 0 || text.len() > bytes_left {
                diff.truncated = true;
                continue;
            }
            lines_left -= 1;
            bytes_left -= text.len();
            kept.lines.push_str(&text);
            if on_old {
                old_line += 1;
                kept.old_lines += 1;
                diff.old_no_final_newline |= old_open && old_line == old_last;
            }
            if on_new {
                new_line += 1;
                kept.new_lines += 1;
                diff.new_no_final_newline |= new_open && new_line == new_last;
            }
        }
        if !kept.lines.is_empty() {
            // A side the cut emptied starts at the line before, as git
            // writes an empty side.
            if kept.old_lines == 0 {
                kept.old_start = to_u32(hunk.left_line_range.start);
            }
            if kept.new_lines == 0 {
                kept.new_start = to_u32(hunk.right_line_range.start);
            }
            diff.hunks.push(kept);
        }
    }
}

/// What [`file_diff`] reads each side with.
struct Sides<'a> {
    from: &'a MergedTree,
    to: &'a MergedTree,
    read: Option<usize>,
    materialize: &'a ConflictMaterializeOptions,
}

/// One entry of the diff stream as a [`FileDiffInfo`].
async fn file_diff(
    entry: jj_lib::copies::CopiesTreeDiffEntry,
    path: String,
    sides: &Sides<'_>,
    options: &DiffOptions,
) -> Result<FileDiffInfo, DiffError> {
    let values = entry.values?;
    let mut diff = FileDiffInfo {
        path,
        old_type: entry_kind(&values.before),
        new_type: entry_kind(&values.after),
        ..FileDiffInfo::default()
    };
    diff.kind = Some(match entry.path.copy_operation() {
        Some(CopyOperation::Rename) => ChangeKind::Renamed,
        Some(CopyOperation::Copy) => ChangeKind::Copied,
        None if diff.old_type.is_none() => ChangeKind::Added,
        None if diff.new_type.is_none() => ChangeKind::Deleted,
        None => ChangeKind::Modified,
    });
    if entry.path.copy_operation().is_some() {
        diff.old_path = Some(entry.path.source().as_internal_file_string().to_owned());
    }
    diff.conflict =
        diff.old_type == Some(EntryKind::Conflict) || diff.new_type == Some(EntryKind::Conflict);
    let old = read_side(
        sides.from,
        entry.path.source(),
        values.before,
        sides.read,
        sides.materialize,
    )
    .await?;
    let new = read_side(
        sides.to,
        entry.path.target(),
        values.after,
        sides.read,
        sides.materialize,
    )
    .await?;
    diff.binary = old.binary || new.binary;
    diff.too_large = old.too_large || new.too_large;
    if sides.read.is_some() && !diff.binary && !diff.too_large {
        diff.invalid_utf8 =
            std::str::from_utf8(&old.bytes).is_err() || std::str::from_utf8(&new.bytes).is_err();
        compare(&mut diff, &old.bytes, &new.bytes, options);
    }
    Ok(diff)
}

/// The paths that differ from commit `from` to commit `to` in `repo`, in
/// path order, `limit` at most: no copy detection and no content read, so
/// it stays cheap after every snapshot (R2.10's editing presence).
pub async fn changed_paths(
    repo: &ReadonlyRepo,
    from: &CommitIdHex,
    to: &CommitIdHex,
    limit: usize,
) -> Result<Vec<String>, DiffError> {
    let store = repo.store();
    let from_tree = store.get_commit_async(&from.to_id()).await?.tree();
    let to_tree = store.get_commit_async(&to.to_id()).await?.tree();
    let mut stream = from_tree.diff_stream(&to_tree, &EverythingMatcher);
    let mut paths = Vec::new();
    while paths.len() < limit
        && let Some(entry) = stream.next().await
    {
        paths.push(entry.path.as_internal_file_string().to_owned());
    }
    Ok(paths)
}

/// The diff from `from` to `to` in `repo`: a page of up to `options.limit`
/// files after `options.after`, in path order, and the count over every
/// page. `should_stop` is checked between files; when it returns true the
/// page ends there, with `more` set.
pub async fn diff_page(
    repo: &ReadonlyRepo,
    from: &DiffBase,
    to: &CommitIdHex,
    options: &DiffOptions,
    should_stop: &dyn Fn() -> bool,
) -> Result<DiffPage, DiffError> {
    let store = repo.store();
    let to_commit = store.get_commit_async(&to.to_id()).await?;
    let from_commits: Vec<Commit> = match from {
        DiffBase::Parents => to_commit.parents().await?,
        DiffBase::Commit(id) => vec![store.get_commit_async(&id.to_id()).await?],
    };
    let from_tree = merge_commit_trees(repo, &from_commits).await?;
    let to_tree = to_commit.tree();
    let matcher: Box<dyn Matcher> = if options.paths.is_empty() {
        Box::new(EverythingMatcher)
    } else {
        Box::new(PrefixMatcher::new(&options.paths))
    };
    // Rename and copy detection costs more than linear in the files that
    // differ (the git backend compares them pairwise), and every page
    // would pay it again; past git's own default `diff.renameLimit` it is
    // skipped, as git skips it, and files are added and deleted.
    let mut copies = CopyRecords::default();
    let mut differ = 0;
    let mut counting = from_tree.diff_stream(&to_tree, &*matcher);
    while differ <= RENAME_LIMIT && counting.next().await.is_some() {
        differ += 1;
    }
    drop(counting);
    if differ <= RENAME_LIMIT && !should_stop() {
        for commit in &from_commits {
            copy_records(repo, commit.id(), to_commit.id(), &mut copies).await?;
        }
    }
    let materialize = materialize_options(repo.settings())?;
    let read = match options.format {
        DiffFormat::Summary => None,
        DiffFormat::Stat | DiffFormat::Hunks => Some(options.max_file_bytes),
    };
    let mut page = DiffPage {
        files: Vec::new(),
        total: 0,
        more: false,
        from: from_commits
            .iter()
            .map(|c| CommitIdHex::of(c.id()))
            .collect(),
        to: to.clone(),
    };
    let mut stream = from_tree.diff_stream_with_copies(&to_tree, &*matcher, &copies);
    while let Some(entry) = stream.next().await {
        if should_stop() {
            // The RPC is gone: `total` no longer matters.
            page.more = true;
            break;
        }
        page.total += 1;
        if options
            .after
            .as_ref()
            .is_some_and(|after| entry.path.target() <= after.as_ref())
        {
            continue;
        }
        if page.more {
            continue;
        }
        if page.files.len() == options.limit {
            page.more = true;
            continue;
        }
        let path = entry.path.target().as_internal_file_string().to_owned();
        let sides = Sides {
            from: &from_tree,
            to: &to_tree,
            read,
            materialize: &materialize,
        };
        page.files
            .push(file_diff(entry, path, &sides, options).await?);
    }
    Ok(page)
}

/// Adds the backend's copy records between `from` and `to` to `records`; a
/// backend without them adds none.
async fn copy_records(
    repo: &ReadonlyRepo,
    from: &CommitId,
    to: &CommitId,
    records: &mut CopyRecords,
) -> Result<(), DiffError> {
    let stream = match repo.store().get_copy_records(None, from, to) {
        Ok(stream) => stream,
        Err(BackendError::Unsupported(_)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let found: Vec<_> = stream.collect().await;
    let found: Vec<_> = found.into_iter().collect::<Result<_, _>>()?;
    records.add_records(found);
    Ok(())
}

/// Up to `cap` conflicted paths of `commit`'s tree in path order, and how
/// many it has in all.
pub fn conflicted_paths(
    repo: &ReadonlyRepo,
    commit: &CommitIdHex,
    cap: usize,
) -> Result<(Vec<String>, usize), DiffError> {
    let commit = repo.store().get_commit(&commit.to_id())?;
    let mut paths = Vec::new();
    let mut total = 0;
    for (path, value) in commit.tree().conflicts() {
        value?;
        if paths.len() < cap {
            paths.push(path.as_internal_file_string().to_owned());
        }
        total += 1;
    }
    Ok((paths, total))
}
