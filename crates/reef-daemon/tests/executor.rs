//! The executor protocol through the daemon (R3.05, ADR 0059; tests 2 to
//! 12 of the brief): submissions and their refusals, claims and matching,
//! leases, lapses and deadlines on a moved clock, evidence acceptance and
//! completeness, cancellation, the remote listener's rules, secrets, GC
//! and forget, and a fake executor running the loop of
//! `docs/executor-protocol.md` over the socket and the remote listener.
//!
//! No test sleeps for a lease: the queue's clock moves with
//! `Executor::advance`, and `Executor::tick` applies what came due.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

use reef_api::v1::{
    self, CancelJobRequest, ClaimJobRequest, CompleteJobRequest, CreateIdentityRequest,
    CreateWorkspaceRequest, ExecutorInfo, GetJobRequest, IdentityKind, Job, JobOutcome, JobState,
    ListEvidenceRequest, ListJobsRequest, PresenceKind, RemoteScope, SignedEvidence,
    SubmitJobRequest, SubscribePresenceRequest, UpdateJobRequest, Workspace, submit_job_request,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_evidence::session::Actor;
use reef_evidence::{Artifact, Claim, Outcome, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNER: &str = "tester@host";

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
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

fn submit_request(ws: &Workspace, kinds: &[&str], targets: &[&str]) -> SubmitJobRequest {
    SubmitJobRequest {
        workspace: ws.id.clone(),
        source: Some(submit_job_request::Source::Revision("@".to_owned())),
        kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
        ..SubmitJobRequest::default()
    }
}

async fn submit(client: &Client, request: SubmitJobRequest) -> (Job, bool) {
    let response = client.submit_job(request).await.unwrap();
    (response.job.unwrap(), response.created)
}

fn executor(name: &str, kinds: &[&str], labels: &[&str]) -> ExecutorInfo {
    ExecutorInfo {
        name: name.to_owned(),
        version: "1.0".to_owned(),
        kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
    }
}

fn claim_request(info: ExecutorInfo, wait: Duration) -> ClaimJobRequest {
    ClaimJobRequest {
        executor: Some(info),
        protocol_version: 1,
        repos: Vec::new(),
        wait: Some(prost_types::Duration {
            seconds: i64::try_from(wait.as_secs()).unwrap(),
            nanos: i32::try_from(wait.subsec_nanos()).unwrap(),
        }),
        lease: None,
    }
}

/// Claims at once; `None` when nothing matched.
async fn claim(client: &Client, info: ExecutorInfo) -> Option<(Job, String)> {
    let response = client
        .claim_job(claim_request(info, Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(response.protocol_version, 1);
    response.job.map(|job| (job, response.claim_token))
}

async fn get(client: &Client, id: &str) -> Job {
    client
        .get_job(GetJobRequest {
            repo: String::new(),
            job_id: id.to_owned(),
        })
        .await
        .unwrap()
        .job
        .unwrap()
}

fn reason(error: &ClientError) -> (String, String) {
    let meta = error.metadata();
    let key = ["field", "state", "rule", "limit", "missing_count"]
        .into_iter()
        .find(|key| meta.contains_key(*key))
        .unwrap_or("");
    (
        error.reason().to_owned(),
        meta.get(key).cloned().unwrap_or_default(),
    )
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32]).unwrap()
}

fn claim_of(kind: &str, result: Outcome, target: Option<&str>, started: SystemTime) -> Claim {
    Claim {
        kind: kind.to_owned(),
        result,
        target: target.map(str::to_owned),
        started_on: started,
        finished_on: started + Duration::from_secs(1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
}

fn signed(claim: &Claim, tree: &str, key: &SigningKey) -> SignedEvidence {
    SignedEvidence {
        envelope: claim
            .sign(&TreeDigest::git(tree).unwrap(), key)
            .unwrap()
            .to_bytes(),
        public_key: key.public_key().spki().to_vec(),
    }
}

fn now() -> SystemTime {
    // Whole seconds, so `startedOn` round-trips.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

async fn update(
    client: &Client,
    job: &Job,
    token: &str,
    evidence: Vec<SignedEvidence>,
) -> Result<v1::UpdateJobResponse, ClientError> {
    client
        .update_job(UpdateJobRequest {
            job_id: job.job_id.clone(),
            claim_token: token.to_owned(),
            progress: None,
            evidence,
            release: false,
        })
        .await
}

async fn complete(
    client: &Client,
    job: &Job,
    token: &str,
    outcome: JobOutcome,
    evidence: Vec<SignedEvidence>,
) -> Result<v1::CompleteJobResponse, ClientError> {
    client
        .complete_job(CompleteJobRequest {
            job_id: job.job_id.clone(),
            claim_token: token.to_owned(),
            outcome: outcome.into(),
            evidence,
            usage: None,
            message: String::new(),
        })
        .await
}

fn advance(daemon: &TestDaemon, by: Duration) {
    let executor = &daemon.handle.state().executor;
    executor.advance(by);
    executor.tick();
}

fn tree(job: &Job) -> String {
    job.spec.as_ref().unwrap().tree_id.clone()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

// 2
#[expect(clippy::too_many_lines, reason = "every submission rule in turn")]
#[tokio::test(flavor = "multi_thread")]
async fn submit_and_refusals() {
    let (daemon, repo) = fixture_with(|config| config.file.executor.max_open_jobs = 4).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    // `@` after an unsnapshotted write includes it.
    std::fs::write(std::path::Path::new(&ws.path).join("new.txt"), "x\n").unwrap();
    let (job, created) = submit(client, submit_request(&ws, &["test", "lint"], &["b", "a"])).await;
    assert!(created);
    assert!(job.job_id.starts_with("job_") && job.job_id.len() == 20);
    let spec = job.spec.clone().unwrap();
    assert_eq!(spec.kinds, ["lint", "test"]);
    assert_eq!(spec.targets, ["a", "b"]);
    assert!(!spec.commit_id.is_empty() && !spec.change_id.is_empty());
    assert_eq!(
        spec.change_id, ws.change_id,
        "the workspace's change, snapshotted first"
    );
    assert_eq!(job.state(), JobState::Queued);
    assert_eq!(job.max_attempts, 3);
    assert_eq!(job.protocol_version, 1);
    let wall = spec.budget.unwrap().wall_time.unwrap();
    assert_eq!(wall.seconds, 1800);

    // By tree: neither commit nor change.
    let by_tree = SubmitJobRequest {
        workspace: String::new(),
        repo: repo.id.clone(),
        source: Some(submit_job_request::Source::TreeId(spec.tree_id.clone())),
        kinds: vec!["build".to_owned()],
        ..SubmitJobRequest::default()
    };
    let (other, _) = submit(client, by_tree.clone()).await;
    let other_spec = other.spec.unwrap();
    assert!(other_spec.commit_id.is_empty() && other_spec.change_id.is_empty());

    // Dedupe: the open one, priority raised; `duplicate` makes another.
    let (again, created) = submit(
        client,
        SubmitJobRequest {
            priority: 7,
            ..by_tree.clone()
        },
    )
    .await;
    assert!(!created);
    assert_eq!(again.job_id, other.job_id);
    assert_eq!(again.spec.unwrap().priority, 7);
    let (dup, created) = submit(
        client,
        SubmitJobRequest {
            duplicate: true,
            ..by_tree.clone()
        },
    )
    .await;
    assert!(created && dup.job_id != other.job_id);
    // A finished job does not dedupe.
    client
        .cancel_job(CancelJobRequest {
            repo: String::new(),
            job_id: dup.job_id.clone(),
            reason: String::new(),
        })
        .await
        .unwrap();
    client
        .cancel_job(CancelJobRequest {
            repo: String::new(),
            job_id: other.job_id.clone(),
            reason: String::new(),
        })
        .await
        .unwrap();
    let (fresh_job, created) = submit(client, by_tree.clone()).await;
    assert!(created && fresh_job.job_id != other.job_id && fresh_job.job_id != dup.job_id);

    // Refusals, each with its field.
    let refused = |request: SubmitJobRequest| async move {
        reason(&client.submit_job(request).await.unwrap_err())
    };
    let base = || submit_request(&ws, &["test"], &[]);
    let spec_field = |field: &str| ("INVALID_JOB_SPEC".to_owned(), field.to_owned());
    assert_eq!(
        refused(SubmitJobRequest {
            kinds: (0..17).map(|i| format!("k{i}")).collect(),
            ..base()
        })
        .await,
        spec_field("kinds")
    );
    assert_eq!(
        refused(SubmitJobRequest {
            targets: (0..1001).map(|i| format!("t{i}")).collect(),
            ..base()
        })
        .await,
        spec_field("targets")
    );
    assert_eq!(
        refused(SubmitJobRequest {
            targets: vec!["a\nb".into()],
            ..base()
        })
        .await,
        spec_field("targets")
    );
    assert_eq!(
        refused(SubmitJobRequest {
            kinds: vec!["a".into(), "b".into()],
            targets: (0..501).map(|i| format!("t{i}")).collect(),
            ..base()
        })
        .await,
        spec_field("pairs")
    );
    assert_eq!(
        refused(SubmitJobRequest {
            budget: Some(v1::Budget {
                wall_time: Some(prost_types::Duration {
                    seconds: 7 * 3600,
                    nanos: 0
                }),
                cpu_time: None,
            }),
            ..base()
        })
        .await,
        spec_field("wall_time")
    );
    assert_eq!(
        refused(SubmitJobRequest {
            requirements: vec!["no-equals".into()],
            ..base()
        })
        .await,
        spec_field("requirements")
    );
    let missing_tree = SubmitJobRequest {
        source: Some(submit_job_request::Source::TreeId("ab".repeat(20))),
        ..by_tree.clone()
    };
    assert_eq!(refused(missing_tree).await.0, "TREE_NOT_FOUND");
    let bad_tree = SubmitJobRequest {
        source: Some(submit_job_request::Source::TreeId("XYZ".into())),
        ..by_tree.clone()
    };
    assert_eq!(refused(bad_tree).await, spec_field("tree_id"));
    let no_tree = SubmitJobRequest {
        source: None,
        ..by_tree.clone()
    };
    assert_eq!(refused(no_tree).await, spec_field("tree_id"));

    // The cap: 2 open now; two more fit, the next is refused.
    for _ in 0..2 {
        submit(
            client,
            SubmitJobRequest {
                duplicate: true,
                ..by_tree.clone()
            },
        )
        .await;
    }
    let error = client
        .submit_job(SubmitJobRequest {
            duplicate: true,
            ..by_tree.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), ("TOO_MANY_JOBS".into(), "4".into()));

    // A queued entry, by the daemon, with the change.
    let entries = daemon
        .handle
        .state()
        .presence
        .current(&reef_daemon::presence::PresenceFilter::default());
    let entry = entries
        .iter()
        .map(|change| &change.presence)
        .find(|p| p.job.as_ref().is_some_and(|j| j.job_id == job.job_id))
        .expect("the job's presence entry");
    assert_eq!(entry.kind(), PresenceKind::Job);
    assert!(entry.by_daemon);
    assert_eq!(entry.change_id, ws.change_id);
    assert_eq!(entry.job.as_ref().unwrap().state(), JobState::Queued);
    daemon.stop().await;
}

// 3
#[expect(
    clippy::too_many_lines,
    reason = "matching, order, the long poll, contention, then refusals"
)]
#[tokio::test(flavor = "multi_thread")]
async fn claims_match_and_order() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;

    // Nothing queued: at once, with the protocol version.
    let response = client
        .claim_job(claim_request(executor("e", &["test"], &[]), Duration::ZERO))
        .await
        .unwrap();
    assert!(response.job.is_none() && response.claim_token.is_empty());
    assert_eq!(response.protocol_version, 1);

    let (low, _) = submit(client, submit_request(&ws, &["test"], &["a"])).await;
    let (high, _) = submit(
        client,
        SubmitJobRequest {
            priority: 5,
            ..submit_request(&ws, &["test"], &["b"])
        },
    )
    .await;
    let (linux, _) = submit(
        client,
        SubmitJobRequest {
            requirements: vec!["os=linux".into()],
            ..submit_request(&ws, &["test"], &["c"])
        },
    )
    .await;
    let (both, _) = submit(client, submit_request(&ws, &["test", "lint"], &["d"])).await;

    // Kinds must all be the executor's; requirements its labels.
    let (first, _) = claim(client, executor("e1", &["test"], &[])).await.unwrap();
    assert_eq!(first.job_id, high.job_id, "priority first");
    let (second, _) = claim(client, executor("e2", &["test"], &[])).await.unwrap();
    assert_eq!(second.job_id, low.job_id, "then submission order");
    assert!(
        claim(client, executor("e3", &["test"], &[]))
            .await
            .is_none(),
        "the linux job needs os=linux, the other lint"
    );
    let (third, _) = claim(client, executor("e4", &["test"], &["os=linux"]))
        .await
        .unwrap();
    assert_eq!(third.job_id, linux.job_id);
    // Another repository only: nothing.
    let other = ClaimJobRequest {
        repos: vec!["ffffffffffffffff".into()],
        ..claim_request(executor("e5", &["test", "lint"], &[]), Duration::ZERO)
    };
    assert_eq!(
        client.claim_job(other).await.unwrap_err().reason(),
        "REPOSITORY_NOT_FOUND"
    );
    let mine = ClaimJobRequest {
        repos: vec![repo.id.clone()],
        ..claim_request(executor("e5", &["lint", "test"], &[]), Duration::ZERO)
    };
    let fourth = client.claim_job(mine).await.unwrap().job.unwrap();
    assert_eq!(fourth.job_id, both.job_id);
    let running = get(client, &fourth.job_id).await;
    assert_eq!(running.state(), JobState::Running);
    assert_eq!(running.attempt, 1);
    assert_eq!(running.executor.unwrap().info.unwrap().name, "e5");

    // A long poll returns soon after a submit.
    let mut samples = Vec::new();
    for round in 0..5 {
        let waiting = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .claim_job(claim_request(
                        executor("poller", &["build"], &[]),
                        Duration::from_secs(30),
                    ))
                    .await
                    .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        submit(
            client,
            submit_request(&ws, &["build"], &[&format!("poll-{round}")]),
        )
        .await;
        let claimed = waiting.await.unwrap();
        samples.push(started.elapsed());
        assert!(claimed.job.is_some());
    }
    let latency = median(samples);
    println!("claim latency after a submit: median {latency:?}");
    assert!(latency < Duration::from_secs(2), "{latency:?}");

    // Two claims of one job: one wins.
    let (contended, _) = submit(client, submit_request(&ws, &["fmt"], &[])).await;
    let (a, b) = tokio::join!(
        client.claim_job(claim_request(executor("a", &["fmt"], &[]), Duration::ZERO)),
        client.claim_job(claim_request(executor("b", &["fmt"], &[]), Duration::ZERO)),
    );
    let won: Vec<String> = [a.unwrap(), b.unwrap()]
        .into_iter()
        .filter_map(|response| response.job.map(|job| job.job_id))
        .collect();
    assert_eq!(won, [contended.job_id]);

    // Bad executor fields.
    let refused = |request: ClaimJobRequest| async move {
        reason(&client.claim_job(request).await.unwrap_err())
    };
    let field = |field: &str| ("INVALID_EXECUTOR".to_owned(), field.to_owned());
    let good = || claim_request(executor("e", &["test"], &[]), Duration::ZERO);
    assert_eq!(
        refused(ClaimJobRequest {
            executor: Some(executor("", &["test"], &[])),
            ..good()
        })
        .await,
        field("name")
    );
    assert_eq!(
        refused(ClaimJobRequest {
            executor: Some(executor("e", &[], &[])),
            ..good()
        })
        .await,
        field("kinds")
    );
    assert_eq!(
        refused(ClaimJobRequest {
            executor: Some(executor("e", &["test"], &["nolabel"])),
            ..good()
        })
        .await,
        field("labels")
    );
    assert_eq!(
        refused(ClaimJobRequest {
            protocol_version: 0,
            ..good()
        })
        .await,
        field("protocol_version")
    );
    assert_eq!(
        refused(claim_request(
            executor("e", &["test"], &[]),
            Duration::from_secs(61)
        ))
        .await,
        field("wait")
    );
    assert_eq!(
        refused(ClaimJobRequest {
            lease: Some(prost_types::Duration {
                seconds: 5,
                nanos: 0
            }),
            ..good()
        })
        .await,
        field("lease")
    );
    daemon.stop().await;
}

// 4
#[expect(
    clippy::too_many_lines,
    reason = "one job through its lapses, then a restart"
)]
#[tokio::test(flavor = "multi_thread")]
async fn leases_lapse_and_restart() {
    let (daemon, repo) = fixture().await;
    let client = daemon.client.clone();
    let ws = create(&client, &repo.id).await;
    let mut presence = client
        .subscribe_presence(SubscribePresenceRequest {
            kinds: vec![PresenceKind::Job.into()],
            ..SubscribePresenceRequest::default()
        })
        .await
        .unwrap();
    let (job, _) = submit(&client, submit_request(&ws, &["test"], &[])).await;
    let info = executor("runner", &["test"], &[]);
    let (_, token) = claim(&client, info.clone()).await.unwrap();

    // An update extends the lease past where it was.
    advance(&daemon, Duration::from_secs(40));
    update(&client, &job, &token, Vec::new()).await.unwrap();
    advance(&daemon, Duration::from_secs(40));
    assert_eq!(get(&client, &job.job_id).await.state(), JobState::Running);

    // A lapse requeues: attempt 2, the old token lapsed.
    advance(&daemon, Duration::from_secs(25));
    let requeued = get(&client, &job.job_id).await;
    assert_eq!(requeued.state(), JobState::Queued);
    assert_eq!(requeued.attempt, 1);
    let error = update(&client, &job, &token, Vec::new()).await.unwrap_err();
    assert_eq!(reason(&error), ("JOB_NOT_CLAIMED".into(), "lapsed".into()));

    // Release requeues at once.
    let (again, token) = claim(&client, info.clone()).await.unwrap();
    assert_eq!(again.attempt, 2);
    let released = client
        .update_job(UpdateJobRequest {
            job_id: job.job_id.clone(),
            claim_token: token,
            progress: None,
            evidence: vec![signed(
                &claim_of("test", Outcome::Error, None, now()),
                &tree(&job),
                &key(6),
            )],
            release: true,
        })
        .await
        .unwrap();
    assert_eq!(released.accepted.len(), 1, "stored before the release");
    let requeued = get(&client, &job.job_id).await;
    assert_eq!(requeued.state(), JobState::Queued);
    assert_eq!(requeued.evidence_count, 1);

    // The third attempt's lapse expires it.
    let (third, _) = claim(&client, info.clone()).await.unwrap();
    assert_eq!(third.attempt, 3);
    advance(&daemon, Duration::from_secs(61));
    let expired = get(&client, &job.job_id).await;
    assert_eq!(expired.state(), JobState::Expired);
    assert_eq!(expired.end_reason(), v1::JobEndReason::Attempts);
    assert!(expired.elapsed_ms > 0);

    // The entry followed, and its end carries the terminal state.
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    'events: while Instant::now() < deadline {
        let frame = tokio::time::timeout(Duration::from_secs(5), presence.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        for event in frame.events {
            let presence = event.presence.unwrap();
            let Some(entry) = presence.job else { continue };
            if entry.job_id != job.job_id {
                continue;
            }
            seen.push(entry.state());
            if event.ended {
                assert_eq!(entry.state(), JobState::Expired);
                assert_eq!(entry.end_reason(), v1::JobEndReason::Attempts);
                break 'events;
            }
        }
    }
    // The hub folds changes within 50 ms into one broadcast, so only the
    // first state and the end are certain.
    assert_eq!(seen.last(), Some(&JobState::Expired));

    // A restart keeps queued and running jobs, and the executor goes on.
    let (kept, _) = submit(&client, submit_request(&ws, &["lint"], &[])).await;
    let (held, _) = submit(&client, submit_request(&ws, &["fmt"], &[])).await;
    let (_, held_token) = claim(&client, executor("r2", &["fmt"], &[])).await.unwrap();
    // While the daemon is down, the held lease runs out: the start must
    // extend it, or the timer's first tick would lapse the job.
    let stopped = daemon.stop_keeping().await;
    let db = rusqlite::Connection::open(stopped.config.paths.db.clone()).unwrap();
    db.execute(
        "UPDATE jobs SET lease_expires_at = 1 WHERE id = ?1",
        [&held.job_id],
    )
    .unwrap();
    drop(db);
    let daemon = stopped.start().await;
    let client = daemon.client.clone();
    daemon.handle.state().executor.tick();
    assert_eq!(get(&client, &kept.job_id).await.state(), JobState::Queued);
    let resumed = get(&client, &held.job_id).await;
    assert_eq!(resumed.state(), JobState::Running);
    // The test's clock offset went with the old daemon; the lease is now
    // at least one lease ahead of the wall clock.
    let wall = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        resumed.lease_expires_at.unwrap().seconds > i64::try_from(wall).unwrap(),
        "extended at start"
    );
    update(&client, &held, &held_token, Vec::new())
        .await
        .unwrap();
    let entries = daemon
        .handle
        .state()
        .presence
        .current(&reef_daemon::presence::PresenceFilter::default());
    let republished: Vec<String> = entries
        .iter()
        .filter_map(|change| change.presence.job.as_ref().map(|j| j.job_id.clone()))
        .collect();
    assert!(republished.contains(&kept.job_id) && republished.contains(&held.job_id));
    daemon.stop().await;
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn deadlines_and_queue_timeouts() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let (waiting, _) = submit(
        client,
        SubmitJobRequest {
            queue_timeout: Some(prost_types::Duration {
                seconds: 120,
                nanos: 0,
            }),
            ..submit_request(&ws, &["lint"], &[])
        },
    )
    .await;
    let (running, _) = submit(
        client,
        SubmitJobRequest {
            budget: Some(v1::Budget {
                wall_time: Some(prost_types::Duration {
                    seconds: 100,
                    nanos: 0,
                }),
                cpu_time: None,
            }),
            ..submit_request(&ws, &["test"], &[])
        },
    )
    .await;
    let lease = prost_types::Duration {
        seconds: 600,
        nanos: 0,
    };
    let claimed = client
        .claim_job(ClaimJobRequest {
            lease: Some(lease),
            ..claim_request(executor("slow", &["test"], &[]), Duration::ZERO)
        })
        .await
        .unwrap();
    let token = claimed.claim_token;
    let job = claimed.job.unwrap();
    let deadline = job.deadline_at.unwrap().seconds - job.claimed_at.unwrap().seconds;
    assert_eq!(deadline, 160, "wall time and the 60 s grace");

    advance(&daemon, Duration::from_secs(121));
    let timed_out = get(client, &waiting.job_id).await;
    assert_eq!(timed_out.state(), JobState::Expired);
    assert_eq!(timed_out.end_reason(), v1::JobEndReason::QueueTimeout);
    assert_eq!(
        get(client, &running.job_id).await.state(),
        JobState::Running
    );

    advance(&daemon, Duration::from_secs(40));
    let past = get(client, &running.job_id).await;
    assert_eq!(past.state(), JobState::Expired);
    assert_eq!(past.end_reason(), v1::JobEndReason::Deadline);
    let error = update(client, &job, &token, Vec::new()).await.unwrap_err();
    assert_eq!(reason(&error), ("JOB_NOT_CLAIMED".into(), "expired".into()));

    // A requeue keeps the queue expiry, extended to a lease from the
    // requeue.
    let (late, _) = submit(
        client,
        SubmitJobRequest {
            queue_timeout: Some(prost_types::Duration {
                seconds: 60,
                nanos: 0,
            }),
            ..submit_request(&ws, &["fmt"], &[])
        },
    )
    .await;
    let claimed = client
        .claim_job(ClaimJobRequest {
            lease: Some(prost_types::Duration {
                seconds: 120,
                nanos: 0,
            }),
            ..claim_request(executor("x", &["fmt"], &[]), Duration::ZERO)
        })
        .await
        .unwrap();
    assert!(claimed.job.is_some());
    advance(&daemon, Duration::from_secs(121));
    let requeued = get(client, &late.job_id).await;
    assert_eq!(requeued.state(), JobState::Queued);
    let expiry = requeued.queue_expires_at.unwrap().seconds;
    let submitted = requeued.submitted_at.unwrap().seconds;
    assert!(
        expiry - submitted >= 120 + 120,
        "a lease from the requeue, not the original minute"
    );
    daemon.stop().await;
}

