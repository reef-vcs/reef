//! A full disk under the daemon (R1.23): `ENOSPC` on the filesystem that
//! holds the state directory and the repository, a 256 MB sparse APFS image
//! on macOS and a 256 MB tmpfs on Linux. The daemon stays up, its database
//! stays sound, snapshots fail and are logged at `error`, and once space is
//! freed the next snapshot takes the edit made while the disk was full.
//!
//! On Linux mounting the tmpfs needs root (CI's privileged job); elsewhere
//! the tests print `skipped: <reason>` and pass.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; skips are reported on stderr"
)]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::short_tempdir;
use common::spawned::{SpawnedDaemon, file_at};
use reef_daemon::config::FileConfig;
use reef_daemon::model::{ReclaimReason, WorkspaceId};
use reef_daemon::proto::v0::{CreateRequest, Workspace};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use serde_json::Value;
use tempfile::TempDir;

const OWNER: &str = "tester@host";
const FALLBACK: Duration = Duration::from_secs(5);
const BOUND: Duration = Duration::from_secs(30);
/// Freed after the disk is full, for the one file the test writes then.
const RESERVE: usize = 256 << 10;

fn configure(config: &mut FileConfig) {
    let lease = &mut config.lease;
    lease.default = Duration::from_secs(3);
    lease.grace = Duration::from_secs(1);
    lease.heartbeat = Duration::from_secs(1);
    lease.tick = Duration::from_millis(500);
    config.watcher.debounce = Duration::from_millis(200);
    config.watcher.fallback_interval = FALLBACK;
}

/// A 256 MB filesystem mounted at a short temporary path; unmounted on
/// drop.
struct SmallVolume {
    mountpoint: PathBuf,
    _tmp: TempDir,
}

impl SmallVolume {
    /// Mounts the volume, or says why it cannot be.
    fn mount() -> Result<SmallVolume, String> {
        let tmp = short_tempdir();
        let mountpoint = tmp.path().join("vol");
        std::fs::create_dir(&mountpoint).unwrap();
        if cfg!(target_os = "macos") {
            let image = tmp.path().join("vol.sparseimage");
            run(Command::new("hdiutil")
                .args([
                    "create",
                    "-quiet",
                    "-size",
                    "256m",
                    "-fs",
                    "APFS",
                    "-type",
                    "SPARSE",
                    "-volname",
                    "reef-full",
                ])
                .arg(&image))?;
            run(Command::new("hdiutil")
                .args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"])
                .arg(&mountpoint)
                .arg(&image))?;
        } else {
            if !nix::unistd::geteuid().is_root() {
                return Err("mounting a tmpfs needs root".to_owned());
            }
            run(Command::new("mount")
                .args(["-t", "tmpfs", "-o", "size=256m", "tmpfs"])
                .arg(&mountpoint))?;
        }
        Ok(SmallVolume {
            mountpoint: std::fs::canonicalize(&mountpoint).unwrap(),
            _tmp: tmp,
        })
    }

    fn path(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for SmallVolume {
    fn drop(&mut self) {
        if cfg!(target_os = "macos") {
            let detach = |force: bool| {
                let mut command = Command::new("hdiutil");
                command.args(["detach", "-quiet"]);
                if force {
                    command.arg("-force");
                }
                run(command.arg(&self.mountpoint))
            };
            if detach(false).is_err() {
                let _ = detach(true);
            }
        } else {
            #[cfg(target_os = "linux")]
            common::spawned::unmount_under(&self.mountpoint);
        }
    }
}

fn run(command: &mut Command) -> Result<(), String> {
    match command.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "{command:?}: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Err(format!("{command:?}: {error}")),
    }
}

/// Writes `dir/name` until the filesystem refuses even 4 KB more.
fn fill(dir: &Path, name: &str) {
    let mut file = std::fs::File::create(dir.join(name)).unwrap();
    for size in [1 << 20, 64 << 10, 4 << 10] {
        let chunk = vec![0xa5_u8; size];
        loop {
            match file.write_all(&chunk) {
                Ok(()) => {}
                Err(error) if is_enospc(&error) => break,
                Err(error) => panic!("fill {name}: {error}"),
            }
        }
    }
    let _ = file.sync_all();
}

fn is_enospc(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOSPC) || error.kind() == std::io::ErrorKind::StorageFull
}

/// Fills `volume`, leaving room for exactly one small write: `write` runs
/// in the reserve freed for it, then the reserve's remainder is filled too.
fn fill_around(volume: &Path, write: impl FnOnce()) {
    std::fs::write(volume.join("reserve"), vec![0x5a_u8; RESERVE]).unwrap();
    fill(volume, "filler-0");
    std::fs::remove_file(volume.join("reserve")).unwrap();
    write();
    fill(volume, "filler-1");
    // The disk is full: a 1 MB write fails.
    let error = std::fs::write(volume.join("probe"), vec![0_u8; 1 << 20]).unwrap_err();
    assert!(is_enospc(&error), "{error}");
    let _ = std::fs::remove_file(volume.join("probe"));
}

fn free(volume: &Path) {
    for name in ["filler-0", "filler-1"] {
        std::fs::remove_file(volume.join(name)).unwrap();
    }
}

