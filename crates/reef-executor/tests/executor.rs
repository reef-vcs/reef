//! The reference executor against a real daemon (R3.06, ADR 0060; tests 4
//! to 8, 11 and 12 of the brief): materialization and its checks, a job
//! end to end over the socket and the remote listener, budgets, timeouts,
//! cancellation, shutdown, secrets and concurrency.
//!
//! Checks run in this platform's sandbox when it applies here, and as
//! plain processes otherwise (a Seatbelt runner already inside a sandbox);
//! `REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX=1` makes that a failure.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; a panic is the failure report, and a skip is said"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reef_api::v1::{
    self, CreateIdentityRequest, CreateWorkspaceRequest, IdentityKind, JobOutcome, JobState,
    ListEvidenceRequest, RemoteScope, SubmitJobRequest, Workspace, submit_job_request,
};
use reef_daemon::client::Client;
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::config::Config as DaemonConfig;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_executor::config::{Config, SandboxKind, SandboxSection};
use reef_executor::job::{Context, Hidden};
use reef_executor::materialize::{self, BlobCache, MaterializeError};
use reef_executor::report::JobReport;
use reef_executor::sandbox::Backend;
use reef_executor::serve::{self, Options};
use reef_testing::synth::Preset;
use tokio::sync::watch;

const OWNER: &str = "tester@host";

