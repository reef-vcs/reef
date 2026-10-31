//! The thin client of the Reef daemon (R2.05, ADR 0033): everything a
//! process needs to talk to a daemon without being one, and without
//! jj-lib. The CLI (`reef`) is built on it, and the daemon re-exports it as
//! `reef_daemon::client` for its own tests.
//!
//! [`Client`] wraps the generated `reef.api.v1` clients, one method per
//! implemented RPC plus `*_all` helpers that walk every page, so nothing
//! else depends on their shape. [`WorkspacesApi`] is the same surface, less
//! maintenance, as a trait, for code that wants to run against a fake
//! daemon (R1.21's heartbeater); [`json`] is the stable JSON form of the
//! responses that `reef -o json` prints (R1.22), and [`brief`] the views of
//! it that `reef` prints on a pipe (ADR 0045). [`paths`] finds the
//! daemon's socket as the daemon does, [`error`] reads the API's error
//! details, [`logging`] sets up stderr logging from `REEF_LOG*`, and
//! [`backup`] reads a backup's manifest.
//!
//! A daemon of another version answers some calls with `UNIMPLEMENTED`. The
//! client never returns that as is: it asks the daemon what it is
//! (`GetDaemonStatus`, then 0.1's `reef.daemon.v0.Daemon/Status`) and
//! returns [`ClientError::Incompatible`], which the CLI acts on (R2.01).

pub mod backup;
pub mod brief;
pub mod error;
pub mod json;
pub mod logging;
pub mod paths;
pub mod presence;
pub mod scip;
pub mod symbol_text;

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
use reef_api::v1::file_service_client::FileServiceClient;
use reef_api::v1::history_service_client::HistoryServiceClient;
use reef_api::v1::maintenance_service_client::MaintenanceServiceClient;
use reef_api::v1::operation_service_client::OperationServiceClient;
use reef_api::v1::presence_service_client::PresenceServiceClient;
use reef_api::v1::repository_service_client::RepositoryServiceClient;
use reef_api::v1::status_service_client::StatusServiceClient;
use reef_api::v1::store_service_client::StoreServiceClient;
use reef_api::v1::symbol_service_client::SymbolServiceClient;
use reef_api::v1::workspace_service_client::WorkspaceServiceClient;
use reef_api::v1::{
    AbandonChangeRequest, AbandonChangeResponse, DescribeChangeRequest, DescribeChangeResponse,
    EditFilesRequest, EditFilesResponse, ListFilesRequest, ListFilesResponse, ReadFileRequest,
    ReadFileResponse, SubscribeOperationsRequest, SubscribeOperationsResponse,
    UndoOperationRequest, UndoOperationResponse,
};
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
    ClearPresenceRequest, ReportPresenceRequest, ReportPresenceResponse, SubscribePresenceRequest,
    SubscribePresenceResponse,
};
use reef_api::v1::{
    GetChangeRequest, GetChangeResponse, GetDiffRequest, GetDiffResponse, GetOperationRequest,
    GetOperationResponse, GetStatusRequest, GetStatusResponse, ListChangesRequest,
    ListChangesResponse, ListOperationsRequest, ListOperationsResponse, LogEntry, Operation,
};
use reef_api::v1::{
    IndexSymbolsRequest, IndexSymbolsResponse, ListSymbolsRequest, ListSymbolsResponse,
    QueryChangesRequest, QueryChangesResponse, UploadScipDocumentsRequest,
    UploadScipDocumentsResponse,
};
use reef_error::{Advice, Classify, ErrorKind};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::error::{advice_from_detail, detail_of, kind_from_status};

/// The largest request a daemon decodes: tonic's default limit, which the
/// server keeps (R2.04).
pub const MAX_REQUEST_BYTES: usize = 4 << 20;

/// What a create's default start (R1.31) reports as
/// `CreateWorkspaceResponse.start` when no bookmark matched.
pub const ROOT_CHECKOUT: &str = "root checkout";

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
    /// Sent as `reef-agent` on every call.
    agent: AgentInterceptor,
}

/// Adds the `reef-agent` request metadata (R2.03) to every call, when the
/// client has one.
#[derive(Clone, Debug, Default)]
struct AgentInterceptor(Option<tonic::metadata::AsciiMetadataValue>);

