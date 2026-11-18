//! Stacks (R3.10, the brief's tests 3 to 11): proposing a stack in one
//! operation; landing it in one land operation; the prefix that lands when a
//! later change is blocked at the gate, or fails its batch's checks; a
//! sign-off on a change accepting the changes below it; a stack joining a
//! batch whole; a policy-file change ending the prefix; and the request
//! errors that name a change of the stack.
//!
//! The bisect test's in-test executor claims jobs with alice's executor key
//! and signs `fail` for any tree with `bad.txt` in it, `pass` otherwise.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, ClaimJobRequest, CompleteJobRequest, CreateIdentityRequest,
    EditFilesRequest, ExecutorInfo, FileEdit, GetLandingRequest, IdentityKind as Kind, JobOutcome,
    LandChangeRequest, LandingOutcome as Outcome, LandingReason as Reason, LifecycleState as State,
    OperationKind, ReadFileRequest, SignedEvidence, TransitionChangeRequest, UndoOperationRequest,
    edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Verdict, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

/// A policy that lands on submitted trees, with `extra` after it.
fn policy(extra: &str) -> String {
    format!(
        r#"
version = 1

[owners]
identities = ["alice"]

[landing]
checks = "proposal"
{extra}
"#
    )
}

/// Every change needs alice's sign-off.
const REVIEWED: &str = r#"
[[signoff]]
name = "review"
paths = ["**"]
identities = ["alice"]
"#;

/// `test` from alice's executor keys, tested on the batch's tree.
const TESTED: &str = r#"
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
batch_max = 8
batch_wait = "10s"
"#;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

fn reason(error: &ClientError) -> (String, Option<String>) {
    match error {
        ClientError::Rpc {
            reason, metadata, ..
        } => (reason.clone(), metadata.get("change_id").cloned()),
        other => panic!("{other:?}"),
    }
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

/// A stack of `names.len()` changes on `on`, each writing `<name>.txt`.
async fn stack_on(client: &Client, repo: &str, on: &str, names: &[&str]) -> Vec<v1::Change> {
    let mut changes: Vec<v1::Change> = Vec::new();
    for name in names {
        let parent = changes
            .last()
            .map_or(on.to_owned(), |c| c.commit_id.clone());
        let file = format!("{name}.txt");
        changes.push(commit_on(client, repo, &parent, &[(file.as_str(), "x\n")]).await);
    }
    changes
}

/// Sets (or, with `None`, deletes) a local bookmark from outside the
/// daemon, as `jj bookmark set` would.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: Option<&str>) {
    let (root, name, commit) = (
        repo.root.clone(),
        name.to_owned(),
        commit.map(str::to_owned),
    );
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = test_settings();
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
        let target = commit.map_or_else(RefTarget::absent, |commit| {
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap())
        });
        tx.repo_mut()
            .set_local_bookmark_target(RefName::new(&name), target);
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

/// `main` with `policy` on a base commit; alice exists.
async fn setup(client: &Client, repo: &SyntheticRepo, policy: &str) -> v1::Change {
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
    bookmark(repo, "main", Some(&main.commit_id)).await;
    main
}

fn transition(repo: &str, revision: &str, to: State, stack: bool) -> TransitionChangeRequest {
    TransitionChangeRequest {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        to: to.into(),
        stack,
        ..TransitionChangeRequest::default()
    }
}

async fn propose_stack(client: &Client, repo: &str, head: &str) -> v1::TransitionChangeResponse {
    client
        .transition_change(transition(repo, head, State::Proposed, true))
        .await
        .unwrap()
}

fn land(repo: &str, revision: &str, wait_ms: u32) -> LandChangeRequest {
    LandChangeRequest {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        wait_ms,
        ..LandChangeRequest::default()
    }
}

/// The outcomes of a stack's landings, bottom first, the head last.
fn outcomes(response: &v1::LandChangeResponse) -> Vec<(Outcome, Reason)> {
    response
        .stack
        .iter()
        .chain(response.landing.iter())
        .map(|landing| (landing.outcome(), landing.reason()))
        .collect()
}

