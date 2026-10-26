//! The `reef.api.v1` surface (R2.01, tests 8 to 13): declared RPCs say they
//! are not implemented, unknown methods and 0.1 clients are named, lists
//! page, and writes return the operation they recorded.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use prost::Message as _;
use reef_api::v1::change_service_client::ChangeServiceClient;
use reef_api::v1::diff_service_client::DiffServiceClient;
use reef_api::v1::file_service_client::FileServiceClient;
use reef_api::v1::operation_service_client::OperationServiceClient;
use reef_api::v1::presence_service_client::PresenceServiceClient;
use reef_api::v1::status_service_client::StatusServiceClient;
use reef_api::v1::{
    self, CreateWorkspaceRequest, ErrorKind, GetDaemonStatusRequest, IfNotJj,
    ListRepositoriesRequest, ListStoresRequest, ListWorkspacesRequest, MoveWorkspaceRequest,
    OperationKind, SnapshotKind, move_workspace_request,
};
use reef_daemon::client::ClientError;
use reef_daemon::testing::{TestDaemon, init_synthetic_repo, test_settings};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use tokio::net::UnixStream;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};

async fn channel(socket: &Path) -> Channel {
    let path = socket.to_owned();
    Endpoint::from_static("http://[::]:0")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move { Ok::<_, io::Error>(TokioIo::new(UnixStream::connect(path).await?)) }
        }))
        .await
        .unwrap()
}

/// Asserts `status` is a declared RPC's `NOT_IMPLEMENTED` for `method`.
fn assert_not_implemented(status: &tonic::Status, method: &str) {
    assert_eq!(status.code(), Code::Unimplemented, "{method}: {status:?}");
    let detail = reef_api::error::detail(status).unwrap_or_else(|| panic!("{method}: no detail"));
    assert_eq!(detail.kind(), ErrorKind::Unsupported, "{method}");
    assert_eq!(detail.reason, reef_api::error::NOT_IMPLEMENTED, "{method}");
    assert_eq!(detail.metadata["method"], method);
    assert_ne!(status.message(), "");
}

// 8
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one call per declared RPC")]
async fn declared_rpcs_say_not_implemented() {
    let daemon = TestDaemon::start().await;
    let channel = channel(daemon.handle.socket_path()).await;
    let mut seen = BTreeSet::new();
    macro_rules! check {
        ($client:expr, $rpc:ident, $request:ident, $path:literal) => {{
            let status = $client
                .$rpc(v1::$request::default())
                .await
                .map(|_| ())
                .unwrap_err();
            assert_not_implemented(&status, $path);
            seen.insert($path);
        }};
    }
    let mut status = StatusServiceClient::new(channel.clone());
    check!(
        status,
        get_status,
        GetStatusRequest,
        "/reef.api.v1.StatusService/GetStatus"
    );
    let mut changes = ChangeServiceClient::new(channel.clone());
    check!(
        changes,
        list_changes,
        ListChangesRequest,
        "/reef.api.v1.ChangeService/ListChanges"
    );
    check!(
        changes,
        get_change,
        GetChangeRequest,
        "/reef.api.v1.ChangeService/GetChange"
    );
    check!(
        changes,
        describe_change,
        DescribeChangeRequest,
        "/reef.api.v1.ChangeService/DescribeChange"
    );
    check!(
        changes,
        abandon_change,
        AbandonChangeRequest,
        "/reef.api.v1.ChangeService/AbandonChange"
    );
    let mut diff = DiffServiceClient::new(channel.clone());
    check!(
        diff,
        get_diff,
        GetDiffRequest,
        "/reef.api.v1.DiffService/GetDiff"
    );
    let mut operations = OperationServiceClient::new(channel.clone());
    check!(
        operations,
        list_operations,
        ListOperationsRequest,
        "/reef.api.v1.OperationService/ListOperations"
    );
    check!(
        operations,
        get_operation,
        GetOperationRequest,
        "/reef.api.v1.OperationService/GetOperation"
    );
    check!(
        operations,
        undo_operation,
        UndoOperationRequest,
        "/reef.api.v1.OperationService/UndoOperation"
    );
    check!(
        operations,
        subscribe_operations,
        SubscribeOperationsRequest,
        "/reef.api.v1.OperationService/SubscribeOperations"
    );
    let mut files = FileServiceClient::new(channel.clone());
    check!(
        files,
        read_file,
        ReadFileRequest,
        "/reef.api.v1.FileService/ReadFile"
    );
    check!(
        files,
        list_files,
        ListFilesRequest,
        "/reef.api.v1.FileService/ListFiles"
    );
    check!(
        files,
        edit_files,
        EditFilesRequest,
        "/reef.api.v1.FileService/EditFiles"
    );
    let mut presence = PresenceServiceClient::new(channel.clone());
    check!(
        presence,
        report_presence,
        ReportPresenceRequest,
        "/reef.api.v1.PresenceService/ReportPresence"
    );
    check!(
        presence,
        clear_presence,
        ClearPresenceRequest,
        "/reef.api.v1.PresenceService/ClearPresence"
    );
    check!(
        presence,
        subscribe_presence,
        SubscribePresenceRequest,
        "/reef.api.v1.PresenceService/SubscribePresence"
    );
    assert_eq!(seen.len(), 16);
    // The client does not take a declared RPC of its own version for a
    // version mismatch.
    let error = daemon
        .client
        .daemon_status(true)
        .await
        .map(|status| status.api_revision)
        .unwrap();
    assert_eq!(error, reef_api::API_REVISION);
    daemon.stop().await;
}

