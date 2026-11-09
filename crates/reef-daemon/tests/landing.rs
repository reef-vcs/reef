//! The landing service, local (R3.13, the brief's tests 3 to 15): landing a
//! proposal with trusted evidence; refusing untrusted evidence with every
//! object's reason; the policy from the destination; rebases, overlaps and
//! conflicts; parents and destinations; workspaces on the landed commit;
//! pins; undo; and the gate's request errors.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, CreateIdentityRequest, CreateWorkspaceRequest, EditFilesRequest,
    FileEdit, GetChangeRequest, GetLandingRequest, IdentityKind as Kind, LandChangeRequest,
    LandingOutcome as Outcome, LandingReason as Reason, LifecycleState as State,
    ListLandingsRequest, ListTransitionsRequest, OperationKind, PolicyDecision, TransitionCause,
    TransitionChangeRequest, UndoOperationRequest, edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Result_, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;
use tonic::Code;

const OWNER: &str = "tester@host";

/// `main`'s policy: `test` from alice's executor keys.
const POLICY: &str = r#"
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
"#;

/// A policy that allows everything but a rebase over shared paths.
const OPEN: &str = r#"
version = 1

[owners]
identities = ["alice"]

[[rule]]
name = "disjoint"
expr = 'landing.overlap_total == 0'
message = "touches nothing the destination changed"
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

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

async fn change_at(client: &Client, repo: &str, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
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

/// Sets (or, with `None`, deletes) a local bookmark from outside the
/// daemon, as `jj bookmark set` would.
fn set_bookmark(root: &Path, name: &str, commit: Option<&str>) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let settings = test_settings();
    let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &settings,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
    let mut tx = repo.start_transaction();
    let target = match commit {
        Some(commit) => RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
        None => RefTarget::absent(),
    };
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new(name), target);
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: Option<&str>) {
    let (root, name, commit) = (
        repo.root.clone(),
        name.to_owned(),
        commit.map(str::to_owned),
    );
    tokio::task::spawn_blocking(move || set_bookmark(&root, &name, commit.as_deref()))
        .await
        .unwrap();
}

/// The commit `name` points at, read through the daemon.
async fn head_of(client: &Client, repo: &str, name: &str) -> String {
    change_at(client, repo, name).await.commit_id
}

fn claim(kind: &str) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: kind.to_owned(),
        result: Result_::Pass,
        target: None,
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
}

/// Adds `test` evidence about `tree`, signed by `key`.
async fn add(client: &Client, repo: &str, tree: &str, key: &SigningKey) -> v1::Evidence {
    let envelope = claim("test")
        .sign(&TreeDigest::git(tree).unwrap(), key)
        .unwrap()
        .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: String::new(),
            envelope,
            public_key: key.public_key().spki().to_vec(),
            revision: String::new(),
        })
        .await
        .unwrap()
        .evidence
        .unwrap()
}

/// Alice's executor key: what [`POLICY`] trusts.
async fn trusted_key(client: &Client) -> SigningKey {
    client
        .issue_session_key("alice", Actor::Executor, None, "")
        .await
        .unwrap()
        .signing_key
}

async fn propose(client: &Client, repo: &str, revision: &str, to: &str) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            to: State::Proposed.into(),
            destination: to.to_owned(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

fn land(repo: &str, revision: &str) -> LandChangeRequest {
    LandChangeRequest {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        wait_ms: 60_000,
        ..LandChangeRequest::default()
    }
}

async fn landed(client: &Client, request: LandChangeRequest) -> v1::LandChangeResponse {
    client.land_change(request).await.unwrap()
}

/// `main` with `policy` on a base commit, and a change on it.
struct Setup {
    main: v1::Change,
    change: v1::Change,
}

async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo, policy: &str) -> Setup {
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let base = commit_on(client, &repo.id, "root()", &[("README.md", "base\n")]).await;
    let main = commit_on(
        client,
        &repo.id,
        &base.commit_id,
        &[("reef-policy.toml", policy)],
    )
    .await;
    bookmark(repo, "main", Some(&main.commit_id)).await;
    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    Setup { main, change }
}

