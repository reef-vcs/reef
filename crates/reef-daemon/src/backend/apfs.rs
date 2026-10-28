//! The APFS clone backend (macOS, R1.08): one `clonefile(2)` of the base
//! directory per workspace. APFS clones the hierarchy as if each item were
//! cloned individually, copying attributes (mtimes included) and sharing
//! every data block, so a workspace costs metadata only and the base's
//! `tree_state` describes it as is.
//!
//! `man 2 clonefile` discourages cloning directories in favour of
//! `copyfile(3)`, which clones item by item; that is 13 to 15 times slower
//! (13.3 s against 1.0 s on the 102,005-file `large` tree), so the directory
//! call is used and the per-item walk is only the fallback when it fails
//! (ADR 0015). `ENOTSUP` and `EXDEV` mean nothing on this volume can clone,
//! so they are [`BackendError::Unavailable`] rather than a fallback; a plain
//! copy is never made under this backend's name.
//!
//! The same directory clone builds new bases from recent ones
//! ([`DirCloner`], `BaseCache::ensure_from`).

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::Instant;

use reef_lib::error::ErrorChain;
use reef_lib::telemetry::elapsed_ms;

use super::base_cache::DirCloner;
use super::fs_util::{
    FileCopier, cannot_clone, changed_files_vs_base, copy_tree, tear_down_root, trial_clone,
};
use super::{
    Availability, Backend, BackendError, BackendKind, BaseEntry, DiskUsage, SwapOutcome,
    WorkspacePaths,
};

/// `<sys/clonefile.h>`: clone a symlink itself, not its target. `libc` 0.2
/// does not export the `CLONE_*` flags.
const CLONE_NOFOLLOW: u32 = 0x0001;
/// `<sys/clonefile.h>`: do not copy the owner; the clone belongs to the
/// caller, as a copy would.
const CLONE_NOOWNERCOPY: u32 = 0x0002;

#[cfg(test)]
thread_local! {
    /// Makes [`ApfsBackend::materialize`] on this thread skip the directory
    /// call, to test the per-file fallback.
    static FORCE_PER_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// APFS clone materialization: one `clonefile(2)` of the base directory,
/// per-file clones as the fallback.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApfsBackend;

/// `clonefile(2)`: clones `src` (a file, a symlink with
/// [`CLONE_NOFOLLOW`], or a directory hierarchy) to `dst`, which must not
/// exist and whose parent must. Atomic: on failure nothing was created.
/// Errors carry the errno.
pub(crate) fn clonefile(src: &Path, dst: &Path, flags: u32) -> io::Result<()> {
    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    #[allow(
        unsafe_code,
        reason = "FFI; both pointers are NUL-terminated strings that outlive the call"
    )]
    // SAFETY: `src` and `dst` are valid C strings for the whole call, and
    // `clonefile` does not keep them.
    let result = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Clones the directory `from` to `to` (absent; its parent must exist)
/// with one `clonefile` call, without copying ownership.
pub(crate) fn clone_dir(from: &Path, to: &Path) -> io::Result<()> {
    clonefile(from, to, CLONE_NOOWNERCOPY)
}

/// The bytes of the file at `path` that no clone or filesystem snapshot
/// shares, which deleting it would free at once: `getattrlist(2)`'s
/// `ATTR_CMNEXT_PRIVATESIZE` (#95). `None` where the filesystem does not
/// return it (anything but APFS). Symlinks are not followed; a directory
/// reports 0.
pub(crate) fn private_bytes(path: &Path) -> io::Result<Option<u64>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    // A u32 length, the returned `attribute_set_t` (five u32s), then the
    // `off_t`, packed at 4-byte alignment.
    let mut buffer = [0_u8; 64];
    #[allow(
        unsafe_code,
        reason = "FFI; the path is a NUL-terminated string, and the buffer's real length is passed"
    )]
    // SAFETY: `path` is a valid C string and `request` a valid `attrlist`
    // for the whole call; the kernel writes at most `buffer.len()` bytes to
    // `buffer`, and keeps none of the pointers.
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut request).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let u32_at = |at: usize| u32::from_ne_bytes(buffer[at..at + 4].try_into().unwrap_or_default());
    // `attribute_set_t.forkattr` is its fifth word, after the length.
    let returned_fork = u32_at(4 + 4 * 4);
    if returned_fork & libc::ATTR_CMNEXT_PRIVATESIZE == 0 {
        return Ok(None);
    }
    let size = i64::from_ne_bytes(buffer[24..32].try_into().unwrap_or_default());
    Ok(Some(u64::try_from(size).unwrap_or(0)))
}