// 6
#[expect(clippy::too_many_lines, reason = "each acceptance rule in turn")]
#[tokio::test(flavor = "multi_thread")]
async fn evidence_is_bound_to_the_job() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let (job, _) = submit(client, submit_request(&ws, &["test"], &["a"])).await;
    let tree = tree(&job);
    let (_, token) = claim(client, executor("r", &["test", "lint"], &[]))
        .await
        .unwrap();
    let session = client
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    let signer = &session.signing_key;
    let started = now();

    // Accepted, and read BOUND three ways.
    let accepted = update(
        client,
        &job,
        &token,
        vec![signed(
            &claim_of("test", Outcome::Pass, Some("a"), started),
            &tree,
            signer,
        )],
    )
    .await
    .unwrap();
    let row = &accepted.accepted[0];
    assert!(row.created && row.fresh);
    assert_eq!(row.result(), v1::EvidenceResult::Pass);
    let evidence_id = row.evidence_id.clone();
    let read = get(client, &job.job_id).await;
    assert_eq!(read.evidence_count, 1);
    assert_eq!(read.coverage[0].evidence_id, evidence_id);
    assert_eq!(read.coverage[0].signature(), v1::SignatureCheck::Valid);
    for request in [
        ListEvidenceRequest {
            repo: repo.id.clone(),
            job_id: job.job_id.clone(),
            ..ListEvidenceRequest::default()
        },
        ListEvidenceRequest {
            repo: repo.id.clone(),
            tree_id: tree.clone(),
            ..ListEvidenceRequest::default()
        },
    ] {
        let listed = client.list_evidence(request, 0).await.unwrap().items;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].evidence_id, evidence_id);
        assert_eq!(
            listed[0].identity.as_ref().unwrap().status(),
            v1::IdentityStatus::Bound
        );
        assert!(listed[0].change_ids.is_empty(), "no link to the change");
    }

    // The binding.
    let other_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let mismatch = update(
        client,
        &job,
        &token,
        vec![signed(
            &claim_of("test", Outcome::Pass, None, started),
            other_tree,
            signer,
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(
        reason(&mismatch),
        ("JOB_EVIDENCE_MISMATCH".into(), "tree".into())
    );
    assert_eq!(mismatch.metadata()["index"], "0");
    let kind = update(
        client,
        &job,
        &token,
        vec![signed(
            &claim_of("lint", Outcome::Pass, None, started),
            &tree,
            signer,
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(
        reason(&kind),
        ("JOB_EVIDENCE_MISMATCH".into(), "kind".into())
    );

    // A broken envelope, a forged signature, and all or nothing.
    let good = signed(
        &claim_of("test", Outcome::Fail, Some("b"), started),
        &tree,
        signer,
    );
    let broken = SignedEvidence {
        envelope: b"{}".to_vec(),
        public_key: signer.public_key().spki().to_vec(),
    };
    let error = update(client, &job, &token, vec![good.clone(), broken])
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "INVALID_EVIDENCE");
    assert_eq!(error.metadata()["index"], "1");
    // Another claim's signature in this envelope: the key id matches, the
    // signature does not.
    let forged = {
        let other = signed(
            &claim_of("test", Outcome::Pass, Some("z"), started),
            &tree,
            signer,
        );
        let mut envelope: serde_json::Value = serde_json::from_slice(&good.envelope).unwrap();
        let theirs: serde_json::Value = serde_json::from_slice(&other.envelope).unwrap();
        envelope["signatures"][0]["sig"] = theirs["signatures"][0]["sig"].clone();
        SignedEvidence {
            envelope: serde_json::to_vec(&envelope).unwrap(),
            ..good.clone()
        }
    };
    let error = update(
        client,
        &job,
        &token,
        vec![good.clone(), good.clone(), forged],
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), "SIGNATURE_INVALID");
    assert_eq!(error.metadata()["index"], "2");
    assert_eq!(
        get(client, &job.job_id).await.evidence_count,
        1,
        "none stored"
    );

    // A job filter that is not a full job id.
    let bad = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: "job_12".into(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap_err();
    assert_eq!(bad.reason(), "INVALID_JOB_ID");

    // 51 is too many.
    let many = vec![good.clone(); 51];
    assert_eq!(
        reason(&update(client, &job, &token, many).await.unwrap_err()),
        ("INVALID_JOB_UPDATE".into(), "evidence".into())
    );

    // Freshness: re-sent is neither created nor fresh; started six
    // minutes before the claim is created but not fresh.
    let resent = update(
        client,
        &job,
        &token,
        vec![signed(
            &claim_of("test", Outcome::Pass, Some("a"), started),
            &tree,
            signer,
        )],
    )
    .await
    .unwrap();
    assert!(!resent.accepted[0].created && !resent.accepted[0].fresh);
    let old = update(
        client,
        &job,
        &token,
        vec![signed(
            &claim_of(
                "test",
                Outcome::Pass,
                Some("old"),
                started - Duration::from_secs(360),
            ),
            &tree,
            signer,
        )],
    )
    .await
    .unwrap();
    assert!(old.accepted[0].created && !old.accepted[0].fresh);

    // Artifacts: uploaded first reads stored; never uploaded reads absent.
    let log = b"ok\n".to_vec();
    let digest = reef_evidence::sha256_hex(&log);
    let file = daemon.tmp.path().join("test.log");
    std::fs::write(&file, &log).unwrap();
    client
        .upload_artifact_file_as(&repo.id, &file, Some(&digest))
        .await
        .unwrap();
    let mut with_logs = claim_of("test", Outcome::Pass, Some("logs"), started);
    with_logs.artifacts = vec![
        Artifact {
            name: "test.log".into(),
            sha256: digest,
            uri: None,
            media_type: None,
        },
        Artifact {
            name: "missing.log".into(),
            sha256: reef_evidence::sha256_hex(b"never"),
            uri: None,
            media_type: None,
        },
    ];
    let accepted = update(
        client,
        &job,
        &token,
        vec![signed(&with_logs, &tree, signer)],
    )
    .await
    .unwrap();
    let id = accepted.accepted[0].evidence_id.clone();
    let evidence = client
        .get_evidence(&repo.id, &id)
        .await
        .unwrap()
        .evidence
        .unwrap();
    let states: Vec<v1::ArtifactState> = evidence
        .artifacts
        .iter()
        .map(v1::EvidenceArtifact::state)
        .collect();
    assert_eq!(
        states,
        [v1::ArtifactState::Stored, v1::ArtifactState::Absent]
    );
    daemon.stop().await;
}

// 7
#[expect(
    clippy::too_many_lines,
    reason = "completeness, retries, then the other outcomes"
)]
#[tokio::test(flavor = "multi_thread")]
async fn completion_needs_every_pair() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let (job, _) = submit(client, submit_request(&ws, &["test", "lint"], &["a", "b"])).await;
    let tree = tree(&job);
    let (_, token) = claim(client, executor("r", &["test", "lint"], &[]))
        .await
        .unwrap();
    let signer = key(1);
    let started = now();
    let pair = |kind: &str, target: &str| {
        signed(
            &claim_of(kind, Outcome::Pass, Some(target), started),
            &tree,
            &signer,
        )
    };
    update(
        client,
        &job,
        &token,
        vec![
            pair("test", "a"),
            pair("test", "b"),
            pair("lint", "a/finer"),
        ],
    )
    .await
    .unwrap();
    let error = complete(
        client,
        &job,
        &token,
        JobOutcome::Done,
        vec![pair("lint", "b")],
    )
    .await
    .unwrap_err();
    assert_eq!(
        reason(&error),
        ("JOB_INCOMPLETE".into(), "1".into()),
        "a finer target is not a summary"
    );
    assert_eq!(error.metadata()["missing"], "lint:a");
    assert_eq!(get(client, &job.job_id).await.state(), JobState::Running);

    // The refused call stored nothing, so lint:b is still to send.
    let done = complete(
        client,
        &job,
        &token,
        JobOutcome::Done,
        vec![pair("lint", "a"), pair("lint", "b")],
    )
    .await
    .unwrap();
    let job_now = done.job.unwrap();
    assert_eq!(job_now.state(), JobState::Completed);
    assert_eq!(job_now.outcome(), JobOutcome::Done);
    let read = get(client, &job.job_id).await;
    let order: Vec<(String, String)> = read
        .coverage
        .iter()
        .map(|c| (c.kind.clone(), c.target.clone()))
        .collect();
    assert_eq!(
        order,
        [
            ("lint".into(), "a".into()),
            ("lint".into(), "b".into()),
            ("test".into(), "a".into()),
            ("test".into(), "b".into())
        ]
    );
    assert!(read.coverage.iter().all(|c| !c.evidence_id.is_empty()));
    assert_eq!(read.evidence_count, 5, "the finer object is kept");

    // A lost response is safe to retry; new evidence is not.
    let retried = complete(
        client,
        &job,
        &token,
        JobOutcome::Done,
        vec![pair("lint", "a")],
    )
    .await
    .unwrap();
    assert_eq!(retried.job.unwrap().state(), JobState::Completed);
    let error = complete(
        client,
        &job,
        &token,
        JobOutcome::Done,
        vec![pair("lint", "c")],
    )
    .await
    .unwrap_err();
    assert_eq!(
        reason(&error),
        ("JOB_NOT_CLAIMED".into(), "completed".into())
    );

    // Budget exhausted and failed take partial evidence and a message.
    for outcome in [JobOutcome::BudgetExhausted, JobOutcome::Failed] {
        let (job, _) = submit(
            client,
            SubmitJobRequest {
                duplicate: true,
                ..submit_request(&ws, &["test"], &["x"])
            },
        )
        .await;
        let (_, token) = claim(client, executor("r", &["test"], &[])).await.unwrap();
        let too_long = CompleteJobRequest {
            job_id: job.job_id.clone(),
            claim_token: token.clone(),
            outcome: outcome.into(),
            evidence: Vec::new(),
            usage: None,
            message: "m".repeat(1025),
        };
        assert_eq!(
            reason(&client.complete_job(too_long.clone()).await.unwrap_err()),
            ("INVALID_JOB_UPDATE".into(), "message".into())
        );
        let completed = client
            .complete_job(CompleteJobRequest {
                message: "m".repeat(1024),
                usage: Some(v1::JobUsage {
                    wall_time_ms: 1500,
                    cpu_time_ms: 900,
                }),
                ..too_long
            })
            .await
            .unwrap()
            .job
            .unwrap();
        assert_eq!(completed.outcome(), outcome);
        assert_eq!(completed.message.len(), 1024);
        assert_eq!(completed.usage.unwrap().cpu_time_ms, 900);
    }
    // A re-sent old pass never becomes the latest word on a pair (S-78).
    let (pair_job, _) = submit(
        client,
        SubmitJobRequest {
            duplicate: true,
            ..submit_request(&ws, &["test"], &["p"])
        },
    )
    .await;
    let (_, pair_token) = claim(client, executor("r", &["test"], &[])).await.unwrap();
    let pass = signed(
        &claim_of("test", Outcome::Pass, Some("p"), started),
        &tree,
        &signer,
    );
    let fail = signed(
        &claim_of(
            "test",
            Outcome::Fail,
            Some("p"),
            started + Duration::from_secs(5),
        ),
        &tree,
        &signer,
    );
    update(client, &pair_job, &pair_token, vec![pass.clone()])
        .await
        .unwrap();
    update(client, &pair_job, &pair_token, vec![fail])
        .await
        .unwrap();
    let resent = update(client, &pair_job, &pair_token, vec![pass])
        .await
        .unwrap();
    assert!(!resent.accepted[0].created && !resent.accepted[0].fresh);
    let read = get(client, &pair_job.job_id).await;
    assert_eq!(read.coverage[0].result(), v1::EvidenceResult::Fail);
    assert_eq!(read.evidence_count, 2);
    // A message with a terminal escape is refused.
    let escape = client
        .complete_job(CompleteJobRequest {
            job_id: pair_job.job_id.clone(),
            claim_token: pair_token.clone(),
            outcome: JobOutcome::Failed.into(),
            evidence: Vec::new(),
            usage: None,
            message: "\u{1b}]0;owned\u{7}".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&escape),
        ("INVALID_JOB_UPDATE".into(), "message".into())
    );
    let unset = complete(client, &job, &token, JobOutcome::Unspecified, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        reason(&unset),
        ("INVALID_JOB_UPDATE".into(), "outcome".into())
    );
    daemon.stop().await;
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn cancellation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let cancel = |id: String| async move {
        client
            .cancel_job(CancelJobRequest {
                repo: String::new(),
                job_id: id,
                reason: "superseded".into(),
            })
            .await
    };
    let (queued, _) = submit(client, submit_request(&ws, &["lint"], &[])).await;
    let cancelled = cancel(queued.job_id.clone()).await.unwrap().job.unwrap();
    assert_eq!(cancelled.state(), JobState::Cancelled);
    assert_eq!(cancelled.message, "superseded");
    // Again: returned as it is. By a prefix too.
    let prefix = queued.job_id[..10].to_owned();
    assert_eq!(
        cancel(prefix).await.unwrap().job.unwrap().state(),
        JobState::Cancelled
    );

    let (running, _) = submit(client, submit_request(&ws, &["test"], &[])).await;
    let (_, token) = claim(client, executor("r", &["test"], &[])).await.unwrap();
    let signer = key(2);
    update(
        client,
        &running,
        &token,
        vec![signed(
            &claim_of("test", Outcome::Pass, None, now()),
            &tree(&running),
            &signer,
        )],
    )
    .await
    .unwrap();
    cancel(running.job_id.clone()).await.unwrap();
    let error = update(client, &running, &token, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        ("JOB_NOT_CLAIMED".into(), "cancelled".into())
    );
    // The claim is checked before the evidence: a broken envelope does not
    // hide that the claim is over.
    let broken = SignedEvidence {
        envelope: b"{}".to_vec(),
        public_key: signer.public_key().spki().to_vec(),
    };
    let error = update(client, &running, &token, vec![broken])
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        ("JOB_NOT_CLAIMED".into(), "cancelled".into())
    );
    let kept = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                job_id: running.job_id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(kept.items.len(), 1);

    let (finished, _) = submit(client, submit_request(&ws, &["fmt"], &[])).await;
    let (_, token) = claim(client, executor("r", &["fmt"], &[])).await.unwrap();
    complete(client, &finished, &token, JobOutcome::Failed, Vec::new())
        .await
        .unwrap();
    let error = cancel(finished.job_id.clone()).await.unwrap_err();
    assert_eq!(reason(&error), ("JOB_FINISHED".into(), "completed".into()));
    let error = cancel("job_zzzz".into()).await.unwrap_err();
    assert_eq!(error.reason(), "INVALID_JOB_ID");
    let error = cancel("job_0123456789abcdef".into()).await.unwrap_err();
    assert_eq!(error.reason(), "JOB_NOT_FOUND");
    daemon.stop().await;
}

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn listening(daemon: &TestDaemon) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            return addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A remote client with a fresh token of `scope`, linked to `identity`
/// (empty for none), and the token's id.
async fn remote_client(
    daemon: &TestDaemon,
    name: &str,
    scope: RemoteScope,
    identity: &str,
) -> (Client, String) {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), identity)
        .await
        .unwrap();
    let token_id = created.token.unwrap().id;
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    (Client::connect_remote(&target).await.unwrap(), token_id)
}

