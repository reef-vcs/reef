//! The daemon skeleton (R1.03): the `Status` RPC over the Unix socket, the
//! single-instance lock, socket and pid file handling, the log file, the
//! binary's signal handling, and the repository threads.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use common::{
    TestDaemon, daemon_command, init_synthetic_repo, log_files, run_binary, short_tempdir,
    terminate, test_settings, wait_for_socket,
};
use futures::FutureExt as _;
use reef_daemon::client::Client;
use reef_daemon::config::{Config, ConfigError, PathOverrides, Paths, SOCKET_PATH_MAX};
use reef_daemon::db::{Db, SCHEMA_VERSION};
use reef_daemon::lock::LockError;
use reef_daemon::model::{RepoId, RepositoryRecord, Timestamp};
use reef_daemon::repo::{RepoRegistry, RepoThreadError};
use reef_daemon::{Daemon, StartError};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use serde_json::Value;

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
async fn status_answers_over_socket() {
    let daemon = TestDaemon::start().await;
    let first = daemon.client.status().await.unwrap();
    assert_eq!(first.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(first.pid, std::process::id());
    assert_eq!(first.state_dir, daemon.state_dir().display().to_string());
    assert_eq!(
        first.socket,
        daemon.handle.socket_path().display().to_string()
    );
    assert_eq!(first.schema_version, SCHEMA_VERSION);
    assert_eq!(first.repositories, 0);
    assert_eq!(first.active_workspaces, 0);
    assert!(
        first
            .backends
            .iter()
            .any(|b| b.kind == "copy" && b.available),
        "{:?}",
        first.backends
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = daemon.client.status().await.unwrap();
    assert!(
        second.uptime_ms > first.uptime_ms,
        "{first:?} then {second:?}"
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_instance_is_refused() {
    let daemon = TestDaemon::start().await;
    let err = Daemon::start(daemon.config.clone()).await.unwrap_err();
    assert!(
        matches!(err, StartError::Lock(LockError::Held { pid: Some(pid), .. }) if pid == std::process::id()),
        "{err:?}"
    );
    // The refused start left the running daemon's files alone.
    daemon.client.status().await.unwrap();
    assert!(daemon.config.paths.pid.exists());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_socket_is_replaced() {
    let tmp = short_tempdir();
    let paths = Paths::in_state_dir(&tmp.path().join("state")).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(&paths.socket, b"left behind").unwrap();
    let config = Config::with_defaults(paths.clone());
    let handle = Daemon::start(config).await.unwrap();
    let client = Client::connect(&paths.socket).await.unwrap();
    client.status().await.unwrap();
    handle.shutdown();
    handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn start_sweeps_workspace_trash() {
    // A registered repository whose root is gone: the daemon logs that it
    // cannot open it and still sweeps its workspace root (R1.07).
    let tmp = short_tempdir();
    let paths = Paths::in_state_dir(&tmp.path().join("state")).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let id = RepoId::generate();
    let db = Db::open(&paths.db).unwrap();
    db.insert_repository(&RepositoryRecord {
        id,
        root: tmp.path().join("gone"),
        jj_repo_path: tmp.path().join("gone/.jj/repo"),
        colocated: false,
        registered_at: Timestamp::now(),
        tracked_files: None,
    })
    .unwrap();
    drop(db);
    let config = Config::with_defaults(paths.clone());
    let ws_root = config.workspace_root(id);
    std::fs::create_dir_all(ws_root.join("00000000000000aa.trash-1/sub")).unwrap();
    std::fs::write(ws_root.join("00000000000000aa.trash-1/sub/file"), b"x").unwrap();
    std::fs::create_dir(ws_root.join("00000000000000bb")).unwrap();

    let handle = Daemon::start(config).await.unwrap();
    assert!(!ws_root.join("00000000000000aa.trash-1").exists());
    assert!(ws_root.join("00000000000000bb").is_dir(), "live roots stay");
    handle.shutdown();
    handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn live_socket_of_another_daemon_is_not_replaced() {
    let first = TestDaemon::start().await;
    let tmp = short_tempdir();
    let overrides = PathOverrides {
        state_dir: Some(tmp.path().join("state")),
        socket: Some(first.handle.socket_path().to_owned()),
        config_file: None,
    };
    let config = Config::with_defaults(Paths::resolve(&overrides).unwrap());
    let err = Daemon::start(config).await.unwrap_err();
    assert!(matches!(err, StartError::SocketInUse { .. }), "{err:?}");
    first.client.status().await.unwrap();
    first.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_removes_socket_and_pid() {
    let daemon = TestDaemon::start().await;
    let paths = daemon.config.paths.clone();
    assert!(paths.socket.exists() && paths.pid.exists());
    assert_eq!(
        std::fs::read_to_string(&paths.pid).unwrap().trim(),
        std::process::id().to_string()
    );
    let TestDaemon {
        handle,
        client,
        tmp,
        ..
    } = daemon;
    drop(client);
    handle.shutdown();
    handle.join().await.unwrap();
    assert!(!paths.socket.exists(), "socket removed");
    assert!(!paths.pid.exists(), "pid file removed");
    assert!(paths.lock.exists(), "the lock file remains");
    drop(tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_mode_is_0600() {
    let daemon = TestDaemon::start().await;
    assert_eq!(mode(daemon.handle.socket_path()), 0o600);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn state_dir_mode_is_0700() {
    let daemon = TestDaemon::start().await;
    assert_eq!(mode(daemon.state_dir()), 0o700);
    daemon.stop().await;
}

#[test]
fn log_file_is_json_lines() {
    let (output, tmp) = run_binary(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files = log_files(tmp.path());
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.starts_with("daemon.log."), "dated by rotation: {name}");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?}: {e}")))
        .collect();
    // A fresh state directory logs its migration first, on Linux the watcher
    // its inotify limits (R1.13), and with `watchman` installed the socket it
    // found (R1.14); nothing else at info comes before the start event.
    let started = events
        .iter()
        .position(|event| event["message"] == "daemon started")
        .unwrap_or_else(|| panic!("no start event in {text}"));
    let allowed = [
        "database migrated",
        "inotify limits",
        "watchman socket found; WATCHMAN_SOCK set",
        "using WATCHMAN_SOCK from the environment",
    ];
    assert!(
        events[..started]
            .iter()
            .all(|e| allowed.iter().any(|message| e["message"] == *message)),
        "{text}"
    );
    let event = &events[started];
    assert_eq!(event["level"], "INFO");
    assert_eq!(event["state_dir"], tmp.path().display().to_string());
    assert_eq!(
        event["socket"],
        tmp.path().join("daemon.sock").display().to_string()
    );
    assert!(event["pid"].is_u64());
    assert!(
        events.iter().any(|e| e["message"] == "daemon stopped"),
        "the writer is flushed on exit: {text}"
    );
}

#[test]
fn binary_runs_and_stops_on_sigterm() {
    let tmp = short_tempdir();
    // Not created beforehand: the binary creates it, privately.
    let state = tmp.path().join("state");
    let socket = state.join("daemon.sock");
    let mut child = daemon_command(&state, &[]).spawn().unwrap();
    assert!(
        wait_for_socket(&mut child, &socket),
        "the daemon exited early"
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let status = runtime.block_on(async {
        let client = Client::connect(&socket).await.unwrap();
        client.status().await.unwrap()
    });
    assert_eq!(status.pid, child.id());
    assert_eq!(mode(&state), 0o700, "state directory");
    assert_eq!(mode(&state.join("logs")), 0o700, "log directory");
    assert_eq!(status.socket, socket.display().to_string());
    let output = terminate(child);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!socket.exists());
}

#[test]
fn binary_refuses_a_second_instance_with_the_holder_pid() {
    let tmp = short_tempdir();
    let mut first = daemon_command(tmp.path(), &[]).spawn().unwrap();
    assert!(wait_for_socket(&mut first, &tmp.path().join("daemon.sock")));
    let second = daemon_command(tmp.path(), &[]).output().unwrap();
    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&second.stderr);
    let lock = tmp.path().join("daemon.lock");
    assert!(
        stderr.contains(&format!(
            "another reef-daemon (pid {}) holds {}",
            first.id(),
            lock.display()
        )),
        "{stderr}"
    );
    assert_eq!(terminate(first).status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_thread_runs_jobs_in_order_and_reloads() {
    let tmp = short_tempdir();
    let root = tmp.path().join("repo");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let record = RepositoryRecord {
        id: RepoId::generate(),
        jj_repo_path: root.join(".jj").join("repo"),
        root: root.clone(),
        colocated: false,
        registered_at: Timestamp::now(),
        tracked_files: None,
    };
    let registry = std::sync::Arc::new(RepoRegistry::new());
    let handle = {
        let registry = std::sync::Arc::clone(&registry);
        let record = record.clone();
        tokio::task::spawn_blocking(move || registry.open(&record, test_settings()))
            .await
            .unwrap()
            .unwrap()
    };
    let before = handle.call(|head| head.repo.op_id()).await.unwrap();

    // The first job commits through a second `Repo`, so the head's repo is
    // stale until the thread reloads. Both jobs are queued before either is
    // awaited.
    let first = handle.call(move |head| {
        std::fs::write(head.root.join("new-file.txt"), b"hello").unwrap();
        let mut other = pollster::block_on(Repo::open(&head.root, &head.settings)).unwrap();
        pollster::block_on(other.snapshot("reef: test snapshot")).unwrap();
        let committed = other.op_id();
        assert_ne!(
            head.repo.op_id(),
            committed,
            "the head is stale until reloaded"
        );
        head.note_committed();
        committed
    });
    let second = handle.call(|head| head.repo.op_id());
    let committed = first.await.unwrap();
    let seen = second.await.unwrap();
    assert_ne!(before, committed);
    assert_eq!(seen, committed, "the second job sees the first's operation");

    // `call_async` drives a jj future on the thread.
    let async_op = handle
        .call_async(|head| async move { head.repo.op_id() }.boxed_local())
        .await
        .unwrap();
    assert_eq!(async_op, committed);

    // A panicking job fails alone; the thread keeps serving.
    let panicked = handle.call(|_| panic!("job panics on purpose")).await;
    assert!(matches!(panicked, Err(RepoThreadError::Panicked { .. })));
    assert_eq!(
        handle.call(|head| head.repo.op_id()).await.unwrap(),
        committed
    );

    // A pause holds later jobs until the guard drops.
    let guard = handle.pause(Duration::from_secs(30)).await.unwrap();
    let mut held = Box::pin(handle.call(|_| ()));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut held)
            .await
            .is_err(),
        "a job waits while the thread is paused"
    );
    drop(guard);
    held.await.unwrap();

    // Closing stops the thread; later calls fail as closed.
    registry.close(record.id);
    assert!(registry.get(record.id).is_none());
    let late = handle.call(|_| ()).await;
    assert!(
        matches!(late, Err(RepoThreadError::Closed { .. })),
        "{late:?}"
    );
    tokio::task::spawn_blocking(move || registry.close_all())
        .await
        .unwrap();
}

#[test]
fn socket_path_too_long_fails_early() {
    let long: PathBuf = format!("/tmp/{}", "s".repeat(115)).into();
    assert_eq!(long.as_os_str().len(), 120);
    let err = Paths::resolve(&PathOverrides {
        socket: Some(long.clone()),
        ..PathOverrides::default()
    })
    .unwrap_err();
    assert!(
        matches!(err, ConfigError::SocketPathTooLong { ref path, max: SOCKET_PATH_MAX } if *path == long),
        "{err:?}"
    );

    let tmp = short_tempdir();
    let output = daemon_command(tmp.path(), &[("REEF_SOCKET", long.to_str().unwrap())])
        .output()
        .unwrap();
    // A configuration error, so `InvalidArgument`'s exit code (R1.04).
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is 120 bytes; the limit is 103"),
        "{stderr}"
    );
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "nothing created before the check"
    );
}

/// R1.04: a `REEF_CONFIG` file with an unknown key stops the binary with
/// exit 2 and a message naming the key and the file, before it creates
/// anything.
#[test]
fn unknown_config_key_exits_2() {
    let tmp = short_tempdir();
    let files = short_tempdir();
    let bad = files.path().join("bad.toml");
    std::fs::write(&bad, "[lease]\ndefualt = \"2h\"\n").unwrap();
    let output = daemon_command(tmp.path(), &[("REEF_CONFIG", bad.to_str().unwrap())])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(stderr.contains("`lease.defualt`"), "{stderr}");
    assert!(stderr.contains(&bad.display().to_string()), "{stderr}");
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "nothing created"
    );
}

/// R1.04: the daemon's jj settings record the host and user on operations.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_settings_carry_the_operation_identity() {
    let daemon = TestDaemon::start().await;
    let settings = &daemon.handle.state().settings;
    let host = nix::unistd::gethostname().unwrap().into_string().unwrap();
    assert_eq!(settings.operation_hostname(), host);
    assert_eq!(
        settings.operation_username(),
        daemon.config.operation.username
    );
    assert_eq!(
        settings.user_email(),
        format!("reef@{}", host.to_lowercase())
    );
    daemon.stop().await;
}
