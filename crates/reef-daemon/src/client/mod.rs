//! The daemon client the CLI and tests use. It wraps the generated
//! `reef.api.v1` clients, one method per implemented RPC plus `*_all`
//! helpers that walk every page, so nothing else depends on their shape.
//! [`WorkspacesApi`] is the same surface, less maintenance, as a trait, for
//! code that wants to run against a fake daemon (R1.21's heartbeater);
//! [`json`] is the stable JSON form of the responses that `reef -o json`
//! prints (R1.22).
//!
//! A daemon of another version answers some calls with `UNIMPLEMENTED`. The
//! client never returns that as is: it asks the daemon what it is
//! (`GetDaemonStatus`, then 0.1's `reef.daemon.v0.Daemon/Status`) and
//! returns [`ClientError::Incompatible`], which the CLI acts on (R2.01).

pub mod json;

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use hyper_util::rt::TokioIo;
use reef_api::v0::daemon_client::DaemonClient as V0DaemonClient;
use reef_api::v1::change_service_client::ChangeServiceClient;
use reef_api::v1::daemon_service_client::DaemonServiceClient;
use reef_api::v1::diff_service_client::DiffServiceClient;
use reef_api::v1::maintenance_service_client::MaintenanceServiceClient;
use reef_api::v1::operation_service_client::OperationServiceClient;
use reef_api::v1::repository_service_client::RepositoryServiceClient;
use reef_api::v1::status_service_client::StatusServiceClient;
use reef_api::v1::store_service_client::StoreServiceClient;
use reef_api::v1::workspace_service_client::WorkspaceServiceClient;
use reef_api::v1::{
    BackupRequest, BackupResponse, BuildStoreRequest, BuildStoreResponse, CollectGarbageRequest,
    CollectGarbageResponse, CreateWorkspaceRequest, CreateWorkspaceResponse,
    ForgetRepositoryRequest, ForgetRepositoryResponse, GetDaemonStatusRequest,
    GetDaemonStatusResponse, GetRepositoryRequest, GetRepositoryResponse, GetWorkspaceRequest,
    GetWorkspaceResponse, IfNotJj, ListRepositoriesRequest, ListRepositoriesResponse,
    ListStoresRequest, ListStoresResponse, ListWorkspacesRequest, ListWorkspacesResponse,
    MoveWorkspaceRequest, MoveWorkspaceResponse, QuiesceRequest, QuiesceResponse,
    RegisterRepositoryRequest, RegisterRepositoryResponse, ReleaseWorkspaceRequest,
    ReleaseWorkspaceResponse, RenewLeaseRequest, RenewLeaseResponse, Repository, ResumeRequest,
    ResumeResponse, SnapshotWorkspaceRequest, SnapshotWorkspaceResponse, StoreInfo, Workspace,
};
use reef_api::v1::{
    GetChangeRequest, GetChangeResponse, GetDiffRequest, GetDiffResponse, GetOperationRequest,
    GetOperationResponse, GetStatusRequest, GetStatusResponse, ListChangesRequest,
    ListChangesResponse, ListOperationsRequest, ListOperationsResponse, LogEntry,
};
use reef_lib::error::{Classify, ErrorKind};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

pub use reef_lib::jj::settings::parse_byte_size;

use crate::error::{Advice, detail_of, kind_from_status};

/// This build's version, which the client compares with the daemon's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Set once any call through any [`Client`] in this process has been
/// answered with success.
/// The page size the `*_all` helpers ask for: the largest a daemon serves,
/// so walking a long list takes as few calls as it can.
const ALL_PAGE_SIZE: i32 = 1000;

static ANSWERED: AtomicBool = AtomicBool::new(false);

/// Whether any call through a [`Client`] in this process has succeeded. The
/// CLI reruns a command against a restarted daemon only while this is false
/// (R2.01): a call that succeeded may have changed something, and a rerun
/// would repeat it.
pub fn any_call_answered() -> bool {
    ANSWERED.load(Ordering::SeqCst)
}

/// A connection to a daemon's socket. Cheap to clone; requests on clones
/// share the connection.
#[derive(Clone, Debug)]
pub struct Client {
    channel: Channel,
    socket: PathBuf,
}

