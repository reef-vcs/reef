//! `Workspace.step` through a change's lifecycle (#262, ADR 0080):
//! `GetWorkspace` with `sync_first` reads the files on disk, and the step
//! follows the change from nothing changed to proposed, amended and
//! declined, in a repository that lands through proposals (a policy at
//! `trunk()`, or a recorded proposal) and in one that does not; a proposal
//! onto a destination that lands automatically (R3.45) waits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ChangeStep as Step, CreateIdentityRequest, CreateWorkspaceRequest, DescribeChangeRequest,
    EditFilesRequest, FileEdit, GetWorkspaceRequest, LifecycleState as State,
    TransitionChangeRequest, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;

const POLICY: &str = "version = 1\n";

/// A policy that lands automatically and requires a `test` check from
/// alice's executors, which no executor runs here, so a proposal stays
/// proposed.
const AUTOMATIC: &str = r#"version = 1

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
automatic = true
"#;

/// Automatic snapshots off, so only `sync_first` can record a write.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await
}

async fn create(client: &Client, repo: &SyntheticRepo, on: Option<&str>) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "stepper@test".to_owned(),
            start: on.map(|commit| v1::create_workspace_request::Start::NewOn(commit.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// The workspace as `reef here` reads it.
async fn read(client: &Client, ws: &v1::Workspace) -> v1::Workspace {
    client
        .get_workspace(GetWorkspaceRequest {
            workspace: ws.id.clone(),
            skip_disk_usage: true,
            sync_first: true,
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write(ws: &v1::Workspace, path: &str, text: &str) {
    std::fs::write(Path::new(&ws.path).join(path), text).unwrap();
}

async fn describe(client: &Client, ws: &v1::Workspace, text: &str) {
    client
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: text.to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
}

async fn transition(client: &Client, ws: &v1::Workspace, to: State) {
    client
        .transition_change(TransitionChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            to: to.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

/// Points local bookmark `name` at `commit` from outside the daemon, as
/// `jj bookmark set` would.
async fn set_bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories, default_workspace_loader_factory,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        let settings = test_settings();
        let workspace = default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let loaded = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = loaded.start_transaction();
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&name),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

/// A `main` bookmark on a new commit holding `path`, with `text`.
async fn main_with(client: &Client, repo: &SyntheticRepo, path: &str, text: &str) -> String {
    let main = client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::NewOn("root()".to_owned())),
            edits: vec![FileEdit {
                path: path.to_owned(),
                action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .commit_id;
    set_bookmark(repo, "main", &main).await;
    main
}

#[tokio::test(flavor = "multi_thread")]
async fn without_proposals_the_step_ends_at_publish() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, None).await;
    let fresh = read(client, &ws).await;
    assert_eq!(fresh.step(), Step::Work);
    assert_eq!(fresh.changed_file_count, Some(0));
    assert_eq!(fresh.lifecycle.unwrap().state(), State::Draft);

    // No automatic snapshot runs: only the sync can have seen the write.
    write(&ws, "new.txt", "new\n");
    let written = read(client, &ws).await;
    assert_eq!(written.step(), Step::Describe);
    assert_eq!(written.changed_file_count, Some(1));

    describe(client, &ws, "Add new.txt").await;
    assert_eq!(read(client, &ws).await.step(), Step::Publish);

    // Without `sync_first` nothing is snapshotted: a later write is not
    // counted until a read syncs.
    write(&ws, "later.txt", "later\n");
    let unsynced = client
        .get_workspace(GetWorkspaceRequest {
            workspace: ws.id.clone(),
            skip_disk_usage: true,
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(unsynced.changed_file_count, Some(1));
    assert_eq!(read(client, &ws).await.changed_file_count, Some(2));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_at_trunk_walks_the_proposal() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = main_with(client, &repo, "reef-policy.toml", POLICY).await;
    let ws = create(client, &repo, Some(&main)).await;
    write(&ws, "a.txt", "a\n");
    assert_eq!(read(client, &ws).await.step(), Step::Describe);
    describe(client, &ws, "Add a.txt").await;
    assert_eq!(read(client, &ws).await.step(), Step::Propose);

    transition(client, &ws, State::Proposed).await;
    let proposed = read(client, &ws).await;
    assert_eq!(proposed.step(), Step::Land);
    let lifecycle = proposed.lifecycle.unwrap();
    assert_eq!(lifecycle.state(), State::Proposed);
    assert_eq!(lifecycle.destination, "main");

    write(&ws, "a.txt", "a2\n");
    let amended = read(client, &ws).await;
    assert_eq!(amended.step(), Step::Propose);
    assert!(amended.lifecycle.unwrap().amended);

    transition(client, &ws, State::Declined).await;
    let declined = read(client, &ws).await;
    assert_eq!(declined.step(), Step::Revise);
    assert_eq!(declined.lifecycle.unwrap().round, 1);

    transition(client, &ws, State::Proposed).await;
    transition(client, &ws, State::Withdrawn).await;
    assert_eq!(read(client, &ws).await.step(), Step::Propose);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_proposal_is_enough() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = main_with(client, &repo, "README.md", POLICY).await;
    let first = create(client, &repo, Some(&main)).await;
    write(&first, "one.txt", "one\n");
    describe(client, &first, "One").await;
    assert_eq!(read(client, &first).await.step(), Step::Publish);
    transition(client, &first, State::Proposed).await;

    let second = create(client, &repo, Some(&main)).await;
    write(&second, "two.txt", "two\n");
    describe(client, &second, "Two").await;
    assert_eq!(read(client, &second).await.step(), Step::Propose);

    // A workspace no longer active has none.
    client
        .release_workspace(&repo.id, &second.id, true, false)
        .await
        .unwrap();
    let gone = client
        .get_workspace(GetWorkspaceRequest {
            workspace: second.id.clone(),
            skip_disk_usage: true,
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(gone.step(), Step::Unspecified);
    assert!(gone.lifecycle.is_none());
    daemon.stop().await;
}

/// R3.45: the daemon lands a proposal onto a destination whose policy says
/// `automatic = true` with no request, so its step is `wait`, not `land`,
/// once the automatic service has judged it. An amendment still asks for a
/// new proposal.
#[tokio::test(flavor = "multi_thread")]
async fn an_automatic_destination_waits() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: v1::IdentityKind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let main = main_with(client, &repo, "reef-policy.toml", AUTOMATIC).await;
    let ws = create(client, &repo, Some(&main)).await;
    write(&ws, "a.txt", "a\n");
    describe(client, &ws, "Add a.txt").await;
    assert_eq!(read(client, &ws).await.step(), Step::Propose);

    transition(client, &ws, State::Proposed).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let waiting = loop {
        let proposed = read(client, &ws).await;
        if proposed.step() == Step::Wait {
            break proposed;
        }
        assert_eq!(
            proposed.step(),
            Step::Land,
            "judged yet or not: {proposed:#?}"
        );
        assert!(Instant::now() < deadline, "never waited: {proposed:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(waiting.lifecycle.unwrap().state(), State::Proposed);

    write(&ws, "a.txt", "a2\n");
    let amended = read(client, &ws).await;
    assert_eq!(amended.step(), Step::Propose);
    assert!(amended.lifecycle.unwrap().amended);
    daemon.stop().await;
}