fn signed(
    kind: &str,
    verdict: Verdict,
    target: Option<&str>,
    tree: &str,
    key: &SigningKey,
) -> SignedEvidence {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let started = UNIX_EPOCH + Duration::from_secs(secs);
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

async fn add(client: &Client, repo: &str, evidence: SignedEvidence) {
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: String::new(),
            envelope: evidence.envelope,
            public_key: evidence.public_key,
            revision: String::new(),
        })
        .await
        .unwrap();
}

/// Alice's sign-off on `tree`, as a person.
async fn sign_off(client: &Client, repo: &str, tree: &str) {
    let key = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap()
        .signing_key;
    add(
        client,
        repo,
        signed("signoff", Verdict::Pass, None, tree, &key),
    )
    .await;
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

// 3. A stack is proposed in one operation; `stack: false` proposes one
// change; one undo reverts the whole stack.
#[tokio::test(flavor = "multi_thread")]
async fn propose_stack_in_one_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("")).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b", "c"]).await;

    let alone = client
        .transition_change(transition(
            &repo.id,
            &stack[1].commit_id,
            State::Proposed,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(alone.transitions.len(), 1, "without stack, one change");

    let response = propose_stack(client, &repo.id, &stack[2].commit_id).await;
    let proposed: Vec<&str> = response
        .transitions
        .iter()
        .map(|transition| transition.change_id.as_str())
        .collect();
    assert_eq!(
        proposed,
        [stack[0].change_id.as_str(), stack[2].change_id.as_str()],
        "the bottom and the head; the middle was proposed already"
    );
    assert_eq!(
        response.transition.as_ref().unwrap().change_id,
        stack[2].change_id
    );
    let operation = response.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Transition);
    assert_eq!(operation.transitions.len(), 2);

    // Nothing left to propose: no operation.
    let again = propose_stack(client, &repo.id, &stack[2].commit_id).await;
    assert!(again.operation.is_none() && again.transitions.is_empty());

    // One undo reverts both.
    client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: operation.id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap();
    for (change, state) in [
        (&stack[0], State::Draft),
        (&stack[1], State::Proposed),
        (&stack[2], State::Draft),
    ] {
        let lifecycle = client
            .get_change(v1::GetChangeRequest {
                repo: repo.id.clone(),
                revision: change.commit_id.clone(),
                skip_snapshot: true,
                ..v1::GetChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
            .lifecycle
            .unwrap();
        assert_eq!(lifecycle.state(), state, "{}", change.change_id);
    }

    // A withdrawal of the stack takes every member in review.
    propose_stack(client, &repo.id, &stack[2].commit_id).await;
    let withdrawn = client
        .transition_change(transition(
            &repo.id,
            &stack[2].commit_id,
            State::Withdrawn,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(withdrawn.transitions.len(), 3);

    // Supersede takes no stack.
    let refused = client
        .transition_change(TransitionChangeRequest {
            superseded_by: stack[0].commit_id.clone(),
            ..transition(&repo.id, &stack[2].commit_id, State::Superseded, true)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused).0, "STACK_NOT_ALLOWED");

    // More than 32 changes.
    let names: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let tall = stack_on(client, &repo.id, &main.commit_id, &names).await;
    let too_large = client
        .transition_change(transition(
            &repo.id,
            &tall.last().unwrap().commit_id,
            State::Proposed,
            true,
        ))
        .await
        .unwrap_err();
    assert_eq!(reason(&too_large).0, "STACK_TOO_LARGE");
    daemon.stop().await;
}

// 4. `reef land` on the head lands the stack in one land operation.
#[tokio::test(flavor = "multi_thread")]
async fn land_stack_whole() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("")).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b", "c"]).await;
    propose_stack(client, &repo.id, &stack[2].commit_id).await;
    let response = client
        .land_change(land(&repo.id, &stack[2].commit_id, 60_000))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&response),
        [(Outcome::Landed, Reason::Unspecified); 3],
        "{response:#?}"
    );
    let landing = response.landing.unwrap();
    assert_eq!(landing.stack, landing.id);
    assert!(
        response
            .stack
            .iter()
            .all(|member| member.stack == landing.id)
    );
    let operations: Vec<&str> = response
        .stack
        .iter()
        .chain([&landing])
        .map(|member| member.operation_id.as_str())
        .collect();
    assert!(
        operations.iter().all(|op| *op == operations[0]),
        "one land operation: {operations:?}"
    );
    // A fast-forward under `keep` lands the commits as they are.
    assert_eq!(head_of(client, &repo.id, "main").await, stack[2].commit_id);
    let read = client
        .get_landing(GetLandingRequest {
            repo: repo.id.clone(),
            id: landing.id.clone(),
            ..GetLandingRequest::default()
        })
        .await
        .unwrap();
    let below: Vec<&str> = read
        .stack
        .iter()
        .map(|member| member.changes[0].change_id.as_str())
        .collect();
    assert_eq!(
        below,
        [stack[0].change_id.as_str(), stack[1].change_id.as_str()]
    );
    daemon.stop().await;
}