async fn create(daemon: &SpawnedDaemon, repo: &str, lease_ms: u64) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            lease_ms,
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn inspect(daemon: &SpawnedDaemon, id: &str) -> Workspace {
    daemon
        .client
        .inspect_workspace("", id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Polls `check` every 100 ms until it holds, failing after [`BOUND`].
async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let begun = Instant::now();
    while !check().await {
        assert!(begun.elapsed() < BOUND, "{what}: not within {BOUND:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `error`-level events about a failed snapshot whose error chain says
/// the disk is full.
fn enospc_errors(daemon: &SpawnedDaemon) -> Vec<Value> {
    daemon
        .stderr_lines("snapshot failed")
        .into_iter()
        .filter(|event| {
            event["level"] == "ERROR"
                && event["error"]
                    .as_str()
                    .is_some_and(|chain| chain.contains("No space left on device"))
        })
        .collect()
}

/// The database passes `PRAGMA integrity_check`, and the repository opens
/// and reads its operation log.
async fn check_sound(daemon: &SpawnedDaemon, root: &Path) {
    let conn = rusqlite::Connection::open(daemon.state_dir.join("reef.sqlite3")).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let repo = Repo::open(&root, &common::test_settings()).await.unwrap();
            repo.op_log(10).await.unwrap();
        });
    })
    .await
    .unwrap();
}

macro_rules! volume_or_skip {
    () => {
        match SmallVolume::mount() {
            Ok(volume) => volume,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        }
    };
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn enospc_does_not_corrupt_or_crash() {
    let volume = volume_or_skip!();
    let mut daemon = SpawnedDaemon::spawn_in(volume.path(), configure).await;
    let repo = daemon.register_synth(Preset::Small).await;
    let ws = create(&daemon, &repo.id, 120_000).await;
    let path = PathBuf::from(&ws.path);

    fill_around(volume.path(), || {
        std::fs::write(path.join("while-full.txt"), "written while full\n").unwrap();
    });
    wait_for("an ENOSPC snapshot failure logged", || {
        let found = !enospc_errors(&daemon).is_empty();
        async move { found }
    })
    .await;
    daemon.client.status().await.unwrap();
    assert!(daemon.is_running());
    assert_eq!(inspect(&daemon, &ws.id).await.state, "active");

    free(volume.path());
    wait_for("the edit snapshotted once space is free", || async {
        let commit = inspect(&daemon, &ws.id).await.commit_id;
        file_at(&repo.root, &commit, "while-full.txt")
            .await
            .as_deref()
            == Some(&b"written while full\n"[..])
    })
    .await;
    check_sound(&daemon, &repo.root).await;
    daemon.kill();
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn enospc_during_final_snapshot_keeps_directory() {
    let volume = volume_or_skip!();
    let mut daemon = SpawnedDaemon::spawn_in(volume.path(), configure).await;
    let repo = daemon.register_synth(Preset::Small).await;
    let ws = create(&daemon, &repo.id, 8_000).await;
    let path = PathBuf::from(&ws.path);
    // Heartbeat while the disk fills, which can outlast the lease on a slow
    // runner; the lease then expires on a full disk. A heartbeat that fails
    // once the disk is full only stops extending it.
    let filling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let beating = tokio::spawn({
        let (client, id, lease) = (
            daemon.client.clone(),
            ws.id.clone(),
            ws.lease.clone().unwrap().id,
        );
        let filling = std::sync::Arc::clone(&filling);
        async move {
            while filling.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = client.heartbeat_workspace("", &id, &lease).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    let (volume_path, written) = (volume.path().to_owned(), path.clone());
    tokio::task::spawn_blocking(move || {
        fill_around(&volume_path, || {
            std::fs::write(written.join("while-full.txt"), "written while full\n").unwrap();
        });
    })
    .await
    .unwrap();
    filling.store(false, std::sync::atomic::Ordering::Relaxed);
    beating.await.unwrap();

    // The reclaim is refused one of two ways: the final snapshot fails
    // (`final-snapshot-failed`), or, as on a fresh daemon whose SQLite log
    // must grow for any write, marking the workspace `releasing` does, and
    // there is no attention to record; the lease loop logs that (at `error`
    // on the first failed tick and every tenth) and retries every tick.
    // The reclaim once space is free shows the retry.
    wait_for("the reclaim refused", || async {
        inspect(&daemon, &ws.id).await.attention == "final-snapshot-failed"
            || !daemon
                .stderr_lines("cannot reclaim the workspace")
                .is_empty()
    })
    .await;
    let kept = inspect(&daemon, &ws.id).await;
    assert_eq!(kept.state, "active");
    assert!(path.is_dir());
    let id: WorkspaceId = ws.id.parse().unwrap();
    let reclaimed = || {
        daemon
            .db()
            .reclaims(None, 100)
            .unwrap()
            .into_iter()
            .filter(|row| row.workspace_id == id)
            .collect::<Vec<_>>()
    };
    assert_eq!(reclaimed(), []);

    free(volume.path());
    wait_for("reclaimed once space is free", || async {
        inspect(&daemon, &ws.id).await.state == "reclaimed"
    })
    .await;
    let rows = reclaimed();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].reason, ReclaimReason::Expired);
    assert!(rows[0].snapshot_taken);
    assert!(!path.exists());
    let commit = inspect(&daemon, &ws.id).await.commit_id;
    assert_eq!(
        file_at(&repo.root, &commit, "while-full.txt")
            .await
            .as_deref(),
        Some(&b"written while full\n"[..])
    );
    check_sound(&daemon, &repo.root).await;
    daemon.kill();
}
