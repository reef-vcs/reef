//! The overlay backends' own tests (R1.09). R1.11's conformance suite runs
//! against both kinds too (`tests/backend_conformance.rs`).
//!
//! The FUSE tests run wherever `fuse-overlayfs` is on `PATH` and `/dev/fuse`
//! opens, and print `skipped: ...` elsewhere. The kernel tests need
//! `--features privileged-tests` and `CAP_SYS_ADMIN` (CI runs them as
//! root). Every fixture lives in the temporary directory, which must not
//! be an overlay itself: overlayfs refuses an overlay as the upper layer.

#![allow(
    clippy::print_stderr,
    reason = "skips and timings are reported on stderr"
)]

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::model::RepoId;

/// Three files and a directory, as a base entry with an empty `tree_state`
/// beside it.
fn base_in(dir: &Path) -> BaseEntry {
    let tree = dir.join("bases/tree");
    for (name, contents) in [
        ("a.txt", "a\n"),
        ("dir/b.txt", "b\n"),
        ("dir/deep/c.txt", "c\n"),
    ] {
        let path = tree.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let state_dir = dir.join("bases/tree.state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("tree_state"), b"").unwrap();
    BaseEntry {
        repo_id: RepoId::generate(),
        tree_id: "00".into(),
        path: tree,
        state_dir,
        bytes: 0,
        files: 0,
    }
}

fn workspace(dir: &Path, id: &str) -> WorkspacePaths {
    WorkspacePaths::under(&dir.join("repo"), id.parse().unwrap())
}

/// The FUSE backend, if `fuse-overlayfs` is on `PATH` and `/dev/fuse`
/// opens; prints why not otherwise. A broken installation still fails the
/// test that uses it.
fn fuse_backend() -> Option<OverlayBackend> {
    let config = OverlayConfig::default();
    if find_program(&config.fuse_overlayfs).is_none() {
        eprintln!("skipped: fuse-overlayfs is not on PATH");
        return None;
    }
    if let Err(error) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
    {
        eprintln!("skipped: /dev/fuse: {error}");
        return None;
    }
    Some(OverlayBackend::fuse(&config))
}

/// The kernel backend, if this process has `CAP_SYS_ADMIN`.
#[cfg(feature = "privileged-tests")]
fn kernel_backend() -> Option<OverlayBackend> {
    if has_cap_sys_admin().unwrap() {
        Some(OverlayBackend::kernel(&OverlayConfig::default()))
    } else {
        eprintln!("skipped: needs CAP_SYS_ADMIN (run as root)");
        None
    }
}

fn is_mounted(path: &Path) -> bool {
    mount_info(path).unwrap().is_some()
}

/// Whether `path` in an upper directory is a whiteout: the kernel's
/// character device 0/0, or `fuse-overlayfs`'s `.wh.<name>` file when it
/// cannot make one.
fn is_whiteout(upper: &Path, name: &str) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    let device = fs::symlink_metadata(upper.join(name))
        .is_ok_and(|metadata| metadata.file_type().is_char_device() && metadata.rdev() == 0);
    device || upper.join(format!(".wh.{name}")).exists()
}

/// Mounts, writes through the merged view, deletes a base file, measures,
/// and tears down, asserting each step's effect on the layers.
fn mounts_and_unmounts(backend: &OverlayBackend) {
    let dir = tempfile::tempdir().unwrap();
    let base = base_in(dir.path());
    let ws = workspace(dir.path(), "00000000000000aa");

    backend.materialize(&base, &ws).unwrap();
    assert!(is_mounted(&ws.root));
    backend.check_materialized(&ws).unwrap();
    assert_eq!(
        fs::read_to_string(ws.root.join("dir/b.txt")).unwrap(),
        "b\n"
    );
    assert_eq!(
        backend.disk_usage(&base, &ws).unwrap(),
        DiskUsage::default(),
        "a fresh workspace uses nothing"
    );

    fs::write(ws.root.join("new.txt"), "written\n").unwrap();
    assert_eq!(
        fs::read_to_string(ws.upper.join("new.txt")).unwrap(),
        "written\n"
    );
    assert!(
        !base.path.join("new.txt").exists(),
        "nothing reaches the lower"
    );
    fs::remove_file(ws.root.join("a.txt")).unwrap();
    assert!(base.path.join("a.txt").is_file());
    assert!(
        is_whiteout(&ws.upper, "a.txt"),
        "deleting a lower file whites it out"
    );
    let usage = backend.disk_usage(&base, &ws).unwrap();
    assert_eq!(
        usage.files_changed, 2,
        "the new file and the whiteout: {usage:?}"
    );
    assert!(usage.allocated_bytes > 0, "{usage:?}");

    backend.tear_down(&ws).unwrap();
    assert!(!is_mounted(&ws.root));
    assert!(fs::symlink_metadata(&ws.root).is_err(), "the root is gone");
    assert!(
        !ws.upper.exists() && !ws.work.exists(),
        "deleted inline without a runtime"
    );
    let error = backend.check_materialized(&ws).unwrap_err();
    assert!(
        matches!(error, BackendError::Unavailable { .. }),
        "{error:?}"
    );
    let error = backend.tear_down(&ws).unwrap_err();
    assert!(matches!(error, BackendError::RootMissing(_)), "{error:?}");
    assert_eq!(fs::read_to_string(base.path.join("a.txt")).unwrap(), "a\n");
}

// 1
/// Not the same thing in general (a root without the capability, a user
/// with it), but the same on CI and in plain containers.
#[test]
fn has_cap_sys_admin_matches_id() {
    assert_eq!(
        has_cap_sys_admin().unwrap(),
        nix::unistd::geteuid().is_root()
    );
}

// 2
#[test]
fn mount_info_parses_proc_mounts() {
    let text = b"/dev/sda1 / ext4 rw,relatime 0 0\n\
        overlay /state/repos/r/ws/with\\040space overlay rw,lowerdir=/b\\134x,upperdir=/u,workdir=/w 0 0\n\
        fuse-overlayfs /tab\\011new\\012line fuse.fuse-overlayfs rw,nosuid,user_id=1000 0 0\n\
        short line\n";
    let mounts = parse_mounts(text);
    assert_eq!(mounts.len(), 3, "{mounts:?}");
    assert_eq!(
        mounts[1],
        MountInfo {
            source: "overlay".into(),
            mountpoint: "/state/repos/r/ws/with space".into(),
            fstype: KERNEL_FSTYPE.into(),
            options: "rw,lowerdir=/b\\x,upperdir=/u,workdir=/w".into(),
        }
    );
    assert_eq!(mounts[2].mountpoint, Path::new("/tab\tnew\nline"));
    assert_eq!(mounts[2].fstype, FUSE_FSTYPE);
    assert_eq!(unescape(b"\\999 \\04"), b"\\999 \\04", "not escapes");

    assert!(mount_info(Path::new("/")).unwrap().is_some());
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(mount_info(dir.path()).unwrap(), None);
    assert_eq!(mount_info(&dir.path().join("no/such")).unwrap(), None);
}

// 3
#[test]
fn kernel_options_escape_separators() {
    let options = layer_options(Path::new("/b:a,s\\e"), Path::new("/u"), Path::new("/w,1"));
    assert_eq!(
        String::from_utf8(options).unwrap(),
        "lowerdir=/b\\:a\\,s\\\\e,upperdir=/u,workdir=/w\\,1"
    );
}

#[test]
fn probes_name_their_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let named = |binary: &str| OverlayConfig {
        fuse_overlayfs: binary.to_owned(),
        nosuid: false,
    };
    assert_eq!(
        probe_fuse(dir.path(), &named("reef-no-such-fuse-overlayfs")),
        Availability::Unavailable("reef-no-such-fuse-overlayfs not on PATH".into())
    );
    assert_eq!(
        probe_fuse(dir.path(), &named("/no/such/fuse-overlayfs")),
        Availability::Unavailable("/no/such/fuse-overlayfs is not an executable file".into())
    );
    if !has_cap_sys_admin().unwrap() {
        assert_eq!(
            probe_kernel(dir.path(), &OverlayConfig::default()),
            Availability::Unavailable("no CAP_SYS_ADMIN".into())
        );
    }
    // Whatever the outcome, the trial leaves nothing behind.
    probe_fuse(dir.path(), &OverlayConfig::default());
    probe_kernel(dir.path(), &OverlayConfig::default());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

// 4
#[test]
fn fuse_overlayfs_mounts_and_unmounts() {
    if let Some(backend) = fuse_backend() {
        assert_eq!(backend.kind(), BackendKind::FuseOverlayfs);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_fuse(dir.path(), &OverlayConfig::default()),
            Availability::Available
        );
        mounts_and_unmounts(&backend);
    }
}

