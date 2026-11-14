//! One walk of a workspace's directory (#272, ADR 0076): what its files
//! cost, told apart by whether the working copy tracks them.
//!
//! The walk never follows a symlink, skips store mounts, counts a hard
//! link once, and stops at [`Caps`] with `incomplete`. A file the working
//! copy tracks (its own `tree_state`, as of the last snapshot) counts as
//! tracked when its mtime or size differs from the base's; one it does not
//! track counts as build output. On macOS one `getattrlistbulk` call per
//! directory reads each entry's type, times, sizes and APFS private size
//! (`disk::bulk`); elsewhere `walkdir` and `symlink_metadata`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{DiskFigure, DiskPathFigure};
use crate::model::Timestamp;

/// Where a walk stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Entries visited, directories included.
    pub max_entries: u64,
    /// Time since the walk started.
    pub max_time: Duration,
}

impl Caps {
    /// ADR 0073's caps: 2,000,000 entries or 30 seconds.
    pub const DEFAULT: Caps = Caps {
        max_entries: 2_000_000,
        max_time: Duration::from_secs(30),
    };
}

/// How the walked directory relates to the workspace's cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// A clone of the base (APFS): unchanged files share blocks, and each
    /// file's private size is what it costs alone.
    Apfs,
    /// A per-file reflink clone: sharing cannot be told cheaply.
    Reflink,
    /// A plain copy: nothing is shared, so every file costs what it holds.
    Copy,
    /// An overlay's upper directory: only what was written is there, and
    /// none of it is shared.
    OverlayUpper,
}

/// What a walk is given.
#[derive(Debug)]
pub struct WalkInput {
    /// The directory walked: the workspace's root, or an overlay's upper
    /// directory.
    pub dir: PathBuf,
    /// How it relates to the base.
    pub layout: Layout,
    /// The base's `tree_state`: path → (mtime in ms, size). `None` when the
    /// base is no longer cached: every tracked file then counts.
    pub base: Option<HashMap<String, (i64, u64)>>,
    /// The paths the working copy tracks. `None` when its `tree_state`
    /// cannot be read: the base's paths stand in.
    pub tracked: Option<HashSet<String>>,
    /// Absolute paths under `dir` not to descend into (store mounts).
    pub skip: Vec<PathBuf>,
    /// Where to stop.
    pub caps: Caps,
}

/// What a walk found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walked {
    /// The figure, `measured_at` now.
    pub figure: DiskFigure,
    /// Files that differ from the base (tracked and not), as
    /// `DiskUsage.files_changed` has always counted them.
    pub files_changed: u64,
}

impl Walked {
    /// `DiskUsage.allocated_bytes`: every file that differs from the base.
    pub fn changed_bytes(&self) -> u64 {
        self.figure.tracked_bytes + self.figure.build_bytes
    }
}

/// One entry as either walker sees it.
#[derive(Debug)]
pub(super) struct Seen {
    pub kind: Kind,
    /// Allocated bytes.
    pub allocated: u64,
    /// APFS's private size, where the filesystem tells.
    pub private: Option<u64>,
    /// The file's length.
    pub size: u64,
    /// The mtime in milliseconds, truncated as jj truncates it.
    pub mtime_ms: Option<i64>,
    /// `(device or 0, inode)` when the file has more than one link.
    pub hard_link: Option<(u64, u64)>,
}

/// An entry's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
    Symlink,
    /// A device, FIFO or socket: an overlay's whiteout is a character
    /// device.
    Other,
}

/// Walks `input.dir`. A missing or unreadable top directory is an error
/// (an active workspace's directory that is gone has no figure); entries
/// that vanish or cannot be read during the walk are skipped.
pub fn walk(input: &WalkInput) -> io::Result<Walked> {
    let started = Instant::now();
    let mut state = State::new(input, started);
    match std::fs::symlink_metadata(&input.dir) {
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", input.dir.display()),
            ));
        }
        Ok(_) => {}
    }
    walk_tree(input, &mut state)?;
    Ok(state.finish(input))
}