/// Clones one regular file: the per-file fallback's [`FileCopier`].
/// `reflink_copy::reflink` fails rather than copying, and on macOS is
/// `clonefile` with `CLONE_NOOWNERCOPY`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApfsCloner;

impl FileCopier for ApfsCloner {
    fn copy_file(&self, from: &Path, to: &Path) -> io::Result<()> {
        reflink_copy::reflink(from, to)
    }
}

impl DirCloner for ApfsBackend {
    fn clone_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        clone_dir(from, to)
    }
}

impl ApfsBackend {
    /// The directory call, unless a test forces the fallback.
    fn clone_tree(from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(test)]
        if FORCE_PER_FILE.get() {
            return Err(io::Error::other("forced per-file fallback"));
        }
        clone_dir(from, to)
    }
}

impl Backend for ApfsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ApfsClone
    }

    /// Clones the base into `ws.root` with one `clonefile`, or file by file
    /// if that fails with anything but `EEXIST`, `ENOENT`, `ENOTSUP`, or
    /// `EXDEV`. A failure of the per-file walk leaves a partial tree, which
    /// the daemon tears down (R1.05).
    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError> {
        if std::fs::symlink_metadata(&ws.root).is_ok() {
            return Err(BackendError::RootExists(ws.root.clone()));
        }
        if let Some(parent) = ws.root.parent() {
            std::fs::create_dir_all(parent).map_err(|source| BackendError::Io {
                op: "create directory",
                path: parent.to_owned(),
                source,
            })?;
        }
        let begun = Instant::now();
        let method = match Self::clone_tree(&base.path, &ws.root) {
            Ok(()) => "dir",
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(BackendError::RootExists(ws.root.clone()));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(BackendError::Io {
                    op: "clone",
                    path: base.path.clone(),
                    source,
                });
            }
            Err(error) if cannot_clone(&error) => {
                return Err(BackendError::Unavailable {
                    kind: BackendKind::ApfsClone,
                    reason: format!(
                        "cannot clone {} to {}: {error}",
                        base.path.display(),
                        ws.root.display()
                    ),
                });
            }
            Err(error) => {
                tracing::warn!(
                    path = %ws.root.display(),
                    errno = error.raw_os_error(),
                    error = %ErrorChain(&error),
                    "directory clone failed; cloning file by file"
                );
                copy_tree(&base.path, &ws.root, &ApfsCloner)?;
                "per-file"
            }
        };
        tracing::info!(
            workspace_id = %ws.id,
            backend = "apfs",
            elapsed_ms = elapsed_ms(begun),
            method,
            "workspace materialized"
        );
        Ok(())
    }

    /// The allocated blocks of files whose mtime or size differs from the
    /// base's `tree_state`, and of files the base does not have. An
    /// unmodified clone reports its full size in `st_blocks` although it
    /// shares every block, so the whole tree cannot simply be summed. An
    /// estimate: a file rewritten with identical contents counts.
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
    /// background (4.4 s for the `large` tree).
    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        tear_down_root(&ws.root)
    }

    fn dir_cloner(&self) -> Option<&'static dyn DirCloner> {
        Some(&ApfsBackend)
    }
}