/// A `fuse-overlayfs` server that died leaves a mount whose root answers
/// `ENOTCONN`; reconciling mounts it again over the same upper directory.
#[test]
fn fuse_overlayfs_reconcile_replaces_a_dead_server() {
    let Some(backend) = fuse_backend() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let base = base_in(dir.path());
    let ws = workspace(dir.path(), "00000000000000ab");
    backend.materialize(&base, &ws).unwrap();
    fs::write(ws.root.join("edit.txt"), "kept\n").unwrap();

    let server = fuse_server_of(&ws.root).expect("the fuse-overlayfs process");
    nix::sys::signal::kill(server, nix::sys::signal::Signal::SIGKILL).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_disconnected(&ws.root) {
        assert!(Instant::now() < deadline, "the mount never disconnected");
        std::thread::sleep(Duration::from_millis(20));
    }

    let report = reconcile_mounts(
        &[ws.root.parent().unwrap().to_owned()],
        &[OverlayWorkspace {
            paths: ws.clone(),
            kind: BackendKind::FuseOverlayfs,
            lower: Some(base.path.clone()),
        }],
        &OverlayConfig::default(),
    )
    .unwrap();
    assert_eq!(report.remounted, [ws.id], "{report:?}");
    assert_eq!(
        fs::read_to_string(ws.root.join("edit.txt")).unwrap(),
        "kept\n"
    );
    assert_eq!(fs::read_to_string(ws.root.join("a.txt")).unwrap(), "a\n");
    backend.tear_down(&ws).unwrap();
}

