//! The workspace spec end to end (R1.19): `reef.toml` in the target
//! commit's tree sets a new workspace's lease length and snapshot excludes,
//! an invalid one fails the create, and a restart reads the spec again from
//! the workspace's base tree, not from its edited files.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::{SyntheticRepo, TestDaemon, proto_millis, test_settings};
use reef_api::v1::{CreateWorkspaceRequest, ListWorkspacesRequest, MissingStores, Workspace};
use reef_daemon::client::ClientError;
use reef_daemon::client::json::duration_millis;
use reef_daemon::model::WorkspaceId;
use reef_daemon::workspace::SnapshotSpec;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
const DEFAULT: Duration = Duration::from_mins(30);
const MAX: Duration = Duration::from_hours(1);

const SPEC: &str = r#"
[workspace]
lease = "45m"

[snapshot]
exclude = ["target/"]

[[store]]
name = "deps"
lockfile = "deps.lock"
mount = "vendor/deps"
build = ["true"]
"#;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.lease.default = DEFAULT;
        config.file.lease.max = MAX;
    })
    .await
}

/// Writes `files` at the registered root and snapshots its `default`
/// workspace, which a create without a start revision starts on.
async fn commit_at_root(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        std::fs::write(root.join(path), contents).unwrap();
    }
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut repo = Repo::open(&root, &test_settings()).await.unwrap();
            repo.snapshot("edit reef.toml").await.unwrap();
        });
    })
    .await
    .unwrap();
}

async fn create(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    lease_ms: u64,
) -> Result<Workspace, ClientError> {
    let response = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            lease_length: Some(proto_millis(lease_ms)),
            // The spec's store is never built here; R1.20's tests build it.
            missing_stores: MissingStores::Skip.into(),
            ..CreateWorkspaceRequest::default()
        })
        .await?;
    Ok(response.workspace.unwrap())
}

fn length(ws: &Workspace) -> Duration {
    Duration::from_millis(duration_millis(
        ws.lease.as_ref().unwrap().length.as_ref().unwrap(),
    ))
}

fn snapshot_spec(daemon: &TestDaemon, ws: &Workspace) -> SnapshotSpec {
    let id: WorkspaceId = ws.id.parse().unwrap();
    daemon.handle.state().manager.entry(id).unwrap().spec()
}

fn expected_spec() -> SnapshotSpec {
    SnapshotSpec {
        excludes: vec!["target/".to_owned()],
        mounts: vec!["vendor/deps".to_owned()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_applies_the_tree_spec() {
    let (daemon, repo) = fixture().await;
    let plain = create(&daemon, &repo, 0).await.unwrap();
    assert_eq!(length(&plain), DEFAULT);
    assert_eq!(snapshot_spec(&daemon, &plain), SnapshotSpec::default());

    commit_at_root(&repo.root, &[("reef.toml", SPEC), ("deps.lock", "v1\n")]).await;
    let ws = create(&daemon, &repo, 0).await.unwrap();
    assert_eq!(length(&ws), Duration::from_mins(45), "reef.toml's lease");
    assert_eq!(snapshot_spec(&daemon, &ws), expected_spec());
    // The request's length wins over reef.toml's.
    let asked = create(&daemon, &repo, 600_000).await.unwrap();
    assert_eq!(length(&asked), Duration::from_mins(10));

    // Above lease.max, reef.toml's lease is capped; a request's is refused.
    let long = SPEC.replace("45m", "2h");
    commit_at_root(&repo.root, &[("reef.toml", &long)]).await;
    let capped = create(&daemon, &repo, 0).await.unwrap();
    assert_eq!(length(&capped), MAX);
    let refused = create(&daemon, &repo, 2 * 3_600_000).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_spec_fails_the_create() {
    let (daemon, repo) = fixture().await;
    commit_at_root(
        &repo.root,
        &[("reef.toml", "[workspace]\nleese = \"1h\"\n")],
    )
    .await;
    let error = create(&daemon, &repo, 0).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(error.to_string().contains("leese"), "{error}");

    // A declared lockfile missing from the tree.
    commit_at_root(&repo.root, &[("reef.toml", SPEC)]).await;
    let error = create(&daemon, &repo, 0).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);

    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert!(listed.is_empty(), "a failed create leaves no row");
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_reads_the_base_spec_again() {
    let (daemon, repo) = fixture().await;
    commit_at_root(&repo.root, &[("reef.toml", SPEC), ("deps.lock", "v1\n")]).await;
    let ws = create(&daemon, &repo, 0).await.unwrap();
    // An edit inside the workspace, snapshotted, does not change its spec,
    // before or after a restart.
    std::fs::write(
        Path::new(&ws.path).join("reef.toml"),
        "[snapshot]\nexclude = [\"other/\"]\n",
    )
    .unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert_eq!(snapshot_spec(&daemon, &ws), expected_spec());
    let daemon = daemon.restart().await;
    assert_eq!(snapshot_spec(&daemon, &ws), expected_spec());
}