impl tonic::service::Interceptor for AgentInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(agent) = &self.0 {
            request
                .metadata_mut()
                .insert(reef_api::AGENT_METADATA, agent.clone());
        }
        Ok(request)
    }
}

/// The transport of every generated client: the channel, with the agent.
type Svc = tonic::service::interceptor::InterceptedService<Channel, AgentInterceptor>;

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
    /// A request larger than the daemon decodes (gRPC's 4 MiB), refused
    /// before it is sent (R2.04).
    #[error("the request is {bytes} bytes; the daemon accepts at most {max}")]
    TooLarge {
        /// The encoded request.
        bytes: usize,
        /// The limit.
        max: usize,
    },
    /// [`Client::with_agent`] was given a value the daemon would refuse.
    #[error("an agent must be visible ASCII of at most {max} bytes: {0:?}", max = reef_api::AGENT_MAX_BYTES)]
    InvalidAgent(String),
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
            Self::Connect { .. } | Self::Incompatible { .. } | Self::InvalidAgent(_) => None,
            Self::TooLarge { .. } => Some(Advice::hint(
                "Edit large files in a workspace, or split the edits across requests.",
            )),
            Self::Rpc { status, .. } => detail_of(status).and_then(|d| advice_from_detail(&d)),
        }
    }

    /// The error's `ErrorDetail.metadata`: the values that go with its
    /// reason; empty for an error that is not the daemon's.
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        static NONE: BTreeMap<String, String> = BTreeMap::new();
        match self {
            Self::Rpc { metadata, .. } => metadata,
            _ => &NONE,
        }
    }

    /// The error's `ErrorDetail.reason`; for [`ClientError::Incompatible`],
    /// `API_VERSION_MISMATCH`.
    pub fn reason(&self) -> &str {
        match self {
            Self::Connect { .. } | Self::TooLarge { .. } => "",
            Self::InvalidAgent(_) => reef_api::error::INVALID_AGENT,
            Self::Rpc { reason, .. } => reason,
            Self::Incompatible { .. } => reef_api::error::API_VERSION_MISMATCH,
        }
    }
}

