//! Batches and bisect (R3.14, the brief's tests 3 to 12): a batch landing
//! on a passing head tree in one operation; a fast-forward needing no job;
//! bisect finding the culprit and landing the passing prefix; failing
//! fast; infrastructure failures and `unavailable`; re-forming when the
//! destination moves; `alone` paths; restarts; and the batch RPCs.
//!
//! An in-test executor claims jobs through the API with alice's executor
//! key, which the test policy trusts, and signs `fail` for any tree with
//! `bad.txt` in it and `pass` otherwise.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, ClaimJobRequest, CompleteJobRequest, CreateIdentityRequest,
    EditFilesRequest, ExecutorInfo, FileEdit, GetBatchRequest, GetLandingRequest,
    IdentityKind as Kind, JobOutcome, LandChangeRequest, LandingOutcome as Outcome,
    LandingReason as Reason, ListBatchesRequest, OperationKind, ReadFileRequest, SignedEvidence,
    TransitionChangeRequest, UndoOperationRequest, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Verdict, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

/// `main`'s policy: `test` from alice's executor keys, batches of up to
/// `max` waiting `wait` to fill.
fn policy(max: u32, wait: &str, extra: &str) -> String {
    format!(
        r#"
version = 1

[owners]
identities = ["alice"]

[attestors.local]
identities = ["alice"]
actors = ["executor"]

[[require]]
name = "tests"
kinds = ["test"]
attestors = ["local"]

[landing]
batch_max = {max}
batch_wait = "{wait}"
{extra}
"#
    )
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

/// A new commit on `on` writing each of `files`, outside any workspace.
async fn commit_on(client: &Client, repo: &str, on: &str, files: &[(&str, &str)]) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: files
                .iter()
                .map(|(path, text)| FileEdit {
                    path: (*path).to_owned(),
                    action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
                    executable: None,
                })
                .collect(),
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`
/// would.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = reef_daemon::testing::test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&name),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

async fn head_of(client: &Client, repo: &str, name: &str) -> String {
    client
        .get_change(v1::GetChangeRequest {
            repo: repo.to_owned(),
            revision: name.to_owned(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .commit_id
}

fn now() -> SystemTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn signed(
    kind: &str,
    verdict: Verdict,
    target: Option<&str>,
    tree: &str,
    key: &SigningKey,
) -> SignedEvidence {
    let started = now();
    let claim = Claim {
        kind: kind.to_owned(),
        result: verdict,
        target: target.map(str::to_owned),
        started_on: started,
        finished_on: started + Duration::from_secs(1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    };
    SignedEvidence {
        envelope: claim
            .sign(&TreeDigest::git(tree).unwrap(), key)
            .unwrap()
            .to_bytes(),
        public_key: key.public_key().spki().to_vec(),
    }
}

/// Alice's executor key: what the policy trusts.
async fn trusted_key(client: &Client) -> SigningKey {
    client
        .issue_session_key("alice", Actor::Executor, None, "")
        .await
        .unwrap()
        .signing_key
}

/// `main` with the policy on a base commit; alice exists.
async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo, policy: &str) -> v1::Change {
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[("README.md", "base\n"), ("reef-policy.toml", policy)],
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    main
}

/// A change on `on` writing `files`, with passing `test` evidence on its
/// own tree, proposed and queued for `main` (not waited for).
async fn queue(
    client: &Client,
    repo: &str,
    on: &str,
    files: &[(&str, &str)],
    key: &SigningKey,
) -> v1::Change {
    let change = commit_on(client, repo, on, files).await;
    let signed = signed("test", Verdict::Pass, None, &change.tree_id, key);
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: String::new(),
            envelope: signed.envelope,
            public_key: signed.public_key,
            revision: String::new(),
        })
        .await
        .unwrap();
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    let response = client
        .land_change(LandChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            wait_ms: 0,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        response.landing.as_ref().unwrap().outcome(),
        Outcome::Queued,
        "{response:#?}"
    );
    change
}

/// The change's latest landing, once it is no longer queued.
async fn finished(client: &Client, repo: &str, change: &str) -> v1::Landing {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(response) = client
            .get_landing(GetLandingRequest {
                repo: repo.to_owned(),
                change: change.to_owned(),
                ..GetLandingRequest::default()
            })
            .await
        {
            let landing = response.landing.unwrap();
            if landing.outcome() != Outcome::Queued {
                return landing;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the landing never finished"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until `check` holds for the repository's newest batch.
async fn batch_until(
    client: &Client,
    repo: &str,
    check: impl Fn(&v1::LandingBatch) -> bool,
) -> v1::LandingBatch {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let batches = client
            .list_batches(
                ListBatchesRequest {
                    repo: repo.to_owned(),
                    ..ListBatchesRequest::default()
                },
                1,
            )
            .await
            .unwrap();
        if let Some(batch) = batches.items.into_iter().next()
            && check(&batch)
        {
            return batch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the batch never got there"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The in-test executor: claims `test` jobs and signs `fail` for each
/// requested pair on a tree with `bad.txt`, `pass` otherwise; `flaky`
/// jobs first complete FAILED with no evidence.
struct Executor {
    stop: Arc<AtomicBool>,
    jobs: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Executor {
    fn start(client: &Client, repo: &str, key: SigningKey, flaky: usize) -> Executor {
        let stop = Arc::new(AtomicBool::new(false));
        let jobs = Arc::new(AtomicUsize::new(0));
        let client = client.clone();
        let repo = repo.to_owned();
        let (stopping, count) = (Arc::clone(&stop), Arc::clone(&jobs));
        let task = tokio::spawn(async move {
            while !stopping.load(Ordering::SeqCst) {
                let Ok(response) = client
                    .claim_job(ClaimJobRequest {
                        executor: Some(ExecutorInfo {
                            name: "test-executor".to_owned(),
                            version: "1".to_owned(),
                            kinds: vec!["test".to_owned()],
                            labels: Vec::new(),
                        }),
                        protocol_version: 1,
                        repos: Vec::new(),
                        wait: Some(prost_types::Duration {
                            seconds: 1,
                            nanos: 0,
                        }),
                        lease: None,
                    })
                    .await
                else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let Some(job) = response.job else {
                    continue;
                };
                let n = count.fetch_add(1, Ordering::SeqCst);
                let spec = job.spec.clone().unwrap();
                if n < flaky {
                    let _ = client
                        .complete_job(CompleteJobRequest {
                            job_id: job.job_id.clone(),
                            claim_token: response.claim_token.clone(),
                            outcome: JobOutcome::Failed.into(),
                            evidence: Vec::new(),
                            usage: None,
                            message: "a broken runner".to_owned(),
                        })
                        .await;
                    continue;
                }
                let bad = client
                    .read_file(ReadFileRequest {
                        repo: repo.clone(),
                        revision: spec.commit_id.clone(),
                        path: "bad.txt".to_owned(),
                        skip_snapshot: true,
                        ..ReadFileRequest::default()
                    })
                    .await
                    .is_ok();
                let verdict = if bad { Verdict::Fail } else { Verdict::Pass };
                let targets: Vec<Option<&str>> = if spec.targets.is_empty() {
                    vec![None]
                } else {
                    spec.targets.iter().map(|t| Some(t.as_str())).collect()
                };
                let evidence = targets
                    .into_iter()
                    .map(|target| signed("test", verdict, target, &spec.tree_id, &key))
                    .collect();
                let _ = client
                    .complete_job(CompleteJobRequest {
                        job_id: job.job_id.clone(),
                        claim_token: response.claim_token.clone(),
                        outcome: JobOutcome::Done.into(),
                        evidence,
                        usage: None,
                        message: String::new(),
                    })
                    .await;
            }
        });
        Executor { stop, jobs, task }
    }

    fn jobs(&self) -> usize {
        self.jobs.load(Ordering::SeqCst)
    }

    async fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.task.await;
    }
}

// 3. A passing head lands every member in one operation.
#[tokio::test(flavor = "multi_thread")]
async fn batch_lands_on_pass() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(4, "1m", "")).await;
    let key = trusted_key(client).await;
    let executor = Executor::start(client, &repo.id, trusted_key(client).await, 0);
    let mut changes = Vec::new();
    for i in 0..4 {
        let file = format!("f{i}.txt");
        changes.push(
            queue(
                client,
                &repo.id,
                &main.commit_id,
                &[(file.as_str(), "x\n")],
                &key,
            )
            .await,
        );
    }
    let mut operations = Vec::new();
    let mut batches = Vec::new();
    for change in &changes {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        operations.push(landing.operation_id.clone());
        batches.push(landing.batch_id.clone());
        let landed = &landing.changes[0];
        assert!(!landed.tested_tree_id.is_empty(), "{landed:#?}");
        assert!(!landed.tested_commit_id.is_empty(), "{landed:#?}");
    }
    operations.dedup();
    batches.dedup();
    assert_eq!(operations.len(), 1, "one land operation: {operations:?}");
    assert_eq!(batches.len(), 1, "one batch: {batches:?}");
    let batch = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: batches[0].clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(batch.outcome(), v1::BatchOutcome::Landed, "{batch:#?}");
    assert_eq!(batch.landed_count, 4);
    assert_eq!(batch.trials.len(), 1, "only the head was tested");
    let trial = &batch.trials[0];
    assert_eq!(trial.prefix, 4);
    assert_eq!(trial.result(), v1::TrialResult::Passed);
    assert_eq!(trial.job_ids.len(), 1, "one job, for the one kind");
    // The head tree is main's tree now, and every landing names it.
    let head = client
        .get_change(v1::GetChangeRequest {
            repo: repo.id.clone(),
            revision: "main".to_owned(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(head.tree_id, trial.tree_id);
    // The land operation names the batch, and the trial commit is gone
    // from the log.
    let op = client
        .get_operation(&repo.id, &operations[0])
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(op.kind(), OperationKind::Land);
    assert_eq!(op.batch_id, batch.id);
    let trial_change = client
        .get_change(v1::GetChangeRequest {
            repo: repo.id.clone(),
            revision: trial.commit_id.clone(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert!(trial_change.hidden, "{trial_change:#?}");
    // The trial operation is not undoable.
    let undo = client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: trial.operation_id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(undo.reason(), "OPERATION_NOT_UNDOABLE", "{undo:?}");
    assert_eq!(executor.jobs(), 1);
    executor.stop().await;
    daemon.stop().await;
}

// 4. A fast-forward batch of one needs no job.
#[tokio::test(flavor = "multi_thread")]
async fn fast_forward_needs_no_job() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(1, "1m", "")).await;
    let key = trusted_key(client).await;
    let change = queue(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], &key).await;
    let landing = finished(client, &repo.id, &change.change_id).await;
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let batch = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            change: change.change_id.clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(batch.trials.len(), 1);
    assert!(batch.trials[0].job_ids.is_empty(), "{batch:#?}");
    assert!(batch.trials[0].commit_id.is_empty(), "no trial commit");
    assert_eq!(landing.changes[0].tested_tree_id, change.tree_id);
    daemon.stop().await;
}

// 5 and 6. Bisect finds the culprit; the passing prefix lands, the rest are
// requeued and land in the next batch.
#[tokio::test(flavor = "multi_thread")]
async fn bisect_finds_the_culprit() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(8, "1m", "")).await;
    let key = trusted_key(client).await;
    let executor = Executor::start(client, &repo.id, trusted_key(client).await, 0);
    let mut changes = Vec::new();
    for i in 0..8 {
        let file = if i == 5 {
            "bad.txt".to_owned()
        } else {
            format!("f{i}.txt")
        };
        changes.push(
            queue(
                client,
                &repo.id,
                &main.commit_id,
                &[(file.as_str(), "x\n")],
                &key,
            )
            .await,
        );
    }
    let culprit = finished(client, &repo.id, &changes[5].change_id).await;
    assert_eq!(culprit.outcome(), Outcome::Dequeued, "{culprit:#?}");
    assert_eq!(culprit.reason(), Reason::Checks);
    let tested = culprit.changes[0].tested_evaluation.as_ref().unwrap();
    assert!(
        tested
            .rules
            .iter()
            .any(|rule| rule.failed.iter().any(|need| need.kind == "test")),
        "{tested:#?}"
    );
    let batch = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: culprit.batch_id.clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(batch.outcome(), v1::BatchOutcome::Split, "{batch:#?}");
    assert_eq!(batch.culprit_position, 6);
    assert_eq!(batch.landed_count, 5);
    let prefixes: Vec<u32> = batch.trials.iter().map(|trial| trial.prefix).collect();
    assert_eq!(prefixes, [8, 4, 6, 5], "the head, then binary search");
    for (index, change) in changes.iter().enumerate() {
        if index == 5 {
            continue;
        }
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{index}: {landing:#?}");
        if index > 5 {
            assert_ne!(landing.batch_id, batch.id, "requeued, then landed next");
        }
    }
    executor.stop().await;
    daemon.stop().await;
}

// 7. A job that ran but gave no verdict is asked again; one nobody claims
// makes the batch unavailable.
#[tokio::test(flavor = "multi_thread")]
async fn infrastructure() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(2, "1m", "")).await;
    let key = trusted_key(client).await;

    // Nobody claims: the job expires after the queue timeout.
    let one = queue(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], &key).await;
    let two = queue(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")], &key).await;
    batch_until(client, &repo.id, |batch| {
        batch.trials.iter().any(|trial| !trial.job_ids.is_empty())
    })
    .await;
    let executor = &daemon.handle.state().executor;
    executor.advance(reef_daemon::landing::BATCH_QUEUE_TIMEOUT + Duration::from_mins(1));
    executor.tick();
    for change in [&one, &two] {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Dequeued, "{landing:#?}");
        assert_eq!(landing.reason(), Reason::Unavailable);
        assert!(
            landing.message.starts_with("no executor claimed"),
            "{}",
            landing.message
        );
    }

    // A broken runner: the job is asked for again, then passes.
    let flaky = Executor::start(client, &repo.id, trusted_key(client).await, 1);
    let head = head_of(client, &repo.id, "main").await;
    let three = queue(client, &repo.id, &head, &[("c.txt", "c\n")], &key).await;
    let four = queue(client, &repo.id, &head, &[("d.txt", "d\n")], &key).await;
    for change in [&three, &four] {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    }
    let batch = batch_until(client, &repo.id, |batch| {
        batch.outcome() == v1::BatchOutcome::Landed
    })
    .await;
    assert_eq!(batch.trials[0].attempts, 2, "{batch:#?}");
    assert_eq!(flaky.jobs(), 2);
    flaky.stop().await;
    daemon.stop().await;
}

// 8 and 11. A moved destination re-forms the batch, and a restart
// re-forms it, reusing the trial commit and the open job.
#[tokio::test(flavor = "multi_thread")]
async fn reforms_and_restart() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(2, "1m", "")).await;
    let key = trusted_key(client).await;
    let one = queue(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], &key).await;
    let two = queue(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")], &key).await;
    let first = batch_until(client, &repo.id, |batch| {
        batch.trials.iter().any(|trial| !trial.job_ids.is_empty())
    })
    .await;

    // Someone moves main: the batch re-forms on the new head.
    let moved = commit_on(client, &repo.id, &main.commit_id, &[("m.txt", "m\n")]).await;
    bookmark(&repo, "main", &moved.commit_id).await;
    let second = batch_until(client, &repo.id, |batch| {
        batch.id != first.id && batch.trials.iter().any(|trial| !trial.job_ids.is_empty())
    })
    .await;
    assert_eq!(second.base_commit_id, moved.commit_id);
    let old = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: first.id.clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(old.outcome(), v1::BatchOutcome::Reformed, "{old:#?}");

    // A restart: the open batch is interrupted, and the next one reuses
    // its trial commit and its job.
    let daemon = daemon.restart().await;
    let client = &daemon.client;
    let interrupted = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: second.id.clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(interrupted.outcome(), v1::BatchOutcome::Interrupted);
    let third = batch_until(client, &repo.id, |batch| {
        batch.id != second.id && batch.trials.iter().any(|trial| !trial.job_ids.is_empty())
    })
    .await;
    assert_eq!(third.trials[0].commit_id, second.trials[0].commit_id);
    assert_eq!(third.trials[0].job_ids, second.trials[0].job_ids);
    let executor = Executor::start(client, &repo.id, trusted_key(client).await, 0);
    for change in [&one, &two] {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    }
    assert_eq!(executor.jobs(), 1, "the job asked for before the restart");
    executor.stop().await;
    daemon.stop().await;
}

// 9 and 10. An `alone` path lands in a batch of its own; under
// `checks = "proposal"` a rebased batch lands with no job.
#[tokio::test(flavor = "multi_thread")]
async fn alone_and_proposal_checks() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(
        &daemon,
        &repo,
        &policy(
            3,
            "5s",
            "checks = \"proposal\"\nalone = [\"migrations/**\"]",
        ),
    )
    .await;
    let key = trusted_key(client).await;
    let one = queue(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], &key).await;
    let migration = queue(
        client,
        &repo.id,
        &main.commit_id,
        &[("migrations/1.sql", "x\n")],
        &key,
    )
    .await;
    let three = queue(client, &repo.id, &main.commit_id, &[("c.txt", "c\n")], &key).await;
    let mut batch_ids = Vec::new();
    for change in [&one, &migration, &three] {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        assert_eq!(landing.changes[0].tested_tree_id, "");
        batch_ids.push(landing.batch_id);
    }
    assert_ne!(batch_ids[0], batch_ids[1], "the migration lands alone");
    assert_ne!(batch_ids[1], batch_ids[2]);
    let batches = client
        .list_batches(
            ListBatchesRequest {
                repo: repo.id.clone(),
                ..ListBatchesRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert!(
        batches
            .items
            .iter()
            .all(|batch| batch.checks == "proposal" && batch.trials.is_empty()),
        "{batches:#?}"
    );
    let open = client
        .list_batches(
            ListBatchesRequest {
                repo: repo.id.clone(),
                open_only: true,
                ..ListBatchesRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(open.items.len(), 0, "{:#?}", open.items);
    let missing = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: "bat_0000000000000000".to_owned(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(missing.reason(), "BATCH_NOT_FOUND");
    daemon.stop().await;
}

// 8. A member withdrawn while its batch tests re-forms the batch without
// it; the rest land.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_member_reforms() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, &policy(2, "1m", "")).await;
    let key = trusted_key(client).await;
    let one = queue(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], &key).await;
    let two = queue(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")], &key).await;
    let first = batch_until(client, &repo.id, |batch| {
        batch.trials.iter().any(|trial| !trial.job_ids.is_empty())
    })
    .await;
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.id.clone(),
            revision: two.commit_id.clone(),
            to: v1::LifecycleState::Withdrawn.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    let left = finished(client, &repo.id, &two.change_id).await;
    assert_eq!(left.outcome(), Outcome::Cancelled, "{left:#?}");
    let executor = Executor::start(client, &repo.id, trusted_key(client).await, 0);
    let landing = finished(client, &repo.id, &one.change_id).await;
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    assert_ne!(landing.batch_id, first.id);
    let old = client
        .get_batch(GetBatchRequest {
            repo: repo.id.clone(),
            id: first.id.clone(),
            ..GetBatchRequest::default()
        })
        .await
        .unwrap()
        .batch
        .unwrap();
    assert_eq!(old.outcome(), v1::BatchOutcome::Reformed, "{old:#?}");
    assert_eq!(
        old.members[1].result(),
        v1::BatchMemberResult::Left,
        "{old:#?}"
    );
    executor.stop().await;
    daemon.stop().await;
}
