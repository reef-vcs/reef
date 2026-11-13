//! Issue #271: the machine-wide build limit, through the daemon. The
//! environment `GetBuildEnvironment` answers, the `build` object of
//! `GetDaemonStatus`, a real cargo build drawing from the jobserver, and
//! tokens a killed build lost coming back.
//!
//! Concurrency is asserted as an upper bound only, never as a speed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use reef_api::v1::{self, CreateWorkspaceRequest, JobserverSkip};
use reef_daemon::client::Client;
use reef_daemon::config::{BuildJobs, Config};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

fn limited(jobs: u32) -> impl FnOnce(&mut Config) {
    move |config: &mut Config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.symbols.enabled = false;
        config.file.build.jobs = BuildJobs::Count(jobs);
        config.build_interval = Duration::from_millis(100);
    }
}

async fn create(client: &Client, repo: &SyntheticRepo) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Renews `ws`'s lease, which publishes its heartbeat entry: a builder.
async fn heartbeat(client: &Client, ws: &v1::Workspace) {
    let lease = ws.lease.as_ref().unwrap();
    client.renew_lease("", &ws.id, &lease.id).await.unwrap();
}

fn names(response: &v1::GetBuildEnvironmentResponse) -> Vec<&str> {
    response
        .variables
        .iter()
        .map(|variable| variable.name.as_str())
        .collect()
}

fn value<'a>(response: &'a v1::GetBuildEnvironmentResponse, name: &str) -> Option<&'a str> {
    response
        .variables
        .iter()
        .find(|variable| variable.name == name)
        .map(|variable| variable.value.as_str())
}

async fn status(client: &Client) -> v1::BuildStatus {
    client.daemon_status(true).await.unwrap().build.unwrap()
}

/// Waits until `check` holds, or fails after `within`.
async fn eventually<F: Future<Output = bool>>(
    within: Duration,
    what: &str,
    mut check: impl FnMut() -> F,
) {
    let deadline = Instant::now() + within;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn get_build_environment() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, limited(8)).await;
    let client = &daemon.client;
    let fifo = daemon.state_dir().join("build").join("jobserver");

    // The machine's defaults.
    let machine = client.get_build_environment("", "").await.unwrap();
    assert!(machine.enabled);
    assert_eq!((machine.jobs, machine.builders, machine.share), (8, 1, 8));
    assert_eq!(machine.nice, 10);
    assert_eq!(machine.jobserver, fifo.display().to_string());
    assert_eq!(
        value(&machine, "CARGO_MAKEFLAGS"),
        Some(format!("-j --jobserver-auth=fifo:{}", fifo.display()).as_str())
    );

    // A workspace counts once; a second heartbeating one halves the share.
    let first = create(client, &repo).await;
    heartbeat(client, &first).await;
    let alone = client.get_build_environment("", &first.id).await.unwrap();
    assert_eq!((alone.builders, alone.share), (1, 8));
    let second = create(client, &repo).await;
    heartbeat(client, &second).await;
    let shared = client.get_build_environment("", &first.id).await.unwrap();
    assert_eq!((shared.builders, shared.share), (2, 4));
    assert_eq!(value(&shared, "RUST_TEST_THREADS"), Some("4"));
    assert_eq!(value(&shared, "NEXTEST_TEST_THREADS"), Some("4"));
    assert_eq!(
        names(&shared)[0],
        "CARGO_MAKEFLAGS",
        "the jobserver comes first"
    );

    // A released workspace is not found.
    client
        .release_workspace("", &second.id, false, true)
        .await
        .unwrap();
    let error = client
        .get_build_environment("", &second.id)
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::WORKSPACE_NOT_FOUND);
    // Its heartbeat entry ended with it.
    let after = client.get_build_environment("", &first.id).await.unwrap();
    assert_eq!((after.builders, after.share), (1, 8));
    daemon.stop().await;
}