#[cfg(target_os = "macos")]
fn walk_tree(input: &WalkInput, state: &mut State<'_>) -> io::Result<()> {
    let mut stack = vec![(input.dir.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = match super::bulk::read_dir(&dir) {
            Ok(entries) => entries,
            // The top directory must be readable; below it, a directory
            // that vanished or cannot be read is skipped.
            Err(error) if rel.is_empty() => return Err(error),
            Err(error) => {
                tracing::debug!(dir = %dir.display(), %error, "skipped in the disk walk");
                continue;
            }
        };
        for (name, seen) in entries {
            let path = dir.join(&name);
            let child = join_rel(&rel, &name.to_string_lossy());
            if state.visit(&path, &child, &seen) == Visit::Stop {
                return Ok(());
            }
            if seen.kind == Kind::Dir && !input.skip.contains(&path) {
                stack.push((path, child));
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn walk_tree(input: &WalkInput, state: &mut State<'_>) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let entries = walkdir::WalkDir::new(&input.dir)
        .min_depth(1)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| !input.skip.iter().any(|skip| entry.path() == skip));
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.depth() == 0 => return Err(io::Error::from(error)),
            Err(error) => {
                tracing::debug!(%error, "skipped in the disk walk");
                continue;
            }
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            Kind::Dir
        } else if file_type.is_file() {
            Kind::File
        } else if file_type.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        let seen = Seen {
            kind,
            allocated: metadata.blocks() * 512,
            private: None,
            size: metadata.len(),
            mtime_ms: crate::backend::fs_util::mtime_millis(&metadata),
            hard_link: (kind != Kind::Dir && metadata.nlink() > 1)
                .then(|| (metadata.dev(), metadata.ino())),
        };
        let rel = entry
            .path()
            .strip_prefix(&input.dir)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_default();
        if state.visit(entry.path(), &rel, &seen) == Visit::Stop {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Visit {
    Go,
    Stop,
}

/// The running totals.
struct State<'a> {
    input: &'a WalkInput,
    started: Instant,
    /// Directories that hold a tracked file, the root (`""`) included.
    tracked_dirs: HashSet<String>,
    /// The base's paths not seen yet (deleted files, once the walk ends).
    unseen_base: HashSet<&'a str>,
    hard_links: HashSet<(u64, u64)>,
    entries: u64,
    tracked_bytes: u64,
    build_bytes: u64,
    unshared: Option<u64>,
    files_changed: u64,
    /// Untracked directory → bytes under it.
    untracked_dirs: HashMap<String, u64>,
    incomplete: bool,
}

/// How deep an untracked directory may be to be listed in `largest`.
const LARGEST_DEPTH: usize = 3;

impl<'a> State<'a> {
    fn new(input: &'a WalkInput, started: Instant) -> Self {
        let tracked: Box<dyn Iterator<Item = &str>> = match (&input.tracked, &input.base) {
            (Some(tracked), _) => Box::new(tracked.iter().map(String::as_str)),
            (None, Some(base)) => Box::new(base.keys().map(String::as_str)),
            (None, None) => Box::new(std::iter::empty()),
        };
        let mut tracked_dirs = HashSet::from([String::new()]);
        for path in tracked {
            let mut at = path;
            while let Some((parent, _)) = at.rsplit_once('/') {
                if !tracked_dirs.insert(parent.to_owned()) {
                    break;
                }
                at = parent;
            }
        }
        let unseen_base = match input.layout {
            // An overlay's upper holds only what changed: deletions are
            // whiteouts there, counted as they are seen.
            Layout::OverlayUpper => HashSet::new(),
            _ => input
                .base
                .as_ref()
                .map(|base| base.keys().map(String::as_str).collect())
                .unwrap_or_default(),
        };
        Self {
            input,
            started,
            tracked_dirs,
            unseen_base,
            hard_links: HashSet::new(),
            entries: 0,
            tracked_bytes: 0,
            build_bytes: 0,
            unshared: match input.layout {
                Layout::Reflink => None,
                _ => Some(0),
            },
            files_changed: 0,
            untracked_dirs: HashMap::new(),
            incomplete: false,
        }
    }

    fn is_tracked(&self, rel: &str) -> bool {
        match (&self.input.tracked, &self.input.base) {
            (Some(tracked), _) => tracked.contains(rel),
            (None, Some(base)) => base.contains_key(rel),
            (None, None) => false,
        }
    }

    fn visit(&mut self, _path: &Path, rel: &str, seen: &Seen) -> Visit {
        self.entries += 1;
        if self.entries > self.input.caps.max_entries
            || self.started.elapsed() > self.input.caps.max_time
        {
            self.incomplete = true;
            return Visit::Stop;
        }
        if seen.kind == Kind::Dir {
            return Visit::Go;
        }
        self.unseen_base.remove(rel);
        if let Some(key) = seen.hard_link
            && !self.hard_links.insert(key)
        {
            return Visit::Go;
        }
        if self.input.layout == Layout::OverlayUpper && seen.kind == Kind::Other && seen.size == 0 {
            // A whiteout: a deleted file, which costs nothing.
            self.files_changed += 1;
            return Visit::Go;
        }
        // What the file costs alone.
        if let Some(sum) = self.unshared.as_mut() {
            *sum += match self.input.layout {
                Layout::Apfs => seen.private.unwrap_or(seen.allocated),
                _ => seen.allocated,
            };
        }
        let tracked = self.is_tracked(rel);
        if tracked {
            let unchanged = self.input.layout != Layout::OverlayUpper
                && self.input.base.as_ref().is_some_and(|base| {
                    base.get(rel).is_some_and(|&(mtime_ms, size)| {
                        size == seen.size && seen.mtime_ms == Some(mtime_ms)
                    })
                });
            if !unchanged {
                self.tracked_bytes += seen.allocated;
                self.files_changed += 1;
            }
        } else {
            self.build_bytes += seen.allocated;
            self.files_changed += 1;
            if let Some(dir) = self.untracked_root(rel) {
                *self.untracked_dirs.entry(dir).or_default() += seen.allocated;
            }
        }
        Visit::Go
    }

    /// The shallowest ancestor directory of `rel`, at most
    /// [`LARGEST_DEPTH`] deep, that holds no tracked file: `target` for
    /// `target/debug/x`, `web/node_modules` for `web/node_modules/a/b`.
    /// `None` for a file directly in a directory with tracked files.
    fn untracked_root(&self, rel: &str) -> Option<String> {
        let mut end = 0;
        for (depth, component) in rel.split('/').enumerate() {
            if depth >= LARGEST_DEPTH {
                return None;
            }
            end += component.len();
            if end >= rel.len() {
                // The file itself.
                return None;
            }
            let dir = &rel[..end];
            if !self.tracked_dirs.contains(dir) {
                return Some(dir.to_owned());
            }
            end += 1;
        }
        None
    }

    fn finish(self, input: &WalkInput) -> Walked {
        let mut largest: Vec<DiskPathFigure> = self
            .untracked_dirs
            .into_iter()
            .map(|(path, bytes)| DiskPathFigure { path, bytes })
            .collect();
        largest.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        largest.truncate(3);
        let deleted = if self.incomplete {
            0
        } else {
            self.unseen_base.len() as u64
        };
        Walked {
            figure: DiskFigure {
                tracked_bytes: self.tracked_bytes,
                build_bytes: self.build_bytes,
                unshared_bytes: self.unshared,
                files: self.entries.min(input.caps.max_entries),
                largest,
                missing_dependencies: Vec::new(),
                incomplete: self.incomplete,
                measured_at: Timestamp::now(),
            },
            files_changed: self.files_changed + deleted,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn input(dir: &Path, layout: Layout) -> WalkInput {
        WalkInput {
            dir: dir.to_owned(),
            layout,
            base: None,
            tracked: None,
            skip: Vec::new(),
            caps: Caps::DEFAULT,
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7_u8; bytes]).unwrap();
    }

    fn state_of(path: &Path) -> (i64, u64) {
        let metadata = fs::metadata(path).unwrap();
        (
            crate::backend::fs_util::mtime_millis(&metadata).unwrap(),
            metadata.len(),
        )
    }

    #[test]
    fn classify() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("ws");
        write(&root.join("src/same.rs"), 10);
        write(&root.join("src/edited.rs"), 10);
        write(&root.join("README.md"), 10);
        let mut base = HashMap::new();
        for path in ["src/same.rs", "src/edited.rs", "README.md", "gone.txt"] {
            let on_disk = root.join(path);
            let state = if on_disk.exists() {
                state_of(&on_disk)
            } else {
                (0, 1)
            };
            base.insert(path.to_owned(), state);
        }
        // Edited after the base recorded it.
        write(&root.join("src/edited.rs"), 20_000);
        write(&root.join("new-untracked.txt"), 5_000);
        write(&root.join("target/debug/big"), 300_000);
        write(&root.join("target/debug/small"), 1_000);
        write(&root.join("web/node_modules/a/index.js"), 100_000);
        write(&root.join("web/package.json"), 10);
        // A store mount, skipped.
        write(&root.join("store/blob"), 900_000);
        // A symlink out of the tree, not followed.
        let outside = temp.path().join("outside");
        write(&outside.join("huge"), 800_000);
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        // A hard link, counted once.
        fs::hard_link(
            root.join("target/debug/big"),
            root.join("target/debug/big2"),
        )
        .unwrap();
        let tracked: HashSet<String> = [
            "src/same.rs",
            "src/edited.rs",
            "README.md",
            "web/package.json",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let walked = walk(&WalkInput {
            base: Some(base),
            tracked: Some(tracked),
            skip: vec![root.join("store")],
            ..input(&root, Layout::Copy)
        })
        .unwrap();
        let figure = &walked.figure;
        assert!(figure.tracked_bytes >= 20_000, "{figure:?}");
        assert!(figure.tracked_bytes < 40_000, "{figure:?}");
        // The new file, target, node_modules, web/package.json is tracked.
        assert!(figure.build_bytes >= 406_000, "{figure:?}");
        assert!(
            figure.build_bytes < 600_000,
            "no store, no symlink target, one link: {figure:?}"
        );
        let paths: Vec<&str> = figure.largest.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["target", "web/node_modules"]);
        assert!(!figure.incomplete);
        // The copy shares nothing: unshared covers every file.
        assert!(figure.unshared_bytes.unwrap() >= figure.tracked_bytes + figure.build_bytes);
        // edited, web/package.json (tracked, not in the base), new, big,
        // small, index.js, link, and the deleted gone.txt.
        assert_eq!(walked.files_changed, 8, "{walked:?}");
    }

    #[test]
    fn caps() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write(&temp.path().join(format!("d{}/f{i}", i % 5)), 10);
        }
        let capped = walk(&WalkInput {
            caps: Caps {
                max_entries: 20,
                max_time: Duration::from_secs(30),
            },
            ..input(temp.path(), Layout::Copy)
        })
        .unwrap();
        assert!(capped.figure.incomplete);
        assert_eq!(capped.figure.files, 20);
        let timed = walk(&WalkInput {
            caps: Caps {
                max_entries: 1_000,
                max_time: Duration::ZERO,
            },
            ..input(temp.path(), Layout::Copy)
        })
        .unwrap();
        assert!(timed.figure.incomplete);
        let whole = walk(&input(temp.path(), Layout::Copy)).unwrap();
        assert!(!whole.figure.incomplete);
        assert_eq!(whole.figure.files, 55);
    }

    #[test]
    fn missing_dir_is_an_error_and_reflink_has_no_unshared() {
        let temp = tempfile::tempdir().unwrap();
        let missing = walk(&input(&temp.path().join("nope"), Layout::Apfs)).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        write(&temp.path().join("a"), 10);
        let reflink = walk(&input(temp.path(), Layout::Reflink)).unwrap();
        assert_eq!(reflink.figure.unshared_bytes, None);
    }

    /// An APFS clone shares its blocks until written (D4).
    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clone_is_unshared_only_once_written() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write(&source.join("blob"), 1 << 20);
        let clone = temp.path().join("clone");
        crate::backend::apfs::clone_dir(&source, &clone).unwrap();
        let unshared = |dir: &Path| {
            walk(&input(dir, Layout::Apfs))
                .unwrap()
                .figure
                .unshared_bytes
                .unwrap()
        };
        if unshared(&source) == 0 {
            // Not APFS (a tmpfs or HFS temporary directory): nothing to show.
            return;
        }
        assert_eq!(unshared(&clone), 0);
        write(&clone.join("blob"), 1 << 20);
        assert!(unshared(&clone) >= 1 << 20);
    }
}
