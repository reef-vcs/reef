//! Filesystem helpers shared by every backend: disk accounting and the
//! stat-only comparison with a base (R1.02), and the tree copy walk, mtime
//! restoration, and rename-then-delete teardown (R1.07). Backends do not
//! grow private copies of these.
//!
//! Walks never follow symlinks, the walk's root included
//! (`follow_root_links(false)`: walkdir follows a symlinked root by
//! default).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::sys::stat::{UtimensatFlags, utimensat};
use nix::sys::time::TimeSpec;
use reef_lib::error::{Classify, ErrorChain, ErrorKind, classify_by_source};
use reef_lib::jj::materialize::tree_state_entries;
use walkdir::WalkDir;

use super::{BackendError, BaseEntry, DiskUsage, WorkspacePaths};

/// `st_blocks` is in 512-byte units on macOS and Linux, whatever the
/// filesystem's block size.
const BLOCK_UNIT: u64 = 512;

/// Allocated bytes (`st_blocks * 512`) of `path` and, for a directory,
/// everything under it, directories included. A file with several hard
/// links under `path` is counted once. A missing `path` is zero.
pub fn allocated_bytes(path: &Path) -> Result<u64, BackendError> {
    let mut seen = HashSet::new();
    let mut total = 0;
    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.depth() == 0 && is_not_found(&error) => return Ok(0),
            Err(error) => return Err(walk_error(path, error)),
        };
        let metadata = entry.metadata().map_err(|error| walk_error(path, error))?;
        if metadata.nlink() > 1
            && !metadata.is_dir()
            && !seen.insert((metadata.dev(), metadata.ino()))
        {
            continue;
        }
        total += metadata.blocks() * BLOCK_UNIT;
    }
    Ok(total)
}

/// Bytes available to an unprivileged user on the filesystem holding
/// `path` (`statvfs`: available blocks times the fragment size).
#[allow(
    clippy::useless_conversion,
    reason = "fsblkcnt_t and c_ulong are u64 on Linux but not on every target"
)]
pub fn free_bytes(path: &Path) -> Result<u64, BackendError> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(|errno| BackendError::Io {
        op: "statvfs",
        path: path.to_owned(),
        source: io::Error::from(errno),
    })?;
    Ok(u64::from(stat.blocks_available()).saturating_mul(u64::from(stat.fragment_size())))
}

/// Whether `a` and `b` are on the same filesystem (`st_dev`), as clone
/// backends and renames require. Symlinks are followed.
pub fn same_volume(a: &Path, b: &Path) -> Result<bool, BackendError> {
    let dev = |path: &Path| {
        fs::metadata(path)
            .map(|metadata| metadata.dev())
            .map_err(|source| BackendError::Io {
                op: "stat",
                path: path.to_owned(),
                source,
            })
    };
    Ok(dev(a)? == dev(b)?)
}

/// Disk usage of a clone or copy workspace relative to its base, by a
/// stat-only walk of `ws.root` compared with the base's `tree_state`.
///
/// A file counts as changed when the base has no file at its path or when
/// its mtime or size differs from what jj recorded for the base; its
/// allocated blocks count towards [`DiskUsage::allocated_bytes`]. Base files
/// missing from the workspace count as changed with no bytes. Unchanged
/// files are assumed to share the base's blocks (clones) or are what any
/// copy costs anyway, and are not counted. Ignored files (build output)
/// are not in the base, so they count. Directories are not counted.
pub fn changed_files_vs_base(
    base: &BaseEntry,
    ws: &WorkspacePaths,
) -> Result<DiskUsage, BackendError> {
    let mut expected: HashMap<String, (i64, u64)> = tree_state_entries(&base.state_dir)?
        .into_iter()
        .map(|entry| (entry.path, (entry.mtime_ms, entry.size)))
        .collect();
    let mut usage = DiskUsage::default();
    for entry in WalkDir::new(&ws.root)
        .min_depth(1)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = entry.map_err(|error| walk_error(&ws.root, error))?;
        if entry.file_type().is_dir() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| walk_error(&ws.root, error))?;
        // Paths jj records are UTF-8 with `/` separators, which is what a
        // Unix path that is valid UTF-8 already is.
        let recorded = entry
            .path()
            .strip_prefix(&ws.root)
            .ok()
            .and_then(Path::to_str)
            .and_then(|path| expected.remove(path));
        let unchanged = recorded.is_some_and(|(mtime_ms, size)| {
            size == metadata.len() && mtime_millis(&metadata) == Some(mtime_ms)
        });
        if !unchanged {
            usage.files_changed += 1;
            usage.allocated_bytes += metadata.blocks() * BLOCK_UNIT;
        }
    }
    usage.files_changed += expected.len() as u64;
    Ok(usage)
}