async fn ci_identity(client: &Client, name: &str) {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: IdentityKind::Ci.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
}

/// A log sink for the secrets test.
#[derive(Clone, Default)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

// 9, 10
#[expect(
    clippy::too_many_lines,
    reason = "each remote rule, then a scan for the claim token"
)]
#[tokio::test(flavor = "multi_thread")]
async fn remote_rules_and_secrets() {
    // Global: the daemon's tasks run on the runtime's threads. nextest runs
    // each test in its own process.
    let sink = Sink::default();
    tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .init();
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    ci_identity(client, "ci-main").await;
    let (writer, writer_token) = remote_client(&daemon, "w", RemoteScope::Write, "").await;
    let (reader, _) = remote_client(&daemon, "r", RemoteScope::Read, "").await;
    let (ci, ci_token) = remote_client(&daemon, "ci", RemoteScope::Write, "ci-main").await;
    let (other_ci, _) = remote_client(&daemon, "ci2", RemoteScope::Write, "ci-main").await;

    // A write token submits; a read token reads and cannot write.
    let ws = create(client, &repo.id).await;
    let by_tree = |tree: &str, kind: &str| SubmitJobRequest {
        repo: repo.id.clone(),
        source: Some(submit_job_request::Source::TreeId(tree.to_owned())),
        kinds: vec![kind.to_owned()],
        ..SubmitJobRequest::default()
    };
    let (local, _) = submit(client, submit_request(&ws, &["test"], &[])).await;
    let tree = tree(&local);
    let (job, _) = submit(&writer, by_tree(&tree, "lint")).await;
    assert_eq!(job.requested_by_token, writer_token);
    assert_eq!(get(&reader, &job.job_id).await.job_id, job.job_id);
    let refused = reader.submit_job(by_tree(&tree, "fmt")).await.unwrap_err();
    assert_eq!(refused.reason(), "REMOTE_ACCESS_DENIED");

    // An unlinked write token cannot claim; a linked one can, and is named.
    let error = writer
        .claim_job(claim_request(executor("w", &["lint"], &[]), Duration::ZERO))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        ("EXECUTOR_REFUSED".into(), "token_not_linked".into())
    );
    let claimed = ci
        .claim_job(claim_request(
            executor("ci-runner", &["lint"], &[]),
            Duration::ZERO,
        ))
        .await
        .unwrap();
    let token = claimed.claim_token.clone();
    let held = claimed.job.unwrap();
    assert_eq!(held.job_id, job.job_id);
    let executor_of = held.executor.clone().unwrap();
    assert_eq!(executor_of.identity, "ci-main");
    assert_eq!(executor_of.remote_token_id, ci_token);

    // Another token cannot update, complete or cancel, even with the claim
    // token.
    let error = update(&other_ci, &held, &token, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "JOB_NOT_OWNED");
    let error = complete(&other_ci, &held, &token, JobOutcome::Failed, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "JOB_NOT_OWNED");
    let error = ci
        .cancel_job(CancelJobRequest {
            repo: String::new(),
            job_id: job.job_id.clone(),
            reason: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        "JOB_NOT_OWNED",
        "only the submitter cancels"
    );

    // Dedupe is per submitter: the same spec from another token is a job of
    // its own, which that token can cancel.
    let (theirs, _) = submit(&writer, by_tree(&tree, "dedupe")).await;
    let (ours, created) = submit(&ci, by_tree(&tree, "dedupe")).await;
    assert!(created && ours.job_id != theirs.job_id);
    ci.cancel_job(CancelJobRequest {
        repo: String::new(),
        job_id: ours.job_id.clone(),
        reason: String::new(),
    })
    .await
    .unwrap();

    // The claiming token completes, with a key bound to its identity.
    let session = ci
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    let done = complete(
        &ci,
        &held,
        &token,
        JobOutcome::Done,
        vec![signed(
            &claim_of("lint", Outcome::Pass, None, now()),
            &tree,
            &session.signing_key,
        )],
    )
    .await
    .unwrap();
    assert_eq!(done.job.unwrap().state(), JobState::Completed);
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
    let identity = evidence[0].identity.as_ref().unwrap();
    assert_eq!(identity.status(), v1::IdentityStatus::Bound);
    assert_eq!(identity.key.as_ref().unwrap().identity, "ci-main");

    // A claim that lapsed and went to another token: the first claimant is
    // told to stop (JOB_NOT_CLAIMED), not that the job is another's.
    let (handed, _) = submit(client, by_tree(&tree, "handoff")).await;
    let first = ci
        .claim_job(claim_request(
            executor("ci-a", &["handoff"], &[]),
            Duration::ZERO,
        ))
        .await
        .unwrap();
    let first_token = first.claim_token.clone();
    let executor_state = &daemon.handle.state().executor;
    executor_state.advance(Duration::from_secs(61));
    executor_state.tick();
    let second = other_ci
        .claim_job(claim_request(
            executor("ci-b", &["handoff"], &[]),
            Duration::ZERO,
        ))
        .await
        .unwrap();
    assert_eq!(second.job.unwrap().job_id, handed.job_id);
    let error = update(&ci, &handed, &first_token, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), ("JOB_NOT_CLAIMED".into(), "lapsed".into()));
    // And the current claim is still another token's.
    let error = update(&ci, &handed, &second.claim_token, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "JOB_NOT_OWNED");

    // The writer cancels its own.
    let (mine, _) = submit(&writer, by_tree(&tree, "fmt")).await;
    writer
        .cancel_job(CancelJobRequest {
            repo: String::new(),
            job_id: mine.job_id.clone(),
            reason: String::new(),
        })
        .await
        .unwrap();

    // 10: no claim token in reads, presence, the logs or the database.
    let listed = client
        .list_jobs(ListJobsRequest::default(), 0)
        .await
        .unwrap()
        .items;
    let read = format!("{:?}{:?}", listed, get(client, &job.job_id).await);
    let secret = token.split_once('.').unwrap().1.to_owned();
    assert!(!read.contains(&secret));
    let presence = format!(
        "{:?}",
        daemon
            .handle
            .state()
            .presence
            .current(&reef_daemon::presence::PresenceFilter::default())
    );
    assert!(!presence.contains(&secret));
    let logs = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    assert!(logs.contains("job transition"), "the audit log ran");
    assert!(!logs.contains(&secret));
    let db = std::fs::read(daemon.state_dir().join("reef.sqlite3")).unwrap();
    let wal = std::fs::read(daemon.state_dir().join("reef.sqlite3-wal")).unwrap_or_default();
    for bytes in [db, wal] {
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the claim token's text is in the database"
        );
    }
    daemon.stop().await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn gc_and_forget() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.executor.job_retention = Duration::from_hours(24);
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let (old, _) = submit(client, submit_request(&ws, &["test"], &[])).await;
    let (_, token) = claim(client, executor("r", &["test"], &[])).await.unwrap();
    complete(
        client,
        &old,
        &token,
        JobOutcome::Done,
        vec![signed(
            &claim_of("test", Outcome::Pass, None, now()),
            &tree(&old),
            &key(3),
        )],
    )
    .await
    .unwrap();
    let (open, _) = submit(client, submit_request(&ws, &["lint"], &[])).await;

    // Not yet a day old: nothing goes.
    let report = client.collect_garbage(&repo.id, false).await.unwrap();
    assert_eq!(report.reports[0].jobs_removed, 0);
    advance(&daemon, Duration::from_hours(25));
    let dry = client.collect_garbage(&repo.id, true).await.unwrap();
    assert_eq!(dry.reports[0].jobs_removed, 1);
    assert!(daemon.handle.state().db.job(&old.job_id).unwrap().is_some());
    let report = client.collect_garbage(&repo.id, false).await.unwrap();
    assert_eq!(report.reports[0].jobs_removed, 1);
    assert_eq!(report.reports[0].jobs_error, "");
    assert!(daemon.handle.state().db.job(&old.job_id).unwrap().is_none());
    assert_eq!(
        client
            .list_evidence(
                ListEvidenceRequest {
                    repo: repo.id.clone(),
                    ..ListEvidenceRequest::default()
                },
                0
            )
            .await
            .unwrap()
            .items
            .len(),
        1,
        "the evidence stays"
    );
    // The open job was expired by its queue timeout when the clock moved,
    // and is newer than the retention.
    assert!(
        daemon
            .handle
            .state()
            .db
            .job(&open.job_id)
            .unwrap()
            .is_some()
    );

    // Forget: jobs go, and an executor's next call is JOB_NOT_FOUND.
    let (held, _) = submit(client, submit_request(&ws, &["fmt"], &[])).await;
    let (_, token) = claim(client, executor("r", &["fmt"], &[])).await.unwrap();
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    client.forget_repository(&repo.id).await.unwrap();
    let error = update(client, &held, &token, Vec::new()).await.unwrap_err();
    assert_eq!(error.reason(), "JOB_NOT_FOUND");
    assert_eq!(daemon.handle.state().db.open_jobs().unwrap(), Vec::new());
    let entries = daemon
        .handle
        .state()
        .presence
        .current(&reef_daemon::presence::PresenceFilter::default());
    assert!(
        entries.iter().all(|change| change.presence.job.is_none()),
        "forget ends the jobs' entries"
    );
    daemon.stop().await;
}