impl Classify for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Connect { .. } => ErrorKind::Unavailable,
            Self::InvalidAgent(_) | Self::TooLarge { .. } => ErrorKind::InvalidArgument,
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
            agent: AgentInterceptor::default(),
        })
    }

    /// This client, sending `agent` as the `reef-agent` request metadata on
    /// every call (R2.03): the operations those calls record name it as
    /// their agent instead of the workspace's owner. Trimmed; it must be
    /// visible ASCII of at most [`reef_api::AGENT_MAX_BYTES`] bytes, as the
    /// daemon requires. Empty sends none, as the daemon reads an empty
    /// value.
    pub fn with_agent(mut self, agent: impl Into<String>) -> Result<Client, ClientError> {
        let agent = agent.into();
        let Some(text) = check_agent(&agent)? else {
            self.agent = AgentInterceptor::default();
            return Ok(self);
        };
        let value = text
            .parse()
            .map_err(|_| ClientError::InvalidAgent(agent.clone()))?;
        self.agent = AgentInterceptor(Some(value));
        Ok(self)
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

    fn repositories(&self) -> RepositoryServiceClient<Svc> {
        RepositoryServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    fn workspaces(&self) -> WorkspaceServiceClient<Svc> {
        WorkspaceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    fn maintenance(&self) -> MaintenanceServiceClient<Svc> {
        MaintenanceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    fn stores(&self) -> StoreServiceClient<Svc> {
        StoreServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
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
        let mut client =
            StatusServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("GetStatus", client.get_status(request)).await
    }

    /// `ChangeService.ListChanges`: one page of the log.
    pub async fn list_changes_page(
        &self,
        request: ListChangesRequest,
    ) -> Result<ListChangesResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
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

    /// The first `limit` entries of the log `request` asks for (all when
    /// 0), all read at the first page's operation, and whether more exist;
    /// `request`'s page fields are ignored (R2.27).
    pub async fn list_changes_limited(
        &self,
        request: ListChangesRequest,
        limit: usize,
    ) -> Result<Limited<LogEntry>, ClientError> {
        let mut request = ListChangesRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_changes_page(request.clone()).await?;
            if limited.at_operation.is_empty() {
                limited.at_operation = response.at_operation;
            }
            limited.items.extend(response.entries);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `ChangeService.GetChange`.
    pub async fn get_change(
        &self,
        request: GetChangeRequest,
    ) -> Result<GetChangeResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("GetChange", client.get_change(request)).await
    }

    /// `DiffService.GetDiff`: one page of files.
    pub async fn get_diff_page(
        &self,
        request: GetDiffRequest,
    ) -> Result<GetDiffResponse, ClientError> {
        let mut client =
            DiffServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
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
        let mut client =
            OperationServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListOperations", client.list_operations(request))
            .await
    }

    /// The newest `limit` operations `request` asks for (all when 0), and
    /// whether more exist; `request`'s page fields are ignored (R2.27).
    pub async fn list_operations_limited(
        &self,
        request: ListOperationsRequest,
        limit: usize,
    ) -> Result<Limited<Operation>, ClientError> {
        let mut request = ListOperationsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_operations_page(request.clone()).await?;
            limited.items.extend(response.operations);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `OperationService.GetOperation`, with its effects. `operation_id` is
    /// an id or unique prefix.
    pub async fn get_operation(
        &self,
        repo: &str,
        operation_id: &str,
    ) -> Result<GetOperationResponse, ClientError> {
        let mut client =
            OperationServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        let request = GetOperationRequest {
            repo: repo.to_owned(),
            operation_id: operation_id.to_owned(),
        };
        self.call("GetOperation", client.get_operation(request))
            .await
    }

    /// `ChangeService.DescribeChange` (R2.03).
    pub async fn describe_change(
        &self,
        request: DescribeChangeRequest,
    ) -> Result<DescribeChangeResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("DescribeChange", client.describe_change(request))
            .await
    }

    /// `ChangeService.AbandonChange` (R2.03).
    pub async fn abandon_change(
        &self,
        request: AbandonChangeRequest,
    ) -> Result<AbandonChangeResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("AbandonChange", client.abandon_change(request))
            .await
    }

    /// `OperationService.SubscribeOperations` (R2.09): the stream of
    /// frames, once the daemon has accepted the subscription. No deadline:
    /// the caller bounds the call and each wait for a frame itself. An
    /// error mid-stream converts with `ClientError::from`.
    pub async fn subscribe_operations(
        &self,
        request: SubscribeOperationsRequest,
    ) -> Result<tonic::Streaming<SubscribeOperationsResponse>, ClientError> {
        let mut client =
            OperationServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("SubscribeOperations", client.subscribe_operations(request))
            .await
    }

    /// `PresenceService.ReportPresence` (R2.10): a new entry, or a
    /// replacement of the one `presence_id` names.
    pub async fn report_presence(
        &self,
        request: ReportPresenceRequest,
    ) -> Result<ReportPresenceResponse, ClientError> {
        let mut client =
            PresenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ReportPresence", client.report_presence(request))
            .await
    }

    /// `PresenceService.ClearPresence` (R2.10).
    pub async fn clear_presence(&self, presence_id: &str) -> Result<(), ClientError> {
        let mut client =
            PresenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        let request = ClearPresenceRequest {
            presence_id: presence_id.to_owned(),
        };
        self.call("ClearPresence", client.clear_presence(request))
            .await
            .map(|_| ())
    }

    /// `PresenceService.SubscribePresence` (R2.10): the stream of frames,
    /// the current set first. No deadline, as
    /// [`Client::subscribe_operations`].
    pub async fn subscribe_presence(
        &self,
        request: SubscribePresenceRequest,
    ) -> Result<tonic::Streaming<SubscribePresenceResponse>, ClientError> {
        let mut client =
            PresenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("SubscribePresence", client.subscribe_presence(request))
            .await
    }

    /// `OperationService.UndoOperation` (R2.03).
    pub async fn undo_operation(
        &self,
        request: UndoOperationRequest,
    ) -> Result<UndoOperationResponse, ClientError> {
        let mut client =
            OperationServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("UndoOperation", client.undo_operation(request))
            .await
    }

    /// `FileService.ReadFile` (R2.04): at most 1 MiB of the file.
    pub async fn read_file(
        &self,
        request: ReadFileRequest,
    ) -> Result<ReadFileResponse, ClientError> {
        let mut client =
            FileServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ReadFile", client.read_file(request)).await
    }

    /// The file `request` names from its `offset` to the end, over as many
    /// reads as it takes; `request`'s `limit` is ignored. Reads after the
    /// first name the commit the first one read, so a write in between
    /// cannot mix two versions.
    pub async fn read_file_to_end(
        &self,
        request: ReadFileRequest,
    ) -> Result<ReadFileResponse, ClientError> {
        let start = request.offset;
        let mut whole = self
            .read_file(ReadFileRequest {
                limit: 0,
                ..request.clone()
            })
            .await?;
        let mut next = ReadFileRequest {
            revision: whole.commit_id.clone(),
            skip_snapshot: true,
            limit: 0,
            ..request
        };
        loop {
            let read = u64::try_from(whole.content.len()).unwrap_or(u64::MAX);
            next.offset = start.saturating_add(read);
            if next.offset >= whole.size {
                return Ok(whole);
            }
            let page = self.read_file(next.clone()).await?;
            if page.content.is_empty() {
                return Ok(whole);
            }
            whole.content.extend(page.content);
        }
    }

    /// `FileService.ListFiles` (R2.04): one page.
    pub async fn list_files_page(
        &self,
        request: ListFilesRequest,
    ) -> Result<ListFilesResponse, ClientError> {
        let mut client =
            FileServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListFiles", client.list_files(request)).await
    }

    /// The whole listing `request` asks for: every page's entries, with the
    /// first page's other fields, all read at the first page's operation;
    /// `request`'s page fields are ignored.
    pub async fn list_files(
        &self,
        request: ListFilesRequest,
    ) -> Result<ListFilesResponse, ClientError> {
        let mut request = ListFilesRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        let mut whole = self.list_files_page(request.clone()).await?;
        while !whole.next_page_token.is_empty() {
            request.page_token = std::mem::take(&mut whole.next_page_token);
            let next = self.list_files_page(request.clone()).await?;
            whole.entries.extend(next.entries);
            whole.next_page_token = next.next_page_token;
        }
        Ok(whole)
    }

    /// `FileService.EditFiles` (R2.04). A request the daemon would not
    /// decode (over gRPC's 4 MiB) is [`ClientError::TooLarge`], not sent.
    pub async fn edit_files(
        &self,
        request: EditFilesRequest,
    ) -> Result<EditFilesResponse, ClientError> {
        let bytes = prost::Message::encoded_len(&request);
        if bytes > MAX_REQUEST_BYTES {
            return Err(ClientError::TooLarge {
                bytes,
                max: MAX_REQUEST_BYTES,
            });
        }
        let mut client =
            FileServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("EditFiles", client.edit_files(request)).await
    }

    /// `SymbolService.IndexSymbols` (R2.14).
    pub async fn index_symbols(
        &self,
        request: IndexSymbolsRequest,
    ) -> Result<IndexSymbolsResponse, ClientError> {
        let mut client =
            SymbolServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("IndexSymbols", client.index_symbols(request))
            .await
    }

    /// `SymbolService.UploadScipDocuments` (R2.14). A request the daemon
    /// would not decode (over gRPC's 4 MiB) is [`ClientError::TooLarge`],
    /// not sent; [`scip::batches`] splits an index to fit.
    pub async fn upload_scip_documents(
        &self,
        request: UploadScipDocumentsRequest,
    ) -> Result<UploadScipDocumentsResponse, ClientError> {
        let bytes = prost::Message::encoded_len(&request);
        if bytes > MAX_REQUEST_BYTES {
            return Err(ClientError::TooLarge {
                bytes,
                max: MAX_REQUEST_BYTES,
            });
        }
        let mut client =
            SymbolServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("UploadScipDocuments", client.upload_scip_documents(request))
            .await
    }

    /// One page of `HistoryService.QueryChanges` (R2.16).
    pub async fn query_changes(
        &self,
        request: QueryChangesRequest,
    ) -> Result<QueryChangesResponse, ClientError> {
        let mut client =
            HistoryServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("QueryChanges", client.query_changes(request))
            .await
    }

    /// One page of `SymbolService.ListSymbols` (R2.14).
    pub async fn list_symbols_page(
        &self,
        request: ListSymbolsRequest,
    ) -> Result<ListSymbolsResponse, ClientError> {
        let mut client =
            SymbolServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListSymbols", client.list_symbols(request)).await
    }

    /// The whole listing `request` asks for: every page's occurrences, with
    /// the first page's other fields, all read at the first page's
    /// operation; `request`'s page fields are ignored.
    pub async fn list_symbols(
        &self,
        request: ListSymbolsRequest,
    ) -> Result<ListSymbolsResponse, ClientError> {
        let mut request = ListSymbolsRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        let mut whole = self.list_symbols_page(request.clone()).await?;
        while !whole.next_page_token.is_empty() {
            request.page_token = std::mem::take(&mut whole.next_page_token);
            let next = self.list_symbols_page(request.clone()).await?;
            whole.symbols.extend(next.symbols);
            whole.next_page_token = next.next_page_token;
        }
        Ok(whole)
    }
}

/// The first items of a list read up to a limit (R2.27).
#[derive(Clone, Debug, PartialEq)]
pub struct Limited<T> {
    /// At most the limit's worth.
    pub items: Vec<T>,
    /// Whether the list has more than `items`.
    pub more: bool,
    /// The operation the list was read at; empty for the operation log.
    pub at_operation: String,
}

impl<T> Default for Limited<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            more: false,
            at_operation: String::new(),
        }
    }
}

