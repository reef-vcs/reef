//! The daemon client the CLI, the benchmarks, and the tests use. It wraps
//! the generated clients, one method per v1 RPC (those whose ticket has not
//! landed answer `UNIMPLEMENTED`), so nothing else depends on their shape.
//! [`WorkspacesApi`] is the same surface, less maintenance, as a trait, for
//! code that wants to run against a fake daemon (R1.21's heartbeater);
//! [`crate::json`] is the stable JSON form of the responses that `reef -o json`
//! prints (R1.22).

use std::io;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::advice::Advice;
use crate::error::{Classify, ErrorKind, detail_of, kind_from_status};
use crate::proto::v1::changes_client::ChangesClient;
use crate::proto::v1::daemon_client::DaemonClient;
use crate::proto::v1::maintenance_client::MaintenanceClient;
use crate::proto::v1::operations_client::OperationsClient;
use crate::proto::v1::presence_client::PresenceClient;
use crate::proto::v1::queries_client::QueriesClient;
use crate::proto::v1::register_request::IfNotJj;
use crate::proto::v1::remote_client::RemoteClient;
use crate::proto::v1::repositories_client::RepositoriesClient;
use crate::proto::v1::stores_client::StoresClient;
use crate::proto::v1::symbols_client::SymbolsClient;
use crate::proto::v1::trees_client::TreesClient;
use crate::proto::v1::workspaces_client::WorkspacesClient;
use crate::proto::v1::{
    AbandonChangeRequest, AbandonChangeResponse, AbortStoreBuildRequest, AbortStoreBuildResponse,
    ApplyEditsChunk, ApplyEditsResponse, BackupProgress, BackupRequest, BuildEvent, BuildResult,
    BuildStoreRequest, CreateRequest, CreateResponse, DaemonStatusRequest, DaemonStatusResponse,
    DescribeChangeRequest, DescribeChangeResponse, DiffRequest, DiffResponse, FileChunk,
    ForgetRequest, ForgetResponse, Frame, GcRequest, GcResponse, GetChangeRequest,
    GetChangeResponse, GetOperationRequest, GetOperationResponse, HeartbeatRequest,
    HeartbeatResponse, IndexSymbolsRequest, IndexSymbolsResponse, IngestSymbolsChunk,
    IngestSymbolsResponse, InspectRequest, InspectResponse, IssueTokenRequest, IssueTokenResponse,
    ListQueriesRequest, ListQueriesResponse, ListRepositoriesRequest, ListStoresRequest,
    ListTokensRequest, ListTokensResponse, ListTreeRequest, ListTreeResponse,
    ListWorkspacesRequest, ListWorkspacesResponse, LogChangesRequest, LogChangesResponse,
    LogOperationsRequest, LogOperationsResponse, LookupSymbolsRequest, LookupSymbolsResponse,
    MoveRequest, MoveResponse, Page, PrepareStoreBuildRequest, PrepareStoreBuildResponse,
    PresenceUpdate, QuiesceRequest, QuiesceResponse, ReadFileRequest, RegisterRequest,
    RegisterResponse, ReleaseRequest, ReleaseResponse, ReplayRequest, ReplayState,
    ReportPresenceResponse, Repository, ResumeRequest, ResumeResponse, RevokeTokenRequest,
    RevokeTokenResponse, RunQueryRequest, RunQueryResponse, SealStoreBuildRequest, SnapshotRequest,
    SnapshotResponse, SquashChangeRequest, SquashChangeResponse, StoreInfo,
    SubscribePresenceRequest, SubscribeRequest, UndoRequest, UndoResponse, Workspace,
    WorkspaceStatusRequest, WorkspaceStatusResponse,
};

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
    /// The daemon answered with an error. `kind` and `chain` come from the
    /// status's `ErrorDetail`, or from its code when it carries none (an
    /// error from tonic itself, such as a dropped connection).
    #[error("{message}")]
    Rpc {
        /// The error's kind on the daemon.
        kind: ErrorKind,
        /// The daemon's message: the error chain joined with `: `.
        message: String,
        /// The error's message and each source's, outermost first.
        chain: Vec<String>,
        /// The status as received.
        status: tonic::Status,
    },
}