/// Failure to reach the daemon or of a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Nothing answered on the socket.
    #[error("connect to the daemon at {}", socket.display())]
    Connect {
        /// The socket.
        socket: PathBuf,
        /// The underlying error.
        #[source]
        source: tonic::transport::Error,
    },
    /// The daemon answered with an error. `kind`, `reason`, `chain` and
    /// `metadata` come from the status's `ErrorDetail`, or `kind` from its
    /// code when it carries none (an error from tonic itself, such as a
    /// dropped connection).
    #[error("{message}")]
    Rpc {
        /// The error's kind on the daemon.
        kind: ErrorKind,
        /// The specific condition, such as `INVALID_PAGE_TOKEN`; empty when
        /// there is none.
        reason: String,
        /// The daemon's message: the error chain joined with `: `.
        message: String,
        /// The error's message and each source's, outermost first.
        chain: Vec<String>,
        /// Values that go with `reason`.
        metadata: BTreeMap<String, String>,
        /// The status as received.
        status: tonic::Status,
    },
    /// The daemon is another version and cannot serve the call: a 0.1 daemon
    /// (API v0), or a v1 daemon that does not have the method.
    #[error(
        "the daemon at {} is reef {daemon_version} (API {daemon_api}) and cannot serve {method} for reef {VERSION} (API v1)",
        socket.display()
    )]
    Incompatible {
        /// The socket.
        socket: PathBuf,
        /// The method the daemon could not serve.
        method: String,
        /// The daemon's version; `unknown` when it would not say.
        daemon_version: String,
        /// `v0` or `v1`.
        daemon_api: String,
        /// Its active workspaces, which decide whether the CLI may restart
        /// it.
        active_workspaces: u32,
    },
}

impl From<tonic::Status> for ClientError {
    fn from(status: tonic::Status) -> Self {
        let detail = detail_of(&status).unwrap_or_default();
        ClientError::Rpc {
            kind: kind_from_status(&status),
            reason: detail.reason,
            message: status.message().to_owned(),
            chain: detail.chain,
            metadata: detail.metadata,
            status,
        }
    }
}

impl ClientError {
    /// The fix the daemon attached to its error (`ErrorDetail.hint` and
    /// `next`, R1.26), if any.
    pub fn advice(&self) -> Option<Advice> {
        match self {
            Self::Connect { .. } | Self::Incompatible { .. } => None,
            Self::Rpc { status, .. } => detail_of(status).and_then(|d| Advice::from_detail(&d)),
        }
    }

    /// The error's `ErrorDetail.reason`; for [`ClientError::Incompatible`],
    /// `API_VERSION_MISMATCH`.
    pub fn reason(&self) -> &str {
        match self {
            Self::Connect { .. } => "",
            Self::Rpc { reason, .. } => reason,
            Self::Incompatible { .. } => reef_api::error::API_VERSION_MISMATCH,
        }
    }
}

impl Classify for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Connect { .. } => ErrorKind::Unavailable,
            Self::Rpc { kind, .. } => *kind,
            Self::Incompatible { .. } => ErrorKind::Unsupported,
        }
    }
}

/// Whether `status` may say the daemon cannot serve the method: tonic's
/// bare `UNIMPLEMENTED` (no Reef detail), `UNKNOWN_METHOD`, or a declared
/// RPC the daemon does not implement, which is a mismatch only when the
/// daemon is another version (the probe tells).
fn is_unserved(status: &tonic::Status) -> bool {
    if status.code() != tonic::Code::Unimplemented {
        return false;
    }
    detail_of(status).is_none_or(|detail| {
        detail.reason == reef_api::error::UNKNOWN_METHOD
            || detail.reason == reef_api::error::NOT_IMPLEMENTED
    })
}

