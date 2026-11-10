//! Resolver assignments through the daemon (R3.27, ADR 0068): two
//! workspaces write one file from one base, the second change is rebased
//! onto the first outside the daemon, as a user's `jj rebase` would, and the
//! conflict it now carries is assigned, queued, claimed, released, taken
//! and closed. Grace periods are set through the state's test hook, never
//! slept through.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use reef_api::v1::{
    self, AssignConflictRequest, ClaimConflictRequest, ConflictAssignment,
    ConflictAssignmentReason as Reason, ConflictAssignmentState as State, CreateWorkspaceRequest,
    GetConflictAssignmentRequest, GetConflictRequest, ListConflictAssignmentsRequest,
    ProvenanceDeclaration, ReleaseConflictRequest, Workspace,
};
use reef_daemon::client::Client;
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        config.file.remote.enabled = true;
        "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
        config.file.remote.names = vec!["localhost".to_owned()];
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    // A long grace by default: a test that wants an owner queued says so.
    daemon
        .handle
        .state()
        .assignments
        .set_grace(Duration::from_hours(1));
    (daemon, repo)
}

async fn create_as(
    client: &Client,
    repo: &str,
    owner: &str,
    client_session: Option<&str>,
    change: Option<&str>,
) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            start: change.map(|c| v1::create_workspace_request::Start::Change(c.to_owned())),
            provenance: client_session.map(|session| ProvenanceDeclaration {
                client_session: Some(session.to_owned()),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn create(client: &Client, repo: &str, owner: &str) -> Workspace {
    create_as(client, repo, owner, None, None).await
}

async fn write(client: &Client, ws: &Workspace, file: &str, text: &str) -> String {
    std::fs::write(Path::new(&ws.path).join(file), text).unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    snapshot.commit_id
}

/// The repository at `root`, loaded at its head with jj-lib.
fn load(root: &Path) -> std::sync::Arc<ReadonlyRepo> {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
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
    pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap()
}

/// `commit` rebased onto `onto` outside the daemon, as `jj rebase -r`
/// would; the new commit's id.
fn rebase(root: &Path, commit: &str, onto: &str) -> String {
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let old = pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(commit).unwrap()))
        .unwrap();
    let new = pollster::block_on(jj_lib::rewrite::rebase_commit(
        tx.repo_mut(),
        old.clone(),
        vec![CommitId::try_from_hex(onto).unwrap()],
    ))
    .unwrap();
    tx.repo_mut()
        .set_rewritten_commit(old.id().clone(), new.id().clone());
    pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
    pollster::block_on(tx.commit("rebase")).unwrap();
    new.id().hex()
}