// 5 and 6. A sign-off on a change accepts the changes below it; the gate
// lands the prefix it accepts and blocks the rest, the first with its
// reason and those above it with `unlanded_parents`.
#[tokio::test(flavor = "multi_thread")]
async fn gate_lands_the_signed_prefix() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy(REVIEWED)).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b", "c", "d"]).await;
    propose_stack(client, &repo.id, &stack[3].commit_id).await;
    sign_off(client, &repo.id, &stack[1].tree_id).await;
    let response = client
        .land_change(land(&repo.id, &stack[3].commit_id, 60_000))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&response),
        [
            (Outcome::Landed, Reason::Unspecified),
            (Outcome::Landed, Reason::Unspecified),
            (Outcome::Blocked, Reason::Policy),
            (Outcome::Blocked, Reason::UnlandedParents),
        ],
        "{response:#?}"
    );
    assert!(
        response
            .landing
            .as_ref()
            .unwrap()
            .message
            .contains(&stack[2].change_id[..12]),
        "the head's message names the change below it"
    );
    assert_eq!(head_of(client, &repo.id, "main").await, stack[1].commit_id);
    // A sign-off on the head accepts what is left.
    sign_off(client, &repo.id, &stack[3].tree_id).await;
    let rest = client
        .land_change(land(&repo.id, &stack[3].commit_id, 60_000))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&rest),
        [(Outcome::Landed, Reason::Unspecified); 2],
        "{rest:#?}"
    );
    assert_eq!(head_of(client, &repo.id, "main").await, stack[3].commit_id);
    daemon.stop().await;
}

/// The in-test executor: claims `test` jobs and signs `fail` for a tree with
/// `bad.txt`, `pass` otherwise, until `stop`.
fn executor(
    client: &Client,
    repo: &str,
    key: SigningKey,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let (client, repo) = (client.clone(), repo.to_owned());
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
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
            let spec = job.spec.clone().unwrap();
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
            let _ = client
                .complete_job(CompleteJobRequest {
                    job_id: job.job_id.clone(),
                    claim_token: response.claim_token.clone(),
                    outcome: JobOutcome::Done.into(),
                    evidence: vec![signed("test", verdict, None, &spec.tree_id, &key)],
                    usage: None,
                    message: String::new(),
                })
                .await;
        }
    })
}