/// The fake executor of test 12: the loop of `docs/executor-protocol.md`,
/// written only against `reef-client` and `reef-evidence`.
struct FakeExecutor {
    client: Client,
    name: String,
}

impl FakeExecutor {
    /// Claims one job (waiting), and runs it: one session key, a log
    /// uploaded, a summary and two finer objects per pair over several
    /// updates with progress, then `DONE`. Returns the job's id.
    async fn run_one(&self, repo: &str) -> String {
        let claimed = self
            .client
            .claim_job(claim_request(
                executor(&self.name, &["test", "lint"], &["os=test"]),
                Duration::from_secs(30),
            ))
            .await
            .unwrap();
        let job = claimed.job.expect("a job within the wait");
        let token = claimed.claim_token;
        let spec = job.spec.clone().unwrap();
        let session = self
            .client
            .issue_session_key("", Actor::Executor, None, "")
            .await
            .unwrap();
        let log = format!("{} ran\n", job.job_id).into_bytes();
        let digest = reef_evidence::sha256_hex(&log);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run.log");
        std::fs::write(&file, &log).unwrap();
        self.client
            .upload_artifact_file_as(repo, &file, Some(&digest))
            .await
            .unwrap();
        let pairs: Vec<(String, String)> = spec
            .kinds
            .iter()
            .flat_map(|kind| spec.targets.iter().map(move |t| (kind.clone(), t.clone())))
            .collect();
        let total = u64::try_from(pairs.len()).unwrap();
        for (done, (kind, target)) in pairs.iter().enumerate() {
            let started = now();
            let mut summary = claim_of(kind, Outcome::Pass, Some(target), started);
            summary.artifacts = vec![Artifact {
                name: "run.log".into(),
                sha256: digest.clone(),
                uri: None,
                media_type: Some("text/plain".into()),
            }];
            let finer = |n: u8| {
                claim_of(
                    kind,
                    Outcome::Pass,
                    Some(&format!("{target}/case{n}")),
                    started,
                )
            };
            let response = self
                .client
                .update_job(UpdateJobRequest {
                    job_id: job.job_id.clone(),
                    claim_token: token.clone(),
                    progress: Some(v1::JobProgress {
                        phase: kind.clone(),
                        done: u64::try_from(done).unwrap() + 1,
                        total,
                        unit: "targets".into(),
                    }),
                    evidence: [summary, finer(1), finer(2)]
                        .iter()
                        .map(|c| signed(c, &spec.tree_id, &session.signing_key))
                        .collect(),
                    release: false,
                })
                .await
                .unwrap();
            assert_eq!(response.accepted.len(), 3);
            // Past the hub's 50 ms fold, so a subscriber sees each phase.
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        let done = self
            .client
            .complete_job(CompleteJobRequest {
                job_id: job.job_id.clone(),
                claim_token: token,
                outcome: JobOutcome::Done.into(),
                evidence: Vec::new(),
                usage: Some(v1::JobUsage {
                    wall_time_ms: 10,
                    cpu_time_ms: 5,
                }),
                message: String::new(),
            })
            .await
            .unwrap();
        assert_eq!(done.job.unwrap().state(), JobState::Completed);
        job.job_id
    }
}

// 12
#[expect(
    clippy::too_many_lines,
    reason = "the loop over the socket and the remote listener, then every check"
)]
#[tokio::test(flavor = "multi_thread")]
async fn conformance() {
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    ci_identity(client, "ci-main").await;
    let ws = create(client, &repo.id).await;
    let (ci, _) = remote_client(&daemon, "ci", RemoteScope::Write, "ci-main").await;
    for (fake, identity) in [
        (
            FakeExecutor {
                client: client.clone(),
                name: "local".into(),
            },
            None,
        ),
        (
            FakeExecutor {
                client: ci,
                name: "ci-runner".into(),
            },
            Some("ci-main"),
        ),
    ] {
        let mut stream = client
            .subscribe_presence(SubscribePresenceRequest {
                repos: vec![repo.id.clone()],
                kinds: vec![PresenceKind::Job.into()],
                ..SubscribePresenceRequest::default()
            })
            .await
            .unwrap();
        let repo_id = repo.id.clone();
        let running = tokio::spawn(async move { fake.run_one(&repo_id).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (job, _) = submit(
            client,
            SubmitJobRequest {
                requirements: vec!["os=test".into()],
                duplicate: true,
                ..submit_request(&ws, &["test", "lint"], &["a", "b"])
            },
        )
        .await;
        let ran = running.await.unwrap();
        assert_eq!(ran, job.job_id);
        // The subscriber saw queued, running with each phase, and the end.
        let mut states = Vec::new();
        let mut phases = Vec::new();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), stream.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let mut ended = false;
            for event in frame.events {
                let Some(entry) = event.presence.and_then(|p| p.job) else {
                    continue;
                };
                if entry.job_id != job.job_id {
                    continue;
                }
                states.push(entry.state());
                if let Some(progress) = entry.progress {
                    phases.push(progress.phase);
                }
                ended |= event.ended;
            }
            if ended {
                break;
            }
        }
        assert_eq!(states.first(), Some(&JobState::Queued));
        assert!(states.contains(&JobState::Running));
        assert_eq!(states.last(), Some(&JobState::Completed));
        assert!(phases.contains(&"lint".to_owned()) && phases.contains(&"test".to_owned()));
        let read = get(client, &job.job_id).await;
        assert_eq!(read.evidence_count, 12);
        assert_eq!(read.coverage.len(), 4);
        assert!(
            read.coverage
                .iter()
                .all(|c| c.result() == v1::EvidenceResult::Pass && c.fresh)
        );
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
        assert_eq!(evidence.len(), 12);
        let summary = evidence.iter().find(|e| e.target == "a").unwrap();
        assert_eq!(summary.artifacts[0].state(), v1::ArtifactState::Stored);
        let bound = summary.identity.as_ref().unwrap();
        assert_eq!(bound.status(), v1::IdentityStatus::Bound);
        if let Some(identity) = identity {
            assert_eq!(bound.key.as_ref().unwrap().identity, identity);
        }
    }
    daemon.stop().await;
}