// 3. Exit criterion 1.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one landing through every read")]
async fn lands_with_trusted_evidence() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, POLICY).await;
    propose(client, &repo.id, &change.commit_id, "").await;
    let key = trusted_key(client).await;
    let evidence = add(client, &repo.id, &change.tree_id, &key).await;

    let response = landed(client, land(&repo.id, &change.commit_id)).await;
    let landing = response.landing.clone().unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    assert_eq!(landing.destination, "main");
    assert_eq!(landing.destination_before, main.commit_id);
    let first = &landing.changes[0];
    assert!(!first.rebased, "a fast-forward");
    assert_eq!(first.landed_commit_id, change.commit_id);
    assert_eq!(landing.destination_after, change.commit_id);
    assert_eq!(
        first.pinned_evidence_ids,
        std::slice::from_ref(&evidence.evidence_id)
    );
    let evaluation = first.evaluation.as_ref().unwrap();
    assert_eq!(evaluation.decision(), PolicyDecision::Allow);
    assert_eq!(
        evaluation.source.as_ref().unwrap().commit_id,
        main.commit_id
    );
    assert_eq!(head_of(client, &repo.id, "main").await, change.commit_id);

    // The enqueue and the land, as transitions with cause LANDING; the land
    // in the operation that moved main.
    let enqueue = response.enqueue.unwrap();
    assert_eq!(enqueue.kind(), OperationKind::Transition);
    let operation = response.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Land);
    assert_eq!(operation.landing_id, landing.id);
    assert_eq!(operation.transitions.len(), 1);
    assert_eq!(operation.transitions[0].to(), State::Landed);
    assert_eq!(operation.transitions[0].commit_id, change.commit_id);
    let transitions = client
        .list_transitions(
            ListTransitionsRequest {
                repo: repo.id.clone(),
                changes: vec![change.change_id.clone()],
                ..ListTransitionsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    let causes: Vec<(State, TransitionCause)> = transitions
        .iter()
        .map(|transition| (transition.to(), transition.cause()))
        .collect();
    assert_eq!(
        causes,
        [
            (State::Landed, TransitionCause::Landing),
            (State::Queued, TransitionCause::Landing),
            (State::Proposed, TransitionCause::Request),
        ]
    );
    let after = change_at(client, &repo.id, &change.commit_id).await;
    assert_eq!(after.lifecycle.unwrap().state(), State::Landed);

    // Read back, with the policy bytes.
    let got = client
        .get_landing(GetLandingRequest {
            repo: repo.id.clone(),
            change: change.change_id.clone(),
            include_policy: true,
            ..GetLandingRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(got.landing.unwrap().id, landing.id);
    assert_eq!(got.policy, POLICY);
    let listed = client
        .list_landings(
            ListLandingsRequest {
                repo: repo.id.clone(),
                outcomes: vec![Outcome::Landed.into()],
                ..ListLandingsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].changes[0]
            .evaluation
            .as_ref()
            .unwrap()
            .rules
            .len(),
        0,
        "a listing leaves the rules out"
    );

    // Landing it again: it is landed.
    let again = client
        .land_change(land(&repo.id, &change.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&again),
        (Code::FailedPrecondition, "NOT_PROPOSED".to_owned())
    );

    // Undo refuses the enqueue and the land.
    for op in [enqueue.id, operation.id] {
        let error = client
            .undo_operation(UndoOperationRequest {
                repo: repo.id.clone(),
                operation_id: op,
                ..UndoOperationRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(reason(&error).1, "OPERATION_NOT_UNDOABLE", "{error:?}");
    }
}

// 4. Exit criterion 2.
#[tokio::test(flavor = "multi_thread")]
async fn refuses_untrusted_evidence() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, POLICY).await;
    propose(client, &repo.id, &change.commit_id, "").await;
    let agent = client
        .issue_session_key("", Actor::Agent, None, "")
        .await
        .unwrap()
        .signing_key;
    let bare = SigningKey::from_seed(&[9; 32]).unwrap();
    let from_agent = add(client, &repo.id, &change.tree_id, &agent).await;
    let unbound = add(client, &repo.id, &change.tree_id, &bare).await;

    let blocked = landed(client, land(&repo.id, &change.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(blocked.outcome(), Outcome::Blocked);
    assert_eq!(blocked.reason(), Reason::Policy);
    let evaluation = blocked.changes[0].evaluation.as_ref().unwrap();
    assert_eq!(evaluation.decision(), PolicyDecision::Block);
    let tests = evaluation
        .rules
        .iter()
        .find(|rule| rule.name == "tests")
        .unwrap();
    let rejected: Vec<(&str, &str)> = tests.missing[0]
        .rejected
        .iter()
        .map(|r| (r.evidence_id.as_str(), r.why.as_str()))
        .collect();
    assert!(
        rejected.contains(&(from_agent.evidence_id.as_str(), "untrusted")),
        "{rejected:?}"
    );
    assert!(
        rejected.contains(&(unbound.evidence_id.as_str(), "key_unbound")),
        "{rejected:?}"
    );
    assert_eq!(tests.missing[0].rejected_total, 2);
    // Nothing queued, nothing moved, the change still proposed.
    assert_eq!(head_of(client, &repo.id, "main").await, main.commit_id);
    let still = change_at(client, &repo.id, &change.commit_id).await;
    assert_eq!(still.lifecycle.unwrap().state(), State::Proposed);

    // A retry with nothing new counts on the same row.
    let retried = landed(client, land(&repo.id, &change.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(retried.id, blocked.id);
    assert_eq!(retried.attempts, 2);
}

// 7. The policy is the destination's, and none lands nothing.
#[tokio::test(flavor = "multi_thread")]
async fn no_policy_lands_nothing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = commit_on(client, &repo.id, "root()", &[("README.md", "base\n")]).await;
    bookmark(&repo, "main", Some(&main.commit_id)).await;
    // The change brings its own permissive policy: it does not judge
    // itself.
    let change = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("reef-policy.toml", OPEN)],
    )
    .await;
    propose(client, &repo.id, &change.commit_id, "").await;
    let blocked = landed(client, land(&repo.id, &change.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(blocked.outcome(), Outcome::Blocked);
    assert_eq!(
        blocked.changes[0].evaluation.as_ref().unwrap().decision(),
        PolicyDecision::NoPolicy
    );
}

// 5. Rebases, overlaps and conflicts.
#[tokio::test(flavor = "multi_thread")]
async fn rebases_when_destination_moved() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, OPEN).await;
    propose(client, &repo.id, &change.commit_id, "").await;
    // main moves on with a disjoint change.
    let moved = commit_on(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")]).await;
    bookmark(&repo, "main", Some(&moved.commit_id)).await;

    let landing = landed(client, land(&repo.id, &change.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let first = &landing.changes[0];
    assert!(first.rebased);
    assert_eq!(first.overlap_total, 0);
    assert_ne!(first.landed_commit_id, change.commit_id);
    let landed_change = change_at(client, &repo.id, "main").await;
    assert_eq!(landed_change.change_id, change.change_id, "same change");
    assert_eq!(
        landed_change.parent_commit_ids,
        std::slice::from_ref(&moved.commit_id)
    );

    // An overlapping clean rebase is blocked by the policy's rule.
    let shared = commit_on(
        client,
        &repo.id,
        &landed_change.commit_id,
        &[("c.txt", "one\ntwo\nthree\n")],
    )
    .await;
    bookmark(&repo, "main", Some(&shared.commit_id)).await;
    let ours = commit_on(
        client,
        &repo.id,
        &shared.commit_id,
        &[("c.txt", "ONE\ntwo\nthree\n")],
    )
    .await;
    let theirs = commit_on(
        client,
        &repo.id,
        &shared.commit_id,
        &[("c.txt", "one\ntwo\nTHREE\n")],
    )
    .await;
    bookmark(&repo, "main", Some(&theirs.commit_id)).await;
    propose(client, &repo.id, &ours.commit_id, "").await;
    let overlap = landed(client, land(&repo.id, &ours.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(overlap.outcome(), Outcome::Blocked);
    assert_eq!(overlap.reason(), Reason::Policy);
    assert_eq!(overlap.changes[0].overlap, ["c.txt"]);

    // A conflicting rebase is refused with its paths, and writes nothing.
    let clash = commit_on(
        client,
        &repo.id,
        &shared.commit_id,
        &[("c.txt", "one\nTWO?\nTHREE!\n")],
    )
    .await;
    propose(client, &repo.id, &clash.commit_id, "").await;
    let conflicted = landed(client, land(&repo.id, &clash.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(conflicted.outcome(), Outcome::Blocked);
    assert_eq!(conflicted.reason(), Reason::Conflicted);
    assert_eq!(conflicted.changes[0].conflicts, ["c.txt"]);
    assert_eq!(head_of(client, &repo.id, "main").await, theirs.commit_id);
    let untouched = change_at(client, &repo.id, &clash.commit_id).await;
    assert!(!untouched.conflict);
}

// 12. Parents, destinations, and the gate's request errors.
#[tokio::test(flavor = "multi_thread")]
async fn gate_refusals() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, OPEN).await;

    // A draft is not proposed.
    let draft = client
        .land_change(land(&repo.id, &change.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&draft),
        (Code::FailedPrecondition, "NOT_PROPOSED".to_owned())
    );

    // A change on an unlanded parent.
    let child = commit_on(client, &repo.id, &change.commit_id, &[("d.txt", "d\n")]).await;
    propose(client, &repo.id, &child.commit_id, "").await;
    let unlanded = landed(client, land(&repo.id, &child.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(unlanded.outcome(), Outcome::Blocked);
    assert_eq!(unlanded.reason(), Reason::UnlandedParents);

    // A proposal for a bookmark that is gone.
    bookmark(&repo, "feature", Some(&main.commit_id)).await;
    let other = commit_on(client, &repo.id, &main.commit_id, &[("e.txt", "e\n")]).await;
    propose(client, &repo.id, &other.commit_id, "feature").await;
    bookmark(&repo, "feature", None).await;
    let gone = client
        .land_change(land(&repo.id, &other.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&gone),
        (Code::FailedPrecondition, "DESTINATION_NOT_FOUND".to_owned())
    );

    // A change already on its (non-trunk) destination is refused, not
    // rebased onto itself.
    bookmark(&repo, "release", Some(&main.commit_id)).await;
    let released = commit_on(client, &repo.id, &main.commit_id, &[("r.txt", "r\n")]).await;
    propose(client, &repo.id, &released.commit_id, "release").await;
    bookmark(&repo, "release", Some(&released.commit_id)).await;
    let already = landed(client, land(&repo.id, &released.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(already.outcome(), Outcome::Blocked, "{already:#?}");
    assert_eq!(already.reason(), Reason::Destination);
    assert_eq!(
        head_of(client, &repo.id, "release").await,
        released.commit_id
    );

    // A wait over ten minutes.
    let long = client
        .land_change(LandChangeRequest {
            wait_ms: 600_001,
            ..land(&repo.id, &change.commit_id)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&long),
        (Code::InvalidArgument, "INVALID_WAIT".to_owned())
    );
}

// 6, 9. A workspace that proposed `@`: amended at the gate, then landed,
// with a new working-copy commit on the landed one (S-121).
#[tokio::test(flavor = "multi_thread")]
async fn workspace_on_landed_commit() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, .. } = setup(&daemon, &repo, OPEN).await;
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            start: Some(v1::create_workspace_request::Start::NewOn(
                main.commit_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w\n").unwrap();
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            to: State::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    // An edit after proposing: the proposal is amended.
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w2\n").unwrap();
    let amended = client
        .land_change(LandChangeRequest {
            workspace: ws.id.clone(),
            ..land(&repo.id, "@")
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&amended),
        (Code::FailedPrecondition, "PROPOSAL_AMENDED".to_owned())
    );
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            to: State::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    let response = landed(
        client,
        LandChangeRequest {
            workspace: ws.id.clone(),
            ..land(&repo.id, "@")
        },
    )
    .await;
    let landing = response.landing.unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let head = head_of(client, &repo.id, "main").await;
    assert_eq!(head, landing.changes[0].landed_commit_id);

    // The workspace is on a new empty change on top; an edit there does not
    // move main.
    let at = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(at.parent_commit_ids, std::slice::from_ref(&head));
    assert!(at.empty);
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w3\n").unwrap();
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert_eq!(head_of(client, &repo.id, "main").await, head);
}

const DAY: Duration = Duration::from_hours(24);

/// Unix milliseconds `ago` before now.
fn ms_ago(ago: Duration) -> i64 {
    let at = std::time::SystemTime::now() - ago;
    i64::try_from(at.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

// 10. What a landing used outlives retention; its artifacts do not get a
// pin.
#[tokio::test(flavor = "multi_thread")]
async fn pins_survive_retention() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = DAY;
        config.file.evidence.retention = DAY;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo, POLICY).await;
    propose(client, &repo.id, &change.commit_id, "").await;
    let key = trusted_key(client).await;
    let used = add(client, &repo.id, &change.tree_id, &key).await;
    let landing = landed(client, land(&repo.id, &change.commit_id))
        .await
        .landing
        .unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    // Evidence nobody landed with, on the same tree; then main moves on, so
    // the landed tree is no longer live.
    let bare = SigningKey::from_seed(&[7; 32]).unwrap();
    let unused = add(client, &repo.id, &change.tree_id, &bare).await;
    let next = commit_on(client, &repo.id, &change.commit_id, &[("n.txt", "n\n")]).await;
    bookmark(&repo, "main", Some(&next.commit_id)).await;
    {
        let conn = rusqlite::Connection::open(daemon.state_dir().join("reef.sqlite3")).unwrap();
        for id in [&used.evidence_id, &unused.evidence_id] {
            conn.execute(
                "UPDATE evidence SET added_at = ?1 WHERE id = ?2",
                rusqlite::params![ms_ago(30 * DAY), id],
            )
            .unwrap();
        }
    }
    let reports = client
        .collect_garbage(&repo.id, false)
        .await
        .unwrap()
        .reports;
    assert_eq!(reports[0].evidence_error, "", "{reports:?}");
    let exists = async |id: &str| client.get_evidence(&repo.id, id).await.is_ok();
    assert!(
        exists(&used.evidence_id).await,
        "the landing's evidence stays"
    );
    assert!(!exists(&unused.evidence_id).await, "the rest expires");
    let after = client
        .get_landing(GetLandingRequest {
            repo: repo.id.clone(),
            id: landing.id.clone(),
            ..GetLandingRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap();
    assert_eq!(
        after.changes[0].pinned_evidence_ids,
        std::slice::from_ref(&used.evidence_id)
    );
    daemon.stop().await;
}

// 15. `EvaluatePolicy` shows a policy the landing it would make.
#[tokio::test(flavor = "multi_thread")]
async fn evaluate_policy_previews_the_landing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, OPEN).await;
    propose(client, &repo.id, &change.commit_id, "").await;
    let moved = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "theirs\n")]).await;
    bookmark(&repo, "main", Some(&moved.commit_id)).await;
    let evaluation = client
        .evaluate_policy(v1::EvaluatePolicyRequest {
            repo: repo.id.clone(),
            revision: change.commit_id.clone(),
            include_input: true,
            ..v1::EvaluatePolicyRequest::default()
        })
        .await
        .unwrap()
        .evaluation
        .unwrap();
    let input: reef_policy::Input = serde_json::from_str(&evaluation.input_json).unwrap();
    assert_eq!(input.change.state, "proposed");
    assert_eq!(input.landing.destination, "trunk()");
    assert_eq!(input.landing.destination_commit, moved.commit_id);
    assert!(input.landing.rebased);
    assert_eq!(input.landing.overlap, ["a.txt"]);
    assert_eq!(evaluation.decision(), PolicyDecision::Block);
}

/// The change's latest landing, once it is no longer queued.
async fn finished(client: &Client, repo: &str, change: &str) -> v1::Landing {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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

// 8. The queue lands in order, and resumes at start.
#[tokio::test(flavor = "multi_thread")]
async fn queue_order_and_restart() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo, OPEN).await;
    let second = commit_on(client, &repo.id, &main.commit_id, &[("x.txt", "x\n")]).await;
    let third = commit_on(client, &repo.id, &main.commit_id, &[("y.txt", "y\n")]).await;
    for queued in [&change, &second, &third] {
        propose(client, &repo.id, &queued.commit_id, "").await;
        let response = landed(
            client,
            LandChangeRequest {
                wait_ms: 0,
                ..land(&repo.id, &queued.commit_id)
            },
        )
        .await;
        assert!(response.enqueue.is_some(), "{response:?}");
    }
    let mut ends = Vec::new();
    for queued in [&change, &second, &third] {
        let landing = finished(client, &repo.id, &queued.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        ends.push(landing.finished_at.unwrap());
    }
    assert!(
        ends.windows(2)
            .all(|w| (w[0].seconds, w[0].nanos) <= (w[1].seconds, w[1].nanos)),
        "landed in queue order: {ends:?}"
    );

    // A queued change whose landing row a crash lost lands after a restart.
    let fourth = commit_on(
        client,
        &repo.id,
        &head_of(client, &repo.id, "main").await,
        &[("z.txt", "z\n")],
    )
    .await;
    propose(client, &repo.id, &fourth.commit_id, "").await;
    {
        let conn = rusqlite::Connection::open(daemon.state_dir().join("reef.sqlite3")).unwrap();
        conn.execute(
            "UPDATE lifecycle SET state = 'queued' WHERE change_id = ?1",
            [&fourth.change_id],
        )
        .unwrap();
    }
    let daemon = daemon.restart().await;
    let resumed = finished(&daemon.client, &repo.id, &fourth.change_id).await;
    assert_eq!(resumed.outcome(), Outcome::Landed, "{resumed:#?}");
    assert_eq!(
        head_of(&daemon.client, &repo.id, "main").await,
        resumed.changes[0].landed_commit_id
    );
    daemon.stop().await;
}