// 7. Phase 3's exit criterion: a stack lands as a prefix when a later
// change fails its batch's checks. The culprit returns with `checks`, the
// change above it with `unlanded_parents`, and an unrelated change in the
// same batch is requeued and lands next.
#[tokio::test(flavor = "multi_thread")]
async fn bisect_lands_the_prefix() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, TESTED).await;
    let key = client
        .issue_session_key("alice", Actor::Executor, None, "")
        .await
        .unwrap()
        .signing_key;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("bad.txt", "b\n")]).await;
    let c = commit_on(client, &repo.id, &b.commit_id, &[("c.txt", "c\n")]).await;
    let x = commit_on(client, &repo.id, &main.commit_id, &[("x.txt", "x\n")]).await;
    for change in [&a, &b, &c, &x] {
        add(
            client,
            &repo.id,
            signed("test", Verdict::Pass, None, &change.tree_id, &key),
        )
        .await;
    }
    propose_stack(client, &repo.id, &c.commit_id).await;
    propose_stack(client, &repo.id, &x.commit_id).await;
    let queued = client
        .land_change(land(&repo.id, &c.commit_id, 0))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&queued),
        [(Outcome::Queued, Reason::Unspecified); 3],
        "{queued:#?}"
    );
    client
        .land_change(land(&repo.id, &x.commit_id, 0))
        .await
        .unwrap();
    // The destination moves, so the batch's trees are new and tested: on a
    // fast-forward each prefix's tree is a submitted one, whose evidence
    // the gate already required.
    let moved = commit_on(client, &repo.id, &main.commit_id, &[("m.txt", "m\n")]).await;
    bookmark(&repo, "main", Some(&moved.commit_id)).await;
    let stop = Arc::new(AtomicBool::new(false));
    let executor = executor(client, &repo.id, key, Arc::clone(&stop));
    let culprit = finished(client, &repo.id, &b.change_id).await;
    assert_eq!(
        (culprit.outcome(), culprit.reason()),
        (Outcome::Dequeued, Reason::Checks),
        "{culprit:#?}"
    );
    let bottom = finished(client, &repo.id, &a.change_id).await;
    assert_eq!(bottom.outcome(), Outcome::Landed, "{bottom:#?}");
    let above = finished(client, &repo.id, &c.change_id).await;
    assert_eq!(
        (above.outcome(), above.reason()),
        (Outcome::Dequeued, Reason::UnlandedParents),
        "{above:#?}"
    );
    let other = finished(client, &repo.id, &x.change_id).await;
    assert_eq!(other.outcome(), Outcome::Landed, "{other:#?}");
    assert_ne!(
        other.batch_id, culprit.batch_id,
        "requeued, then landed next"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = executor.await;
    daemon.stop().await;
}

// 8. A stack joins a batch whole: past `batch_max` only as the batch's
// first group, never split across two batches.
#[tokio::test(flavor = "multi_thread")]
async fn stack_joins_a_batch_whole() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(
        client,
        &repo,
        &policy("batch_max = 2\nbatch_wait = \"3s\"\n"),
    )
    .await;
    let x = commit_on(client, &repo.id, &main.commit_id, &[("x.txt", "x\n")]).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b", "c"]).await;
    propose_stack(client, &repo.id, &x.commit_id).await;
    propose_stack(client, &repo.id, &stack[2].commit_id).await;
    client
        .land_change(land(&repo.id, &x.commit_id, 0))
        .await
        .unwrap();
    client
        .land_change(land(&repo.id, &stack[2].commit_id, 0))
        .await
        .unwrap();
    let alone = finished(client, &repo.id, &x.change_id).await;
    assert_eq!(alone.outcome(), Outcome::Landed, "{alone:#?}");
    let mut batches = Vec::new();
    for change in &stack {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        batches.push(landing.batch_id);
    }
    assert!(
        batches.iter().all(|batch| *batch == batches[0]),
        "{batches:?}"
    );
    assert_ne!(batches[0], alone.batch_id);
    daemon.stop().await;
}

// 9. A change to the policy file above the bottom ends the prefix: the
// changes below it are queued, it is blocked `alone`, and those above it
// `unlanded_parents`.
#[tokio::test(flavor = "multi_thread")]
async fn policy_change_ends_the_prefix() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("")).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let p = commit_on(
        client,
        &repo.id,
        &a.commit_id,
        &[("reef-policy.toml", &policy("batch_max = 4\n"))],
    )
    .await;
    let c = commit_on(client, &repo.id, &p.commit_id, &[("c.txt", "c\n")]).await;
    // The owners sign the policy change off.
    sign_off(client, &repo.id, &p.tree_id).await;
    propose_stack(client, &repo.id, &c.commit_id).await;
    let response = client
        .land_change(land(&repo.id, &c.commit_id, 60_000))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&response),
        [
            (Outcome::Landed, Reason::Unspecified),
            (Outcome::Blocked, Reason::Alone),
            (Outcome::Blocked, Reason::UnlandedParents),
        ],
        "{response:#?}"
    );
    daemon.stop().await;
}