impl From<tonic::Status> for ClientError {
    fn from(status: tonic::Status) -> Self {
        let chain = detail_of(&status)
            .map(|detail| detail.chain)
            .unwrap_or_default();
        ClientError::Rpc {
            kind: kind_from_status(&status),
            message: status.message().to_owned(),
            chain,
            status,
        }
    }
}

impl ClientError {
    /// The fix the daemon attached to its error (`ErrorDetail.hint` and
    /// `next`, R1.26), if any.
    pub fn advice(&self) -> Option<Advice> {
        match self {
            Self::Connect { .. } => None,
            Self::Rpc { status, .. } => detail_of(status).and_then(|d| Advice::from_detail(&d)),
        }
    }
}

impl Classify for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Connect { .. } => ErrorKind::Unavailable,
            Self::Rpc { kind, .. } => *kind,
        }
    }
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

    /// `Daemon.Status`.
    pub async fn status(&self) -> Result<DaemonStatusResponse, ClientError> {
        let mut client = DaemonClient::new(self.channel.clone());
        Ok(client.status(DaemonStatusRequest {}).await?.into_inner())
    }

    fn repositories(&self) -> RepositoriesClient<Channel> {
        RepositoriesClient::new(self.channel.clone())
    }

    fn workspaces(&self) -> WorkspacesClient<Channel> {
        WorkspacesClient::new(self.channel.clone())
    }

    /// `Repositories.Register`: registers the jj repository containing
    /// `path`, which must be absolute. `if_not_jj` says what to do with a
    /// git checkout that has no jj repository yet (R1.25).
    pub async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterResponse, ClientError> {
        let request = RegisterRequest {
            path: path.display().to_string(),
            if_not_jj: if_not_jj.into(),
        };
        Ok(self.repositories().register(request).await?.into_inner())
    }

    /// `Repositories.List`.
    pub async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
        let response = self
            .repositories()
            .list(ListRepositoriesRequest {})
            .await?
            .into_inner();
        Ok(response.repositories)
    }

    /// `Repositories.Forget`. `repo` is an id, an id prefix, or a path
    /// inside the root.
    pub async fn forget_repository(&self, repo: &str) -> Result<ForgetResponse, ClientError> {
        let request = ForgetRequest {
            repo: repo.to_owned(),
        };
        Ok(self.repositories().forget(request).await?.into_inner())
    }

    /// `Workspaces.Create`.
    pub async fn create_workspace(
        &self,
        request: CreateRequest,
    ) -> Result<CreateResponse, ClientError> {
        Ok(self.workspaces().create(request).await?.into_inner())
    }

    /// `Workspaces.List`, every page: follows `next_page_token` from
    /// `request.page` (the first page when unset) to the last page.
    pub async fn list_workspaces(
        &self,
        mut request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError> {
        let mut all = Vec::new();
        loop {
            let response = self.list_workspaces_page(request.clone()).await?;
            all.extend(response.workspaces);
            let next = response.page.map(|page| page.next_page_token);
            match next.filter(|token| !token.is_empty()) {
                Some(token) => request.page.get_or_insert_with(Page::default).page_token = token,
                None => return Ok(all),
            }
        }
    }

    /// `Workspaces.List`, one page.
    pub async fn list_workspaces_page(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<ListWorkspacesResponse, ClientError> {
        Ok(self.workspaces().list(request).await?.into_inner())
    }

    /// `Workspaces.Inspect`. `repo` may be empty to look the workspace up
    /// in every repository.
    pub async fn inspect_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
    ) -> Result<InspectResponse, ClientError> {
        self.inspect(InspectRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            snapshot_first,
            skip_disk_usage: false,
        })
        .await
    }

    /// `Workspaces.Inspect` with every field of the request: `workspace`
    /// may also be a path inside the workspace (R1.26).
    pub async fn inspect(&self, request: InspectRequest) -> Result<InspectResponse, ClientError> {
        Ok(self.workspaces().inspect(request).await?.into_inner())
    }

    /// `Workspaces.Release`, taking a final snapshot first when
    /// `snapshot_first` (the request's `skip_final_snapshot` inverted).
    pub async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseResponse, ClientError> {
        let request = ReleaseRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            force,
            skip_final_snapshot: !snapshot_first,
        };
        Ok(self.workspaces().release(request).await?.into_inner())
    }

    /// `Workspaces.Snapshot`. An empty `description` takes the default.
    pub async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotResponse, ClientError> {
        let request = SnapshotRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            description: description.to_owned(),
        };
        Ok(self.workspaces().snapshot(request).await?.into_inner())
    }

    /// `Workspaces.Move` (R1.06). `repo` may be empty to look the
    /// workspace up in every repository.
    pub async fn move_workspace(&self, request: MoveRequest) -> Result<MoveResponse, ClientError> {
        Ok(self.workspaces().r#move(request).await?.into_inner())
    }

    /// `Workspaces.Heartbeat`: renews `lease_id` on the workspace. `repo`
    /// may be empty to look the workspace up in every repository.
    pub async fn heartbeat_workspace(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<HeartbeatResponse, ClientError> {
        let request = HeartbeatRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            lease_id: lease_id.to_owned(),
        };
        Ok(self.workspaces().heartbeat(request).await?.into_inner())
    }

    /// `Maintenance.Gc` (R1.17). `repo` empty for every registered
    /// repository. Can take minutes on a large repository; the caller sets
    /// its own deadline.
    pub async fn gc(&self, repo: &str, dry_run: bool) -> Result<GcResponse, ClientError> {
        let request = GcRequest {
            repo: repo.to_owned(),
            dry_run,
        };
        let mut client = MaintenanceClient::new(self.channel.clone());
        Ok(client.gc(request).await?.into_inner())
    }

    /// `Maintenance.Backup` (R1.18) of `repo` (empty for every repository)
    /// into `dest`, absolute: progress as it goes, the last message with
    /// `done`; an error ends the stream instead. Copies the whole state, so
    /// the caller sets its own deadline.
    pub async fn backup(
        &self,
        dest: &Path,
        repo: &str,
    ) -> Result<tonic::Streaming<BackupProgress>, ClientError> {
        let request = BackupRequest {
            dest: dest.display().to_string(),
            repo: repo.to_owned(),
        };
        let mut client = MaintenanceClient::new(self.channel.clone());
        Ok(client.backup(request).await?.into_inner())
    }

    /// `Maintenance.Quiesce` (R1.18) of `repo` (empty for every
    /// repository), resuming by itself after `max` (zero for
    /// `backup.quiesce_max`).
    pub async fn quiesce(
        &self,
        repo: &str,
        max: std::time::Duration,
    ) -> Result<QuiesceResponse, ClientError> {
        let request = QuiesceRequest {
            repo: repo.to_owned(),
            max_ms: u64::try_from(max.as_millis()).unwrap_or(u64::MAX),
        };
        let mut client = MaintenanceClient::new(self.channel.clone());
        Ok(client.quiesce(request).await?.into_inner())
    }

    /// `Maintenance.Resume` (R1.18) of the quiesce `token` names.
    pub async fn resume(&self, token: &str) -> Result<ResumeResponse, ClientError> {
        let request = ResumeRequest {
            token: token.to_owned(),
        };
        let mut client = MaintenanceClient::new(self.channel.clone());
        Ok(client.resume(request).await?.into_inner())
    }

    /// `Stores.Build` (R1.20) of store `name` in `repo` at `rev` (empty
    /// for the default), the command given `timeout`: the command's output
    /// lines as they come, then its result; an error ends the stream
    /// instead of a result. The caller sets its own deadline, past
    /// `timeout`.
    pub async fn build_store(
        &self,
        repo: &str,
        name: &str,
        rev: &str,
        timeout: std::time::Duration,
        force: bool,
    ) -> Result<tonic::Streaming<BuildEvent>, ClientError> {
        let request = BuildStoreRequest {
            repo: repo.to_owned(),
            name: name.to_owned(),
            rev: rev.to_owned(),
            timeout: Some(prost_types::Duration {
                seconds: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
                nanos: i32::try_from(timeout.subsec_nanos()).unwrap_or(0),
            }),
            force,
        };
        let mut client = StoresClient::new(self.channel.clone());
        Ok(client.build(request).await?.into_inner())
    }

    /// `Stores.List` (R1.20). `repo` empty for every store.
    pub async fn list_stores(&self, repo: &str) -> Result<Vec<StoreInfo>, ClientError> {
        let request = ListStoresRequest {
            repo: repo.to_owned(),
        };
        let mut client = StoresClient::new(self.channel.clone());
        Ok(client.list(request).await?.into_inner().stores)
    }

    /// `Stores.Prepare` (R2.27).
    pub async fn prepare_store_build(
        &self,
        request: PrepareStoreBuildRequest,
    ) -> Result<PrepareStoreBuildResponse, ClientError> {
        let mut client = StoresClient::new(self.channel.clone());
        Ok(client.prepare(request).await?.into_inner())
    }

    /// `Stores.Seal` (R2.27).
    pub async fn seal_store_build(
        &self,
        request: SealStoreBuildRequest,
    ) -> Result<BuildResult, ClientError> {
        let mut client = StoresClient::new(self.channel.clone());
        Ok(client.seal(request).await?.into_inner())
    }

    /// `Stores.Abort` (R2.27).
    pub async fn abort_store_build(
        &self,
        request: AbortStoreBuildRequest,
    ) -> Result<AbortStoreBuildResponse, ClientError> {
        let mut client = StoresClient::new(self.channel.clone());
        Ok(client.abort(request).await?.into_inner())
    }

    /// `Workspaces.Status` (R2.02).
    pub async fn workspace_status(
        &self,
        request: WorkspaceStatusRequest,
    ) -> Result<WorkspaceStatusResponse, ClientError> {
        Ok(self.workspaces().status(request).await?.into_inner())
    }

    fn changes(&self) -> ChangesClient<Channel> {
        ChangesClient::new(self.channel.clone())
    }

    /// `Changes.Log` (R2.02), one page.
    pub async fn log_changes(
        &self,
        request: LogChangesRequest,
    ) -> Result<LogChangesResponse, ClientError> {
        Ok(self.changes().log(request).await?.into_inner())
    }

    /// `Changes.Get` (R2.02).
    pub async fn get_change(
        &self,
        request: GetChangeRequest,
    ) -> Result<GetChangeResponse, ClientError> {
        Ok(self.changes().get(request).await?.into_inner())
    }

    /// `Changes.Diff` (R2.02; level SYMBOLS R2.15).
    pub async fn diff(&self, request: DiffRequest) -> Result<DiffResponse, ClientError> {
        Ok(self.changes().diff(request).await?.into_inner())
    }

    /// `Changes.Describe` (R2.03).
    pub async fn describe_change(
        &self,
        request: DescribeChangeRequest,
    ) -> Result<DescribeChangeResponse, ClientError> {
        Ok(self.changes().describe(request).await?.into_inner())
    }

    /// `Changes.Abandon` (R2.03).
    pub async fn abandon_change(
        &self,
        request: AbandonChangeRequest,
    ) -> Result<AbandonChangeResponse, ClientError> {
        Ok(self.changes().abandon(request).await?.into_inner())
    }

    /// `Changes.Squash` (R2.03).
    pub async fn squash_change(
        &self,
        request: SquashChangeRequest,
    ) -> Result<SquashChangeResponse, ClientError> {
        Ok(self.changes().squash(request).await?.into_inner())
    }

    fn operations(&self) -> OperationsClient<Channel> {
        OperationsClient::new(self.channel.clone())
    }

    /// `Operations.Log` (R2.02), one page.
    pub async fn log_operations(
        &self,
        request: LogOperationsRequest,
    ) -> Result<LogOperationsResponse, ClientError> {
        Ok(self.operations().log(request).await?.into_inner())
    }

    /// `Operations.Get` (R2.02).
    pub async fn get_operation(
        &self,
        request: GetOperationRequest,
    ) -> Result<GetOperationResponse, ClientError> {
        Ok(self.operations().get(request).await?.into_inner())
    }

    /// `Operations.Undo` (R2.03).
    pub async fn undo(&self, request: UndoRequest) -> Result<UndoResponse, ClientError> {
        Ok(self.operations().undo(request).await?.into_inner())
    }

    /// `Operations.Subscribe` (R2.09): frames until the caller drops the
    /// stream; an error ends it.
    pub async fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<tonic::Streaming<Frame>, ClientError> {
        Ok(self.operations().subscribe(request).await?.into_inner())
    }

    /// `Operations.Replay` (R2.12).
    pub async fn replay(&self, request: ReplayRequest) -> Result<ReplayState, ClientError> {
        Ok(self.operations().replay(request).await?.into_inner())
    }

    fn trees(&self) -> TreesClient<Channel> {
        TreesClient::new(self.channel.clone())
    }

    /// `Trees.List` (R2.04), one page.
    pub async fn list_tree(
        &self,
        request: ListTreeRequest,
    ) -> Result<ListTreeResponse, ClientError> {
        Ok(self.trees().list(request).await?.into_inner())
    }

    /// `Trees.Read` (R2.04): the file's chunks, metadata on the first.
    pub async fn read_file(
        &self,
        request: ReadFileRequest,
    ) -> Result<tonic::Streaming<FileChunk>, ClientError> {
        Ok(self.trees().read(request).await?.into_inner())
    }

    /// `Trees.Apply` (R2.04): `chunks` is the header, then the edits.
    pub async fn apply_edits(
        &self,
        chunks: impl tokio_stream::Stream<Item = ApplyEditsChunk> + Send + 'static,
    ) -> Result<ApplyEditsResponse, ClientError> {
        Ok(self.trees().apply(chunks).await?.into_inner())
    }

    /// `Presence.Subscribe` (R2.10): the live table, then changes.
    pub async fn subscribe_presence(
        &self,
        request: SubscribePresenceRequest,
    ) -> Result<tonic::Streaming<Frame>, ClientError> {
        let mut client = PresenceClient::new(self.channel.clone());
        Ok(client.subscribe(request).await?.into_inner())
    }

    /// `Presence.Report` (R2.10): reports each update `updates` yields
    /// until it ends.
    pub async fn report_presence(
        &self,
        updates: impl tokio_stream::Stream<Item = PresenceUpdate> + Send + 'static,
    ) -> Result<ReportPresenceResponse, ClientError> {
        let mut client = PresenceClient::new(self.channel.clone());
        Ok(client.report(updates).await?.into_inner())
    }

    fn symbols(&self) -> SymbolsClient<Channel> {
        SymbolsClient::new(self.channel.clone())
    }

    /// `Symbols.Index` (R2.14).
    pub async fn index_symbols(
        &self,
        request: IndexSymbolsRequest,
    ) -> Result<IndexSymbolsResponse, ClientError> {
        Ok(self.symbols().index(request).await?.into_inner())
    }

    /// `Symbols.Ingest` (R2.14): `chunks` is the index, the first chunk
    /// naming the repository, the revision, and the source.
    pub async fn ingest_symbols(
        &self,
        chunks: impl tokio_stream::Stream<Item = IngestSymbolsChunk> + Send + 'static,
    ) -> Result<IngestSymbolsResponse, ClientError> {
        Ok(self.symbols().ingest(chunks).await?.into_inner())
    }

    /// `Symbols.Lookup` (R2.14), one page.
    pub async fn lookup_symbols(
        &self,
        request: LookupSymbolsRequest,
    ) -> Result<LookupSymbolsResponse, ClientError> {
        Ok(self.symbols().lookup(request).await?.into_inner())
    }

    /// `Queries.List` (R2.16).
    pub async fn list_queries(&self) -> Result<ListQueriesResponse, ClientError> {
        let mut client = QueriesClient::new(self.channel.clone());
        Ok(client.list(ListQueriesRequest {}).await?.into_inner())
    }

    /// `Queries.Run` (R2.16), one page.
    pub async fn run_query(
        &self,
        request: RunQueryRequest,
    ) -> Result<RunQueryResponse, ClientError> {
        let mut client = QueriesClient::new(self.channel.clone());
        Ok(client.run(request).await?.into_inner())
    }

    fn remote(&self) -> RemoteClient<Channel> {
        RemoteClient::new(self.channel.clone())
    }

    /// `Remote.IssueToken` (R2.08).
    pub async fn issue_token(
        &self,
        request: IssueTokenRequest,
    ) -> Result<IssueTokenResponse, ClientError> {
        Ok(self.remote().issue_token(request).await?.into_inner())
    }

    /// `Remote.ListTokens` (R2.08).
    pub async fn list_tokens(&self) -> Result<ListTokensResponse, ClientError> {
        let request = ListTokensRequest {};
        Ok(self.remote().list_tokens(request).await?.into_inner())
    }

    /// `Remote.RevokeToken` (R2.08). `token` is a name, an id, or a unique
    /// id prefix.
    pub async fn revoke_token(&self, token: &str) -> Result<RevokeTokenResponse, ClientError> {
        let request = RevokeTokenRequest {
            token: token.to_owned(),
        };
        Ok(self.remote().revoke_token(request).await?.into_inner())
    }
}

