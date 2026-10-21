//! The reflink backend (Linux, R1.10): one `FICLONE` per file on btrfs,
//! XFS (`reflink=1`), bcachefs, or any filesystem that implements
//! `remap_file_range`. Linux has no directory clone, so creation is R1.07's
//! [`copy_tree`] walk with a clone in place of each copy: O(files)
//! syscalls, but every data block shared with the base until written. It is
//! the copy-on-write backend for Linux machines that cannot mount overlays
//! (R1.09).
//!
//! A clone that fails is an error, never a copy: `EOPNOTSUPP` and `EXDEV`
//! (the filesystem changed since the probe) are
//! [`BackendError::Unavailable`], anything else [`BackendError::Io`]. The
//! backend is chosen by its probe, a trial clone, never by the filesystem's
//! name.
//!
//! The same walk builds new bases from recent ones ([`DirCloner`],
//! `BaseCache::ensure_from`), so the base cache is shared with R1.08.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Instant;

use reef_lib::telemetry::elapsed_ms;

use super::base_cache::DirCloner;
use super::fs_util::{
    FileCopier, TreeError, cannot_clone, changed_files_vs_base, copy_tree, tear_down_root,
    trial_clone,
};
use super::{
    Availability, Backend, BackendError, BackendKind, BaseEntry, DiskUsage, SwapOutcome,
    WorkspacePaths,
};

/// Per-file reflink (`FICLONE`) materialization for btrfs and XFS.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReflinkBackend;

/// Clones one regular file with `FICLONE`. `reflink_copy::reflink` creates
/// `to` with `O_EXCL`, removes it again if the clone fails, and gives it
/// `from`'s permissions; it never copies.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReflinkCloner;

impl FileCopier for ReflinkCloner {
    fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
        reflink_copy::reflink(from, to)
    }
}

impl DirCloner for ReflinkBackend {
    fn clone_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        copy_tree(from, to, &ReflinkCloner)
            .map(drop)
            .map_err(io::Error::other)
    }
}

impl Backend for ReflinkBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Reflink
    }

    /// Clones the base into `ws.root` file by file. A failure part way
    /// leaves a partial tree, which the daemon tears down (R1.05); a
    /// filesystem that cannot clone fails on the first file, and the probe
    /// forgets what it cached so the next create probes again.
    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError> {
        if std::fs::symlink_metadata(&ws.root).is_ok() {
            return Err(BackendError::RootExists(ws.root.clone()));
        }
        let begun = Instant::now();
        let counts =
            copy_tree(&base.path, &ws.root, &ReflinkCloner).map_err(|error| match error {
                TreeError::Copy { from, to, source } if cannot_clone(&source) => {
                    forget_probes(&to);
                    BackendError::Unavailable {
                        kind: BackendKind::Reflink,
                        reason: format!(
                            "cannot clone {} to {}: {source}",
                            from.display(),
                            to.display()
                        ),
                    }
                }
                error => error.into(),
            })?;
        tracing::info!(
            workspace_id = %ws.id,
            backend = "reflink",
            files = counts.files,
            elapsed_ms = elapsed_ms(begun),
            "workspace materialized"
        );
        Ok(())
    }

    /// The allocated blocks of files whose mtime or size differs from the
    /// base's `tree_state`, and of files the base does not have. A clone's
    /// `st_blocks` reports its full size although it shares every block, so
    /// the whole tree cannot simply be summed. An estimate, as for APFS.
    fn disk_usage(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        changed_files_vs_base(base, ws)
    }

    /// Nothing to do on disk: the daemon applies the tree diff with a jj
    /// check-out, and the files it writes stop sharing blocks with the base.
    fn swap_base(
        &self,
        _: &WorkspacePaths,
        _: &BaseEntry,
        _: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        Ok(SwapOutcome::NeedsCheckout)
    }

    /// Renames the root to a `.trash-*` sibling and deletes it in the
    /// background.
    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        tear_down_root(&ws.root)
    }

    fn dir_cloner(&self) -> Option<&'static dyn DirCloner> {
        Some(&ReflinkBackend)
    }
}

/// A base cache directory (none for the daemon's status) and a target
/// filesystem's `st_dev`.
type ProbeKey = (Option<PathBuf>, u64);

