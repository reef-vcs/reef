//! The default workspace root (#260, ADR 0072): `~/.reef/<repo-short>`
//! for the platform's default state directory. Tests point the daemon at a
//! temporary "home" the way `Config::load` does for the default state
//! directory, so nothing here touches the real `~/.reef`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, short_tempdir};
use reef_api::v1::{CreateWorkspaceRequest, Workspace};
use reef_daemon::config::{Config, REPO_SHORT_PLACEHOLDER, repo_short};
use reef_daemon::model::RepoId;
use reef_testing::synth::Preset;

/// Makes `config` use `home/.reef/<repo-short>` as `Config::load` does on
/// the default state directory.
fn home_root(config: &mut Config, home: &Path) {
    let base = home.join(".reef");
    base.join(REPO_SHORT_PLACEHOLDER)
        .to_str()
        .unwrap()
        .clone_into(&mut config.file.workspace.root);
    config.workspace_base = Some(base);
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "tester@host".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Waits until `dir` holds no `*.trash-*` entry (a release deletes its
/// directory in the background).
async fn wait_for_no_trash(dir: &Path) {
    let begun = Instant::now();
    loop {
        let trash = std::fs::read_dir(dir).map_or(0, |entries| {
            entries
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".trash-")
                })
                .count()
        });
        if trash == 0 {
            return;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(30),
            "trash in {} was not deleted",
            dir.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// #260 upgrade test: a daemon with workspaces under the old root
/// (`<state>/repos/<repo-id>/ws`) keeps serving and releasing them once
/// the root moves; new ones go under `~/.reef/<repo-short>` (`0700`); the
/// start sweeps still clean the old root; forgetting the repository clears
/// only trash under the new one.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_keeps_serving_workspaces_under_the_old_root() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo(Preset::Small).await;
    let id: RepoId = repo.id.parse().unwrap();
    let legacy = daemon.state_dir().join("repos").join(&repo.id).join("ws");
    let old = create(&daemon, &repo).await;
    let old_path = PathBuf::from(&old.path);
    assert_eq!(
        old_path.parent().unwrap(),
        std::fs::canonicalize(&legacy).unwrap()
    );

    let mut stopped = daemon.stop_keeping().await;
    let home = short_tempdir();
    home_root(&mut stopped.config, home.path());
    // A release's trash and an interrupted create, both under the old root.
    std::fs::create_dir_all(legacy.join("00000000000000aa.trash-1/sub")).unwrap();
    std::fs::write(legacy.join("00000000000000aa.trash-1/sub/file"), b"x").unwrap();
    std::fs::create_dir(legacy.join("00000000000000bb")).unwrap();
    let daemon = stopped.start().await;
    assert!(!legacy.join("00000000000000aa.trash-1").exists());
    assert!(!legacy.join("00000000000000bb").exists());
    assert!(!home.path().join(".reef").exists(), "start creates nothing");

    // The old workspace still serves.
    std::fs::write(old_path.join("after-upgrade.txt"), b"still here\n").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &old.id, "")
        .await
        .unwrap();
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &old.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(inspected.path, old.path);

    // A new one goes under the new root, with private parents.
    let new = create(&daemon, &repo).await;
    let base = std::fs::canonicalize(home.path()).unwrap().join(".reef");
    let short_dir = base.join(repo_short(id));
    assert_eq!(PathBuf::from(&new.path), short_dir.join(&new.id));
    assert!(!new.path.contains(' '), "{}", new.path);
    assert_eq!(mode(&base), 0o700);
    assert_eq!(mode(&short_dir), 0o700);

    for ws in [&old, &new] {
        daemon
            .client
            .release_workspace(&repo.id, &ws.id, true, false)
            .await
            .unwrap();
        assert!(!Path::new(&ws.path).exists(), "{}", ws.path);
    }

    // Forget clears trash under `~/.reef` and leaves anything else.
    wait_for_no_trash(&short_dir).await;
    std::fs::create_dir_all(short_dir.join("00000000000000cc.trash-9/sub")).unwrap();
    std::fs::create_dir(short_dir.join("00000000000000dd")).unwrap();
    daemon.client.forget_repository(&repo.id).await.unwrap();
    assert!(!short_dir.join("00000000000000cc.trash-9").exists());
    assert!(short_dir.join("00000000000000dd").is_dir(), "not recursive");
    daemon.stop().await;
}