/// A call of `path` that the server routes by path alone.
async fn raw_call(channel: &Channel, path: &'static str) -> tonic::Status {
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.unwrap();
    let codec =
        tonic_prost::ProstCodec::<GetDaemonStatusRequest, GetDaemonStatusRequest>::default();
    grpc.unary(
        tonic::Request::new(GetDaemonStatusRequest::default()),
        path.parse().unwrap(),
        codec,
    )
    .await
    .unwrap_err()
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_is_named() {
    let daemon = TestDaemon::start().await;
    let channel = channel(daemon.handle.socket_path()).await;
    for path in [
        "/reef.api.v1.WorkspaceService/Nope",
        "/reef.api.v9.Nothing/Nope",
    ] {
        let status = raw_call(&channel, path).await;
        assert_eq!(status.code(), Code::Unimplemented, "{path}");
        assert!(!status.message().is_empty(), "{path}");
        let detail = reef_api::error::detail(&status).unwrap();
        assert_eq!(detail.kind(), ErrorKind::Unsupported);
        assert_eq!(detail.reason, reef_api::error::UNKNOWN_METHOD);
        assert_eq!(detail.metadata["method"], path);
        assert_eq!(detail.metadata["daemon_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            detail.metadata["api_revision"],
            reef_api::API_REVISION.to_string()
        );
    }
    daemon.stop().await;
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn v0_client_is_told_to_upgrade() {
    let daemon = TestDaemon::start().await;
    let channel = channel(daemon.handle.socket_path()).await;
    let status = raw_call(&channel, "/reef.daemon.v0.Workspaces/List").await;
    assert_eq!(status.code(), Code::FailedPrecondition);
    // As 0.1.0 decodes it: the raw detail, not a google.rpc.Status.
    let detail = reef_api::v0::ErrorDetail::decode(status.details()).unwrap();
    assert_eq!(detail.kind, "Conflict");
    let chain = detail.chain.join(": ");
    assert!(
        chain.contains("API v1") && chain.contains("API v0"),
        "{chain}"
    );
    assert!(detail.hint.starts_with("Upgrade reef"), "{}", detail.hint);

    let mut v0 = reef_api::v0::daemon_client::DaemonClient::new(channel);
    let answer = v0
        .status(reef_api::v0::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(answer.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(answer.pid, std::process::id());
    daemon.stop().await;
}

fn create_request(repo: &str, owner: &str) -> CreateWorkspaceRequest {
    CreateWorkspaceRequest {
        repo: repo.to_owned(),
        owner: owner.to_owned(),
        ..CreateWorkspaceRequest::default()
    }
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

// 12
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the three lists, each paged in turn")]
async fn lists_page() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo(Preset::Small).await;
    let client = &daemon.client;
    let mut created = Vec::new();
    for _ in 0..5 {
        let response = client
            .create_workspace(create_request(&repo.id, "pager"))
            .await
            .unwrap();
        created.push(response.workspace.unwrap().id);
        // Distinct creation times, so newest first is one order.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut pages = Vec::new();
    let mut token = String::new();
    let mut second_token = None;
    loop {
        let response = client
            .list_workspaces_page(ListWorkspacesRequest {
                page_size: 2,
                page_token: token.clone(),
                ..ListWorkspacesRequest::default()
            })
            .await
            .unwrap();
        pages.push(
            response
                .workspaces
                .into_iter()
                .map(|ws| ws.id)
                .collect::<Vec<_>>(),
        );
        if response.next_page_token.is_empty() {
            break;
        }
        if pages.len() == 1 {
            second_token = Some(response.next_page_token.clone());
        }
        token = response.next_page_token;
    }
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
    let listed: Vec<String> = pages.concat();
    let newest_first: Vec<String> = created.iter().rev().cloned().collect();
    assert_eq!(listed, newest_first);

    let error = client
        .list_workspaces_page(ListWorkspacesRequest {
            page_size: 2,
            page_token: second_token.unwrap(),
            owner: "pager".to_owned(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::InvalidArgument,
            reef_api::error::INVALID_PAGE_TOKEN.to_owned()
        )
    );
    let error = client
        .list_workspaces_page(ListWorkspacesRequest {
            page_size: -1,
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).0, Code::InvalidArgument);
    let all = client
        .list_workspaces_page(ListWorkspacesRequest {
            page_size: 5000,
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(all.workspaces.len(), 5);
    assert_eq!(all.next_page_token, "");

    // Repositories, oldest first, one per page.
    let second = daemon.tmp.path().join("second");
    let at = second.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let registered = client
        .register_repository(&second, IfNotJj::Fail)
        .await
        .unwrap();
    let first = client
        .list_repositories_page(ListRepositoriesRequest {
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .unwrap();
    assert_eq!(first.repositories[0].id, repo.id);
    let next = client
        .list_repositories_page(ListRepositoriesRequest {
            page_size: 1,
            page_token: first.next_page_token,
        })
        .await
        .unwrap();
    assert_eq!(next.repositories[0].id, registered.repository.unwrap().id);
    assert_eq!(next.next_page_token, "");

    // No store is built: one empty page.
    let stores = client
        .list_stores_page(ListStoresRequest {
            page_size: 1,
            ..ListStoresRequest::default()
        })
        .await
        .unwrap();
    assert!(stores.stores.is_empty() && stores.next_page_token.is_empty());
    daemon.stop().await;
}

/// Asserts `operation` is what a write by `owner` on `workspace` recorded.
fn assert_written(
    operation: Option<&v1::Operation>,
    kind: OperationKind,
    snapshot: SnapshotKind,
    workspace: &str,
    owner: &str,
) -> String {
    let operation = operation.unwrap_or_else(|| panic!("no operation for {kind:?}"));
    assert_eq!(operation.kind(), kind);
    assert_eq!(operation.snapshot_kind(), snapshot, "{kind:?}");
    assert_eq!(operation.workspace_id, workspace, "{kind:?}");
    assert_eq!(operation.agent, owner, "{kind:?}");
    assert!(!operation.parent_ids.is_empty(), "{kind:?}");
    assert!(operation.effects.is_none(), "{kind:?}");
    operation.id.clone()
}

async fn head_op(root: &Path) -> String {
    Repo::open(root, &test_settings())
        .await
        .unwrap()
        .op_id()
        .to_string()
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn writes_carry_their_operation() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        // No watcher snapshot races the explicit ones.
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let client = &daemon.client;
    let owner = "writer@test";
    let created = client
        .create_workspace(create_request(&repo.id, owner))
        .await
        .unwrap();
    let ws = created.workspace.unwrap();
    let id = assert_written(
        created.operation.as_ref(),
        OperationKind::CreateWorkspace,
        SnapshotKind::Unspecified,
        &ws.id,
        owner,
    );
    assert_eq!(id, head_op(&repo.root).await);

    let unchanged = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(!unchanged.changed && unchanged.operation.is_none());
    std::fs::write(Path::new(&ws.path).join("written.txt"), "hi\n").unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    let id = assert_written(
        snapshot.operation.as_ref(),
        OperationKind::Snapshot,
        SnapshotKind::Explicit,
        &ws.id,
        owner,
    );
    assert_eq!(id, head_op(&repo.root).await);

    let moved = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(move_workspace_request::Target::NewOn(ws.commit_id.clone())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    assert_written(
        moved.operation.as_ref(),
        OperationKind::MoveWorkspace,
        SnapshotKind::Unspecified,
        &ws.id,
        owner,
    );

    let released = client
        .release_workspace("", &ws.id, true, false)
        .await
        .unwrap();
    let id = assert_written(
        released.operation.as_ref(),
        OperationKind::ReleaseWorkspace,
        SnapshotKind::Unspecified,
        &ws.id,
        owner,
    );
    assert_eq!(id, head_op(&repo.root).await);
    daemon.stop().await;
}