// 10. A change of the stack that is not proposed, or proposed for another
// destination, refuses the request and is named.
#[tokio::test(flavor = "multi_thread")]
async fn request_errors_name_the_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("")).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b"]).await;
    client
        .transition_change(transition(
            &repo.id,
            &stack[1].commit_id,
            State::Proposed,
            false,
        ))
        .await
        .unwrap();
    let unproposed = client
        .land_change(land(&repo.id, &stack[1].commit_id, 0))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&unproposed),
        ("NOT_PROPOSED".to_owned(), Some(stack[0].change_id.clone()))
    );

    bookmark(&repo, "feature", Some(&main.commit_id)).await;
    client
        .transition_change(TransitionChangeRequest {
            destination: "feature".to_owned(),
            ..transition(&repo.id, &stack[0].commit_id, State::Proposed, false)
        })
        .await
        .unwrap();
    let mismatch = client
        .land_change(land(&repo.id, &stack[1].commit_id, 0))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&mismatch),
        (
            "STACK_DESTINATION_MISMATCH".to_owned(),
            Some(stack[0].change_id.clone())
        )
    );
    daemon.stop().await;
}

// 11. A change withdrawn from the middle of a queued stack takes the
// changes above it out at the next formation; the bottom lands.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_middle_returns_the_rest() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("batch_wait = \"3s\"\n")).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b", "c"]).await;
    propose_stack(client, &repo.id, &stack[2].commit_id).await;
    client
        .land_change(land(&repo.id, &stack[2].commit_id, 0))
        .await
        .unwrap();
    client
        .transition_change(transition(
            &repo.id,
            &stack[1].commit_id,
            State::Withdrawn,
            false,
        ))
        .await
        .unwrap();
    let bottom = finished(client, &repo.id, &stack[0].change_id).await;
    assert_eq!(bottom.outcome(), Outcome::Landed, "{bottom:#?}");
    let head = finished(client, &repo.id, &stack[2].change_id).await;
    assert_eq!(
        (head.outcome(), head.reason()),
        (Outcome::Dequeued, Reason::UnlandedParents),
        "{head:#?}"
    );
    daemon.stop().await;
}

/// A merge of `left` and `right` with both their files, written from
/// outside the daemon as `jj new left right` would, then given `m.txt`
/// through the daemon.
async fn merge_of(client: &Client, repo: &SyntheticRepo, left: &str, right: &str) -> v1::Change {
    let (root, left, right) = (repo.root.clone(), left.to_owned(), right.to_owned());
    let commit = tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::object_id::ObjectId as _;
        use jj_lib::repo::Repo as _;
        let settings = test_settings();
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
        let parents: Vec<_> = [left, right]
            .iter()
            .map(|hex| {
                pollster::block_on(
                    tx.repo()
                        .store()
                        .get_commit_async(&CommitId::try_from_hex(hex).unwrap()),
                )
                .unwrap()
            })
            .collect();
        let tree =
            pollster::block_on(jj_lib::rewrite::merge_commit_trees(tx.repo(), &parents)).unwrap();
        let commit = pollster::block_on(
            tx.repo_mut()
                .new_commit(parents.iter().map(|p| p.id().clone()).collect(), tree)
                .set_description("merge")
                .write(),
        )
        .unwrap();
        pollster::block_on(tx.commit("merge")).unwrap();
        commit.id().hex()
    })
    .await
    .unwrap();
    // Its own edit, so it is not empty.
    client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::Revision(commit)),
            edits: vec![FileEdit {
                path: "m.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"m\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

// Review finding 2: a sign-off reaches only the changes the signed tree
// contains. In a stack that merges two branches, a sign-off on one branch
// does not accept the other.
#[tokio::test(flavor = "multi_thread")]
async fn signoff_reaches_only_ancestors() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy(REVIEWED)).await;
    let x = commit_on(client, &repo.id, &main.commit_id, &[("x.txt", "x\n")]).await;
    let y = commit_on(client, &repo.id, &main.commit_id, &[("y.txt", "y\n")]).await;
    let merged = merge_of(client, &repo, &x.commit_id, &y.commit_id).await;
    propose_stack(client, &repo.id, &merged.commit_id).await;
    sign_off(client, &repo.id, &y.tree_id).await;
    let response = client
        .land_change(land(&repo.id, &merged.commit_id, 60_000))
        .await
        .unwrap();
    let x_landing = response
        .stack
        .iter()
        .find(|landing| landing.changes[0].change_id == x.change_id)
        .unwrap();
    assert_eq!(
        x_landing.outcome(),
        Outcome::Blocked,
        "y's sign-off does not accept x: {response:#?}"
    );
    daemon.stop().await;
}

