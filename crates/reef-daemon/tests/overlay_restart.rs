//! Overlay workspaces across a daemon restart (R1.09), end to end: with the
//! daemon stopped, one workspace loses its mount (as after a reboot),
//! another its upper directory, and a mount appears that no workspace owns.
//! The restarted daemon mounts the first again with its edits, marks the
//! second `upper-missing` and refuses to snapshot it, and removes the
//! third.
//!
//! The FUSE test runs wherever `fuse-overlayfs` is installed and prints
//! `skipped: ...` elsewhere; the kernel test needs
//! `--features privileged-tests`, run as root (CI's `overlayfs` job).

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; skips are reported on stderr"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{SyntheticRepo, TestDaemon};
use reef_daemon::backend::{Availability, BackendChoice, BackendKind, OverlayConfig, probe};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::daemon::Daemon;
use reef_daemon::proto::v0::{CreateRequest, InspectResponse, Workspace};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::synth::Preset;

/// Whether `kind` can mount under a fresh temporary directory; prints why
/// not.
fn available(kind: BackendKind) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let probes = probe(dir.path(), None, &OverlayConfig::default());
    match probes
        .into_iter()
        .find(|probe| probe.kind == kind)
        .map(|probe| probe.availability)
    {
        Some(Availability::Available) => true,
        Some(Availability::Unavailable(reason)) => {
            eprintln!("skipped: {kind}: {reason}");
            false
        }
        None => panic!("{kind} was not probed"),
    }
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: "tester@host".to_owned(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn inspect(client: &Client, ws: &Workspace) -> InspectResponse {
    client.inspect_workspace("", &ws.id, false).await.unwrap()
}

/// Whether `path` is a mountpoint, by `/proc/self/mounts`.
fn is_mounted(path: &Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    let path = path.to_str().unwrap();
    mounts
        .lines()
        .any(|line| line.split(' ').nth(1) == Some(path))
}

/// Runs a command and asserts it succeeded.
fn run(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Unmounts `path` the way an administrator (or a reboot) would.
fn unmount(kind: BackendKind, path: &Path) {
    let path = path.to_str().unwrap();
    match kind {
        BackendKind::Overlayfs => run("umount", &[path]),
        _ => run("fusermount3", &["-u", path]),
    }
}

/// Mounts an overlay of `lower` at `<dir>/<name>` with layers beside it,
/// by hand: a mount that belongs to no workspace.
fn mount_by_hand(kind: BackendKind, lower: &Path, dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    let [upper, work] = [".upper", ".work"].map(|suffix| dir.join(format!("{name}{suffix}")));
    for path in [&root, &upper, &work] {
        std::fs::create_dir_all(path).unwrap();
    }
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let target = root.to_str().unwrap();
    match kind {
        BackendKind::Overlayfs => run(
            "mount",
            &["-t", "overlay", "overlay", "-o", &options, target],
        ),
        _ => run("fuse-overlayfs", &["-o", &options, target]),
    }
    root
}

async fn survives_restart(kind: BackendKind) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.workspace.backend = BackendChoice::Named(kind);
    })
    .await;
    let [kept, lost] = [create(&daemon, &repo).await, create(&daemon, &repo).await];
    for ws in [&kept, &lost] {
        assert_eq!(ws.backend, kind.as_str());
        assert!(is_mounted(Path::new(&ws.path)));
        std::fs::write(Path::new(&ws.path).join("edit.txt"), "kept\n").unwrap();
    }
    let ws_dir = Path::new(&kept.path).parent().unwrap().to_owned();

    let TestDaemon {
        handle,
        client,
        tmp,
        config,
    } = daemon;
    drop(client);
    handle.shutdown();
    handle.join().await.unwrap();
    // Stopping the daemon leaves mounts alone.
    assert!(is_mounted(Path::new(&kept.path)));
    for ws in [&kept, &lost] {
        unmount(kind, Path::new(&ws.path));
    }
    std::fs::remove_dir_all(format!("{}.upper", lost.path)).unwrap();
    let stale = mount_by_hand(kind, &repo.root, &ws_dir, "0000000000000000");

    let handle = Daemon::start(config.clone()).await.unwrap();
    let client = Client::connect(handle.socket_path()).await.unwrap();

    let root = Path::new(&kept.path);
    assert!(is_mounted(root), "mounted again");
    assert_eq!(
        std::fs::read_to_string(root.join("edit.txt")).unwrap(),
        "kept\n"
    );
    let inspected = inspect(&client, &kept).await;
    assert_eq!(inspected.workspace.unwrap().attention, "");
    client.snapshot_workspace("", &kept.id, "").await.unwrap();

    assert!(!is_mounted(Path::new(&lost.path)));
    let inspected = inspect(&client, &lost).await.workspace.unwrap();
    assert_eq!(inspected.attention, "upper-missing");
    assert_eq!(inspected.state, "active");
    let error: ClientError = client
        .snapshot_workspace("", &lost.id, "")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error:?}");

    assert!(!is_mounted(&stale));
    for suffix in ["", ".upper", ".work"] {
        let path = format!("{}{suffix}", stale.display());
        assert!(!Path::new(&path).exists(), "{path} is left");
    }

    client
        .release_workspace("", &kept.id, true, false)
        .await
        .unwrap();
    assert!(!root.exists() && !is_mounted(root));
    // Without its upper, a final snapshot fails, and a forced release
    // skips it.
    client
        .release_workspace("", &lost.id, false, true)
        .await
        .unwrap();
    assert!(!Path::new(&lost.path).exists());
    handle.shutdown();
    handle.join().await.unwrap();
    drop(tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn fuse_overlayfs_workspaces_survive_a_restart() {
    if available(BackendKind::FuseOverlayfs) {
        survives_restart(BackendKind::FuseOverlayfs).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn kernel_overlayfs_workspaces_survive_a_restart() {
    if !cfg!(feature = "privileged-tests") {
        eprintln!("skipped: overlayfs: needs --features privileged-tests, run as root");
        return;
    }
    if available(BackendKind::Overlayfs) {
        survives_restart(BackendKind::Overlayfs).await;
    }
}