/// Whether workspaces under `target_dir` (which exists) can be cloned from
/// bases under `base_dir`, by a trial `clonefile` of a probe file
/// ([`trial_clone`]). Without a `base_dir`, whether `target_dir`'s volume
/// can clone at all.
pub(crate) fn probe_apfs(base_dir: Option<&Path>, target_dir: &Path) -> Availability {
    match trial_clone(base_dir, target_dir, &|from, to| {
        clonefile(from, to, CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)
    }) {
        Ok(()) => Availability::Available,
        Err(reason) => Availability::Unavailable(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::backend::base_cache::PROBE_FILE;
    use crate::backend::fs_util::{allocated_bytes, footprint, free_bytes, same_volume};
    use crate::model::RepoId;

    /// A tree with files of distinct mtimes, an executable, a relative
    /// symlink, and an empty directory. Returns the total file bytes.
    fn build_tree(root: &Path) -> u64 {
        let mut total = 0;
        for (index, name) in ["a.txt", "dir/b.txt", "dir/deep/c.bin"].iter().enumerate() {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let contents = vec![b'a' + u8::try_from(index).unwrap(); 20_000 * (index + 1)];
            total += contents.len() as u64;
            fs::write(&path, contents).unwrap();
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + index as u64);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
        let tool = root.join("bin/tool");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../a.txt", root.join("dir/link")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        total + 10
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

    // 1
    #[test]
    fn clone_dir_preserves_mtime_mode_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        let bytes = build_tree(&from);
        clone_dir(&from, &to).unwrap();
        assert_same_tree(&from, &to);
        assert_eq!(
            fs::read_link(to.join("dir/link")).unwrap(),
            Path::new("../a.txt")
        );
        assert!(to.join("empty").is_dir());
        // `st_blocks` counts shared blocks as the clone's own, which is
        // why `disk_usage` compares with the base instead of summing.
        assert!(
            allocated_bytes(&to).unwrap() >= bytes,
            "an untouched clone reports its full size"
        );
    }

    // 2
    #[test]
    fn clone_dir_is_copy_on_write() {
        const SIZE: usize = 64 << 20;
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("big"), vec![7_u8; SIZE]).unwrap();
        fs::write(from.join("small"), "original").unwrap();

        // The source's blocks are allocated before the first reading.
        fs::File::open(from.join("big"))
            .unwrap()
            .sync_all()
            .unwrap();

        // Other tests write to the same volume meanwhile, so the least of
        // three tries is the clone's own cost.
        let mut least = u64::MAX;
        for attempt in 0..3 {
            let to = dir.path().join(format!("to-{attempt}"));
            let free_before = free_bytes(dir.path()).unwrap();
            clone_dir(&from, &to).unwrap();
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(to.join("big"))
                .unwrap();
            file.write_all(&[1; 4096]).unwrap();
            file.sync_all().unwrap();
            fs::write(to.join("small"), "changed").unwrap();
            let free_after = free_bytes(dir.path()).unwrap();
            least = least.min(free_before.saturating_sub(free_after));
        }

        assert_eq!(fs::read(from.join("small")).unwrap(), b"original");
        assert_eq!(fs::metadata(from.join("big")).unwrap().len(), SIZE as u64);
        // A copy would take the full 64 MiB.
        assert!(least < (SIZE / 2) as u64, "{least} bytes used by the clone");
    }

    // 3
    #[test]
    fn clone_dir_fails_when_destination_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        build_tree(&from);
        fs::create_dir(&to).unwrap();
        let error = clone_dir(&from, &to).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EEXIST), "{error}");
        assert_eq!(
            fs::read_dir(&to).unwrap().count(),
            0,
            "atomic: nothing added"
        );

        let base = base_over(&from);
        let ws = WorkspacePaths::at(
            "00000000000000aa".parse().unwrap(),
            to,
            dir.path().join("wc"),
        );
        let error = ApfsBackend.materialize(&base, &ws).unwrap_err();
        assert!(matches!(error, BackendError::RootExists(_)), "{error:?}");
    }

    #[test]
    fn probe_on_one_volume_is_available_and_leaves_no_clone() {
        let dir = tempfile::tempdir().unwrap();
        let (bases, target) = (dir.path().join("bases"), dir.path().join("ws"));
        fs::create_dir(&target).unwrap();
        assert_eq!(probe_apfs(Some(&bases), &target), Availability::Available);
        assert_eq!(
            probe_apfs(Some(&bases), &target),
            Availability::Available,
            "again, with the probe file in place"
        );
        assert_eq!(fs::read(bases.join(PROBE_FILE)).unwrap(), b"\n");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert_eq!(probe_apfs(None, &target), Availability::Available);
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "no base cache: nothing left"
        );
    }

    // 4
    #[test]
    #[allow(clippy::print_stderr, reason = "says why the test did nothing")]
    fn probe_reports_other_volume() {
        let Some(other) = std::env::var_os("REEF_TEST_OTHER_VOLUME") else {
            eprintln!("skipped: set REEF_TEST_OTHER_VOLUME to a directory on another volume");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir_in(other).unwrap();
        let availability = probe_apfs(Some(&dir.path().join("bases")), target.path());
        assert!(
            matches!(&availability, Availability::Unavailable(reason) if reason.contains("different volume")),
            "{availability:?}"
        );
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    }

    // 5
    #[test]
    fn materialize_falls_back_to_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("bases/tree");
        build_tree(&tree);
        let base = base_over(&tree);
        let repo = dir.path().join("repo");
        let direct = WorkspacePaths::under(&repo, "00000000000000aa".parse().unwrap());
        let per_file = WorkspacePaths::under(&repo, "00000000000000ab".parse().unwrap());

        ApfsBackend.materialize(&base, &direct).unwrap();
        FORCE_PER_FILE.set(true);
        let result = ApfsBackend.materialize(&base, &per_file);
        FORCE_PER_FILE.set(false);
        result.unwrap();
        assert_same_tree(&tree, &direct.root);
        assert_same_tree(&tree, &per_file.root);

        assert_eq!(
            ApfsBackend.swap_base(&direct, &base, &base).unwrap(),
            SwapOutcome::NeedsCheckout
        );
        for ws in [&direct, &per_file] {
            ApfsBackend.tear_down(ws).unwrap();
            assert!(!ws.root.exists());
            let error = ApfsBackend.tear_down(ws).unwrap_err();
            assert!(matches!(error, BackendError::RootMissing(_)), "{error:?}");
        }
        assert!(
            tree.join("dir/deep/c.bin").is_file(),
            "the base is untouched"
        );
    }

    #[test]
    fn missing_base_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let base = base_over(&dir.path().join("bases/gone"));
        let ws = WorkspacePaths::under(
            &dir.path().join("repo"),
            "00000000000000aa".parse().unwrap(),
        );
        let error = ApfsBackend.materialize(&base, &ws).unwrap_err();
        assert!(
            matches!(error, BackendError::Io { op: "clone", .. }),
            "{error:?}"
        );
        assert!(!ws.root.exists());
    }

    // 7
    /// [`ApfsBackend::materialize`] from the `large` synthetic tree
    /// (`<target>/synth/large` if `just synth large` made it there, else
    /// generated here), three times, with the free-space delta. Run by hand:
    /// `cargo test -p reef-daemon --release timing_large -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the large preset; run by hand to record the numbers"]
    #[allow(clippy::print_stderr, reason = "reports the timings")]
    fn timing_large() {
        use reef_testing::synth::{self, Preset, Spec};

        let dir = tempfile::tempdir().unwrap();
        let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
            PathBuf::from,
        );
        let prebuilt = target.join("synth/large");
        let tree = if prebuilt.join(synth::MANIFEST_FILE).is_file()
            && same_volume(&prebuilt, dir.path()).unwrap()
        {
            prebuilt
        } else {
            let tree = dir.path().join("large");
            synth::generate(&tree, &Spec::preset(Preset::Large), 42, false).unwrap();
            tree
        };
        let base = BaseEntry {
            repo_id: RepoId::generate(),
            tree_id: "00".into(),
            path: tree.clone(),
            state_dir: dir.path().join("unused.state"),
            bytes: 0,
            files: 0,
        };
        let entries = walkdir::WalkDir::new(&tree).into_iter().count();
        for run in 1..=3_u8 {
            let ws = WorkspacePaths::under(
                &dir.path().join("repo"),
                format!("00000000000000a{run}").parse().unwrap(),
            );
            let free_before = free_bytes(dir.path()).unwrap();
            let begun = Instant::now();
            ApfsBackend.materialize(&base, &ws).unwrap();
            let elapsed = begun.elapsed();
            let free_after = free_bytes(dir.path()).unwrap();
            eprintln!(
                "apfs materialize, {} ({entries} entries), run {run}: {elapsed:.2?}, free space -{} MiB",
                tree.display(),
                free_before.saturating_sub(free_after) >> 20
            );
        }
    }

    #[test]
    fn footprint_counts_a_clone_once() {
        const SIZE: u64 = 8 << 20;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::write(a.join("big"), vec![7_u8; 8 << 20]).unwrap();
        fs::File::open(a.join("big")).unwrap().sync_all().unwrap();
        clone_dir(&a, &b).unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(b.join("big"))
            .unwrap();
        file.write_all(&[1]).unwrap();
        file.sync_all().unwrap();

        for side in [&a, &b] {
            let measured = footprint(side).unwrap();
            assert!(measured.allocated_bytes >= SIZE, "{measured:?}");
            let unshared = measured.unshared_bytes.expect("APFS tells");
            assert!(unshared < 1 << 20, "{side:?}: {measured:?}");
        }
        fs::remove_dir_all(&a).unwrap();
        let alone = footprint(&b).unwrap();
        assert!(alone.unshared_bytes.unwrap() >= SIZE, "{alone:?}");
    }

    #[test]
    fn private_bytes_is_none_off_apfs() {
        assert_eq!(private_bytes(Path::new("/dev/null")).unwrap(), None);
    }
}