impl Client {
    /// Connects to the daemon listening on `socket`.
    pub async fn connect(socket: &Path) -> Result<Client, ClientError> {
        let path = socket.to_owned();
        // The URI is required by the API and ignored by the connector.
        let channel = Endpoint::from_static("http://[::]:0")
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let path = path.clone();
                async move { Ok::<_, io::Error>(TokioIo::new(UnixStream::connect(path).await?)) }
            }))
            .await
            .map_err(|source| ClientError::Connect {
                socket: socket.to_owned(),
                source,
            })?;
        Ok(Client {
            channel,
            socket: socket.to_owned(),
        })
    }

    /// The socket this client is connected to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Runs one call of `method`, turning an answer that says the daemon
    /// cannot serve it into [`ClientError::Incompatible`].
    async fn call<T>(
        &self,
        method: &str,
        call: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T, ClientError> {
        // Boxed, as the probe below is: a tonic call's future is large, and
        // a CLI command's future holds several.
        match Box::pin(call).await {
            Ok(response) => {
                ANSWERED.store(true, Ordering::SeqCst);
                Ok(response.into_inner())
            }
            // Boxed: the probe's two calls would otherwise be inlined into
            // every call's future.
            Err(status) if is_unserved(&status) => {
                Err(Box::pin(self.incompatible(method, status)).await)
            }
            Err(status) => Err(status.into()),
        }
    }

    /// What the daemon is, for an [`ClientError::Incompatible`]; the
    /// original error when it answers neither status call. Two calls at
    /// most, and only after an `UNIMPLEMENTED`.
    async fn incompatible(&self, method: &str, status: tonic::Status) -> ClientError {
        let incompatible = |daemon_version: String, daemon_api: &str, active_workspaces| {
            ClientError::Incompatible {
                socket: self.socket.clone(),
                method: method.to_owned(),
                daemon_version,
                daemon_api: daemon_api.to_owned(),
                active_workspaces,
            }
        };
        let mut v1 = DaemonServiceClient::new(self.channel.clone());
        let request = GetDaemonStatusRequest {
            skip_backends: true,
        };
        if let Ok(response) = v1.get_daemon_status(request).await {
            let response = response.into_inner();
            if response.version == VERSION
                && detail_of(&status)
                    .is_some_and(|detail| detail.reason == reef_api::error::NOT_IMPLEMENTED)
            {
                return status.into();
            }
            return incompatible(response.version, "v1", response.active_workspace_count);
        }
        let mut v0 = V0DaemonClient::new(self.channel.clone());
        match v0.status(reef_api::v0::StatusRequest {}).await {
            Ok(response) => {
                let response = response.into_inner();
                incompatible(response.version, "v0", response.active_workspaces)
            }
            Err(_) => status.into(),
        }
    }

    /// `DaemonService.GetDaemonStatus`, with the backend probe.
    pub async fn status(&self) -> Result<GetDaemonStatusResponse, ClientError> {
        self.daemon_status(false).await
    }

    /// `DaemonService.GetDaemonStatus`; `skip_backends` leaves the probe,
    /// which touches the filesystem, out.
    pub async fn daemon_status(
        &self,
        skip_backends: bool,
    ) -> Result<GetDaemonStatusResponse, ClientError> {
        let mut client = DaemonServiceClient::new(self.channel.clone());
        let request = GetDaemonStatusRequest { skip_backends };
        self.call("GetDaemonStatus", client.get_daemon_status(request))
            .await
    }

    /// 0.1's `reef.daemon.v0.Daemon/Status`, which a 0.1 daemon answers and
    /// so does this one: how `reef daemon status` and `stop` work against a
    /// daemon of either API (R2.01).
    pub async fn v0_status(&self) -> Result<reef_api::v0::StatusResponse, ClientError> {
        let mut client = V0DaemonClient::new(self.channel.clone());
        Ok(client
            .status(reef_api::v0::StatusRequest {})
            .await?
            .into_inner())
    }

    fn repositories(&self) -> RepositoryServiceClient<Channel> {
        RepositoryServiceClient::new(self.channel.clone())
    }

    fn workspaces(&self) -> WorkspaceServiceClient<Channel> {
        WorkspaceServiceClient::new(self.channel.clone())
    }

    fn maintenance(&self) -> MaintenanceServiceClient<Channel> {
        MaintenanceServiceClient::new(self.channel.clone())
    }

    fn stores(&self) -> StoreServiceClient<Channel> {
        StoreServiceClient::new(self.channel.clone())
    }

    /// `RepositoryService.RegisterRepository`: registers the jj repository
    /// containing `path`, which must be absolute. `if_not_jj` says what to
    /// do with a git checkout that has no jj repository yet (R1.25).
    pub async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterRepositoryResponse, ClientError> {
        let request = RegisterRepositoryRequest {
            path: path.display().to_string(),
            if_not_jj: if_not_jj.into(),
        };
        self.call(
            "RegisterRepository",
            self.repositories().register_repository(request),
        )
        .await
    }

    /// `RepositoryService.GetRepository`. `repo` is an id, an id prefix, or
    /// a path inside the root.
    pub async fn get_repository(&self, repo: &str) -> Result<GetRepositoryResponse, ClientError> {
        let request = GetRepositoryRequest {
            repo: repo.to_owned(),
        };
        self.call("GetRepository", self.repositories().get_repository(request))
            .await
    }

    /// `RepositoryService.ListRepositories`: one page.
    pub async fn list_repositories_page(
        &self,
        request: ListRepositoriesRequest,
    ) -> Result<ListRepositoriesResponse, ClientError> {
        self.call(
            "ListRepositories",
            self.repositories().list_repositories(request),
        )
        .await
    }

    /// Every registered repository, oldest first, over as many pages as it
    /// takes.
    pub async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
        let mut repositories = Vec::new();
        let mut page_token = String::new();
        loop {
            let response = self
                .list_repositories_page(ListRepositoriesRequest {
                    page_size: ALL_PAGE_SIZE,
                    page_token,
                })
                .await?;
            repositories.extend(response.repositories);
            if response.next_page_token.is_empty() {
                return Ok(repositories);
            }
            page_token = response.next_page_token;
        }
    }

    /// `RepositoryService.ForgetRepository`. `repo` is an id, an id prefix,
    /// or a path inside the root.
    pub async fn forget_repository(
        &self,
        repo: &str,
    ) -> Result<ForgetRepositoryResponse, ClientError> {
        let request = ForgetRepositoryRequest {
            repo: repo.to_owned(),
        };
        self.call(
            "ForgetRepository",
            self.repositories().forget_repository(request),
        )
        .await
    }

    /// `WorkspaceService.CreateWorkspace`.
    pub async fn create_workspace(
        &self,
        request: CreateWorkspaceRequest,
    ) -> Result<CreateWorkspaceResponse, ClientError> {
        self.call(
            "CreateWorkspace",
            self.workspaces().create_workspace(request),
        )
        .await
    }

    /// `WorkspaceService.ListWorkspaces`: one page.
    pub async fn list_workspaces_page(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<ListWorkspacesResponse, ClientError> {
        self.call("ListWorkspaces", self.workspaces().list_workspaces(request))
            .await
    }

    /// Every workspace `request` matches, newest first, over as many pages
    /// as it takes; `request`'s page fields are ignored.
    pub async fn list_workspaces(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError> {
        let mut workspaces = Vec::new();
        let mut request = ListWorkspacesRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        loop {
            let response = self.list_workspaces_page(request.clone()).await?;
            workspaces.extend(response.workspaces);
            if response.next_page_token.is_empty() {
                return Ok(workspaces);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `WorkspaceService.GetWorkspace` with every field of the request:
    /// `workspace` may also be a path inside the workspace (R1.26).
    pub async fn get_workspace(
        &self,
        request: GetWorkspaceRequest,
    ) -> Result<GetWorkspaceResponse, ClientError> {
        self.call("GetWorkspace", self.workspaces().get_workspace(request))
            .await
    }

    /// `WorkspaceService.GetWorkspace`. `repo` may be empty to look the
    /// workspace up in every repository.
    pub async fn inspect_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
    ) -> Result<GetWorkspaceResponse, ClientError> {
        self.get_workspace(GetWorkspaceRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            snapshot_first,
            skip_disk_usage: false,
        })
        .await
    }

    /// `WorkspaceService.ReleaseWorkspace`, taking a final snapshot first
    /// when `snapshot_first` (the request's `skip_final_snapshot` inverted).
    pub async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseWorkspaceResponse, ClientError> {
        let request = ReleaseWorkspaceRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            force,
            skip_final_snapshot: !snapshot_first,
        };
        self.call(
            "ReleaseWorkspace",
            self.workspaces().release_workspace(request),
        )
        .await
    }

    /// `WorkspaceService.SnapshotWorkspace`. An empty `description` takes
    /// the default.
    pub async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotWorkspaceResponse, ClientError> {
        let request = SnapshotWorkspaceRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            description: description.to_owned(),
        };
        self.call(
            "SnapshotWorkspace",
            self.workspaces().snapshot_workspace(request),
        )
        .await
    }

    /// `WorkspaceService.MoveWorkspace` (R1.06). `repo` may be empty to look
    /// the workspace up in every repository.
    pub async fn move_workspace(
        &self,
        request: MoveWorkspaceRequest,
    ) -> Result<MoveWorkspaceResponse, ClientError> {
        self.call("MoveWorkspace", self.workspaces().move_workspace(request))
            .await
    }

    /// `WorkspaceService.RenewLease`: renews `lease_id` on the workspace.
    /// `repo` may be empty to look the workspace up in every repository.
    pub async fn renew_lease(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<RenewLeaseResponse, ClientError> {
        let request = RenewLeaseRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            lease_id: lease_id.to_owned(),
        };
        self.call("RenewLease", self.workspaces().renew_lease(request))
            .await
    }

    /// `MaintenanceService.CollectGarbage` (R1.17). `repo` empty for every
    /// registered repository. Can take minutes on a large repository; the
    /// caller sets its own deadline.
    pub async fn collect_garbage(
        &self,
        repo: &str,
        dry_run: bool,
    ) -> Result<CollectGarbageResponse, ClientError> {
        let request = CollectGarbageRequest {
            repo: repo.to_owned(),
            dry_run,
        };
        self.call(
            "CollectGarbage",
            self.maintenance().collect_garbage(request),
        )
        .await
    }

    /// `MaintenanceService.Backup` (R1.18) of `repo` (empty for every
    /// repository) into `dest`, absolute: progress as it goes, the last
    /// message with `BACKUP_STEP_DONE`; an error ends the stream instead.
    /// Copies the whole state, so the caller sets its own deadline.
    pub async fn backup(
        &self,
        dest: &Path,
        repo: &str,
    ) -> Result<tonic::Streaming<BackupResponse>, ClientError> {
        let request = BackupRequest {
            dest: dest.display().to_string(),
            repo: repo.to_owned(),
        };
        self.call("Backup", self.maintenance().backup(request))
            .await
    }

    /// `MaintenanceService.Quiesce` (R1.18) of `repo` (empty for every
    /// repository), resuming by itself after `max` (zero for
    /// `backup.quiesce_max`).
    pub async fn quiesce(
        &self,
        repo: &str,
        max: std::time::Duration,
    ) -> Result<QuiesceResponse, ClientError> {
        let request = QuiesceRequest {
            repo: repo.to_owned(),
            max: Some(proto_duration(max)),
        };
        self.call("Quiesce", self.maintenance().quiesce(request))
            .await
    }

    /// `MaintenanceService.Resume` (R1.18) of the quiesce `token` names.
    pub async fn resume(&self, token: &str) -> Result<ResumeResponse, ClientError> {
        let request = ResumeRequest {
            token: token.to_owned(),
        };
        self.call("Resume", self.maintenance().resume(request))
            .await
    }

    /// `StoreService.BuildStore` (R1.20) of store `name` in `repo` at
    /// `revision` (empty for the default), the command given `timeout`: the
    /// command's output lines as they come, then its result; an error ends
    /// the stream instead of a result. The caller sets its own deadline,
    /// past `timeout`.
    pub async fn build_store(
        &self,
        repo: &str,
        name: &str,
        revision: &str,
        timeout: std::time::Duration,
        force: bool,
    ) -> Result<tonic::Streaming<BuildStoreResponse>, ClientError> {
        let request = BuildStoreRequest {
            repo: repo.to_owned(),
            name: name.to_owned(),
            revision: revision.to_owned(),
            timeout: Some(proto_duration(timeout)),
            force,
        };
        self.call("BuildStore", self.stores().build_store(request))
            .await
    }

    /// `StoreService.ListStores`: one page.
    pub async fn list_stores_page(
        &self,
        request: ListStoresRequest,
    ) -> Result<ListStoresResponse, ClientError> {
        self.call("ListStores", self.stores().list_stores(request))
            .await
    }

    /// Every sealed store (`repo` empty), or those of `repo`, by name then
    /// hash, over as many pages as it takes.
    pub async fn list_stores(&self, repo: &str) -> Result<Vec<StoreInfo>, ClientError> {
        let mut stores = Vec::new();
        let mut request = ListStoresRequest {
            repo: repo.to_owned(),
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
        };
        loop {
            let response = self.list_stores_page(request.clone()).await?;
            stores.extend(response.stores);
            if response.next_page_token.is_empty() {
                return Ok(stores);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `StatusService.GetStatus`.
    pub async fn get_status(
        &self,
        request: GetStatusRequest,
    ) -> Result<GetStatusResponse, ClientError> {
        let mut client = StatusServiceClient::new(self.channel.clone());
        self.call("GetStatus", client.get_status(request)).await
    }

    /// `ChangeService.ListChanges`: one page of the log.
    pub async fn list_changes_page(
        &self,
        request: ListChangesRequest,
    ) -> Result<ListChangesResponse, ClientError> {
        let mut client = ChangeServiceClient::new(self.channel.clone());
        self.call("ListChanges", client.list_changes(request)).await
    }

    /// The whole log `request` asks for, over as many pages as it takes,
    /// all read at the first page's operation; `request`'s page fields are
    /// ignored.
    pub async fn list_changes(
        &self,
        request: ListChangesRequest,
    ) -> Result<Vec<LogEntry>, ClientError> {
        let mut entries = Vec::new();
        let mut request = ListChangesRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        loop {
            let response = self.list_changes_page(request.clone()).await?;
            entries.extend(response.entries);
            if response.next_page_token.is_empty() {
                return Ok(entries);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `ChangeService.GetChange`.
    pub async fn get_change(
        &self,
        request: GetChangeRequest,
    ) -> Result<GetChangeResponse, ClientError> {
        let mut client = ChangeServiceClient::new(self.channel.clone());
        self.call("GetChange", client.get_change(request)).await
    }

    /// `DiffService.GetDiff`: one page of files.
    pub async fn get_diff_page(
        &self,
        request: GetDiffRequest,
    ) -> Result<GetDiffResponse, ClientError> {
        let mut client = DiffServiceClient::new(self.channel.clone());
        self.call("GetDiff", client.get_diff(request)).await
    }

    /// The whole diff `request` asks for: every page's files, with the
    /// first page's other fields; `request`'s page fields are ignored.
    pub async fn get_diff(&self, request: GetDiffRequest) -> Result<GetDiffResponse, ClientError> {
        let mut request = GetDiffRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        let mut whole = self.get_diff_page(request.clone()).await?;
        while !whole.next_page_token.is_empty() {
            request.page_token = std::mem::take(&mut whole.next_page_token);
            let next = self.get_diff_page(request.clone()).await?;
            whole.files.extend(next.files);
            whole.next_page_token = next.next_page_token;
        }
        Ok(whole)
    }

    /// `OperationService.ListOperations`: one page, newest first.
    pub async fn list_operations_page(
        &self,
        request: ListOperationsRequest,
    ) -> Result<ListOperationsResponse, ClientError> {
        let mut client = OperationServiceClient::new(self.channel.clone());
        self.call("ListOperations", client.list_operations(request))
            .await
    }

    /// `OperationService.GetOperation`, with its effects. `operation_id` is
    /// an id or unique prefix.
    pub async fn get_operation(
        &self,
        repo: &str,
        operation_id: &str,
    ) -> Result<GetOperationResponse, ClientError> {
        let mut client = OperationServiceClient::new(self.channel.clone());
        let request = GetOperationRequest {
            repo: repo.to_owned(),
            operation_id: operation_id.to_owned(),
        };
        self.call("GetOperation", client.get_operation(request))
            .await
    }
}

/// A duration as a `google.protobuf.Duration`.
fn proto_duration(duration: std::time::Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or(0),
    }
}

/// The workspace side of the client's surface as a trait: one method per
/// `DaemonService`, `RepositoryService`, and `WorkspaceService` RPC the CLI
/// uses, with the same signatures as [`Client`]'s (maintenance and stores
/// are [`Client`]'s alone). [`Client`] implements it; tests and R1.21's
/// heartbeater implement it over a fake.
#[tonic::async_trait]
pub trait WorkspacesApi: Send + Sync {
    /// `DaemonService.GetDaemonStatus`.
    async fn status(&self) -> Result<GetDaemonStatusResponse, ClientError>;
    /// `RepositoryService.RegisterRepository`.
    async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterRepositoryResponse, ClientError>;
    /// `RepositoryService.ListRepositories`, every page.
    async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError>;
    /// `RepositoryService.ForgetRepository`.
    async fn forget_repository(&self, repo: &str) -> Result<ForgetRepositoryResponse, ClientError>;
    /// `WorkspaceService.CreateWorkspace`.
    async fn create_workspace(
        &self,
        request: CreateWorkspaceRequest,
    ) -> Result<CreateWorkspaceResponse, ClientError>;
    /// `WorkspaceService.ListWorkspaces`, every page.
    async fn list_workspaces(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError>;
    /// `WorkspaceService.GetWorkspace`.
    async fn inspect_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
    ) -> Result<GetWorkspaceResponse, ClientError>;
    /// `WorkspaceService.ReleaseWorkspace`.
    async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseWorkspaceResponse, ClientError>;
    /// `WorkspaceService.SnapshotWorkspace`.
    async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotWorkspaceResponse, ClientError>;
    /// `WorkspaceService.RenewLease`.
    async fn renew_lease(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<RenewLeaseResponse, ClientError>;
}

#[tonic::async_trait]
impl WorkspacesApi for Client {
    async fn status(&self) -> Result<GetDaemonStatusResponse, ClientError> {
        Client::status(self).await
    }

    async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterRepositoryResponse, ClientError> {
        Client::register_repository(self, path, if_not_jj).await
    }

    async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
        Client::list_repositories(self).await
    }

    async fn forget_repository(&self, repo: &str) -> Result<ForgetRepositoryResponse, ClientError> {
        Client::forget_repository(self, repo).await
    }

    async fn create_workspace(
        &self,
        request: CreateWorkspaceRequest,
    ) -> Result<CreateWorkspaceResponse, ClientError> {
        Client::create_workspace(self, request).await
    }

    async fn list_workspaces(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError> {
        Client::list_workspaces(self, request).await
    }

    async fn inspect_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
    ) -> Result<GetWorkspaceResponse, ClientError> {
        Client::inspect_workspace(self, repo, workspace, snapshot_first).await
    }

    async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseWorkspaceResponse, ClientError> {
        Client::release_workspace(self, repo, workspace, snapshot_first, force).await
    }

    async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotWorkspaceResponse, ClientError> {
        Client::snapshot_workspace(self, repo, workspace, description).await
    }

    async fn renew_lease(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<RenewLeaseResponse, ClientError> {
        Client::renew_lease(self, repo, workspace, lease_id).await
    }
}

/// The owner `reef ws create` sends when not given one: `<user>@<host>`,
/// the user the process runs as and the machine's full hostname (README,
/// Owner).
pub fn default_owner() -> String {
    format!(
        "{}@{}",
        crate::config::os_username(),
        crate::config::machine_hostname()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trait stays object-safe, so a heartbeater can hold
    /// `Arc<dyn WorkspacesApi>`.
    #[test]
    fn workspaces_api_is_object_safe() {
        fn takes(_: &dyn WorkspacesApi) {}
        let _ = takes;
    }

    #[test]
    fn default_owner_is_user_at_host() {
        let owner = default_owner();
        let (user, host) = owner.split_once('@').unwrap();
        assert!(!user.is_empty() && !host.is_empty(), "{owner}");
    }

    #[test]
    fn only_unserved_methods_are_probed() {
        assert!(is_unserved(&tonic::Status::unimplemented("")));
        assert!(!is_unserved(&tonic::Status::not_found("x")));
        assert!(is_unserved(&crate::error::unknown_method("/x/Y")));
        assert!(is_unserved(&crate::error::not_implemented("/x/Y")));
        let other = crate::error::status(crate::service::page::NegativePageSize(-1));
        assert!(!is_unserved(&other));
    }
}