/// A file's mtime in milliseconds since the epoch, truncated the way jj's
/// local working copy truncates it, so the two compare equal for an
/// untouched file.
pub fn mtime_millis(metadata: &Metadata) -> Option<i64> {
    let time = metadata.modified().ok()?;
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

/// Infix of a tree being deleted in the background: `<name>.trash-<nanos>`.
pub const TRASH_INFIX: &str = ".trash-";

/// Mode of every directory [`copy_tree`] creates. jj does not track
/// directories, so their own modes and mtimes are not carried over.
const DIR_MODE: u32 = 0o755;

/// How one regular file is produced in the destination tree.
pub trait FileCopier: Sync {
    /// Creates `to` from `from`. `from` is a regular file. Must leave `to`
    /// with the same size as `from`; mode and mtime are restored by the walk.
    fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// What [`copy_tree`] created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeCounts {
    /// Regular files.
    pub files: u64,
    /// Symlinks.
    pub symlinks: u64,
    /// Directories, the destination root included.
    pub dirs: u64,
}

/// Failure of [`copy_tree`]. Whatever was written stays; the caller removes
/// the partial tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TreeError {
    /// The destination already exists. Nothing was written.
    #[error("{} already exists", .0.display())]
    DestinationExists(PathBuf),
    /// The source could not be walked, or is not a directory.
    #[error("walk {}", path.display())]
    Walk {
        /// The entry being read.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A directory could not be created.
    #[error("create directory {}", path.display())]
    Create {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A regular file could not be copied, or the source holds something
    /// that is neither a file, a symlink, nor a directory.
    #[error("copy {} to {}", from.display(), to.display())]
    Copy {
        /// The source file.
        from: PathBuf,
        /// The destination.
        to: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A symlink could not be read or recreated.
    #[error("recreate symlink {}", path.display())]
    Symlink {
        /// The destination symlink.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A destination's mode or mtime could not be set.
    #[error("set the metadata of {}", path.display())]
    Metadata {
        /// The destination entry.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl Classify for TreeError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::DestinationExists(_) => ErrorKind::AlreadyExists,
            // A nested directory that already exists (a case-only name
            // clash on a case-insensitive volume) is the filesystem's
            // doing, not the caller's: `Io`.
            _ => classify_by_source(self),
        }
    }
}

impl From<TreeError> for BackendError {
    fn from(error: TreeError) -> Self {
        let (op, path, source) = match error {
            TreeError::DestinationExists(path) => return BackendError::RootExists(path),
            TreeError::Walk { path, source } => ("walk", path, source),
            TreeError::Create { path, source } => ("create directory", path, source),
            TreeError::Copy { to, source, .. } => ("copy to", to, source),
            TreeError::Symlink { path, source } => ("recreate symlink", path, source),
            TreeError::Metadata { path, source } => ("set the metadata of", path, source),
        };
        BackendError::Io { op, path, source }
    }
}

/// Copies the tree at `src` to `dst` (which must not exist; its parent is
/// created if missing), creating directories (mode `0o755`), producing
/// regular files through `copier`, recreating symlinks verbatim (dangling
/// ones included), and restoring mode bits on regular files and mtimes on
/// regular files and symlinks: what jj's `tree_state` keys on. Ownership,
/// ACLs, xattrs, and hard links are not preserved.
///
/// Entries are visited in file-name order, so a failure is deterministic.
/// Anything other than a file, a symlink, or a directory is an error.
pub fn copy_tree(src: &Path, dst: &Path, copier: &dyn FileCopier) -> Result<TreeCounts, TreeError> {
    // Checked before anything is created, so a missing base leaves nothing.
    check_source_dir(src)?;
    if fs::symlink_metadata(dst).is_ok() {
        return Err(TreeError::DestinationExists(dst.to_owned()));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|source| TreeError::Create {
            path: parent.to_owned(),
            source,
        })?;
    }
    let mut counts = TreeCounts::default();
    for entry in WalkDir::new(src)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| TreeError::Walk {
            path: error.path().unwrap_or(src).to_owned(),
            source: io::Error::from(error),
        })?;
        let relative = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = if relative.as_os_str().is_empty() {
            dst.to_owned()
        } else {
            dst.join(relative)
        };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            // `create`, not `create_all`: the root must not exist, and every
            // other directory's parent was created before it.
            fs::DirBuilder::new()
                .mode(DIR_MODE)
                .create(&target)
                .map_err(|source| {
                    if entry.depth() == 0 && source.kind() == io::ErrorKind::AlreadyExists {
                        TreeError::DestinationExists(target.clone())
                    } else {
                        TreeError::Create {
                            path: target.clone(),
                            source,
                        }
                    }
                })?;
            counts.dirs += 1;
            continue;
        }
        let metadata = entry.metadata().map_err(|error| TreeError::Walk {
            path: entry.path().to_owned(),
            source: io::Error::from(error),
        })?;
        if file_type.is_symlink() {
            let link = fs::read_link(entry.path()).map_err(|source| TreeError::Symlink {
                path: target.clone(),
                source,
            })?;
            std::os::unix::fs::symlink(&link, &target).map_err(|source| TreeError::Symlink {
                path: target.clone(),
                source,
            })?;
            counts.symlinks += 1;
        } else if file_type.is_file() {
            copier
                .copy_file(entry.path(), &target)
                .map_err(|source| TreeError::Copy {
                    from: entry.path().to_owned(),
                    to: target.clone(),
                    source,
                })?;
            fs::set_permissions(&target, fs::Permissions::from_mode(metadata.mode() & 0o777))
                .map_err(|source| TreeError::Metadata {
                    path: target.clone(),
                    source,
                })?;
            counts.files += 1;
        } else {
            return Err(TreeError::Copy {
                from: entry.path().to_owned(),
                to: target,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file, symlink, or directory",
                ),
            });
        }
        // Last, so nothing touches the entry afterwards.
        restore_mtime(&target, &metadata).map_err(|source| TreeError::Metadata {
            path: target.clone(),
            source,
        })?;
    }
    Ok(counts)
}