/// The pid of the `fuse-overlayfs` process serving `root`, found by its
/// command line.
fn fuse_server_of(root: &Path) -> Option<nix::unistd::Pid> {
    let root = root.as_os_str().as_bytes();
    fs::read_dir("/proc").ok()?.find_map(|entry| {
        let entry = entry.ok()?;
        let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
        let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
        let mut args = cmdline.split(|&byte| byte == 0);
        let program = args.next()?;
        (program.ends_with(b"fuse-overlayfs") && args.any(|arg| arg == root))
            .then(|| nix::unistd::Pid::from_raw(pid))
    })
}

// 5
#[test]
#[cfg(feature = "privileged-tests")]
fn kernel_overlayfs_mounts_and_unmounts() {
    let Some(backend) = kernel_backend() else {
        return;
    };
    assert_eq!(backend.kind(), BackendKind::Overlayfs);
    mounts_and_unmounts(&backend);

    // redirect_dir=on: a directory from the lower layer can be renamed.
    let dir = tempfile::tempdir().unwrap();
    let base = base_in(dir.path());
    let ws = workspace(dir.path(), "00000000000000ac");
    backend.materialize(&base, &ws).unwrap();
    fs::rename(ws.root.join("dir"), ws.root.join("moved")).unwrap();
    assert_eq!(
        fs::read_to_string(ws.root.join("moved/deep/c.txt")).unwrap(),
        "c\n"
    );
    assert!(!ws.root.join("dir").exists());
    assert!(
        base.path.join("dir/deep/c.txt").is_file(),
        "the lower is untouched"
    );
    backend.tear_down(&ws).unwrap();
}

// 6
#[test]
#[cfg(feature = "privileged-tests")]
fn tear_down_with_open_file_uses_lazy_unmount() {
    let Some(backend) = kernel_backend() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let base = base_in(dir.path());
    let ws = workspace(dir.path(), "00000000000000ad");
    backend.materialize(&base, &ws).unwrap();
    let mut open = fs::File::open(ws.root.join("dir/b.txt")).unwrap();
    assert!(matches!(
        backend.mounter().unmount(&ws.root, false),
        Err(MountError::Busy { .. })
    ));

    backend.tear_down(&ws).unwrap();
    assert!(!is_mounted(&ws.root), "gone from the namespace");
    assert!(fs::symlink_metadata(&ws.root).is_err(), "the root is gone");
    let mut contents = String::new();
    io::Read::read_to_string(&mut open, &mut contents).unwrap();
    assert_eq!(contents, "b\n", "the open file still reads");
}