// Benchmarks for docs/benchmarks.md: submit, heartbeat, an update with 50
// envelopes, and a claim's selection over 1,000 open jobs. Release only.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "timings; run with --release --run-ignored"]
async fn timings() {
    let (daemon, repo) = fixture_with(|config| config.file.executor.max_open_jobs = 2000).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let (first, _) = submit(client, submit_request(&ws, &["warm"], &[])).await;
    let tree = tree(&first);
    let by_tree = |n: usize| SubmitJobRequest {
        repo: repo.id.clone(),
        source: Some(submit_job_request::Source::TreeId(tree.clone())),
        kinds: vec!["test".into()],
        targets: vec![format!("t{n}")],
        ..SubmitJobRequest::default()
    };
    let mut submits = Vec::new();
    for n in 0..1000 {
        let started = Instant::now();
        submit(client, by_tree(n)).await;
        submits.push(started.elapsed());
    }
    let mut selections = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let none = client
            .claim_job(claim_request(
                executor("x", &["nothing"], &[]),
                Duration::ZERO,
            ))
            .await
            .unwrap();
        selections.push(started.elapsed());
        assert!(none.job.is_none());
    }
    let (job, token) = claim(client, executor("r", &["test"], &[])).await.unwrap();
    let mut heartbeats = Vec::new();
    for _ in 0..50 {
        let started = Instant::now();
        update(client, &job, &token, Vec::new()).await.unwrap();
        heartbeats.push(started.elapsed());
    }
    let signer = key(4);
    let mut batches = Vec::new();
    let target = job.spec.as_ref().unwrap().targets[0].clone();
    for round in 0..10 {
        let evidence = (0..50)
            .map(|n| {
                signed(
                    &claim_of(
                        "test",
                        Outcome::Pass,
                        Some(&format!("{target}/{round}/{n}")),
                        now(),
                    ),
                    &tree,
                    &signer,
                )
            })
            .collect();
        let started = Instant::now();
        update(client, &job, &token, evidence).await.unwrap();
        batches.push(started.elapsed());
    }
    let started = Instant::now();
    get(client, &job.job_id).await;
    let read = started.elapsed();
    println!(
        "submit median {:?}; claim selection over 1,000 open median {:?}; heartbeat median {:?}; update with 50 envelopes median {:?}; GetJob with 500 objects {:?}",
        median(submits),
        median(selections),
        median(heartbeats),
        median(batches),
        read
    );
    daemon.stop().await;
}