#[tokio::test]
async fn limit_off_makes_nothing() {
    let daemon = TestDaemon::start_with(|config| {
        limited(8)(config);
        config.file.build.limit = false;
    })
    .await;
    let response = daemon.client.get_build_environment("", "").await.unwrap();
    assert!(!response.enabled);
    assert!(response.variables.is_empty(), "{response:?}");
    assert_eq!(response.jobserver_skip, i32::from(JobserverSkip::Disabled));
    assert_eq!(response.nice, 0);
    assert!(!daemon.state_dir().join("build").exists());
    let build = status(&daemon.client).await;
    assert!(!build.enabled);
    assert!(build.jobserver.is_empty(), "{build:?}");
    daemon.stop().await;
}

#[tokio::test]
async fn daemon_status_build() {
    let daemon = TestDaemon::start_with(limited(5)).await;
    let build = status(&daemon.client).await;
    assert!(build.enabled);
    assert_eq!((build.jobs, build.tokens, build.available), (5, 4, 4));
    assert_eq!(build.generation, 1);
    assert!(
        build.cargo_config.is_empty(),
        "no cargo file for a test daemon"
    );

    // A client takes two tokens: the pool shows it; it returns them.
    let path = PathBuf::from(&build.jobserver);
    let mut fifo = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut taken = [0u8; 2];
    std::io::Read::read_exact(&mut fifo, &mut taken).unwrap();
    assert_eq!(status(&daemon.client).await.available, 2);
    std::io::Write::write_all(&mut fifo, &taken).unwrap();
    assert_eq!(status(&daemon.client).await.available, 4);

    // A graceful stop removed the path, so a restart makes a new FIFO
    // (reuse after a crash is `jobserver::tests::create_reuse_replace`).
    drop(fifo);
    let daemon = daemon.restart().await;
    let build = status(&daemon.client).await;
    assert_eq!(build.available, 4);
    let path = PathBuf::from(&build.jobserver);
    daemon.stop().await;
    assert!(!path.exists(), "a graceful stop unlinks the FIFO");
}

/// Review finding 3: a FIFO removed while the daemon runs is made again at
/// the next check, so later commands do not name a missing path.
#[tokio::test]
async fn a_removed_fifo_comes_back() {
    let daemon = TestDaemon::start_with(limited(4)).await;
    let build = status(&daemon.client).await;
    let path = PathBuf::from(&build.jobserver);
    std::fs::remove_file(&path).unwrap();
    let client = &daemon.client;
    eventually(Duration::from_secs(10), "a new FIFO", || async {
        let build = status(client).await;
        path.exists() && build.generation == 2 && build.available == build.tokens
    })
    .await;
    daemon.stop().await;
}

// ---- real builds ----