/// Fails unless `src` is a directory (not a symlink to one).
fn check_source_dir(src: &Path) -> Result<(), TreeError> {
    match fs::symlink_metadata(src) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(TreeError::Walk {
            path: src.to_owned(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "not a directory"),
        }),
        Err(source) => Err(TreeError::Walk {
            path: src.to_owned(),
            source,
        }),
    }
}

/// Sets `to`'s mtime (and atime) to `from_meta`'s with `utimensat` and
/// `AT_SYMLINK_NOFOLLOW`: by path without opening the file, so a read-only
/// file works, and a symlink's own times are set rather than its target's.
pub fn restore_mtime(to: &Path, from_meta: &Metadata) -> io::Result<()> {
    let atime = TimeSpec::new(from_meta.atime(), from_meta.atime_nsec());
    let mtime = TimeSpec::new(from_meta.mtime(), from_meta.mtime_nsec());
    utimensat(
        nix::fcntl::AT_FDCWD,
        to,
        &atime,
        &mtime,
        UtimensatFlags::NoFollowSymlink,
    )
    .map_err(io::Error::from)
}

/// Whether `mode` has an executable bit, as jj's working copy decides it.
pub fn is_executable(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// Renames `path` to a `<name>.trash-<nanos>` sibling and deletes that in
/// the background, on the tokio runtime's blocking pool when called with a
/// runtime (`spawn_blocking` threads count), inline otherwise. Returns once
/// `path` is gone: "no directory on disk" holds from the rename.
pub async fn remove_tree_async(path: &Path) -> io::Result<()> {
    remove_tree_in_background(path).map(drop)
}

/// [`remove_tree_async`] without the `async`, for synchronous callers such
/// as [`super::Backend::tear_down`]. Returns the deletion task when it was
/// spawned, so tests can wait for it.
pub(crate) fn remove_tree_in_background(
    path: &Path,
) -> io::Result<Option<tokio::task::JoinHandle<()>>> {
    let trash = move_to_trash(path)?;
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        Ok(Some(runtime.spawn_blocking(move || delete_trash(&trash))))
    } else {
        // A plain thread (a repository thread, a synchronous test): nothing
        // to hand the deletion to, so the caller waits for it.
        tracing::debug!(path = %trash.display(), "no tokio runtime; deleting trash inline");
        delete_trash(&trash);
        Ok(None)
    }
}

/// Renames `path` to an unused `<name>.trash-<nanos>` sibling.
fn move_to_trash(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    // Two removals of one name in the same nanosecond, or a leftover with
    // the same name, take the next number.
    for attempt in 0..16 {
        let mut trash_name = OsString::from(name);
        trash_name.push(format!("{TRASH_INFIX}{}", nanos + attempt));
        let trash = path.with_file_name(trash_name);
        if fs::symlink_metadata(&trash).is_ok() {
            continue;
        }
        fs::rename(path, &trash)?;
        return Ok(trash);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free trash name",
    ))
}