/// Base cache directories and the filesystems (`st_dev`) a trial reflink
/// between them succeeded on, for the daemon's lifetime: a filesystem does
/// not stop supporting reflinks while mounted. If another filesystem is
/// mounted in its place (device numbers are reused), `materialize` fails
/// with [`BackendError::Unavailable`] and drops that filesystem's entries,
/// so only that one create fails. Failures are not remembered, so a transient one (a full
/// disk) is probed again on the next create.
static AVAILABLE: LazyLock<Mutex<HashSet<ProbeKey>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether workspaces under `target_dir` (which exists) can be reflinked
/// from bases under `base_dir`: the two are on one filesystem, and a trial
/// reflink of the probe file into `target_dir` succeeds ([`trial_clone`]).
/// Without a `base_dir` (the daemon's status), whether `target_dir`'s
/// filesystem can reflink at all. A success is cached per base cache
/// directory and target filesystem, so each repository probes once and a
/// `--path` on another filesystem is probed on its own.
pub(crate) fn probe_reflink(base_dir: Option<&Path>, target_dir: &Path) -> Availability {
    let key = std::fs::metadata(target_dir)
        .ok()
        .map(|metadata| (base_dir.map(Path::to_owned), metadata.dev()));
    let cached = |key: &ProbeKey| {
        AVAILABLE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(key)
    };
    if key.as_ref().is_some_and(cached) {
        return Availability::Available;
    }
    match trial_clone(base_dir, target_dir, &|from, to| {
        reflink_copy::reflink(from, to)
    }) {
        Ok(()) => {
            if let Some(key) = key {
                AVAILABLE
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key);
            }
            Availability::Available
        }
        Err(reason) => Availability::Unavailable(reason),
    }
}