/// The workspace side of the client's surface as a trait: one method per
/// `Daemon`, `Repositories`, and `Workspaces` RPC, with the same
/// signatures as [`Client`]'s (maintenance and stores are [`Client`]'s
/// alone). [`Client`] implements it; tests and
/// R1.21's heartbeater implement it over a fake.
#[tonic::async_trait]
pub trait WorkspacesApi: Send + Sync {
    /// `Daemon.Status`.
    async fn status(&self) -> Result<DaemonStatusResponse, ClientError>;
    /// `Repositories.Register`.
    async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterResponse, ClientError>;
    /// `Repositories.List`.
    async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError>;
    /// `Repositories.Forget`.
    async fn forget_repository(&self, repo: &str) -> Result<ForgetResponse, ClientError>;
    /// `Workspaces.Create`.
    async fn create_workspace(&self, request: CreateRequest)
    -> Result<CreateResponse, ClientError>;
    /// `Workspaces.List`.
    async fn list_workspaces(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError>;
    /// `Workspaces.Inspect`.
    async fn inspect_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
    ) -> Result<InspectResponse, ClientError>;
    /// `Workspaces.Release`.
    async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseResponse, ClientError>;
    /// `Workspaces.Snapshot`.
    async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotResponse, ClientError>;
    /// `Workspaces.Heartbeat`.
    async fn heartbeat_workspace(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<HeartbeatResponse, ClientError>;
}

#[tonic::async_trait]
impl WorkspacesApi for Client {
    async fn status(&self) -> Result<DaemonStatusResponse, ClientError> {
        Client::status(self).await
    }