/// A commit on `parent` writing `files`, outside the daemon, as the jj CLI
/// would; its id.
fn commit_outside(root: &Path, parent: &str, files: &[(&str, &str)]) -> String {
    use jj_lib::backend::{CopyId, TreeValue};
    use jj_lib::repo_path::RepoPathBuf;
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let parent =
        pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(parent).unwrap()))
            .unwrap();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(parent.tree());
    for (file, text) in files {
        let path = RepoPathBuf::from_internal_string(*file).unwrap();
        let id = pollster::block_on(store.write_file(&path, &mut text.as_bytes())).unwrap();
        builder.set_or_remove(
            path,
            Some(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
    }
    let tree = pollster::block_on(builder.write_tree()).unwrap();
    let commit = pollster::block_on(
        tx.repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description("outside")
            .write(),
    )
    .unwrap();
    pollster::block_on(tx.commit("outside")).unwrap();
    commit.id().hex()
}

struct Scene {
    a: Workspace,
    b: Workspace,
    rebased: String,
}

/// Workspaces `a` and `b` (declaring `client_session` for `b`) each write
/// `files`; `b`'s change is rebased onto `a`'s while `b` is live.
async fn scene_with(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    client_session: Option<&str>,
    files: &[&str],
) -> Scene {
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a@test").await;
    let b = create_as(client, &repo.id, "agent-b@test", client_session, None).await;
    for file in files {
        std::fs::write(Path::new(&a.path).join(file), "one\nfrom a\nthree\n").unwrap();
        std::fs::write(Path::new(&b.path).join(file), "one\nfrom b\nthree\n").unwrap();
    }
    let a_commit = client
        .snapshot_workspace("", &a.id, "")
        .await
        .unwrap()
        .commit_id;
    let b_commit = client
        .snapshot_workspace("", &b.id, "")
        .await
        .unwrap()
        .commit_id;
    let rebased = rebase(&repo.root, &b_commit, &a_commit);
    Scene { a, b, rebased }
}

async fn scene(daemon: &TestDaemon, repo: &SyntheticRepo) -> Scene {
    scene_with(daemon, repo, None, &["conflict.txt"]).await
}

async fn open(client: &Client, repo: &str) -> Vec<ConflictAssignment> {
    client
        .list_conflict_assignments(
            ListConflictAssignmentsRequest {
                repo: repo.to_owned(),
                ..ListConflictAssignmentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items
}

async fn the_one(client: &Client, repo: &str) -> ConflictAssignment {
    let open = open(client, repo).await;
    assert_eq!(open.len(), 1, "{open:#?}");
    open.into_iter().next().unwrap()
}

fn owner_workspace(assignment: &ConflictAssignment) -> &str {
    assignment
        .owner
        .as_ref()
        .map_or("", |owner| owner.workspace_id.as_str())
}

async fn release(client: &Client, repo: &str, ws: &Workspace) {
    client
        .release_workspace(repo, &ws.id, false, false)
        .await
        .unwrap();
}

fn grace(daemon: &TestDaemon, grace: Duration) {
    daemon.handle.state().assignments.set_grace(grace);
}

// 1: the conflict a change introduces is its live session's; a child that
// only inherits it gets none; an operation of the new kind announces it.
#[tokio::test]
async fn assigns_to_the_later_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let assignment = the_one(client, &repo.id).await;
    assert_eq!(assignment.state(), State::Assigned);
    assert_eq!(assignment.reason(), Reason::Default);
    assert_eq!(assignment.change_id, s.b.change_id);
    assert_eq!(owner_workspace(&assignment), s.b.id);
    let owner = assignment.owner.as_ref().unwrap();
    assert!(owner.live);
    assert_eq!(owner.agent, "agent-b@test");
    assert!(assignment.id.starts_with("asg_"));
    let conflict = client
        .get_conflict(GetConflictRequest {
            repo: repo.id.clone(),
            revision: s.rebased.clone(),
            ..GetConflictRequest::default()
        })
        .await
        .unwrap()
        .conflict
        .unwrap();
    assert_eq!(assignment.conflict_id, conflict.id);
    assert_eq!(conflict.assignment.as_ref().unwrap().id, assignment.id);
    // A child inherits the conflict and introduces none: no assignment.
    let _kid = commit_outside(&repo.root, &s.rebased, &[("kid.txt", "kid\n")]);
    assert_eq!(open(client, &repo.id).await.len(), 1);
    // The operation that announced it.
    let ops = client
        .list_operations_limited(
            v1::ListOperationsRequest {
                repo: repo.id.clone(),
                filter: Some(v1::OperationFilter {
                    kinds: vec![v1::OperationKind::ConflictAssignment.into()],
                    workspaces: vec![s.b.id.clone()],
                    ..v1::OperationFilter::default()
                }),
                ..v1::ListOperationsRequest::default()
            },
            10,
        )
        .await
        .unwrap()
        .items;
    let events: Vec<_> = ops
        .iter()
        .flat_map(|op| op.conflict_events.iter())
        .collect();
    assert!(
        events
            .iter()
            .any(|e| e.assignment_id == assignment.id && e.to_state() == State::Assigned),
        "{ops:#?}"
    );
    // Its workspace's status lists it.
    let status = client
        .get_status(v1::GetStatusRequest {
            workspace: s.b.id.clone(),
            skip_snapshot: true,
            ..v1::GetStatusRequest::default()
        })
        .await
        .unwrap()
        .workspace_status
        .unwrap();
    assert_eq!(status.conflict_assignment_count, 1);
    assert_eq!(status.conflict_assignments[0].id, assignment.id);
    let _ = s.a;
    daemon.stop().await;
}

// 2, 4 (S-157): an ended owner keeps it through the grace, then it is
// queued.
#[tokio::test]
async fn owner_ended_queues_after_grace() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    release(client, &repo.id, &s.b).await;
    let waiting = the_one(client, &repo.id).await;
    assert_eq!(waiting.state(), State::Assigned);
    assert!(waiting.queue_after.is_some(), "{waiting:#?}");
    assert!(!waiting.owner.as_ref().unwrap().live);
    grace(&daemon, Duration::ZERO);
    let queued = the_one(client, &repo.id).await;
    assert_eq!(queued.state(), State::Queued);
    assert_eq!(queued.reason(), Reason::OwnerEnded);
    assert_eq!(queued.queue_position, 1);
    assert!(queued.queued_at.is_some());
    // Its last owner is kept.
    assert_eq!(owner_workspace(&queued), s.b.id);
    daemon.stop().await;
}

// 5 (S-151): followed to a live session of the same identity and client
// session; returned to a resumed one while unclaimed.
#[tokio::test]
async fn follows_and_returns_by_client_session() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene_with(&daemon, &repo, Some("harness-1"), &["conflict.txt"]).await;
    // Another identity declaring the same client session is not followed.
    client
        .create_identity(v1::CreateIdentityRequest {
            name: "mallory".to_owned(),
            kind: v1::IdentityKind::Person.into(),
            ..v1::CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let stranger = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "mallory@test".to_owned(),
            provenance: Some(ProvenanceDeclaration {
                client_session: Some("harness-1".to_owned()),
                identity: "mallory".to_owned(),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    release(client, &repo.id, &s.b).await;
    let waiting = the_one(client, &repo.id).await;
    assert_eq!(waiting.reason(), Reason::Default, "{waiting:#?}");
    assert_eq!(owner_workspace(&waiting), s.b.id);
    assert_ne!(owner_workspace(&waiting), stranger.id);
    // The same identity's next workspace is.
    let next = create_as(client, &repo.id, "agent-b@test", Some("harness-1"), None).await;
    let followed = the_one(client, &repo.id).await;
    assert_eq!(followed.state(), State::Assigned);
    assert_eq!(followed.reason(), Reason::Followed);
    assert_eq!(owner_workspace(&followed), next.id);
    // The session ends with nobody to follow: queued.
    grace(&daemon, Duration::ZERO);
    release(client, &repo.id, &next).await;
    let queued = the_one(client, &repo.id).await;
    assert_eq!(queued.state(), State::Queued);
    // It comes back before anyone claims it: returned.
    let resumed = create_as(client, &repo.id, "agent-b@test", Some("harness-1"), None).await;
    let returned = the_one(client, &repo.id).await;
    assert_eq!(returned.state(), State::Assigned);
    assert_eq!(returned.reason(), Reason::Returned);
    assert_eq!(owner_workspace(&returned), resumed.id);
    // Another client session is not followed.
    release(client, &repo.id, &resumed).await;
    let _other = create_as(client, &repo.id, "agent-c@test", Some("harness-2"), None).await;
    let queued = the_one(client, &repo.id).await;
    assert_eq!(queued.state(), State::Queued);
    // Once a person has queued it, it no longer returns.
    client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Queue(true)),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap();
    let _again = create_as(client, &repo.id, "agent-b@test", Some("harness-1"), None).await;
    let still = the_one(client, &repo.id).await;
    assert_eq!(still.state(), State::Queued);
    assert_eq!(still.reason(), Reason::Assigned);
    let _ = stranger;
    daemon.stop().await;
}

// 3 (S-155): a change Reef never recorded is unowned, not queued, and not
// claimed until a person queues it.
#[tokio::test]
async fn unrecorded_changes_are_unowned() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let base = create(client, &repo.id, "base@test").await;
    let trunk = base.commit_id.clone();
    let first = commit_outside(&repo.root, &trunk, &[("person.txt", "mine\n")]);
    let second = commit_outside(&repo.root, &trunk, &[("person.txt", "theirs\n")]);
    rebase(&repo.root, &second, &first);
    let unowned = the_one(client, &repo.id).await;
    assert_eq!(unowned.state(), State::Unowned);
    assert_eq!(unowned.reason(), Reason::NoOwner);
    assert!(unowned.owner.is_none());
    // A workspace that opens it does not make it anyone's.
    let looking = create_as(
        client,
        &repo.id,
        "looker@test",
        None,
        Some(&unowned.change_id),
    )
    .await;
    assert_eq!(the_one(client, &repo.id).await.state(), State::Unowned);
    release(client, &repo.id, &looking).await;
    let resolver = create(client, &repo.id, "resolver@test").await;
    let error = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            assignment: unowned.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        reef_api::error::ASSIGNMENT_UNOWNED,
        "{error}"
    );
    // Nothing is queued, so the queue's head is QUEUE_EMPTY.
    let empty = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(empty.reason(), reef_api::error::QUEUE_EMPTY, "{empty}");
    let queued = client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: unowned.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Queue(true)),
            reason: "a resolver may take it".to_owned(),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap();
    let row = queued.assignment.unwrap();
    assert_eq!(row.state(), State::Queued);
    assert_eq!(row.reason(), Reason::Assigned);
    assert!(queued.operation.is_some());
    daemon.stop().await;
}