/// Deletes a trashed tree, logging rather than returning failure: the
/// caller has moved on, and the start-up sweep retries.
fn delete_trash(trash: &Path) {
    match remove_all(trash) {
        Ok(()) => tracing::debug!(path = %trash.display(), "trash deleted"),
        Err(error) => tracing::warn!(
            path = %trash.display(),
            error = %ErrorChain(&error),
            "cannot delete trash; the next daemon start retries"
        ),
    }
}

/// `remove_dir_all` (or `remove_file` for a non-directory), retried once
/// after making every directory under `path` writable if the first attempt
/// was refused: an agent may have left a read-only directory in its
/// workspace. Symlinks are never followed.
fn remove_all(path: &Path) -> io::Result<()> {
    let remove = |path: &Path| {
        if fs::symlink_metadata(path)?.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
    };
    match remove(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            for entry in WalkDir::new(path)
                .follow_links(false)
                .follow_root_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_dir())
            {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(DIR_MODE));
            }
            remove(path)
        }
        other => other,
    }
}

/// Removes every `*.trash-*` entry directly under `dir`, synchronously, as
/// the daemon does at start for what background deletions left behind.
/// Returns how many were removed. A missing `dir` has none. An entry that
/// cannot be removed is logged and skipped, so one stuck tree does not hide
/// the others; only failing to list `dir` is an error.
pub fn sweep_trash(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.contains(TRASH_INFIX))
        {
            match remove_all(&entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(
                    path = %entry.path().display(),
                    error = %ErrorChain(&error),
                    "cannot remove workspace trash"
                ),
            }
        }
    }
    Ok(removed)
}

fn is_not_found(error: &walkdir::Error) -> bool {
    error
        .io_error()
        .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
}