async fn fixture_with(configure: impl FnOnce(&mut DaemonConfig)) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn create(client: &Client, repo: &str) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write(ws: &Workspace, path: &str, content: &[u8]) {
    let path = Path::new(&ws.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

async fn submit(client: &Client, ws: &Workspace, kinds: &[&str], targets: &[&str]) -> v1::Job {
    submit_with(client, ws, kinds, targets, None).await
}

async fn submit_with(
    client: &Client,
    ws: &Workspace,
    kinds: &[&str],
    targets: &[&str],
    wall: Option<Duration>,
) -> v1::Job {
    client
        .submit_job(SubmitJobRequest {
            workspace: ws.id.clone(),
            source: Some(submit_job_request::Source::Revision("@".to_owned())),
            kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
            targets: targets.iter().map(|t| (*t).to_owned()).collect(),
            budget: wall.map(|w| v1::Budget {
                wall_time: Some(prost_types::Duration {
                    seconds: i64::try_from(w.as_secs()).unwrap(),
                    nanos: 0,
                }),
                cpu_time: None,
            }),
            duplicate: true,
            ..SubmitJobRequest::default()
        })
        .await
        .unwrap()
        .job
        .unwrap()
}

/// An object's annotations, from its envelope.
async fn annotations(client: &Client, repo: &str, object: &v1::Evidence) -> serde_json::Value {
    let full = client
        .get_evidence(repo, &object.evidence_id)
        .await
        .unwrap()
        .evidence
        .unwrap();
    let envelope: reef_evidence::Envelope = serde_json::from_slice(&full.envelope).unwrap();
    let statement: serde_json::Value =
        serde_json::from_slice(&envelope.payload_bytes().unwrap()).unwrap();
    statement["predicate"]["annotations"].clone()
}

async fn get(client: &Client, id: &str) -> v1::Job {
    client
        .get_job(v1::GetJobRequest {
            job_id: id.to_owned(),
            ..v1::GetJobRequest::default()
        })
        .await
        .unwrap()
        .job
        .unwrap()
}

/// This platform's sandbox if it applies here; `None` (checks run
/// unsandboxed) otherwise, unless the environment requires it.
fn sandbox() -> Option<Backend> {
    let backend = Backend::resolve(&SandboxSection::default()).ok();
    let reason = match &backend {
        Some(backend) => match backend.self_test() {
            Ok(()) => return Some(backend.clone()),
            Err(error) => error.to_string(),
        },
        None => "no sandbox for this platform".to_owned(),
    };
    assert!(
        std::env::var_os("REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX").is_none(),
        "REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX is set and the sandbox does not apply: {reason}"
    );
    eprintln!("running checks unsandboxed: {reason}");
    None
}

struct Executor {
    ctx: Arc<Context>,
    shutdown: Arc<watch::Sender<bool>>,
    work: tempfile::TempDir,
    reports: Arc<Mutex<Vec<JobReport>>>,
}

/// An executor with `config` (TOML), its work directory a fresh temporary
/// one, the daemon's state directory hidden from checks.
fn executor(daemon: &TestDaemon, client: Client, config: &str, keep_work: bool) -> Executor {
    let work = tempfile::tempdir().unwrap();
    let mut text = format!("[work]\ndir = {:?}\n", work.path().display().to_string());
    let backend = sandbox();
    if backend.is_none() {
        text.push_str("[sandbox]\nkind = \"none\"\n");
    }
    text.push_str(config);
    let config = Config::parse(&text, None).unwrap();
    let backend = backend.unwrap_or(Backend::None);
    if backend == Backend::None {
        assert_eq!(config.sandbox.kind, SandboxKind::None);
    }
    serve::take_work_dir(&config.work_dir)
        .map(std::mem::forget)
        .unwrap();
    let cache =
        BlobCache::new(config.work_dir.join("blobs"), config.materialize.cache_size).unwrap();
    let (tx, rx) = watch::channel(false);
    let shutdown = Arc::new(tx);
    let name = config.name();
    let ctx = Arc::new(Context {
        client,
        config: Arc::new(config),
        backend,
        cache,
        hidden: Hidden {
            homes: reef_executor::config::home_dirs(),
            denied: vec![daemon.tmp.path().join("state")],
        },
        name,
        keep_work,
        shutdown: rx,
        stop: Arc::clone(&shutdown),
    });
    Executor {
        ctx,
        shutdown,
        work,
        reports: Arc::new(Mutex::new(Vec::new())),
    }
}

impl Executor {
    /// Claims one job (waiting `wait`) and runs it.
    async fn once(&self, wait: Duration) -> Option<JobReport> {
        self.serve(Options {
            repos: Vec::new(),
            jobs: 1,
            once: true,
            wait,
            exit_when_idle: None,
        })
        .await;
        self.reports.lock().unwrap().pop()
    }

    async fn serve(&self, options: Options) {
        let reports = Arc::clone(&self.reports);
        serve::serve(
            Arc::clone(&self.ctx),
            options,
            Arc::new(move |report: &JobReport| reports.lock().unwrap().push(report.clone())),
        )
        .await
        .unwrap();
    }

    fn jobs_left(&self) -> usize {
        std::fs::read_dir(self.work.path().join("jobs"))
            .unwrap()
            .count()
    }
}

const CHECKS: &str = r#"
[[check]]
name = "exit"
kind = "test"
targets = ["*"]
run_target = ["sh", "-c", "echo running $REEF_CHECK_TARGET; exit \"$1\"", "sh", "{target}"]
error_exit = [75]

[[check]]
name = "fmt"
kind = "lint"
run = ["sh", "-c", "exit 0"]

[[check]]
name = "clippy"
kind = "lint"
run = ["sh", "-c", "echo clippy says no; exit 1"]
artifacts = ["report.txt"]
"#;

// 4
#[expect(clippy::too_many_lines, reason = "a tree, then each check in turn")]
#[tokio::test(flavor = "multi_thread")]
async fn materialize_checks_the_tree() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    write(&ws, "plain.txt", b"hello\n");
    write(&ws, "nested/deep/file.rs", b"fn main() {}\n");
    write(&ws, "empty", b"");
    // Over jj's 1 MiB limit for a new file: tracked small first.
    write(&ws, "big.bin", b"small");
    submit(client, &ws, &["warm"], &[]).await;
    let big: Vec<u8> = (0..3 * 1024 * 1024u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    write(&ws, "big.bin", &big);
    write(&ws, "run.sh", b"#!/bin/sh\necho hi\n");
    std::fs::set_permissions(
        Path::new(&ws.path).join("run.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("plain.txt", Path::new(&ws.path).join("link")).unwrap();
    let job = submit(client, &ws, &["test"], &[]).await;
    let spec = job.spec.clone().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cache = BlobCache::new(work.path().join("blobs"), 1 << 30).unwrap();
    let counted = Arc::new(Mutex::new(0u64));
    let seen = Arc::clone(&counted);
    let progress = move |_: u64, total: u64| *seen.lock().unwrap() = total;
    let tree = materialize::materialize(
        client,
        &repo.id,
        &spec,
        &work.path().join("one"),
        &cache,
        reef_executor::config::Verify::Disk,
        &progress,
    )
    .await
    .unwrap();
    assert!(tree.fetched > 5, "{tree:?}");
    let laid = work.path().join("one");
    assert_eq!(std::fs::read(laid.join("big.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read_link(laid.join("link")).unwrap(),
        PathBuf::from("plain.txt")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(laid.join("run.sh"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o755);

    // One changed file: one blob read.
    write(&ws, "plain.txt", b"changed\n");
    let second = submit(client, &ws, &["test"], &[]).await;
    let tree = materialize::materialize(
        client,
        &repo.id,
        &second.spec.clone().unwrap(),
        &work.path().join("two"),
        &cache,
        reef_executor::config::Verify::Disk,
        &progress,
    )
    .await
    .unwrap();
    assert_eq!(tree.fetched, 1, "{tree:?}");
    assert_eq!(*counted.lock().unwrap(), 1);

    // A cache file changed after it was verified: the disk check fails.
    let blob_id = reef_executor::githash::blob_id(b"changed\n").unwrap();
    let hex = reef_executor::githash::hex(&blob_id);
    let cached = work.path().join("blobs").join(&hex[..2]).join(&hex[2..]);
    std::fs::write(&cached, b"tampered\n").unwrap();
    let error = materialize::materialize(
        client,
        &repo.id,
        &second.spec.clone().unwrap(),
        &work.path().join("three"),
        &cache,
        reef_executor::config::Verify::Disk,
        &progress,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MaterializeError::Disk(_)), "{error}");

    // A tree id that is not the listing's: the listing step fails.
    let mut wrong = spec.clone();
    wrong.tree_id = "0".repeat(40);
    let error = materialize::materialize(
        client,
        &repo.id,
        &wrong,
        &work.path().join("four"),
        &cache,
        reef_executor::config::Verify::Disk,
        &progress,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MaterializeError::Listing(_)), "{error}");

    // A job by tree alone: no commit to read.
    let mut by_tree = spec.clone();
    by_tree.commit_id.clear();
    let error = materialize::materialize(
        client,
        &repo.id,
        &by_tree,
        &work.path().join("five"),
        &cache,
        reef_executor::config::Verify::Disk,
        &progress,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MaterializeError::NoCommit), "{error}");
    daemon.stop().await;
}

// 5
#[expect(clippy::too_many_lines, reason = "one job, then every check of it")]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_end_to_end() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    write(&ws, "reef-checks.toml", CHECKS.as_bytes());
    let exec = executor(&daemon, client.clone(), "", false);

    let job = submit(client, &ws, &["test"], &["0", "1", "75"]).await;
    let mut presence = client
        .subscribe_presence(v1::SubscribePresenceRequest {
            repos: vec![repo.id.clone()],
            kinds: vec![v1::PresenceKind::Job.into()],
            ..v1::SubscribePresenceRequest::default()
        })
        .await
        .unwrap();
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    // Every heartbeat was accepted: the phases a subscriber saw include
    // each pair's (an update the daemon refuses stops the executor).
    let mut phases = Vec::new();
    while let Ok(Some(Ok(frame))) =
        tokio::time::timeout(Duration::from_millis(500), presence.message())
            .await
            .map(Result::transpose)
    {
        for event in frame.events {
            if let Some(progress) = event.presence.and_then(|p| p.job).and_then(|j| j.progress) {
                phases.push(progress.phase);
            }
        }
    }
    assert!(
        phases.contains(&"materialize".to_owned()) || phases.iter().any(|p| p.starts_with("test:")),
        "{phases:?}"
    );
    assert_eq!(report.job_id, job.job_id);
    assert_eq!(report.outcome, "done", "{report:?}");
    assert_eq!(
        (report.pass, report.fail, report.error),
        (1, 1, 1),
        "{report:?}"
    );
    let read = get(client, &job.job_id).await;
    assert_eq!(read.state(), JobState::Completed);
    assert_eq!(read.outcome(), JobOutcome::Done);
    let result = |target: &str| {
        read.coverage
            .iter()
            .find(|c| c.target == target)
            .unwrap()
            .result()
    };
    assert_eq!(result("0"), v1::EvidenceResult::Pass);
    assert_eq!(result("1"), v1::EvidenceResult::Fail);
    assert_eq!(result("75"), v1::EvidenceResult::Error);

    let evidence = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: job.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(evidence.len(), 3);
    let sessions: std::collections::BTreeSet<String> = evidence
        .iter()
        .map(|e| {
            let identity = e.identity.as_ref().unwrap();
            assert_eq!(identity.status(), v1::IdentityStatus::Bound);
            let key = identity.key.as_ref().unwrap();
            assert_eq!(key.actor(), v1::SessionActor::Executor);
            key.session_id.clone()
        })
        .collect();
    assert_eq!(sessions.len(), 1, "one session for the job");
    for object in &evidence {
        assert_eq!(object.artifacts.len(), 1);
        assert_eq!(object.artifacts[0].state(), v1::ArtifactState::Stored);
        let annotations = annotations(client, &repo.id, object).await;
        assert_eq!(annotations["sandbox"]["kind"], exec.ctx.backend.kind());
        assert_eq!(annotations["verify"], "disk");
        assert_eq!(annotations["checks"][0]["source"], "repository");
        assert_eq!(
            annotations["checks"][0]["declaration"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(annotations["repository_checks"].as_str().unwrap().len(), 64);
    }
    assert_eq!(exec.jobs_left(), 0, "the job's directory is removed");

    // The whole lint: two checks, one failing, with finer objects; the
    // declared artifact is missing, which the note says.
    let lint = submit(client, &ws, &["lint"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "done", "{report:?}");
    assert_eq!(report.fail, 1);
    let objects = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: lint.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    let targets: std::collections::BTreeSet<&str> =
        objects.iter().map(|e| e.target.as_str()).collect();
    assert_eq!(targets, ["", "#clippy", "#fmt"].into_iter().collect());
    let summary = objects.iter().find(|e| e.target.is_empty()).unwrap();
    assert_eq!(summary.result(), v1::EvidenceResult::Fail);
    let annotations = annotations(client, &repo.id, summary).await;
    assert!(
        annotations["note"].as_str().unwrap().contains("report.txt"),
        "{annotations}"
    );

    // A kind nothing declares: an error with a note, and still DONE.
    let build = submit(client, &ws, &["build"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "done");
    assert_eq!(report.error, 1);
    assert!(
        report.pairs[0]
            .note
            .as_deref()
            .unwrap()
            .contains("no check")
    );
    assert_eq!(get(client, &build.job_id).await.outcome(), JobOutcome::Done);

    // A job by tree alone completes FAILED.
    let spec = job.spec.clone().unwrap();
    let by_tree = client
        .submit_job(SubmitJobRequest {
            repo: repo.id.clone(),
            source: Some(submit_job_request::Source::TreeId(spec.tree_id)),
            kinds: vec!["test".into()],
            ..SubmitJobRequest::default()
        })
        .await
        .unwrap()
        .job
        .unwrap();
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "failed");
    assert!(report.message.unwrap().contains("no commit"));
    let read = get(client, &by_tree.job_id).await;
    assert_eq!(read.outcome(), JobOutcome::Failed);
    daemon.stop().await;
}

fn remote_on(config: &mut DaemonConfig) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn remote_client(daemon: &TestDaemon, identity: &str) -> (Client, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let addr = loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            break addr;
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(
            "ci",
            RemoteScope::Write,
            Some(Duration::from_hours(1)),
            identity,
        )
        .await
        .unwrap();
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    (
        Client::connect_remote(&target).await.unwrap(),
        created.secret,
    )
}

// 6, 11
#[tokio::test(flavor = "multi_thread")]
async fn remote_with_a_ci_token_and_a_cache() {
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "ci-main".into(),
            kind: IdentityKind::Ci.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let ws = create(client, &repo.id).await;
    // The tree declares a passing test; this executor denies it and runs
    // its own, which counts runs in its cache and prints its environment.
    write(
        &ws,
        "reef-checks.toml",
        b"[[check]]\nname = \"tree\"\nkind = \"test\"\nrun = [\"sh\", \"-c\", \"exit 0\"]\n",
    );
    let (remote, secret) = remote_client(&daemon, "ci-main").await;
    let config = r#"
[repository]
checks = "deny"

[[check]]
name = "cached"
kind = "test"
run = ["sh", "-c", "env; ls -a \"$HOME\"; if [ -f \"$REEF_CHECK_CACHE/seen\" ]; then exit 0; fi; touch \"$REEF_CHECK_CACHE/seen\"; exit 1"]
cache = true
"#;
    let exec = executor(&daemon, remote, config, true);
    let first = submit(client, &ws, &["test"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.job_id, first.job_id);
    assert_eq!(report.fail, 1, "the first run has no cache: {report:?}");
    let second = submit(client, &ws, &["test"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.job_id, second.job_id);
    assert_eq!(
        report.pass, 1,
        "the second sees the first's cache: {report:?}"
    );
    let objects = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: second.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(objects.len(), 1);
    let identity = objects[0].identity.as_ref().unwrap();
    assert_eq!(identity.status(), v1::IdentityStatus::Bound);
    assert_eq!(identity.key.as_ref().unwrap().identity, "ci-main");
    let annotations = annotations(client, &repo.id, &objects[0]).await;
    assert_eq!(annotations["checks"][0]["source"], "executor");
    assert_eq!(annotations["checks"][0]["cache"], true);
    assert!(annotations.get("repository_checks").is_none());

    // 11: no secret in anything the check wrote, or its log.
    let mut scanned = 0;
    for entry in walk(exec.work.path()) {
        let Ok(bytes) = std::fs::read(&entry) else {
            continue;
        };
        scanned += 1;
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(&secret),
            "the remote token in {}",
            entry.display()
        );
        assert!(
            !text.contains("PRIVATE KEY"),
            "a private key in {}",
            entry.display()
        );
        assert!(
            !text.contains("REEF_REMOTE"),
            "a remote variable in {}",
            entry.display()
        );
    }
    assert!(scanned > 2, "the kept job directories were scanned");
    daemon.stop().await;
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn budgets_and_timeouts() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let config = r#"
[repository]
checks = "deny"

[run]
kill_grace = "1s"

[[check]]
name = "slow"
kind = "test"
timeout = "1s"
run = ["sh", "-c", "sleep 30"]

[[check]]
name = "slower"
kind = "lint"
run = ["sh", "-c", "sleep 30"]
"#;
    let exec = executor(&daemon, client.clone(), config, false);
    // A check past its own timeout: error, stopped by timeout, and the job
    // goes on to DONE.
    submit(client, &ws, &["test"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "done", "{report:?}");
    assert_eq!(report.error, 1);
    // A check past the job's wall time: the job is BUDGET_EXHAUSTED.
    let job = submit_with(
        client,
        &ws,
        &["lint", "test"],
        &[],
        Some(Duration::from_secs(10)),
    )
    .await;
    let started = std::time::Instant::now();
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.outcome, "budget_exhausted", "{report:?}");
    assert_eq!(report.pairs[0].result.as_deref(), Some("error"));
    assert_eq!(report.pairs[1].result, None, "the second pair never ran");
    let read = get(client, &job.job_id).await;
    assert_eq!(read.outcome(), JobOutcome::BudgetExhausted);
    let objects = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: job.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    let annotations = annotations(client, &repo.id, &objects[0]).await;
    assert_eq!(annotations["checks"][0]["stopped"], "budget");
    daemon.stop().await;
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn cancel_and_shutdown() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let config = r#"
[executor]
lease = "15s"

[repository]
checks = "deny"

[run]
kill_grace = "1s"

[[check]]
name = "slow"
kind = "test"
run = ["sh", "-c", "sleep 60"]
"#;
    let exec = executor(&daemon, client.clone(), config, false);
    // Cancelled mid-check: the next heartbeat finds it, the check is
    // killed, nothing more is sent.
    let job = submit(client, &ws, &["test"], &[]).await;
    let running = {
        let ctx = Arc::clone(&exec.ctx);
        let reports = Arc::clone(&exec.reports);
        tokio::spawn(async move {
            serve::serve(
                ctx,
                Options {
                    repos: Vec::new(),
                    jobs: 1,
                    once: true,
                    wait: Duration::from_secs(5),
                    exit_when_idle: None,
                },
                Arc::new(move |r: &JobReport| reports.lock().unwrap().push(r.clone())),
            )
            .await
        })
    };
    wait_for(client, &job.job_id, |j| {
        j.progress.as_ref().is_some_and(|p| p.phase == "test")
    })
    .await;
    client
        .cancel_job(v1::CancelJobRequest {
            job_id: job.job_id.clone(),
            reason: "superseded".into(),
            ..v1::CancelJobRequest::default()
        })
        .await
        .unwrap();
    let started = std::time::Instant::now();
    running.await.unwrap().unwrap();
    // The next heartbeat (a third of the 15 s lease) finds it, and the
    // 60 s check is killed: well under the check's own length, with room
    // for a loaded runner.
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    let report = exec.reports.lock().unwrap().pop().unwrap();
    assert_eq!(report.outcome, "lost", "{report:?}");
    assert_eq!(get(client, &job.job_id).await.evidence_count, 0);

    // Shutdown mid-check: the job is released, back in the queue.
    let job = submit(client, &ws, &["test"], &[]).await;
    let running = {
        let ctx = Arc::clone(&exec.ctx);
        let reports = Arc::clone(&exec.reports);
        tokio::spawn(async move {
            serve::serve(
                ctx,
                Options {
                    repos: Vec::new(),
                    jobs: 1,
                    once: true,
                    wait: Duration::from_secs(5),
                    exit_when_idle: None,
                },
                Arc::new(move |r: &JobReport| reports.lock().unwrap().push(r.clone())),
            )
            .await
        })
    };
    wait_for(client, &job.job_id, |j| {
        j.progress.as_ref().is_some_and(|p| p.phase == "test")
    })
    .await;
    exec.shutdown.send(true).unwrap();
    running.await.unwrap().unwrap();
    let report = exec.reports.lock().unwrap().pop().unwrap();
    assert_eq!(report.outcome, "released", "{report:?}");
    assert_eq!(get(client, &job.job_id).await.state(), JobState::Queued);
    assert_eq!(exec.jobs_left(), 0);
    daemon.stop().await;
}

async fn wait_for(client: &Client, id: &str, done: impl Fn(&v1::Job) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let job = get(client, id).await;
        if done(&job) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{job:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn jobs_run_at_once() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let config = r#"
[repository]
checks = "deny"

[[check]]
name = "nap"
kind = "test"
run = ["sh", "-c", "sleep 4"]

[[check]]
name = "quick"
kind = "lint"
run = ["true"]
"#;
    let exec = executor(&daemon, client.clone(), config, false);
    // One job first, so the three below find the blobs cached.
    submit(client, &ws, &["lint"], &[]).await;
    assert_eq!(
        exec.once(Duration::from_secs(5)).await.unwrap().outcome,
        "done"
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(submit(client, &ws, &["test"], &[]).await.job_id);
    }
    exec.serve(Options {
        repos: Vec::new(),
        jobs: 3,
        once: false,
        wait: Duration::ZERO,
        exit_when_idle: Some(Duration::from_secs(3)),
    })
    .await;
    let reports = exec.reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert!(reports.iter().all(|r| r.outcome == "done" && r.pass == 1));
    // All three ran at once: the last claim came before the first finish
    // (one after another, each would finish before the next was claimed).
    // By the daemon's clock, not a wall-time bound a loaded runner misses.
    let mut claimed = Vec::new();
    let mut finished = Vec::new();
    for id in &ids {
        let job = get(client, id).await;
        claimed.push(job.claimed_at.unwrap());
        finished.push(job.finished_at.unwrap());
    }
    let at = |t: &prost_types::Timestamp| (t.seconds, t.nanos);
    let last_claim = claimed.iter().map(at).max().unwrap();
    let first_finish = finished.iter().map(at).min().unwrap();
    assert!(
        last_claim < first_finish,
        "claims {claimed:?}, finishes {finished:?}"
    );
    let mut sessions = std::collections::BTreeSet::new();
    for id in &ids {
        let objects = client
            .list_evidence(
                ListEvidenceRequest {
                    repo: repo.id.clone(),
                    job_id: id.clone(),
                    ..ListEvidenceRequest::default()
                },
                0,
            )
            .await
            .unwrap()
            .items;
        sessions.insert(
            objects[0]
                .identity
                .as_ref()
                .unwrap()
                .key
                .as_ref()
                .unwrap()
                .session_id
                .clone(),
        );
    }
    assert_eq!(sessions.len(), 3, "a session per job");
    daemon.stop().await;
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

// Benchmarks for docs/benchmarks.md: the sandbox's start, a job's
// overhead, and materializing the small and medium presets cold and warm,
// over the socket and the remote listener. Release only.
#[expect(clippy::too_many_lines, reason = "one measurement after another")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "timings; run with --release --run-ignored"]
async fn timings() {
    use reef_executor::config::Verify;
    use reef_executor::sandbox::Policy;
    // The sandbox's start: `true` sandboxed, less `true` alone.
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let policy = Policy {
        cwd: cwd.clone(),
        writable: vec![cwd],
        hidden: reef_executor::config::home_dir().into_iter().collect(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        ..Policy::default()
    };
    let truth = if Path::new("/usr/bin/true").exists() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let time = |backend: &Backend| {
        median(
            (0..30)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let status = backend
                        .command(&policy, &[truth.to_owned()])
                        .unwrap()
                        .status()
                        .unwrap();
                    assert!(status.success());
                    started.elapsed()
                })
                .collect(),
        )
    };
    if let Some(backend) = sandbox() {
        let plain = time(&Backend::None);
        let boxed = time(&backend);
        eprintln!(
            "TIMING sandbox start ({}): {:?} (plain {plain:?}, sandboxed {boxed:?})",
            backend.kind(),
            boxed.saturating_sub(plain)
        );
    }

    for preset in [Preset::Small, Preset::Medium] {
        let (daemon, repo) = TestDaemon::with_synthetic_repo_and(preset, |config| {
            no_automatic_snapshots(config);
            config.file.history.enabled = false;
            remote_on(config);
        })
        .await;
        let client = &daemon.client;
        let ws = create(client, &repo.id).await;
        let job = submit(client, &ws, &["test"], &[]).await;
        let spec = job.spec.clone().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("blobs"), 8 << 30).unwrap();
        let none = |_: u64, _: u64| {};
        let mut n = 0;
        let mut materialize = |client: Client, spec: v1::JobSpec, verify: Verify| {
            n += 1;
            let into = work.path().join(format!("t{n}"));
            let cache = &cache;
            let repo = repo.id.clone();
            async move {
                let started = std::time::Instant::now();
                let tree =
                    materialize::materialize(&client, &repo, &spec, &into, cache, verify, &none)
                        .await
                        .unwrap();
                (started.elapsed(), tree)
            }
        };
        let (cold, tree) = materialize(client.clone(), spec.clone(), Verify::Disk).await;
        let (warm_listing, _) = materialize(client.clone(), spec.clone(), Verify::Listing).await;
        let (warm_disk, _) = materialize(client.clone(), spec.clone(), Verify::Disk).await;
        // One changed file.
        write(&ws, "changed.txt", b"changed\n");
        let changed = submit(client, &ws, &["test"], &[]).await.spec.unwrap();
        let (one, _) = materialize(client.clone(), changed, Verify::Disk).await;
        eprintln!(
            "TIMING materialize {preset:?} ({} entries, {} bytes): cold {cold:?}, warm {warm_disk:?} (listing only {warm_listing:?}, so the disk check {:?}), one changed file {one:?}",
            tree.entries,
            tree.fetched_bytes,
            warm_disk.saturating_sub(warm_listing)
        );
        if preset == Preset::Small {
            // Cold over the remote listener on loopback.
            client
                .create_identity(CreateIdentityRequest {
                    name: "ci-bench".into(),
                    kind: IdentityKind::Ci.into(),
                    ..CreateIdentityRequest::default()
                })
                .await
                .unwrap();
            let (remote, _) = remote_client(&daemon, "ci-bench").await;
            let remote_cache = BlobCache::new(work.path().join("remote-blobs"), 8 << 30).unwrap();
            let started = std::time::Instant::now();
            materialize::materialize(
                &remote,
                &repo.id,
                &spec,
                &work.path().join("remote"),
                &remote_cache,
                Verify::Disk,
                &none,
            )
            .await
            .unwrap();
            eprintln!(
                "TIMING remote materialize Small, cold: {:?}",
                started.elapsed()
            );

            // A job's overhead: one pair running `true`, warm cache, claim
            // to completion.
            let exec = executor(
                &daemon,
                client.clone(),
                "[repository]\nchecks = \"deny\"\n[[check]]\nname = \"t\"\nkind = \"test\"\nrun = [\"true\"]\n",
                false,
            );
            submit(client, &ws, &["test"], &[]).await;
            exec.once(Duration::from_secs(5)).await.unwrap();
            let mut samples = Vec::new();
            for _ in 0..7 {
                submit(client, &ws, &["test"], &[]).await;
                let report = exec.once(Duration::from_secs(5)).await.unwrap();
                assert_eq!(report.pass, 1);
                samples.push(Duration::from_millis(report.elapsed_ms));
            }
            eprintln!(
                "TIMING job overhead (one `true`, warm): {:?}",
                median(samples)
            );
        }
        daemon.stop().await;
    }
}

// 10: a real toolchain inside the sandbox: a crate with a build script and
// a test, built and tested offline with the default read paths and
// environment.
#[tokio::test(flavor = "multi_thread")]
async fn a_rust_crate_builds_in_the_sandbox() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    write(
        &ws,
        "probe-crate/Cargo.toml",
        b"[package]\nname = \"probe-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    write(
        &ws,
        "probe-crate/build.rs",
        b"fn main() { println!(\"cargo:rustc-env=PROBE_BUILT=yes\"); }\n",
    );
    write(
        &ws,
        "probe-crate/src/lib.rs",
        b"pub fn built() -> &'static str { env!(\"PROBE_BUILT\") }\n#[cfg(test)]\nmod tests { #[test] fn it_built() { assert_eq!(super::built(), \"yes\"); } }\n",
    );
    // The toolchain the test runs with: a runner whose rustup has no
    // default (CI's self-hosted Mac) names it only in RUSTUP_TOOLCHAIN,
    // which a check's environment, built from nothing, does not carry.
    // Else the channel this repository pins.
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN")
        .ok()
        .or_else(|| {
            let pinned = std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-toolchain.toml"),
            )
            .ok()?;
            let table: toml::Table = pinned.parse().ok()?;
            Some(table.get("toolchain")?.get("channel")?.as_str()?.to_owned())
        })
        .map(|t| format!("env = {{ RUSTUP_TOOLCHAIN = {t:?} }}\n"))
        .unwrap_or_default();
    write(
        &ws,
        "reef-checks.toml",
        format!(
            "[[check]]\nname = \"cargo\"\nkind = \"test\"\nrun = [\"cargo\", \"test\", \"--offline\", \"--manifest-path\", \"probe-crate/Cargo.toml\"]\ntimeout = \"10m\"\n{toolchain}"
        )
        .as_bytes(),
    );
    let exec = executor(&daemon, client.clone(), "", true);
    submit(client, &ws, &["test"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    let logs: Vec<PathBuf> = walk(exec.work.path())
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    let log = logs
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect::<String>();
    assert_eq!(report.pass, 1, "{report:?}\n{log}");
    assert!(log.contains("test tests::it_built ... ok"), "{log}");
    daemon.stop().await;
}

// 7: CPU time is checked before each check; a pair cut short gets no
// summary, and the job is BUDGET_EXHAUSTED.
#[tokio::test(flavor = "multi_thread")]
async fn cpu_time_runs_out_between_checks() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let config = r#"
[repository]
checks = "deny"

[[check]]
name = "burn"
kind = "lint"
run = ["sh", "-c", "i=0; while [ $i -lt 3000000 ]; do i=$((i+1)); done"]

[[check]]
name = "after"
kind = "lint"
run = ["sh", "-c", "touch never-ran"]
"#;
    let exec = executor(&daemon, client.clone(), config, true);
    let job = client
        .submit_job(SubmitJobRequest {
            workspace: ws.id.clone(),
            source: Some(submit_job_request::Source::Revision("@".to_owned())),
            kinds: vec!["lint".into()],
            budget: Some(v1::Budget {
                wall_time: None,
                cpu_time: Some(prost_types::Duration {
                    seconds: 1,
                    nanos: 0,
                }),
            }),
            ..SubmitJobRequest::default()
        })
        .await
        .unwrap()
        .job
        .unwrap();
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "budget_exhausted", "{report:?}");
    assert!(report.cpu_ms >= 1000, "{report:?}");
    assert!(
        walk(exec.work.path())
            .iter()
            .all(|p| !p.ends_with("never-ran"))
    );
    assert_eq!(get(client, &job.job_id).await.evidence_count, 0);
    daemon.stop().await;
}

// S-86/S-83: a declared artifact behind a link, as a link, or not a
// regular file is refused, and never read from outside the tree.
#[tokio::test(flavor = "multi_thread")]
async fn artifacts_never_follow_links() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside secret").unwrap();
    let target = outside.path().display().to_string();
    let config = format!(
        r#"
[repository]
checks = "deny"

[[check]]
name = "links"
kind = "lint"
run = ["sh", "-c", "ln -s {target}/secret file-link; ln -s {target} dir-link; mkfifo fifo; mkdir d; echo ok > d/real.txt"]
artifacts = ["file-link", "dir-link/secret", "fifo", "d/real.txt"]
"#
    );
    let exec = executor(&daemon, client.clone(), &config, false);
    let job = submit(client, &ws, &["lint"], &[]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.pass, 1, "{report:?}");
    let objects = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: job.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    // The log and d/real.txt; the three others refused with a note.
    assert_eq!(objects[0].artifacts.len(), 2, "{:?}", objects[0].artifacts);
    let names: Vec<&str> = objects[0]
        .artifacts
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert!(names.contains(&"d/real.txt"), "{names:?}");
    let note = annotations(client, &repo.id, &objects[0]).await["note"].clone();
    let note = note.as_str().unwrap();
    for refused in ["file-link", "dir-link/secret", "fifo"] {
        assert!(note.contains(refused), "{note}");
    }
    daemon.stop().await;
}

// R3.42 test 12: an executor with `keyless = true` signs a job's evidence
// with keys Fulcio certified (the fake on loopback), had when the job first
// signs and renewed in one session when less than three minutes remain.
#[tokio::test(flavor = "multi_thread")]
async fn a_keyless_job() {
    use reef_testing::sigstore::San;
    use reef_testing::sigstore::fake::FakeSigstore;
    const ISSUER: &str = "https://token.actions.githubusercontent.com";
    const SUBJECT: &str = "https://github.com/o/r/.github/workflows/reef.yml@refs/heads/main";
    let fake = FakeSigstore::start(ISSUER, San::Uri(SUBJECT.to_owned()));
    // Leaves that end within three minutes: every pair renews its key.
    fake.set_lifetime(150);
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().join("trusted_root.json");
    std::fs::write(&root, fake.trusted_root()).unwrap();
    let (daemon, repo) = fixture_with(|config| {
        config.file.sigstore.trusted_root = root.display().to_string();
    })
    .await;
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "ci-main".into(),
            kind: IdentityKind::Ci.into(),
            oidc: vec![v1::OidcMatchSpec {
                issuer: ISSUER.into(),
                subject: SUBJECT.into(),
            }],
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    // The executor reads its token and endpoints from its environment, as
    // in CI. Each test runs in its own process under nextest.
    #[allow(
        unsafe_code,
        reason = "this test's own process sets its environment before any thread reads it"
    )]
    // SAFETY: the variables are set before the executor starts, and nothing
    // else in this process reads or writes them concurrently.
    unsafe {
        std::env::set_var("SIGSTORE_ID_TOKEN", fake.token());
        std::env::set_var("REEF_SIGSTORE_FULCIO_URL", fake.url());
        std::env::set_var("REEF_SIGSTORE_REKOR_URL", fake.url());
    }
    let ws = create(client, &repo.id).await;
    write(&ws, "reef-checks.toml", CHECKS.as_bytes());
    let exec = executor(
        &daemon,
        client.clone(),
        "[executor]\nkeyless = true\n",
        false,
    );
    let job = submit(client, &ws, &["test"], &["0", "1"]).await;
    let report = exec.once(Duration::from_secs(5)).await.unwrap();
    assert_eq!(report.outcome, "done", "{report:?}");
    let evidence = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: job.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(evidence.len(), 2);
    let mut sessions = std::collections::BTreeSet::new();
    let mut keys = std::collections::BTreeSet::new();
    for object in &evidence {
        let identity = object.identity.as_ref().unwrap();
        assert_eq!(identity.status(), v1::IdentityStatus::Bound);
        let key = identity.key.as_ref().unwrap();
        assert_eq!(key.issuer_kind(), v1::IssuerKind::Sigstore);
        assert_eq!(key.identity, "ci-main");
        assert_eq!(key.actor(), v1::SessionActor::Executor);
        assert_eq!(key.sigstore.as_ref().unwrap().oidc_subject, SUBJECT);
        sessions.insert(key.session_id.clone());
        keys.insert(key.key_id.clone());
    }
    assert_eq!(sessions.len(), 1, "one session for the job");
    assert_eq!(keys.len(), 2, "a renewed key per pair");
    // A keyless executor names no identity.
    let error =
        Config::parse("[executor]\nkeyless = true\nidentity = \"ci-main\"\n", None).unwrap_err();
    assert!(error.to_string().contains("keyless"), "{error}");
}