// 7
#[test]
#[cfg(feature = "privileged-tests")]
fn reconcile_remounts_missing_and_cleans_stale() {
    let Some(backend) = kernel_backend() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let base = base_in(dir.path());
    let [live, stale, no_upper, no_base] = [
        "00000000000000b1",
        "00000000000000b2",
        "00000000000000b3",
        "00000000000000b4",
    ]
    .map(|id| workspace(dir.path(), id));
    for ws in [&live, &stale, &no_upper, &no_base] {
        backend.materialize(&base, ws).unwrap();
        fs::write(ws.root.join("edit.txt"), "kept\n").unwrap();
    }
    // `live` lost its mount (a reboot); `stale` lost its row; `no_upper`
    // lost its mount and upper; `no_base` lost its base entry but is still
    // mounted.
    for ws in [&live, &no_upper] {
        backend.mounter().unmount(&ws.root, false).unwrap();
    }
    fs::remove_dir_all(&no_upper.upper).unwrap();
    // A probe a crash left mounted.
    let probe = live.root.parent().unwrap().join(".reef-probe-1-1");
    for name in ["lower", "upper", "work", "merged"] {
        fs::create_dir_all(probe.join(name)).unwrap();
    }
    backend
        .mounter()
        .mount(
            &probe.join("lower"),
            &probe.join("upper"),
            &probe.join("work"),
            &probe.join("merged"),
        )
        .unwrap();

    // As `/proc/self/mounts` names them, while they exist.
    let canon = |path: &Path| canonical(path).unwrap().unwrap();
    let mut want_cleaned = [canon(&probe.join("merged")), canon(&stale.root)];
    want_cleaned.sort();

    let overlay = |ws: &WorkspacePaths, lower: Option<PathBuf>| OverlayWorkspace {
        paths: ws.clone(),
        kind: BackendKind::Overlayfs,
        lower,
    };
    let report = reconcile_mounts(
        &[live.root.parent().unwrap().to_owned()],
        &[
            overlay(&live, Some(base.path.clone())),
            overlay(&no_upper, Some(base.path.clone())),
            overlay(&no_base, None),
        ],
        &OverlayConfig::default(),
    )
    .unwrap();
    assert_eq!(report.mounted, []);
    assert_eq!(report.remounted, [live.id]);
    assert_eq!(report.upper_missing, [no_upper.id]);
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, no_base.id);
    let mut cleaned = report.cleaned.clone();
    cleaned.sort();
    assert_eq!(cleaned, want_cleaned);

    assert!(is_mounted(&live.root));
    assert_eq!(
        fs::read_to_string(live.root.join("edit.txt")).unwrap(),
        "kept\n"
    );
    assert!(!is_mounted(&stale.root));
    for path in [&stale.root, &stale.upper, &stale.work, &probe] {
        assert!(!path.exists(), "{} is left", path.display());
    }
    for ws in [&no_upper, &no_base] {
        assert!(!is_mounted(&ws.root));
        assert!(backend.check_materialized(ws).is_err());
    }

    let again = reconcile_mounts(
        &[live.root.parent().unwrap().to_owned()],
        &[overlay(&live, Some(base.path.clone()))],
        &OverlayConfig::default(),
    )
    .unwrap();
    assert_eq!(again.mounted, [live.id]);
    assert_eq!(again.remounted, []);
    for ws in [&live, &no_upper, &no_base] {
        backend.tear_down(ws).unwrap();
    }
}

/// The acceptance number: materializing the `large` synthetic tree (100k
/// files) on the kernel tier is one mount, under 50 ms.
#[test]
#[cfg(feature = "privileged-tests")]
fn kernel_overlayfs_materialize_large_is_a_mount() {
    use reef_testing::synth::{self, Preset, Spec};

    let Some(backend) = kernel_backend() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut base = base_in(dir.path());
    base.path = dir.path().join("bases/large");
    let manifest = synth::generate(&base.path, &Spec::preset(Preset::Large), 42, false).unwrap();
    let mut times = Vec::new();
    for run in 1..=3_u8 {
        let ws = workspace(dir.path(), &format!("00000000000000c{run}"));
        let begun = Instant::now();
        backend.materialize(&base, &ws).unwrap();
        times.push(begun.elapsed());
        assert!(ws.root.join(&manifest.text_files[0]).is_file());
        backend.tear_down(&ws).unwrap();
    }
    eprintln!(
        "kernel overlayfs materialize, large preset ({} files): {times:.2?}",
        manifest.files
    );
    let least = times.iter().min().unwrap();
    assert!(*least < Duration::from_millis(50), "{times:?}");
}