/// A cargo workspace of `crates` dependency-free crates under `dir`.
fn generate_workspace(dir: &Path, crates: usize) {
    let members: Vec<String> = (0..crates).map(|i| format!("\"c{i}\"")).collect();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
            members.join(", ")
        ),
    )
    .unwrap();
    for i in 0..crates {
        let src = dir.join(format!("c{i}")).join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join(format!("c{i}")).join("Cargo.toml"),
            format!("[package]\nname = \"c{i}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            src.join("lib.rs"),
            format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
}

/// A `RUSTC_WRAPPER` that marks each running compile in `marks`, logs how
/// many were running when it started, and holds each compile `hold` long.
fn wrapper(dir: &Path, marks: &Path, hold: &str) -> PathBuf {
    std::fs::create_dir_all(marks).unwrap();
    let path = dir.join("rustc-wrapper.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             # cargo's own queries (-vV, --print) are not jobs\n\
             case \"$*\" in *--print*) exec \"$@\" ;; *--crate-name*) ;; *) exec \"$@\" ;; esac\n\
             mark=\"{marks}/$$\"\n\
             : > \"$mark\"\n\
             ls \"{marks}\" | grep -v log | wc -l >> \"{marks}/log\"\n\
             sleep {hold}\n\
             \"$@\"\n\
             status=$?\n\
             rm -f \"$mark\"\n\
             exit $status\n",
            marks = marks.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn most_at_once(marks: &Path) -> usize {
    std::fs::read_to_string(marks.join("log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .max()
        .unwrap_or(0)
}

/// `cargo build` in `dir` with `response`'s variables and nothing of the
/// test's own build environment.
fn cargo_build(
    dir: &Path,
    target: &Path,
    rustc_wrapper: &Path,
    response: &v1::GetBuildEnvironmentResponse,
) -> std::process::Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = std::process::Command::new(cargo);
    command
        .args(["build", "--offline", "--quiet"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .env("RUSTC_WRAPPER", rustc_wrapper)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in [
        "CARGO_MAKEFLAGS",
        "MAKEFLAGS",
        "MFLAGS",
        "CARGO_BUILD_JOBS",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_INCREMENTAL",
    ] {
        command.env_remove(name);
    }
    for variable in &response.variables {
        command.env(&variable.name, &variable.value);
    }
    command
}

#[tokio::test]
async fn bounds_rustc() {
    let daemon = TestDaemon::start_with(limited(3)).await;
    let response = daemon.client.get_build_environment("", "").await.unwrap();
    assert!(
        value(&response, "CARGO_MAKEFLAGS").is_some(),
        "a jobserver: {}",
        response.note
    );
    let temp = tempfile::tempdir().unwrap();
    let marks = temp.path().join("marks");
    let rustc = wrapper(temp.path(), &marks, "0.2");
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    for dir in [&one, &two] {
        std::fs::create_dir_all(dir).unwrap();
        generate_workspace(dir, 8);
    }

    // One build: at most `jobs` compiles (2 tokens and cargo's implicit one).
    let output = cargo_build(&one, &one.join("target"), &rustc, &response)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        most_at_once(&marks) <= 3,
        "one build ran {}",
        most_at_once(&marks)
    );
    assert_eq!(
        status(&daemon.client).await.available,
        2,
        "every token back"
    );

    // Two at once: at most 4 (two implicit tokens).
    std::fs::remove_file(marks.join("log")).unwrap();
    let a = cargo_build(&one, &temp.path().join("t1"), &rustc, &response)
        .spawn()
        .unwrap();
    let b = cargo_build(&two, &temp.path().join("t2"), &rustc, &response)
        .spawn()
        .unwrap();
    for child in [a, b] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        most_at_once(&marks) <= 4,
        "two builds ran {}",
        most_at_once(&marks)
    );
    assert_eq!(
        status(&daemon.client).await.available,
        2,
        "every token back"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn recovers_after_kill() {
    use std::os::unix::process::CommandExt as _;
    let daemon = TestDaemon::start_with(limited(4)).await;
    let response = daemon.client.get_build_environment("", "").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let marks = temp.path().join("marks");
    // Long enough that the kill lands while compiles hold tokens.
    let rustc = wrapper(temp.path(), &marks, "30");
    let dir = temp.path().join("ws");
    std::fs::create_dir_all(&dir).unwrap();
    generate_workspace(&dir, 8);
    let mut child = cargo_build(&dir, &dir.join("target"), &rustc, &response)
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    // Three compiles running: two hold tokens.
    eventually(Duration::from_secs(60), "three compiles", || {
        let marks = marks.clone();
        async move { most_at_once(&marks) >= 3 }
    })
    .await;
    assert!(status(&daemon.client).await.available < 3);
    // SAFETY: kill(2) on the build's own process group.
    #[allow(unsafe_code, reason = "killpg(2)")]
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    let _ = child.wait();
    let client = &daemon.client;
    eventually(Duration::from_secs(30), "the tokens back", || async {
        let build = status(client).await;
        build.available == build.tokens && build.top_ups >= 1
    })
    .await;
    let build = status(client).await;
    assert!(build.tokens_restored >= 2, "{build:?}");
    daemon.stop().await;
}