/// Drops [`AVAILABLE`]'s entries for the filesystem holding `failed`'s
/// directory (all of them if it cannot be read): a clone to `failed` could
/// not be made.
fn forget_probes(failed: &Path) {
    let dev = failed
        .parent()
        .and_then(|dir| std::fs::metadata(dir).ok())
        .map(|metadata| metadata.dev());
    AVAILABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(_, cached)| dev.is_some_and(|dev| dev != *cached));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, SystemTime};

    use reef_lib::error::{Classify as _, ErrorKind};

    use super::*;
    use crate::backend::fs_util::free_bytes;
    use crate::backend::probe::filesystem_name;
    use crate::model::RepoId;

    /// Size of the large file the copy-on-write check appends to.
    const BIG: usize = 4 << 20;

    /// `REEF_TEST_REFLINK_DIR`, if it names a directory on a filesystem
    /// that can reflink; otherwise prints why the test is skipped.
    #[allow(clippy::print_stderr, reason = "says why the test did nothing")]
    fn reflink_dir() -> Option<PathBuf> {
        let Some(dir) = std::env::var_os("REEF_TEST_REFLINK_DIR") else {
            eprintln!("skipped: REEF_TEST_REFLINK_DIR not set");
            return None;
        };
        let dir = PathBuf::from(dir);
        assert!(
            probe_reflink(None, &dir).is_available(),
            "REEF_TEST_REFLINK_DIR ({}, {}) cannot reflink",
            dir.display(),
            filesystem_name(&dir).unwrap_or_default()
        );
        Some(dir)
    }

    /// A tree with a [`BIG`] file, a read-only file, an executable, a
    /// relative symlink, and nested directories, every file with its own
    /// mtime.
    fn build_tree(root: &Path) {
        let files: [(&str, Vec<u8>, u32); 4] = [
            ("big.bin", vec![7; BIG], 0o644),
            ("dir/read-only.txt", b"sealed".to_vec(), 0o444),
            ("dir/deep/c.txt", b"nested".to_vec(), 0o644),
            ("bin/tool", b"#!/bin/sh\n".to_vec(), 0o755),
        ];
        for (index, (name, contents, mode)) in files.into_iter().enumerate() {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(&contents).unwrap();
            file.set_modified(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + index as u64),
            )
            .unwrap();
            // The source's blocks are allocated before free space is read.
            file.sync_all().unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
        std::os::unix::fs::symlink("../big.bin", root.join("dir/link")).unwrap();
    }

    /// Asserts `b` holds `a`'s entries with the same kinds, sizes, modes,
    /// file mtimes, contents, and symlink targets.
    fn assert_same_tree(a: &Path, b: &Path) {
        let entries = |root: &Path| -> Vec<PathBuf> {
            let mut paths: Vec<_> = walkdir::WalkDir::new(root)
                .min_depth(1)
                .into_iter()
                .map(|entry| entry.unwrap().path().strip_prefix(root).unwrap().to_owned())
                .collect();
            paths.sort();
            paths
        };
        let paths = entries(a);
        assert_eq!(paths, entries(b));
        for path in paths {
            let (from, to) = (a.join(&path), b.join(&path));
            let (left, right) = (
                fs::symlink_metadata(&from).unwrap(),
                fs::symlink_metadata(&to).unwrap(),
            );
            assert_eq!(left.file_type(), right.file_type(), "{}", path.display());
            if left.is_dir() {
                continue;
            }
            assert_eq!(left.len(), right.len(), "{}", path.display());
            assert_eq!(
                left.modified().unwrap(),
                right.modified().unwrap(),
                "{}",
                path.display()
            );
            if left.is_symlink() {
                assert_eq!(fs::read_link(&from).unwrap(), fs::read_link(&to).unwrap());
            } else {
                assert_eq!(
                    left.permissions().mode() & 0o777,
                    right.permissions().mode() & 0o777,
                    "{}",
                    path.display()
                );
                assert_eq!(fs::read(&from).unwrap(), fs::read(&to).unwrap());
            }
        }
    }

    /// A base entry over `tree`, with an empty `tree_state` beside it.
    fn base_over(tree: &Path) -> BaseEntry {
        let state_dir = tree.with_extension("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("tree_state"), b"").unwrap();
        BaseEntry {
            repo_id: RepoId::generate(),
            tree_id: "00".into(),
            path: tree.to_owned(),
            state_dir,
            bytes: 0,
            files: 0,
        }
    }

    /// Commits everything on `dir`'s filesystem, so `statvfs` counts the
    /// blocks written so far (btrfs allocates at transaction commit).
    fn sync_fs(dir: &Path) {
        nix::unistd::syncfs(fs::File::open(dir).unwrap()).unwrap();
    }

    // 1
    #[test]
    fn reflink_clone_preserves_mtime_mode_and_shares_blocks() {
        let Some(parent) = reflink_dir() else { return };
        let dir = tempfile::tempdir_in(parent).unwrap();
        let tree = dir.path().join("bases/tree");
        build_tree(&tree);
        let base = base_over(&tree);
        sync_fs(dir.path());

        // Other writers may share the filesystem, so the least of three
        // tries is the clone's own cost.
        let mut least = u64::MAX;
        let mut workspaces = vec![];
        for attempt in 0..3_u8 {
            let ws = WorkspacePaths::under(
                &dir.path().join("repo"),
                format!("00000000000000a{attempt}").parse().unwrap(),
            );
            let free_before = free_bytes(dir.path()).unwrap();
            ReflinkBackend.materialize(&base, &ws).unwrap();
            sync_fs(dir.path());
            let free_after = free_bytes(dir.path()).unwrap();
            least = least.min(free_before.saturating_sub(free_after));
            assert_same_tree(&tree, &ws.root);
            workspaces.push(ws);
        }
        // A copy would take the 4 MiB.
        assert!(least < 1 << 20, "{least} bytes used by the clone");

        let ws = &workspaces[0];
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(ws.root.join("big.bin"))
            .unwrap();
        file.write_all(&[1; 4096]).unwrap();
        drop(file);
        assert_eq!(
            fs::metadata(tree.join("big.bin")).unwrap().len(),
            BIG as u64
        );
        assert_eq!(fs::read(tree.join("big.bin")).unwrap(), vec![7; BIG]);
        assert_eq!(
            ReflinkBackend.swap_base(ws, &base, &base).unwrap(),
            SwapOutcome::NeedsCheckout
        );

        // The base cache's incremental path clones a base the same way.
        let cloned = dir.path().join("bases/cloned");
        ReflinkBackend.clone_dir(&tree, &cloned).unwrap();
        assert_same_tree(&tree, &cloned);

        for ws in &workspaces {
            ReflinkBackend.tear_down(ws).unwrap();
            assert!(!ws.root.exists());
            let error = ReflinkBackend.tear_down(ws).unwrap_err();
            assert!(matches!(error, BackendError::RootMissing(_)), "{error:?}");
        }
        assert!(tree.join("big.bin").is_file(), "the base is untouched");
    }

    /// The probe `select` makes: the base cache and the workspaces on the
    /// reflink filesystem.
    #[test]
    fn probe_on_one_filesystem_is_available_and_cached() {
        let Some(parent) = reflink_dir() else { return };
        let dir = tempfile::tempdir_in(parent).unwrap();
        let (bases, target) = (dir.path().join("bases"), dir.path().join("ws"));
        fs::create_dir(&target).unwrap();
        assert_eq!(
            probe_reflink(Some(&bases), &target),
            Availability::Available
        );
        assert_eq!(
            fs::read(bases.join(crate::backend::base_cache::PROBE_FILE)).unwrap(),
            b"\n"
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0, "no trial left");
        let key = (Some(bases.clone()), fs::metadata(&target).unwrap().dev());
        assert!(AVAILABLE.lock().unwrap().contains(&key));

        // Cached: answered without a trial clone, so even with the probe
        // file unreadable.
        fs::remove_file(bases.join(crate::backend::base_cache::PROBE_FILE)).unwrap();
        fs::create_dir(bases.join(crate::backend::base_cache::PROBE_FILE)).unwrap();
        assert_eq!(
            probe_reflink(Some(&bases), &target),
            Availability::Available
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

        // A failed clone into that filesystem forgets it, and the next
        // probe tries again (and fails on the unreadable probe file).
        forget_probes(&target.join("failed"));
        assert!(!AVAILABLE.lock().unwrap().contains(&key));
        assert!(!probe_reflink(Some(&bases), &target).is_available());
    }

    // 2
    #[test]
    fn probe_reports_unsupported_on_tmpdir() {
        let Some(parent) = reflink_dir() else { return };
        let tmp = tempfile::tempdir().unwrap();
        let fs_name = filesystem_name(tmp.path()).unwrap();
        let target = tmp.path().join("ws");
        fs::create_dir(&target).unwrap();

        // Base cache and workspaces both on the temporary directory's
        // filesystem (ext4 or tmpfs on CI): the trial clone itself fails.
        match probe_reflink(Some(&tmp.path().join("bases")), &target) {
            Availability::Unavailable(reason) => assert!(
                reason.contains("cannot clone files") || reason.contains("different volume"),
                "{fs_name}: {reason}"
            ),
            Availability::Available => {
                #[allow(clippy::print_stderr, reason = "says why the case did nothing")]
                {
                    eprintln!("the temporary directory ({fs_name}) can reflink; case skipped");
                }
            }
        }
        // The base cache on the reflink filesystem and the workspaces on
        // another: refused before any clone is tried.
        let bases = tempfile::tempdir_in(parent).unwrap();
        if !crate::backend::fs_util::same_volume(bases.path(), &target).unwrap() {
            let availability = probe_reflink(Some(bases.path()), &target);
            assert!(
                matches!(&availability, Availability::Unavailable(reason) if reason.contains("different volume")),
                "{availability:?}"
            );
        }
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0, "no probe left");
    }

    // 3
    #[test]
    fn materialize_fails_rather_than_copies() {
        let Some(parent) = reflink_dir() else { return };
        let dir = tempfile::tempdir_in(parent).unwrap();
        let tree = dir.path().join("bases/tree");
        build_tree(&tree);
        let base = base_over(&tree);
        let elsewhere = tempfile::tempdir().unwrap();
        if crate::backend::fs_util::same_volume(dir.path(), elsewhere.path()).unwrap() {
            #[allow(clippy::print_stderr, reason = "says why the test did nothing")]
            {
                eprintln!(
                    "skipped: the temporary directory is on REEF_TEST_REFLINK_DIR's filesystem"
                );
            }
            return;
        }
        let ws = WorkspacePaths::under(elsewhere.path(), "00000000000000aa".parse().unwrap());

        let error = ReflinkBackend.materialize(&base, &ws).unwrap_err();
        assert!(
            matches!(&error, BackendError::Unavailable { kind: BackendKind::Reflink, reason } if reason.contains("cannot clone")),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        let files: Vec<_> = walkdir::WalkDir::new(&ws.root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| !entry.file_type().is_dir())
            .map(|entry| entry.path().to_owned())
            .collect();
        assert!(files.is_empty(), "nothing copied, but {files:?}");
    }

    /// [`ReflinkBackend::materialize`] from the `large` synthetic tree,
    /// generated under `REEF_TEST_REFLINK_DIR`, three times, with the
    /// free-space delta. Run by hand on a btrfs or XFS mount:
    /// `REEF_TEST_REFLINK_DIR=/mnt/reef-btrfs cargo test -p reef-daemon --release --lib reflink::tests::timing_large -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the large preset on a reflink filesystem; run by hand to record the numbers"]
    #[allow(clippy::print_stderr, reason = "reports the timings")]
    fn timing_large() {
        use reef_testing::synth::{self, Preset, Spec};

        let Some(parent) = reflink_dir() else { return };
        let dir = tempfile::tempdir_in(parent).unwrap();
        let tree = dir.path().join("bases/large");
        let manifest = synth::generate(&tree, &Spec::preset(Preset::Large), 42, false).unwrap();
        let base = base_over(&tree);
        sync_fs(dir.path());
        for run in 1..=3_u8 {
            let ws = WorkspacePaths::under(
                &dir.path().join("repo"),
                format!("00000000000000a{run}").parse().unwrap(),
            );
            let free_before = free_bytes(dir.path()).unwrap();
            let begun = Instant::now();
            ReflinkBackend.materialize(&base, &ws).unwrap();
            let elapsed = begun.elapsed();
            sync_fs(dir.path());
            let free_after = free_bytes(dir.path()).unwrap();
            eprintln!(
                "reflink materialize, large ({} files) on {}, run {run}: {elapsed:.2?}, free space -{} MiB",
                manifest.files,
                filesystem_name(dir.path()).unwrap(),
                free_before.saturating_sub(free_after) >> 20
            );
        }
    }
}