// Review finding 5: asking again for a stack already queued answers with
// its open landings.
#[tokio::test(flavor = "multi_thread")]
async fn landing_again_returns_the_open_landings() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("batch_wait = \"10s\"\n")).await;
    let stack = stack_on(client, &repo.id, &main.commit_id, &["a", "b"]).await;
    propose_stack(client, &repo.id, &stack[1].commit_id).await;
    let first = client
        .land_change(land(&repo.id, &stack[1].commit_id, 0))
        .await
        .unwrap();
    let again = client
        .land_change(land(&repo.id, &stack[1].commit_id, 0))
        .await
        .unwrap();
    assert_eq!(
        again.landing.as_ref().unwrap().id,
        first.landing.as_ref().unwrap().id
    );
    assert_eq!(again.stack.len(), 1, "{again:#?}");
    for change in &stack {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    }
    daemon.stop().await;
}

// Review finding 3: a policy change queued by an earlier request still
// lands on its own; a change above it waits.
#[tokio::test(flavor = "multi_thread")]
async fn queued_policy_change_holds_the_changes_above() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("batch_wait = \"10s\"\n")).await;
    let p = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[(
            "reef-policy.toml",
            &policy("batch_wait = \"10s\"\nbatch_max = 4\n"),
        )],
    )
    .await;
    sign_off(client, &repo.id, &p.tree_id).await;
    propose_stack(client, &repo.id, &p.commit_id).await;
    let queued = client
        .land_change(land(&repo.id, &p.commit_id, 0))
        .await
        .unwrap();
    assert_eq!(queued.landing.unwrap().outcome(), Outcome::Queued);
    let above = commit_on(client, &repo.id, &p.commit_id, &[("c.txt", "c\n")]).await;
    propose_stack(client, &repo.id, &above.commit_id).await;
    let response = client
        .land_change(land(&repo.id, &above.commit_id, 0))
        .await
        .unwrap();
    assert_eq!(
        outcomes(&response),
        [
            (Outcome::Queued, Reason::Unspecified),
            (Outcome::Blocked, Reason::UnlandedParents),
        ],
        "{response:#?}"
    );
    daemon.stop().await;
}

// The brief's test 3: a change below that cannot be proposed refuses the
// whole request, named.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_change_below_refuses_the_proposal() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("")).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    // Writing what is there already: an empty change.
    let empty = commit_on(client, &repo.id, &a.commit_id, &[("a.txt", "a\n")]).await;
    assert!(empty.empty, "{empty:#?}");
    let top = commit_on(client, &repo.id, &empty.commit_id, &[("c.txt", "c\n")]).await;
    let refused = client
        .transition_change(transition(&repo.id, &top.commit_id, State::Proposed, true))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&refused),
        ("CHANGE_EMPTY".to_owned(), Some(empty.change_id.clone()))
    );
    daemon.stop().await;
}