/// S-185: `~/.reef` with group or other bits is tightened at start and at
/// a create; a missing one is not created at start.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_base_is_private() {
    let home = short_tempdir();
    let base = home.path().join(".reef");
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (daemon, repo) =
        TestDaemon::with_synthetic_repo_and(Preset::Small, |config| home_root(config, home.path()))
            .await;
    assert_eq!(mode(&base), 0o700);

    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o750)).unwrap();
    let ws = create(&daemon, &repo).await;
    assert_eq!(mode(&base), 0o700);
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    daemon.stop().await;

    let home = short_tempdir();
    let daemon = TestDaemon::start_with(|config| home_root(config, home.path())).await;
    assert!(!home.path().join(".reef").exists());
    daemon.stop().await;
}

/// S-185: a workspace rebuilt after a restore, with `~/.reef` gone, gets
/// private parents as a create does.
#[tokio::test(flavor = "multi_thread")]
async fn rematerialized_workspace_gets_private_parents() {
    let home = short_tempdir();
    let (daemon, repo) =
        TestDaemon::with_synthetic_repo_and(Preset::Small, |config| home_root(config, home.path()))
            .await;
    let ws = create(&daemon, &repo).await;
    let stopped = daemon.stop_keeping().await;
    let base = home.path().join(".reef");
    // On Linux an overlay workspace stays mounted after its daemon stops
    // (R1.09): unmount it first, as a restore onto a new machine finds it.
    #[cfg(target_os = "linux")]
    unmount_under(&base);
    std::fs::remove_dir_all(&base).unwrap();
    let db = reef_daemon::db::Db::open(&stopped.config.paths.db).unwrap();
    db.set_meta(reef_daemon::db::META_REMATERIALIZE_PENDING, "1")
        .unwrap();
    drop(db);
    let daemon = stopped.start().await;
    assert!(Path::new(&ws.path).is_dir(), "rebuilt");
    assert_eq!(mode(&base), 0o700);
    assert_eq!(mode(Path::new(&ws.path).parent().unwrap()), 0o700);
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    daemon.stop().await;
}

/// S-185: a `~/.reef` another uid owns refuses the start, naming
/// `[workspace] root`, as S-01 refuses such a state directory.
#[cfg(feature = "privileged-tests")]
#[tokio::test(flavor = "multi_thread")]
async fn workspace_base_owned_by_another_user_refuses_start() {
    use reef_daemon::config::Paths;
    use reef_daemon::{Daemon, StartError};

    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: needs root to chown");
        return;
    }
    let tmp = short_tempdir();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".reef")).unwrap();
    std::os::unix::fs::chown(home.join(".reef"), Some(1), None).unwrap();
    let mut config = Config::with_defaults(Paths::in_state_dir(&tmp.path().join("state")).unwrap());
    home_root(&mut config, &home);
    let error = Daemon::start(config).await.unwrap_err();
    assert!(
        matches!(&error, StartError::NotOwned { uid: 1, .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("[workspace] root"), "{message}");
    assert!(message.contains(".reef"), "{message}");
}

/// Unmounts every mount at or under `path`, deepest first (as
/// `tests/backup.rs` does).
#[cfg(target_os = "linux")]
fn unmount_under(path: &Path) {
    let Ok(path) = std::fs::canonicalize(path) else {
        return;
    };
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    let mut under: Vec<(String, String)> = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (point, fstype) = (fields.nth(1)?, fields.next()?);
            Path::new(point)
                .starts_with(&path)
                .then(|| (point.to_owned(), fstype.to_owned()))
        })
        .collect();
    under.sort_by_key(|(point, _)| std::cmp::Reverse(point.len()));
    for (point, fstype) in under {
        let (program, args) = if fstype.starts_with("fuse") {
            ("fusermount3", vec!["-uz", point.as_str()])
        } else {
            ("umount", vec!["-l", point.as_str()])
        };
        let output = std::process::Command::new(program)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