/// The scene with `b`'s session ended and its conflict queued.
async fn queued_scene(daemon: &TestDaemon, repo: &SyntheticRepo) -> (Scene, ConflictAssignment) {
    let s = scene(daemon, repo).await;
    grace(daemon, Duration::ZERO);
    release(&daemon.client, &repo.id, &s.b).await;
    let queued = the_one(&daemon.client, &repo.id).await;
    assert_eq!(queued.state(), State::Queued);
    (s, queued)
}

// 7 (S-154): claims come from the queue and are held by the claiming
// workspace's session.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one assignment's life, in turn")]
async fn claims_and_releases() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (s, queued) = queued_scene(&daemon, &repo).await;
    let resolver = create_as(
        client,
        &repo.id,
        "resolver@test",
        None,
        Some(&s.b.change_id),
    )
    .await;
    let claimed = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            expected_conflict_id: queued.conflict_id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap();
    let row = claimed.assignment.unwrap();
    assert_eq!(row.state(), State::Assigned);
    assert_eq!(row.reason(), Reason::Claimed);
    assert_eq!(row.claims, 1);
    assert_eq!(owner_workspace(&row), resolver.id);
    assert_eq!(claimed.change.unwrap().change_id, s.b.change_id);
    let operation = claimed.operation.unwrap();
    assert_eq!(operation.kind(), v1::OperationKind::ConflictAssignment);
    assert_eq!(operation.conflict_events.len(), 1);
    // Claiming again from the same workspace changes nothing.
    let again = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            assignment: row.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap();
    assert!(again.operation.is_none());
    // Another resolver is refused while it is held.
    let other = create(client, &repo.id, "other@test").await;
    let held = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: other.id.clone(),
            assignment: row.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(held.reason(), reef_api::error::ASSIGNMENT_HELD, "{held}");
    assert!(
        held.advice()
            .unwrap()
            .next
            .join(" ")
            .starts_with("reef conflict assign")
    );
    // The claimer gives it up: back of the queue.
    let released = client
        .release_conflict(ReleaseConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            reason: "too hard".to_owned(),
            ..ReleaseConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(released.state(), State::Queued);
    assert_eq!(released.reason(), Reason::Released);
    // A stale conflict id is refused.
    let stale = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: other.id.clone(),
            expected_conflict_id: "cfl_stale".to_owned(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(stale.reason(), reef_api::error::CONFLICT_CHANGED, "{stale}");
    // The claimer's workspace ends: the queue again.
    let reclaimed = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: other.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(reclaimed.claims, 2);
    release(client, &repo.id, &other).await;
    let back = the_one(client, &repo.id).await;
    assert_eq!(back.state(), State::Queued);
    assert_eq!(back.reason(), Reason::OwnerEnded);
    // Its events, oldest first.
    let got = client
        .get_conflict_assignment(GetConflictAssignmentRequest {
            repo: repo.id.clone(),
            assignment: back.id.clone(),
            ..GetConflictAssignmentRequest::default()
        })
        .await
        .unwrap();
    let reasons: Vec<Reason> = got
        .events
        .iter()
        .map(v1::ConflictAssignmentEvent::reason)
        .collect();
    assert_eq!(
        reasons,
        [
            Reason::Default,
            Reason::OwnerEnded,
            Reason::Claimed,
            Reason::Released,
            Reason::Claimed,
            Reason::OwnerEnded
        ],
        "{got:#?}"
    );
    assert_eq!(got.events[3].note, "too hard");
    // By revision too.
    let by_change = client
        .get_conflict_assignment(GetConflictAssignmentRequest {
            repo: repo.id.clone(),
            assignment: s.rebased.clone(),
            ..GetConflictAssignmentRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(by_change.assignment.unwrap().id, back.id);
    let _ = s.a;
    daemon.stop().await;
}

/// A client through the remote listener with a `write` token.
async fn writer(daemon: &TestDaemon) -> Client {
    let deadline = Instant::now() + Duration::from_secs(10);
    let addr = loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            break addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(
            "writer",
            v1::RemoteScope::Write,
            Some(Duration::from_hours(1)),
            "",
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
    Client::connect_remote(&target).await.unwrap()
}

// 8 (S-152, S-156): reassignment, priority, and a person's takeover.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "each kind of assignment, in turn")]
async fn assigns_prioritizes_and_takes_over() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (s, queued) = queued_scene(&daemon, &repo).await;
    let target = create(client, &repo.id, "directed@test").await;
    let assigned = client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Workspace(
                target.id.clone(),
            )),
            priority: Some(7),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(assigned.state(), State::Assigned);
    assert_eq!(assigned.reason(), Reason::Assigned);
    assert_eq!(assigned.priority, 7);
    assert_eq!(owner_workspace(&assigned), target.id);
    // The previous owner's workspace sees the operation that moved it.
    let seen = client
        .list_operations_limited(
            v1::ListOperationsRequest {
                repo: repo.id.clone(),
                filter: Some(v1::OperationFilter {
                    kinds: vec![v1::OperationKind::ConflictAssignment.into()],
                    workspaces: vec![s.b.id.clone()],
                    ..v1::OperationFilter::default()
                }),
                ..v1::ListOperationsRequest::default()
            },
            1,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(seen[0].conflict_events[0].reason(), Reason::Assigned);
    assert_eq!(seen[0].conflict_events[0].from_workspace_id, s.b.id);
    // An agent cannot hand a held conflict to a workspace: claim's rules.
    let other = create(client, &repo.id, "grabber@test").await;
    let grabbed = client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Workspace(
                other.id.clone(),
            )),
            actor: v1::TransitionActor::Agent.into(),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        grabbed.reason(),
        reef_api::error::ASSIGNMENT_HELD,
        "{grabbed}"
    );
    // Out of range, or nothing asked.
    for (priority, reason) in [
        (Some(101), reef_api::error::INVALID_PRIORITY),
        (None, reef_api::error::ASSIGNMENT_TARGET_REQUIRED),
    ] {
        let error = client
            .assign_conflict(AssignConflictRequest {
                repo: repo.id.clone(),
                assignment: queued.id.clone(),
                priority,
                ..AssignConflictRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.reason(), reason, "{error}");
    }
    // An agent cannot take it over.
    let agent = client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Take(true)),
            actor: v1::TransitionActor::Agent.into(),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        agent.reason(),
        reef_api::error::TAKE_NEEDS_PERSON,
        "{agent}"
    );
    // Nor can a remote caller; a write token reassigns and claims for its
    // own workspace.
    let remote = writer(&daemon).await;
    let remote_take = remote
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Take(true)),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        remote_take.reason(),
        reef_api::error::REMOTE_HUMAN_REQUIRED,
        "{remote_take}"
    );
    let own = create(&remote, &repo.id, "remote@test").await;
    remote
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Queue(true)),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap();
    let remote_claim = remote
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: own.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(owner_workspace(&remote_claim), own.id);
    // A person takes it over on the socket; no session's end touches it.
    let taken = client
        .assign_conflict(AssignConflictRequest {
            repo: repo.id.clone(),
            assignment: queued.id.clone(),
            target: Some(v1::assign_conflict_request::Target::Take(true)),
            reason: "I know this code".to_owned(),
            ..AssignConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(taken.state(), State::Taken);
    let person = taken.owner.as_ref().unwrap();
    assert_eq!(person.session_id, "");
    assert_ne!(person.identity_id, "");
    release(client, &repo.id, &own).await;
    release(client, &repo.id, &target).await;
    assert_eq!(the_one(client, &repo.id).await.state(), State::Taken);
    // The person releases it: queued.
    let released = client
        .release_conflict(ReleaseConflictRequest {
            repo: repo.id.clone(),
            assignment: taken.id.clone(),
            ..ReleaseConflictRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(released.state(), State::Queued);
    let _ = s.a;
    daemon.stop().await;
}

// 6: resolving closes it with the session that wrote the resolution; a
// partial resolution keeps it with a new conflict id; undoing the
// resolution reopens it.
#[tokio::test]
async fn closes_changes_and_reopens() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene_with(&daemon, &repo, None, &["one.txt", "two.txt"]).await;
    let first = the_one(client, &repo.id).await;
    assert_eq!(first.files_total, 2);
    let resolver = create_as(
        client,
        &repo.id,
        "resolver@test",
        None,
        Some(&s.b.change_id),
    )
    .await;
    let partial = write(client, &resolver, "one.txt", "one\nmerged\nthree\n").await;
    let changed = the_one(client, &repo.id).await;
    assert_eq!(changed.id, first.id, "a partial resolution keeps it");
    assert_ne!(changed.conflict_id, first.conflict_id);
    assert_eq!(changed.first_conflict_id, first.conflict_id);
    assert_eq!(changed.files_total, 1);
    let _ = partial;
    let resolving = write(client, &resolver, "two.txt", "one\nmerged\nthree\n").await;
    let closed = client
        .get_conflict_assignment(GetConflictAssignmentRequest {
            repo: repo.id.clone(),
            assignment: first.id.clone(),
            ..GetConflictAssignmentRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(closed.state(), State::Closed);
    assert_eq!(closed.reason(), Reason::Resolved);
    assert_eq!(closed.closed_commit_id, resolving);
    assert_eq!(
        closed.resolved_session_id,
        resolver.provenance.unwrap().session_id
    );
    assert_eq!(open(client, &repo.id).await, []);
    // Undo the resolving snapshot: the same conflict is back, and so is
    // its assignment.
    let snapshot_op = client
        .list_operations_limited(
            v1::ListOperationsRequest {
                repo: repo.id.clone(),
                filter: Some(v1::OperationFilter {
                    kinds: vec![v1::OperationKind::Snapshot.into()],
                    workspaces: vec![resolver.id.clone()],
                    ..v1::OperationFilter::default()
                }),
                ..v1::ListOperationsRequest::default()
            },
            1,
        )
        .await
        .unwrap()
        .items[0]
        .id
        .clone();
    client
        .undo_operation(v1::UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: snapshot_op,
            ..v1::UndoOperationRequest::default()
        })
        .await
        .unwrap();
    let reopened = the_one(client, &repo.id).await;
    assert_eq!(reopened.id, first.id);
    assert_eq!(reopened.conflict_id, changed.conflict_id);
    // Abandoning the change closes it.
    client
        .abandon_change(v1::AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: s.b.change_id.clone(),
            ..v1::AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let after = client
        .get_conflict_assignment(GetConflictAssignmentRequest {
            repo: repo.id.clone(),
            assignment: first.id.clone(),
            ..GetConflictAssignmentRequest::default()
        })
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(after.state(), State::Closed);
    assert_eq!(after.reason(), Reason::Abandoned);
    let _ = s.a;
    daemon.stop().await;
}

// 12: undo refuses an assignment's operation.
#[tokio::test]
async fn undo_refuses_assignment_operations() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_s, queued) = queued_scene(&daemon, &repo).await;
    let resolver = create(client, &repo.id, "resolver@test").await;
    let claimed = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            assignment: queued.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap();
    let error = client
        .undo_operation(v1::UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: claimed.operation.unwrap().id,
            ..v1::UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("conflict assignments"),
        "{error}"
    );
    daemon.stop().await;
}

// 10: GC deletes closed assignments past their retention, with their
// events; open ones stay.
#[tokio::test]
async fn retention_deletes_old_closed_assignments() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let first = the_one(client, &repo.id).await;
    let resolver = create_as(
        client,
        &repo.id,
        "resolver@test",
        None,
        Some(&s.b.change_id),
    )
    .await;
    write(client, &resolver, "conflict.txt", "one\nmerged\nthree\n").await;
    assert_eq!(open(client, &repo.id).await, []);
    let state = daemon.handle.state();
    let repo_id = repo.id.parse().unwrap();
    let old = reef_daemon::model::Timestamp(1);
    state.db.age_assignment(repo_id, &first.id, old).unwrap();
    let removed =
        reef_daemon::assign::sweep(&state.db, repo_id, reef_daemon::model::Timestamp::now())
            .unwrap();
    assert_eq!(removed, 1);
    assert!(state.db.assignment(repo_id, &first.id).unwrap().is_none());
    assert_eq!(
        state
            .db
            .assignment_events(repo_id, &first.id, 0, 10)
            .unwrap(),
        []
    );
    daemon.stop().await;
}