fn walk_error(root: &Path, error: walkdir::Error) -> BackendError {
    let path = error.path().unwrap_or(root).to_owned();
    BackendError::Io {
        op: "walk",
        path,
        source: io::Error::from(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_bytes_counts_files_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(allocated_bytes(&dir.path().join("missing")).unwrap(), 0);

        let tree = dir.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let empty = allocated_bytes(&tree).unwrap();
        fs::write(tree.join("big"), vec![7u8; 256 * 1024]).unwrap();
        let one = allocated_bytes(&tree).unwrap();
        assert!(one >= empty + 256 * 1024, "{one} vs {empty}");
        fs::hard_link(tree.join("big"), tree.join("again")).unwrap();
        let linked = allocated_bytes(&tree).unwrap();
        assert!(linked < one + 256 * 1024, "a hard link is counted once");
        std::os::unix::fs::symlink("/", tree.join("root")).unwrap();
        assert!(
            allocated_bytes(&tree).unwrap() < one + 256 * 1024,
            "symlinks are not followed"
        );
    }

    /// A copier through `std::fs::copy`, as the copy backend's.
    struct StdCopy;

    impl FileCopier for StdCopy {
        fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
            fs::copy(from, to).map(drop)
        }
    }

    fn set_mtime(path: &Path, secs: i64, nsec: i64) {
        let time = TimeSpec::new(secs, nsec);
        utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &time,
            &time,
            UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn copy_tree_preserves_mode_mtime_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("nested/deeper")).unwrap();
        fs::create_dir(src.join("empty")).unwrap();
        fs::write(src.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(src.join("nested/plain.txt"), b"plain").unwrap();
        fs::set_permissions(
            src.join("nested/plain.txt"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::write(src.join("nested/deeper/readonly"), b"read only").unwrap();
        fs::set_permissions(
            src.join("nested/deeper/readonly"),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        std::os::unix::fs::symlink("nested/plain.txt", src.join("link")).unwrap();
        std::os::unix::fs::symlink("does/not/exist", src.join("dangling")).unwrap();
        // Distinct, old, sub-millisecond-precise mtimes, so a copy that kept
        // the copy time instead would show.
        for (index, path) in [
            "run.sh",
            "nested/plain.txt",
            "nested/deeper/readonly",
            "link",
            "dangling",
        ]
        .into_iter()
        .enumerate()
        {
            set_mtime(
                &src.join(path),
                1_600_000_000 + i64::try_from(index).unwrap(),
                123_456_789,
            );
        }

        let dst = dir.path().join("out/dst");
        let counts = copy_tree(&src, &dst, &StdCopy).unwrap();
        assert_eq!(
            counts,
            TreeCounts {
                files: 3,
                symlinks: 2,
                dirs: 4,
            }
        );
        for file in ["run.sh", "nested/plain.txt", "nested/deeper/readonly"] {
            let (from, to) = (src.join(file), dst.join(file));
            assert_eq!(mode(&to), mode(&from), "{file}");
            assert_eq!(fs::read(&to).unwrap(), fs::read(&from).unwrap(), "{file}");
        }
        assert!(is_executable(mode(&dst.join("run.sh"))));
        assert!(!is_executable(mode(&dst.join("nested/plain.txt"))));
        for path in [
            "run.sh",
            "nested/plain.txt",
            "nested/deeper/readonly",
            "link",
            "dangling",
        ] {
            let from = fs::symlink_metadata(src.join(path)).unwrap();
            let to = fs::symlink_metadata(dst.join(path)).unwrap();
            assert_eq!(mtime_millis(&to), mtime_millis(&from), "{path}");
        }
        assert_eq!(
            fs::read_link(dst.join("link")).unwrap(),
            Path::new("nested/plain.txt")
        );
        assert_eq!(
            fs::read_link(dst.join("dangling")).unwrap(),
            Path::new("does/not/exist")
        );
        assert!(fs::symlink_metadata(dst.join("link")).unwrap().is_symlink());
        assert!(dst.join("empty").is_dir());
        assert_eq!(mode(&dst.join("empty")) & 0o700, 0o700);
    }

    #[test]
    fn copy_tree_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a"), b"a").unwrap();
        let dst = dir.path().join("dst");
        fs::create_dir(&dst).unwrap();
        let error = copy_tree(&src, &dst, &StdCopy).unwrap_err();
        assert!(
            matches!(&error, TreeError::DestinationExists(path) if *path == dst),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert!(matches!(
            BackendError::from(error),
            BackendError::RootExists(_)
        ));
        assert!(!dst.join("a").exists(), "nothing was written");
    }

    #[test]
    fn nested_clash_is_io_and_missing_source_writes_nothing() {
        // A nested directory that already exists (a case-only clash on a
        // case-insensitive volume) is not the workspace root existing.
        let error = TreeError::Create {
            path: "/ws/root/Sub".into(),
            source: io::Error::from(io::ErrorKind::AlreadyExists),
        };
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(matches!(
            BackendError::from(error),
            BackendError::Io {
                op: "create directory",
                ..
            }
        ));

        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("nested/dst");
        let error = copy_tree(&dir.path().join("missing"), &dst, &StdCopy).unwrap_err();
        assert!(matches!(error, TreeError::Walk { .. }), "{error:?}");
        assert!(!dir.path().join("nested").exists(), "nothing was created");
    }

    /// Names of the entries directly under `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn remove_tree_async_hides_the_directory_before_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("ws");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/file"), b"x").unwrap();
        // A read-only directory an agent left behind still goes.
        fs::set_permissions(tree.join("a"), fs::Permissions::from_mode(0o555)).unwrap();

        let task = remove_tree_in_background(&tree)
            .unwrap()
            .expect("a runtime");
        assert!(!tree.exists(), "gone when the call returns");
        task.await.unwrap();
        assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));

        fs::create_dir(&tree).unwrap();
        remove_tree_async(&tree).await.unwrap();
        assert!(!tree.exists());
        let error = remove_tree_async(&tree).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn remove_tree_without_a_runtime_deletes_inline() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("ws");
        fs::create_dir(&tree).unwrap();
        assert!(remove_tree_in_background(&tree).unwrap().is_none());
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn sweep_trash_removes_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sweep_trash(&dir.path().join("missing")).unwrap(), 0);
        fs::create_dir_all(dir.path().join("ws.trash-1/sub")).unwrap();
        fs::write(dir.path().join("ws.trash-1/sub/file"), b"x").unwrap();
        fs::write(dir.path().join("other.trash-2"), b"x").unwrap();
        fs::create_dir(dir.path().join("live")).unwrap();
        fs::create_dir(dir.path().join("live.upper")).unwrap();
        assert_eq!(sweep_trash(dir.path()).unwrap(), 2);
        assert_eq!(names(dir.path()), ["live", "live.upper"]);
    }

    #[test]
    fn allocated_bytes_counts_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full");
        fs::write(&full, vec![1u8; 1 << 20]).unwrap();
        assert!(allocated_bytes(&full).unwrap() >= 1 << 20);
        let sparse = dir.path().join("sparse");
        fs::File::create(&sparse)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        assert!(allocated_bytes(&sparse).unwrap() < 64 << 20);
    }

    #[test]
    fn free_bytes_and_volumes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).unwrap() > 0);
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(same_volume(dir.path(), &sub).unwrap());
        let error = same_volume(dir.path(), &dir.path().join("missing")).unwrap_err();
        assert!(
            matches!(error, BackendError::Io { op: "stat", .. }),
            "{error:?}"
        );
    }
}