    async fn register_repository(
        &self,
        path: &Path,
        if_not_jj: IfNotJj,
    ) -> Result<RegisterResponse, ClientError> {
        Client::register_repository(self, path, if_not_jj).await
    }

    async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
        Client::list_repositories(self).await
    }

    async fn forget_repository(&self, repo: &str) -> Result<ForgetResponse, ClientError> {
        Client::forget_repository(self, repo).await
    }

    async fn create_workspace(
        &self,
        request: CreateRequest,
    ) -> Result<CreateResponse, ClientError> {
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
    ) -> Result<InspectResponse, ClientError> {
        Client::inspect_workspace(self, repo, workspace, snapshot_first).await
    }

    async fn release_workspace(
        &self,
        repo: &str,
        workspace: &str,
        snapshot_first: bool,
        force: bool,
    ) -> Result<ReleaseResponse, ClientError> {
        Client::release_workspace(self, repo, workspace, snapshot_first, force).await
    }

    async fn snapshot_workspace(
        &self,
        repo: &str,
        workspace: &str,
        description: &str,
    ) -> Result<SnapshotResponse, ClientError> {
        Client::snapshot_workspace(self, repo, workspace, description).await
    }

    async fn heartbeat_workspace(
        &self,
        repo: &str,
        workspace: &str,
        lease_id: &str,
    ) -> Result<HeartbeatResponse, ClientError> {
        Client::heartbeat_workspace(self, repo, workspace, lease_id).await
    }
}

/// The owner `reef ws create` sends when not given one: `<user>@<host>`,
/// the user the process runs as and the machine's full hostname (README,
/// Owner).
pub fn default_owner() -> String {
    format!("{}@{}", os_username(), machine_hostname())
}

/// The machine's full hostname, or `localhost` if it has none.
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
}