impl<T> Limited<T> {
    /// Whether to stop after a page whose next token is `next`: at the
    /// end, or once `limit` (0 for none) items are held, cut to it.
    fn cut(&mut self, limit: usize, next: &str) -> bool {
        if limit > 0 && self.items.len() >= limit {
            self.more = self.items.len() > limit || !next.is_empty();
            self.items.truncate(limit);
            return true;
        }
        next.is_empty()
    }
}

/// The page size for reading `limit` items (0 for all): the limit, at most
/// the largest page a daemon serves.
fn limited_page_size(limit: usize) -> i32 {
    match i32::try_from(limit) {
        Ok(limit) if limit > 0 => limit.min(ALL_PAGE_SIZE),
        _ => ALL_PAGE_SIZE,
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

/// An agent name as [`Client::with_agent`] sends it: trimmed, `None` when
/// empty, and refused unless it is visible ASCII of at most
/// [`reef_api::AGENT_MAX_BYTES`] bytes, as the daemon requires. For a caller
/// that checks the name before it connects (R2.05).
pub fn check_agent(agent: &str) -> Result<Option<&str>, ClientError> {
    let text = agent.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let visible = text.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    if text.len() > reef_api::AGENT_MAX_BYTES || !visible {
        return Err(ClientError::InvalidAgent(agent.to_owned()));
    }
    Ok(Some(text))
}

/// The owner `reef ws create` sends when not given one: `<user>@<host>`,
/// the user the process runs as and the machine's full hostname (README,
/// Owner).
pub fn default_owner() -> String {
    format!("{}@{}", os_username(), machine_hostname())
}

/// The machine's full hostname, or `localhost` if it has none. The daemon
/// records it on operations, so both agree.
pub fn machine_hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// The name of the user the process runs as: the password database entry,
/// else `$USER`, else the numeric uid.
pub fn os_username() -> String {
    let uid = nix::unistd::getuid();
    nix::unistd::User::from_uid(uid)
        .ok()
        .flatten()
        .map(|user| user.name)
        .or_else(|| std::env::var("USER").ok().filter(|name| !name.is_empty()))
        .unwrap_or_else(|| uid.to_string())
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
        let with_reason = |code, reason: &str| {
            let detail = reef_api::v1::ErrorDetail {
                reason: reason.to_owned(),
                ..reef_api::v1::ErrorDetail::default()
            };
            reef_api::error::status(code, "x".to_owned(), &detail)
        };
        let unimplemented = tonic::Code::Unimplemented;
        assert!(is_unserved(&with_reason(
            unimplemented,
            reef_api::error::UNKNOWN_METHOD
        )));
        assert!(is_unserved(&with_reason(
            unimplemented,
            reef_api::error::NOT_IMPLEMENTED
        )));
        assert!(!is_unserved(&with_reason(unimplemented, "OTHER")));
        assert!(!is_unserved(&with_reason(tonic::Code::InvalidArgument, "")));
    }
}
