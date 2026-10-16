//! Filesystem helpers shared by every backend. R1.07 adds the copy walk,
//! mtime restoration, the trash rename, and background deletion here;
//! backends do not grow private copies of these.
//!
//! Walks never follow symlinks, the walk's root included
//! (`follow_root_links(false)`: walkdir follows a symlinked root by
//! default).

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::SystemTime;

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
        std::fs::metadata(path)
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
        std::fs::create_dir(&tree).unwrap();
        let empty = allocated_bytes(&tree).unwrap();
        std::fs::write(tree.join("big"), vec![7u8; 256 * 1024]).unwrap();
        let one = allocated_bytes(&tree).unwrap();
        assert!(one >= empty + 256 * 1024, "{one} vs {empty}");
        std::fs::hard_link(tree.join("big"), tree.join("again")).unwrap();
        let linked = allocated_bytes(&tree).unwrap();
        assert!(linked < one + 256 * 1024, "a hard link is counted once");
        std::os::unix::fs::symlink("/", tree.join("root")).unwrap();
        assert!(
            allocated_bytes(&tree).unwrap() < one + 256 * 1024,
            "symlinks are not followed"
        );
    }

    #[test]
    fn free_bytes_and_volumes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).unwrap() > 0);
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(same_volume(dir.path(), &sub).unwrap());
        let error = same_volume(dir.path(), &dir.path().join("missing")).unwrap_err();
        assert!(
            matches!(error, BackendError::Io { op: "stat", .. }),
            "{error:?}"
        );
    }
}
