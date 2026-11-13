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
//! it that `reef` prints on a pipe (ADR 0045), and [`schema`] their JSON
//! Schemas (R2.24). [`paths`] finds the
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
pub mod build;
pub mod error;
pub mod json;
pub mod keyless;
pub mod logging;
pub mod paths;
pub mod presence;
pub mod remote;
pub mod schema;
pub mod scip;
pub mod symbol_text;

/// The evidence format (R3.01, ADR 0055), which `reef evidence` signs and
/// checks with.
pub use reef_evidence as evidence;

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use hyper_util::rt::TokioIo;
use reef_api::v0::daemon_client::DaemonClient as V0DaemonClient;
use reef_api::v1;
use reef_api::v1::artifact_service_client::ArtifactServiceClient;
use reef_api::v1::change_service_client::ChangeServiceClient;
use reef_api::v1::comment_service_client::CommentServiceClient;
use reef_api::v1::conflict_service_client::ConflictServiceClient;
use reef_api::v1::daemon_service_client::DaemonServiceClient;
use reef_api::v1::diff_service_client::DiffServiceClient;
use reef_api::v1::evidence_service_client::EvidenceServiceClient;
use reef_api::v1::executor_service_client::ExecutorServiceClient;
use reef_api::v1::file_service_client::FileServiceClient;
use reef_api::v1::git_service_client::GitServiceClient;
use reef_api::v1::history_service_client::HistoryServiceClient;
use reef_api::v1::identity_service_client::IdentityServiceClient;
use reef_api::v1::landing_service_client::LandingServiceClient;
use reef_api::v1::maintenance_service_client::MaintenanceServiceClient;
use reef_api::v1::operation_service_client::OperationServiceClient;
use reef_api::v1::policy_service_client::PolicyServiceClient;
use reef_api::v1::presence_service_client::PresenceServiceClient;
use reef_api::v1::provenance_service_client::ProvenanceServiceClient;
use reef_api::v1::remote_service_client::RemoteServiceClient;
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
    AddEvidenceRequest, AddEvidenceResponse, Evidence, GetEvidenceRequest, GetEvidenceResponse,
    ListEvidenceRequest, ListEvidenceResponse,
};
use reef_api::v1::{
    Artifact, GetArtifactRequest, GetArtifactResponse, ReadArtifactRequest, ReadArtifactResponse,
    UploadArtifactRequest, UploadArtifactResponse,
};
use reef_api::v1::{
    BackupRequest, BackupResponse, BuildStoreRequest, BuildStoreResponse, CollectGarbageRequest,
    CollectGarbageResponse, CreateWebSessionRequest, CreateWebSessionResponse,
    CreateWorkspaceRequest, CreateWorkspaceResponse, ForgetRepositoryRequest,
    ForgetRepositoryResponse, GetDaemonStatusRequest, GetDaemonStatusResponse,
    GetRepositoryRequest, GetRepositoryResponse, GetWorkspaceRequest, GetWorkspaceResponse,
    IfNotJj, ListRepositoriesRequest, ListRepositoriesResponse, ListStoresRequest,
    ListStoresResponse, ListWorkspacesRequest, ListWorkspacesResponse, MoveWorkspaceRequest,
    MoveWorkspaceResponse, QuiesceRequest, QuiesceResponse, RegisterRepositoryRequest,
    RegisterRepositoryResponse, ReleaseWorkspaceRequest, ReleaseWorkspaceResponse,
    RenewLeaseRequest, RenewLeaseResponse, Repository, ResumeRequest, ResumeResponse,
    SnapshotWorkspaceRequest, SnapshotWorkspaceResponse, StoreInfo, Workspace,
};
use reef_api::v1::{
    ClearPresenceRequest, ReportPresenceRequest, ReportPresenceResponse, SubscribePresenceRequest,
    SubscribePresenceResponse,
};
use reef_api::v1::{
    CreateIdentityRequest, CreateIdentityResponse, GetIdentityRequest, GetIdentityResponse,
    GetSessionKeyRequest, GetSessionKeyResponse, Identity, IssueSessionKeyRequest,
    IssueSessionKeyResponse, ListAuthoritiesRequest, ListIdentitiesRequest, ListSessionKeysRequest,
    RevokeIdentityRequest, RevokeIdentityResponse, RevokeSessionKeysRequest,
    RevokeSessionKeysResponse, RotateAuthorityRequest, RotateAuthorityResponse,
};
use reef_api::v1::{
    CreateRemoteTokenRequest, CreateRemoteTokenResponse, ListRemoteTokensRequest, RemoteScope,
    RemoteToken, RevokeRemoteTokenRequest, RevokeRemoteTokenResponse, RotateRemoteTokenRequest,
    RotateRemoteTokenResponse,
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
    /// The socket; for a remote daemon, its URL, which errors print.
    socket: PathBuf,
    /// The remote daemon, when connected to one (R2.08).
    remote: Option<remote::RemoteTarget>,
    /// Sent as `reef-agent`, and the bearer token, on every call.
    agent: AgentInterceptor,
}

/// Adds the `reef-agent` request metadata (R2.03) to every call, when the
/// client has one, and a remote daemon's bearer token (R2.08).
#[derive(Clone, Debug, Default)]
struct AgentInterceptor {
    agent: Option<tonic::metadata::AsciiMetadataValue>,
    bearer: Option<tonic::metadata::AsciiMetadataValue>,
}

impl tonic::service::Interceptor for AgentInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(agent) = &self.agent {
            request
                .metadata_mut()
                .insert(reef_api::AGENT_METADATA, agent.clone());
        }
        if let Some(bearer) = &self.bearer {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
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
    /// Nothing answered at a remote daemon's URL, or the connection failed
    /// before TLS (R2.08).
    #[error("connect to the daemon at {url}")]
    RemoteConnect {
        /// The URL.
        url: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The remote daemon's certificate was refused, before anything was
    /// sent (R2.08).
    #[error("refused the daemon at {url}")]
    RemoteTls {
        /// The URL.
        url: String,
        /// Why.
        #[source]
        failure: remote::TlsFailure,
    },
    /// The remote daemon answered that this host name is not one of its
    /// `[remote] names` (HTTP 421, R2.08).
    #[error("the daemon at {url} does not serve the name {host}")]
    RemoteMisdirected {
        /// The URL.
        url: String,
        /// The host the URL names.
        host: String,
    },
    /// A token that cannot be sent as a header.
    #[error("the remote token is not a valid header value")]
    InvalidToken,
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
    /// Whether the daemon does not serve the method: a daemon older than
    /// the call.
    pub fn is_unimplemented(&self) -> bool {
        matches!(self, ClientError::Rpc { status, .. } if status.code() == tonic::Code::Unimplemented)
    }

    /// The fix the daemon attached to its error (`ErrorDetail.hint` and
    /// `next`, R1.26), if any.
    pub fn advice(&self) -> Option<Advice> {
        match self {
            Self::Connect { .. } | Self::Incompatible { .. } | Self::InvalidAgent(_) => None,
            Self::RemoteConnect { .. } => Some(Advice::hint(
                "Nothing answered there: is the daemon running with its remote listener (`reef daemon status` on that machine says `remote: listening`), and is the port open to this machine?",
            )),
            Self::RemoteTls { failure, .. } => Some(remote_tls_advice(failure)),
            Self::RemoteMisdirected { host, .. } => Some(Advice::hint(format!(
                "Add `{host}` to `[remote] names` in the daemon's config.toml and restart it, or connect by a name or IP address it serves (`reef remote show` on that machine lists them)."
            ))),
            Self::InvalidToken => Some(Advice::hint(
                "The token must be the one line `reef remote token create` printed.",
            )),
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
            Self::Connect { .. }
            | Self::TooLarge { .. }
            | Self::RemoteConnect { .. }
            | Self::RemoteTls { .. }
            | Self::RemoteMisdirected { .. }
            | Self::InvalidToken => "",
            Self::InvalidAgent(_) => reef_api::error::INVALID_AGENT,
            Self::Rpc { reason, .. } => reason,
            Self::Incompatible { .. } => reef_api::error::API_VERSION_MISMATCH,
        }
    }
}

impl Classify for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Connect { .. } | Self::RemoteConnect { .. } => ErrorKind::Unavailable,
            Self::RemoteTls { .. } => ErrorKind::PermissionDenied,
            Self::RemoteMisdirected { .. } => ErrorKind::NotFound,
            Self::InvalidToken | Self::InvalidAgent(_) | Self::TooLarge { .. } => {
                ErrorKind::InvalidArgument
            }
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

/// A failed remote connection as a [`ClientError`].
fn remote_error(url: &str, error: Result<remote::TlsFailure, io::Error>) -> ClientError {
    match error {
        Ok(failure) => ClientError::RemoteTls {
            url: url.to_owned(),
            failure,
        },
        Err(source) => ClientError::RemoteConnect {
            url: url.to_owned(),
            source,
        },
    }
}

/// How to fix a refused certificate.
fn remote_tls_advice(failure: &remote::TlsFailure) -> Advice {
    match failure {
        remote::TlsFailure::PinMismatch { presented, .. } => Advice::hint(format!(
            "The daemon presented {presented}, not the pinned key. If `reef remote show` on the daemon's machine prints {presented} (its key changes after a restore), add the remote again with --fingerprint {presented}; otherwise something else answered at that address."
        )),
        remote::TlsFailure::Untrusted { presented, .. } => Advice::hint(format!(
            "The daemon's certificate is not one this machine trusts. If `reef remote show` on the daemon's machine prints {presented}, pin it: add the remote with --fingerprint {presented}."
        )),
        remote::TlsFailure::Handshake { .. } => Advice::hint(
            "The TLS handshake failed: check that the URL names the daemon's remote listener.",
        ),
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
            remote: None,
            agent: AgentInterceptor::default(),
        })
    }

    /// Connects to a daemon's remote listener (R2.08): TCP, then TLS
    /// verified by `target`'s pin or the OS's roots, then HTTP/2 with the
    /// bearer token on every call. The daemon is verified before anything
    /// is sent. Never starts a daemon.
    pub async fn connect_remote(target: &remote::RemoteTarget) -> Result<Client, ClientError> {
        let url = target.url.clone();
        let origin = url.origin();
        let bearer = target
            .token
            .as_ref()
            .map(|token| format!("Bearer {}", token.expose()).parse())
            .transpose()
            .map_err(|_| ClientError::InvalidToken)?;
        // Connect once here, so a refused certificate or a closed port is
        // its own error rather than a transport error from the first call.
        let first = remote::tls_connect(&url, target.fingerprint.as_deref())
            .await
            .map_err(|error| remote_error(&origin, error))?;
        let first = std::sync::Arc::new(std::sync::Mutex::new(Some(first)));
        let pin = target.fingerprint.clone();
        let reconnect_url = url.clone();
        let channel = Endpoint::from_shared(origin.clone())
            .map_err(|source| ClientError::RemoteConnect {
                url: origin.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, source),
            })?
            .http2_keep_alive_interval(std::time::Duration::from_secs(30))
            .keep_alive_while_idle(true)
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let first = first
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let (url, pin) = (reconnect_url.clone(), pin.clone());
                async move {
                    let stream =
                        match first {
                            Some(stream) => stream,
                            None => remote::tls_connect(&url, pin.as_deref()).await.map_err(
                                |error| match error {
                                    Ok(failure) => io::Error::other(failure),
                                    Err(error) => error,
                                },
                            )?,
                        };
                    Ok::<_, io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|source| ClientError::RemoteConnect {
                url: origin.clone(),
                source: io::Error::other(source),
            })?;
        Ok(Client {
            channel,
            socket: PathBuf::from(&origin),
            remote: Some(target.clone()),
            agent: AgentInterceptor {
                agent: None,
                bearer,
            },
        })
    }

    /// The remote daemon this client is connected to, if not a socket.
    pub fn remote(&self) -> Option<&remote::RemoteTarget> {
        self.remote.as_ref()
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
            self.agent.agent = None;
            return Ok(self);
        };
        let value = text
            .parse()
            .map_err(|_| ClientError::InvalidAgent(agent.clone()))?;
        self.agent.agent = Some(value);
        Ok(self)
    }

    /// The agent label this client sends as `reef-agent`, if any.
    pub fn agent_label(&self) -> Option<String> {
        self.agent
            .agent
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
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
            Err(status) => Err(self.error_of(status)),
        }
    }

    /// `status` as a [`ClientError`]: a remote daemon's HTTP 421 (a host
    /// name it does not serve) is [`ClientError::RemoteMisdirected`].
    fn error_of(&self, status: tonic::Status) -> ClientError {
        if let Some(target) = &self.remote
            && status.code() == tonic::Code::Unknown
            && status.message().ends_with("HTTP status code 421")
        {
            return ClientError::RemoteMisdirected {
                url: target.url.origin(),
                host: target.url.host.clone(),
            };
        }
        status.into()
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
        let mut v1 =
            DaemonServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
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
        let mut v0 = V0DaemonClient::with_interceptor(self.channel.clone(), self.agent.clone());
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
        let mut client =
            DaemonServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        let request = GetDaemonStatusRequest { skip_backends };
        self.call("GetDaemonStatus", client.get_daemon_status(request))
            .await
    }

    /// `DaemonService.CreateWebSession` (R2.18): a web UI session, and a
    /// launch file for a browser if `launch_file`.
    pub async fn create_web_session(
        &self,
        launch_file: bool,
    ) -> Result<CreateWebSessionResponse, ClientError> {
        let mut client =
            DaemonServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        let request = CreateWebSessionRequest { launch_file };
        self.call("CreateWebSession", client.create_web_session(request))
            .await
    }

    fn remote_tokens(&self) -> RemoteServiceClient<Svc> {
        RemoteServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `RemoteService.CreateRemoteToken` (R2.08): a token for the remote
    /// listener; `expires_in` `None` is never.
    pub async fn create_remote_token(
        &self,
        name: &str,
        scope: RemoteScope,
        expires_in: Option<std::time::Duration>,
    ) -> Result<CreateRemoteTokenResponse, ClientError> {
        self.create_remote_token_for(name, scope, expires_in, "")
            .await
    }

    /// [`Client::create_remote_token`] linked to the operator identity
    /// `identity` (R3.03), the only one it then gets session keys for;
    /// empty for none.
    pub async fn create_remote_token_for(
        &self,
        name: &str,
        scope: RemoteScope,
        expires_in: Option<std::time::Duration>,
        identity: &str,
    ) -> Result<CreateRemoteTokenResponse, ClientError> {
        let request = CreateRemoteTokenRequest {
            name: name.to_owned(),
            scope: scope.into(),
            never_expires: expires_in.is_none(),
            expires_in: expires_in.map(proto_duration),
            identity: identity.to_owned(),
        };
        self.call(
            "CreateRemoteToken",
            self.remote_tokens().create_remote_token(request),
        )
        .await
    }

    /// `RemoteService.ListRemoteTokens`, every page.
    pub async fn list_remote_tokens_all(
        &self,
        include_ended: bool,
    ) -> Result<Vec<RemoteToken>, ClientError> {
        let mut tokens = Vec::new();
        let mut page_token = String::new();
        loop {
            let request = ListRemoteTokensRequest {
                include_ended,
                page_size: 0,
                page_token: page_token.clone(),
            };
            let page = self
                .call(
                    "ListRemoteTokens",
                    self.remote_tokens().list_remote_tokens(request),
                )
                .await?;
            tokens.extend(page.tokens);
            if page.next_page_token.is_empty() {
                return Ok(tokens);
            }
            page_token = page.next_page_token;
        }
    }

    /// `RemoteService.RotateRemoteToken`.
    pub async fn rotate_remote_token(
        &self,
        name: &str,
        overlap: std::time::Duration,
    ) -> Result<RotateRemoteTokenResponse, ClientError> {
        let request = RotateRemoteTokenRequest {
            name: name.to_owned(),
            overlap: (!overlap.is_zero()).then(|| proto_duration(overlap)),
        };
        self.call(
            "RotateRemoteToken",
            self.remote_tokens().rotate_remote_token(request),
        )
        .await
    }

    /// `RemoteService.RevokeRemoteToken`.
    pub async fn revoke_remote_token(
        &self,
        name: &str,
    ) -> Result<RevokeRemoteTokenResponse, ClientError> {
        let request = RevokeRemoteTokenRequest {
            name: name.to_owned(),
        };
        self.call(
            "RevokeRemoteToken",
            self.remote_tokens().revoke_remote_token(request),
        )
        .await
    }

    /// 0.1's `reef.daemon.v0.Daemon/Status`, which a 0.1 daemon answers and
    /// so does this one: how `reef daemon status` and `stop` work against a
    /// daemon of either API (R2.01).
    pub async fn v0_status(&self) -> Result<reef_api::v0::StatusResponse, ClientError> {
        let mut client = V0DaemonClient::with_interceptor(self.channel.clone(), self.agent.clone());
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

    fn artifacts(&self) -> ArtifactServiceClient<Svc> {
        ArtifactServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
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

    /// [`Client::list_workspaces`], with the machine's disk status from
    /// the first page (#272); `None` from a daemon before it, or at an
    /// operation.
    pub async fn list_workspaces_with_disk(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<(Vec<Workspace>, Option<Box<v1::DiskStatus>>), ClientError> {
        let mut workspaces = Vec::new();
        let mut disk = None;
        let mut request = ListWorkspacesRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        loop {
            let response = self.list_workspaces_page(request.clone()).await?;
            workspaces.extend(response.workspaces);
            disk = disk.or(response.disk_status);
            if response.next_page_token.is_empty() {
                return Ok((workspaces, disk));
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

    /// `WorkspaceService.GetBuildEnvironment` (#271): the environment that
    /// puts a command in `workspace` under the machine's build limit; an
    /// empty `workspace` for the machine's defaults.
    pub async fn get_build_environment(
        &self,
        repo: &str,
        workspace: &str,
    ) -> Result<v1::GetBuildEnvironmentResponse, ClientError> {
        let request = v1::GetBuildEnvironmentRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
        };
        self.call(
            "GetBuildEnvironment",
            self.workspaces().get_build_environment(request),
        )
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
            at_operation: String::new(),
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
            ended_at_or_before: None,
        };
        self.call("GetOperation", client.get_operation(request))
            .await
    }

    /// `OperationService.GetOperation` by time (R2.12): the newest operation
    /// of `repo` that ended at or before `at`, walking back from the
    /// operation heads.
    pub async fn operation_at(
        &self,
        repo: &str,
        at: std::time::SystemTime,
    ) -> Result<GetOperationResponse, ClientError> {
        self.get_operation_with(GetOperationRequest {
            repo: repo.to_owned(),
            operation_id: String::new(),
            ended_at_or_before: Some(prost_types::Timestamp::from(at)),
        })
        .await
    }

    /// `OperationService.GetOperation` with every field of the request.
    pub async fn get_operation_with(
        &self,
        request: GetOperationRequest,
    ) -> Result<GetOperationResponse, ClientError> {
        let mut client =
            OperationServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
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

    /// `ChangeService.TransitionChange` (R3.09): moves a change through its
    /// lifecycle.
    pub async fn transition_change(
        &self,
        request: v1::TransitionChangeRequest,
    ) -> Result<v1::TransitionChangeResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("TransitionChange", client.transition_change(request))
            .await
    }

    /// `ChangeService.ListProposals` (R3.09): one page.
    pub async fn list_proposals_page(
        &self,
        request: v1::ListProposalsRequest,
    ) -> Result<v1::ListProposalsResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListProposals", client.list_proposals(request))
            .await
    }

    /// The first `limit` recorded lifecycles `request` asks for (all when
    /// 0), newest update first, and whether more exist.
    pub async fn list_proposals(
        &self,
        request: v1::ListProposalsRequest,
        limit: usize,
    ) -> Result<Limited<v1::ProposalEntry>, ClientError> {
        let mut request = v1::ListProposalsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_proposals_page(request.clone()).await?;
            limited.items.extend(response.entries);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `ChangeService.ListTransitions` (R3.09): one page.
    pub async fn list_transitions_page(
        &self,
        request: v1::ListTransitionsRequest,
    ) -> Result<v1::ListTransitionsResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListTransitions", client.list_transitions(request))
            .await
    }

    /// The first `limit` transitions `request` asks for (all when 0),
    /// newest first, and whether more exist.
    pub async fn list_transitions(
        &self,
        request: v1::ListTransitionsRequest,
        limit: usize,
    ) -> Result<Limited<v1::ChangeTransition>, ClientError> {
        let mut request = v1::ListTransitionsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_transitions_page(request.clone()).await?;
            limited.items.extend(response.transitions);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
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

    /// `EvidenceService.AddEvidence` (R3.01): stores an envelope the
    /// caller signed.
    pub async fn add_evidence(
        &self,
        request: AddEvidenceRequest,
    ) -> Result<AddEvidenceResponse, ClientError> {
        let mut client =
            EvidenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("AddEvidence", client.add_evidence(request)).await
    }

    /// `EvidenceService.GetEvidence` (R3.01): one object of `repo` by its
    /// id or a unique prefix, with its envelope.
    pub async fn get_evidence(
        &self,
        repo: &str,
        evidence_id: &str,
    ) -> Result<GetEvidenceResponse, ClientError> {
        let mut client =
            EvidenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        let request = GetEvidenceRequest {
            repo: repo.to_owned(),
            evidence_id: evidence_id.to_owned(),
        };
        self.call("GetEvidence", client.get_evidence(request)).await
    }

    /// One page of `EvidenceService.ListEvidence` (R3.01).
    pub async fn list_evidence_page(
        &self,
        request: ListEvidenceRequest,
    ) -> Result<ListEvidenceResponse, ClientError> {
        let mut client =
            EvidenceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("ListEvidence", client.list_evidence(request))
            .await
    }

    /// The first `limit` objects the listing `request` asks for (all when
    /// 0), newest added first, and whether more exist; `request`'s page
    /// fields are ignored.
    pub async fn list_evidence(
        &self,
        request: ListEvidenceRequest,
        limit: usize,
    ) -> Result<Limited<Evidence>, ClientError> {
        let mut request = ListEvidenceRequest {
            page_size: limited_page_size(limit).min(EVIDENCE_PAGE_SIZE),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_evidence_page(request.clone()).await?;
            limited.items.extend(response.evidence);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }
}

impl Client {
    /// `ArtifactService.UploadArtifact` (R3.02): one chunk of an upload, or
    /// a probe (offset 0, no data).
    pub async fn upload_artifact(
        &self,
        request: UploadArtifactRequest,
    ) -> Result<UploadArtifactResponse, ClientError> {
        let mut client = self.artifacts();
        self.call("UploadArtifact", client.upload_artifact(request))
            .await
    }

    /// Uploads the file at `path` as an artifact of `repo` (R3.02): hashes
    /// it, probes, then sends 1 MiB chunks from the bytes the daemon holds,
    /// following `ARTIFACT_OFFSET_MISMATCH`'s `received`. Bytes already
    /// stored are not sent again. A file that changes after it is hashed
    /// ends in `ARTIFACT_DIGEST_MISMATCH`, or in [`UploadError::Changed`].
    pub async fn upload_artifact_file(
        &self,
        repo: &str,
        path: &Path,
    ) -> Result<ArtifactUpload, UploadError> {
        self.upload_artifact_file_as(repo, path, None).await
    }

    /// [`Client::upload_artifact_file`] naming the file by `sha256` when the
    /// caller hashed it already (a claim it signed names it): the daemon
    /// then refuses bytes that changed since with
    /// `ARTIFACT_DIGEST_MISMATCH`, and the file is read once.
    pub async fn upload_artifact_file_as(
        &self,
        repo: &str,
        path: &Path,
        sha256: Option<&str>,
    ) -> Result<ArtifactUpload, UploadError> {
        use std::os::unix::fs::FileExt as _;
        let io = |source| UploadError::Io {
            path: path.to_owned(),
            source,
        };
        let file = std::sync::Arc::new(std::fs::File::open(path).map_err(io)?);
        let size = file.metadata().map_err(io)?.len();
        let sha256 = if let Some(sha256) = sha256 {
            sha256.to_owned()
        } else {
            let hashed = std::sync::Arc::clone(&file);
            tokio::task::spawn_blocking(move || reef_evidence::sha256_hex_of(&*hashed))
                .await
                .map_err(|error| io(io::Error::other(error)))?
                .map_err(io)?
        };
        let request = |offset: u64, data: Vec<u8>| UploadArtifactRequest {
            repo: repo.to_owned(),
            sha256: sha256.clone(),
            size,
            offset,
            data,
        };
        let mut sent = 0u64;
        let mut response = self.upload_artifact(request(0, Vec::new())).await?;
        // A bound on the loop: every chunk, then some for another uploader
        // of the same bytes moving the offset.
        let chunk = u64::try_from(ARTIFACT_CHUNK).unwrap_or(u64::MAX);
        let mut calls = size / chunk + 64;
        loop {
            if let Some(artifact) = response.artifact {
                return Ok(ArtifactUpload {
                    artifact,
                    created: response.created,
                    sent,
                });
            }
            calls = calls.checked_sub(1).ok_or(UploadError::Changed {
                path: path.to_owned(),
            })?;
            let offset = response.received;
            let want =
                usize::try_from((size - offset.min(size)).min(chunk)).unwrap_or(ARTIFACT_CHUNK);
            let reading = std::sync::Arc::clone(&file);
            let data = tokio::task::spawn_blocking(move || {
                let mut data = vec![0; want];
                reading.read_exact_at(&mut data, offset).map(|()| data)
            })
            .await
            .map_err(|error| io(io::Error::other(error)))?
            .map_err(|error| {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    UploadError::Changed {
                        path: path.to_owned(),
                    }
                } else {
                    io(error)
                }
            })?;
            let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
            response = match self.upload_artifact(request(offset, data)).await {
                Ok(response) => {
                    sent += length;
                    response
                }
                Err(error) if error.reason() == reef_api::error::ARTIFACT_OFFSET_MISMATCH => {
                    let received = error
                        .metadata()
                        .get("received")
                        .and_then(|text| text.parse().ok())
                        .ok_or(error)?;
                    UploadArtifactResponse {
                        received,
                        artifact: None,
                        created: false,
                    }
                }
                Err(error) => return Err(error.into()),
            };
        }
    }

    /// `ArtifactService.GetArtifact` (R3.02): one artifact's record of
    /// `repo`, by its digest or a unique prefix.
    pub async fn get_artifact(
        &self,
        repo: &str,
        sha256: &str,
    ) -> Result<GetArtifactResponse, ClientError> {
        let mut client = self.artifacts();
        let request = GetArtifactRequest {
            repo: repo.to_owned(),
            sha256: sha256.to_owned(),
        };
        self.call("GetArtifact", client.get_artifact(request)).await
    }

    /// `ArtifactService.ReadArtifact` (R3.02): a stored artifact's bytes
    /// from `offset`, `length` of them (0 for all), as a stream whose first
    /// message carries the record. An error item after data means the bytes
    /// sent are not the artifact.
    pub async fn read_artifact(
        &self,
        repo: &str,
        sha256: &str,
        offset: u64,
        length: u64,
    ) -> Result<tonic::Streaming<ReadArtifactResponse>, ClientError> {
        let mut client = self.artifacts();
        let request = ReadArtifactRequest {
            repo: repo.to_owned(),
            sha256: sha256.to_owned(),
            offset,
            length,
        };
        self.call("ReadArtifact", client.read_artifact(request))
            .await
    }
}

impl Client {
    fn executor(&self) -> ExecutorServiceClient<Svc> {
        ExecutorServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `ExecutorService.SubmitJob` (R3.05): queues checks on a tree, or
    /// returns the identical open job (`created` false).
    pub async fn submit_job(
        &self,
        request: v1::SubmitJobRequest,
    ) -> Result<v1::SubmitJobResponse, ClientError> {
        self.call("SubmitJob", self.executor().submit_job(request))
            .await
    }

    /// `ExecutorService.GetJob` (R3.05): one job, with its coverage.
    pub async fn get_job(
        &self,
        request: v1::GetJobRequest,
    ) -> Result<v1::GetJobResponse, ClientError> {
        self.call("GetJob", self.executor().get_job(request)).await
    }

    /// One page of `ExecutorService.ListJobs` (R3.05).
    pub async fn list_jobs_page(
        &self,
        request: v1::ListJobsRequest,
    ) -> Result<v1::ListJobsResponse, ClientError> {
        self.call("ListJobs", self.executor().list_jobs(request))
            .await
    }

    /// The first `limit` jobs the listing `request` asks for (all when 0),
    /// and whether more exist; `request`'s page fields are ignored.
    pub async fn list_jobs(
        &self,
        request: v1::ListJobsRequest,
        limit: usize,
    ) -> Result<Limited<v1::Job>, ClientError> {
        let mut request = v1::ListJobsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_jobs_page(request.clone()).await?;
            limited.items.extend(response.jobs);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `ExecutorService.CancelJob` (R3.05).
    pub async fn cancel_job(
        &self,
        request: v1::CancelJobRequest,
    ) -> Result<v1::CancelJobResponse, ClientError> {
        self.call("CancelJob", self.executor().cancel_job(request))
            .await
    }

    /// `ExecutorService.ClaimJob` (R3.05): claims a matching job, waiting
    /// up to the request's `wait` for one.
    pub async fn claim_job(
        &self,
        request: v1::ClaimJobRequest,
    ) -> Result<v1::ClaimJobResponse, ClientError> {
        self.call("ClaimJob", self.executor().claim_job(request))
            .await
    }

    /// `ExecutorService.UpdateJob` (R3.05): a claim's heartbeat, progress
    /// and evidence.
    pub async fn update_job(
        &self,
        request: v1::UpdateJobRequest,
    ) -> Result<v1::UpdateJobResponse, ClientError> {
        self.call("UpdateJob", self.executor().update_job(request))
            .await
    }

    /// `ExecutorService.CompleteJob` (R3.05).
    pub async fn complete_job(
        &self,
        request: v1::CompleteJobRequest,
    ) -> Result<v1::CompleteJobResponse, ClientError> {
        self.call("CompleteJob", self.executor().complete_job(request))
            .await
    }
}

impl Client {
    /// `PolicyService.EvaluatePolicy` (R3.07): the repository's policy
    /// evaluated over one change.
    pub async fn evaluate_policy(
        &self,
        request: v1::EvaluatePolicyRequest,
    ) -> Result<v1::EvaluatePolicyResponse, ClientError> {
        let mut client =
            PolicyServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("EvaluatePolicy", client.evaluate_policy(request))
            .await
    }

    /// `PolicyService.SimulatePolicy` (R3.08): one page of a proposed
    /// policy evaluated over recent landings, beside a baseline.
    pub async fn simulate_policy(
        &self,
        request: v1::SimulatePolicyRequest,
    ) -> Result<v1::SimulatePolicyResponse, ClientError> {
        let mut client =
            PolicyServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("SimulatePolicy", client.simulate_policy(request))
            .await
    }

    fn comments(&self) -> CommentServiceClient<Svc> {
        CommentServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `CommentService.CreateComment` (R3.12): a note or an instruction on
    /// a change.
    pub async fn create_comment(
        &self,
        request: v1::CreateCommentRequest,
    ) -> Result<v1::CreateCommentResponse, ClientError> {
        let mut client = self.comments();
        self.call("CreateComment", client.create_comment(request))
            .await
    }

    /// `CommentService.GetComment` (R3.12): a comment and its replies.
    pub async fn get_comment(
        &self,
        request: v1::GetCommentRequest,
    ) -> Result<v1::GetCommentResponse, ClientError> {
        let mut client = self.comments();
        self.call("GetComment", client.get_comment(request)).await
    }

    /// `CommentService.ListComments` (R3.12): one page.
    pub async fn list_comments_page(
        &self,
        request: v1::ListCommentsRequest,
    ) -> Result<v1::ListCommentsResponse, ClientError> {
        let mut client = self.comments();
        self.call("ListComments", client.list_comments(request))
            .await
    }

    /// The first `limit` comments `request` asks for (all when 0), oldest
    /// first, and whether more exist.
    pub async fn list_comments(
        &self,
        request: v1::ListCommentsRequest,
        limit: usize,
    ) -> Result<Limited<v1::Comment>, ClientError> {
        let mut request = v1::ListCommentsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_comments_page(request.clone()).await?;
            limited.items.extend(response.comments);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `CommentService.UpdateInstruction` (R3.12): confirm, start, finish,
    /// release or cancel an instruction.
    pub async fn update_instruction(
        &self,
        request: v1::UpdateInstructionRequest,
    ) -> Result<v1::UpdateInstructionResponse, ClientError> {
        let mut client = self.comments();
        self.call("UpdateInstruction", client.update_instruction(request))
            .await
    }

    /// `CommentService.ClaimInstruction` (R3.12): take an instruction for a
    /// session.
    pub async fn claim_instruction(
        &self,
        request: v1::ClaimInstructionRequest,
    ) -> Result<v1::ClaimInstructionResponse, ClientError> {
        let mut client = self.comments();
        self.call("ClaimInstruction", client.claim_instruction(request))
            .await
    }

    fn landing(&self) -> LandingServiceClient<Svc> {
        LandingServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    fn git(&self) -> GitServiceClient<Svc> {
        GitServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `GitService.PublishChange` (#258): pushes a change to a branch on a
    /// git remote and records it.
    pub async fn publish_change(
        &self,
        request: v1::PublishChangeRequest,
    ) -> Result<v1::PublishChangeResponse, ClientError> {
        let mut client = self.git();
        self.call("PublishChange", client.publish_change(request))
            .await
    }

    /// `GitService.FetchGitRemote` (#259): fetches branches from a git
    /// remote and records them.
    pub async fn fetch_git_remote(
        &self,
        request: v1::FetchGitRemoteRequest,
    ) -> Result<v1::FetchGitRemoteResponse, ClientError> {
        let mut client = self.git();
        self.call("FetchGitRemote", client.fetch_git_remote(request))
            .await
    }

    /// `ChangeService.RebaseChange` (#259): fetches the destination's
    /// branch, then moves changes onto it, keeping their ids.
    pub async fn rebase_change(
        &self,
        request: v1::RebaseChangeRequest,
    ) -> Result<v1::RebaseChangeResponse, ClientError> {
        let mut client =
            ChangeServiceClient::with_interceptor(self.channel.clone(), self.agent.clone());
        self.call("RebaseChange", client.rebase_change(request))
            .await
    }

    /// `LandingService.LandChange` (R3.13): judges a proposed change, queues
    /// it, and waits up to `request.wait_ms` for its landing.
    pub async fn land_change(
        &self,
        request: v1::LandChangeRequest,
    ) -> Result<v1::LandChangeResponse, ClientError> {
        let mut client = self.landing();
        self.call("LandChange", client.land_change(request)).await
    }

    /// `LandingService.GetLanding` (R3.13).
    pub async fn get_landing(
        &self,
        request: v1::GetLandingRequest,
    ) -> Result<v1::GetLandingResponse, ClientError> {
        let mut client = self.landing();
        self.call("GetLanding", client.get_landing(request)).await
    }

    /// `LandingService.ListLandings` (R3.13): one page.
    pub async fn list_landings_page(
        &self,
        request: v1::ListLandingsRequest,
    ) -> Result<v1::ListLandingsResponse, ClientError> {
        let mut client = self.landing();
        self.call("ListLandings", client.list_landings(request))
            .await
    }

    /// The first `limit` landings `request` asks for (all when 0), newest
    /// first, and whether more exist.
    pub async fn list_landings(
        &self,
        request: v1::ListLandingsRequest,
        limit: usize,
    ) -> Result<Limited<v1::Landing>, ClientError> {
        let mut request = v1::ListLandingsRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_landings_page(request.clone()).await?;
            limited.items.extend(response.landings);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `LandingService.GetBatch` (R3.14): one batch, by id or as a change's
    /// latest.
    pub async fn get_batch(
        &self,
        request: v1::GetBatchRequest,
    ) -> Result<v1::GetBatchResponse, ClientError> {
        let mut client = self.landing();
        self.call("GetBatch", client.get_batch(request)).await
    }

    /// `LandingService.ListBatches` (R3.14): one page.
    pub async fn list_batches_page(
        &self,
        request: v1::ListBatchesRequest,
    ) -> Result<v1::ListBatchesResponse, ClientError> {
        let mut client = self.landing();
        self.call("ListBatches", client.list_batches(request)).await
    }

    /// The first `limit` batches `request` asks for (all when 0), newest
    /// first, and whether more exist.
    pub async fn list_batches(
        &self,
        request: v1::ListBatchesRequest,
        limit: usize,
    ) -> Result<Limited<v1::LandingBatch>, ClientError> {
        let mut request = v1::ListBatchesRequest {
            page_size: limited_page_size(limit),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_batches_page(request.clone()).await?;
            limited.items.extend(response.batches);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `LandingService.ListPushes` (R3.17): every pushing destination's
    /// push state, by destination (all pages).
    pub async fn list_pushes(&self, repo: &str) -> Result<Vec<v1::DestinationPush>, ClientError> {
        let mut request = v1::ListPushesRequest {
            repo: repo.to_owned(),
            ..v1::ListPushesRequest::default()
        };
        let mut pushes = Vec::new();
        loop {
            let mut client = self.landing();
            let response = self
                .call("ListPushes", client.list_pushes(request.clone()))
                .await?;
            pushes.extend(response.pushes);
            if response.next_page_token.is_empty() {
                return Ok(pushes);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// `LandingService.PushLanded` (R3.17): pushes a destination's landed
    /// history now.
    pub async fn push_landed(
        &self,
        request: v1::PushLandedRequest,
    ) -> Result<v1::PushLandedResponse, ClientError> {
        let mut client = self.landing();
        self.call("PushLanded", client.push_landed(request)).await
    }

    fn provenance(&self) -> ProvenanceServiceClient<Svc> {
        ProvenanceServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `ProvenanceService.DeclareProvenance` (R3.04): replaces fields of a
    /// workspace's declared context, snapshotting it first unless
    /// `no_snapshot`.
    pub async fn declare_provenance(
        &self,
        request: v1::DeclareProvenanceRequest,
    ) -> Result<v1::DeclareProvenanceResponse, ClientError> {
        self.call(
            "DeclareProvenance",
            self.provenance().declare_provenance(request),
        )
        .await
    }

    /// `ProvenanceService.GetProvenance` (R3.04): one record, by revision,
    /// commit id or record id, with its envelope.
    pub async fn get_provenance(
        &self,
        request: v1::GetProvenanceRequest,
    ) -> Result<v1::GetProvenanceResponse, ClientError> {
        self.call("GetProvenance", self.provenance().get_provenance(request))
            .await
    }

    /// One page of `ProvenanceService.ListProvenance` (R3.04).
    pub async fn list_provenance_page(
        &self,
        request: v1::ListProvenanceRequest,
    ) -> Result<v1::ListProvenanceResponse, ClientError> {
        self.call("ListProvenance", self.provenance().list_provenance(request))
            .await
    }

    /// The first `limit` records the listing `request` asks for (all when
    /// 0), newest recorded first, and whether more exist; `request`'s page
    /// fields are ignored.
    pub async fn list_provenance(
        &self,
        request: v1::ListProvenanceRequest,
        limit: usize,
    ) -> Result<Limited<v1::ProvenanceRecord>, ClientError> {
        let mut request = v1::ListProvenanceRequest {
            page_size: limited_page_size(limit).min(PROVENANCE_PAGE_SIZE),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_provenance_page(request.clone()).await?;
            limited.items.extend(response.records);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }
}

impl Client {
    fn conflicts(&self) -> ConflictServiceClient<Svc> {
        ConflictServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// One page of `ConflictService.ListConflicts` (R3.25).
    pub async fn list_conflicts_page(
        &self,
        request: v1::ListConflictsRequest,
    ) -> Result<v1::ListConflictsResponse, ClientError> {
        self.call("ListConflicts", self.conflicts().list_conflicts(request))
            .await
    }

    /// The first `limit` conflicts the listing `request` asks for (all
    /// when 0), in the revset's order, and whether more exist; `request`'s
    /// page fields are ignored.
    pub async fn list_conflicts(
        &self,
        request: v1::ListConflictsRequest,
        limit: usize,
    ) -> Result<Limited<v1::Conflict>, ClientError> {
        let mut request = v1::ListConflictsRequest {
            page_size: limited_page_size(limit).min(200),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_conflicts_page(request.clone()).await?;
            limited.items.extend(response.conflicts);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// One page of `ConflictService.GetConflict` (R3.25): the conflict with
    /// one page of its files.
    pub async fn get_conflict_page(
        &self,
        request: v1::GetConflictRequest,
    ) -> Result<v1::GetConflictResponse, ClientError> {
        self.call("GetConflict", self.conflicts().get_conflict(request))
            .await
    }

    /// The conflict `request` asks for with every page's files, and the
    /// first page's other fields; `request`'s page fields are ignored.
    pub async fn get_conflict(
        &self,
        request: v1::GetConflictRequest,
    ) -> Result<v1::GetConflictResponse, ClientError> {
        let mut request = v1::GetConflictRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        let mut whole = self.get_conflict_page(request.clone()).await?;
        while !whole.next_page_token.is_empty() {
            request.page_token = std::mem::take(&mut whole.next_page_token);
            let next = self.get_conflict_page(request.clone()).await?;
            if let (Some(conflict), Some(more)) = (whole.conflict.as_mut(), next.conflict) {
                conflict.files.extend(more.files);
            }
            whole.next_page_token = next.next_page_token;
        }
        Ok(whole)
    }

    /// One page of `ConflictService.ListConflictAssignments` (R3.27).
    pub async fn list_conflict_assignments_page(
        &self,
        request: v1::ListConflictAssignmentsRequest,
    ) -> Result<v1::ListConflictAssignmentsResponse, ClientError> {
        self.call(
            "ListConflictAssignments",
            self.conflicts().list_conflict_assignments(request),
        )
        .await
    }

    /// The first `limit` assignments `request` asks for (all when 0), the
    /// queue first in its order, and whether more exist; `request`'s page
    /// fields are ignored.
    pub async fn list_conflict_assignments(
        &self,
        request: v1::ListConflictAssignmentsRequest,
        limit: usize,
    ) -> Result<Limited<v1::ConflictAssignment>, ClientError> {
        let mut request = v1::ListConflictAssignmentsRequest {
            page_size: limited_page_size(limit).min(200),
            page_token: String::new(),
            ..request
        };
        let mut limited = Limited::default();
        loop {
            let response = self.list_conflict_assignments_page(request.clone()).await?;
            limited.items.extend(response.assignments);
            if limited.cut(limit, &response.next_page_token) {
                return Ok(limited);
            }
            request.page_token = response.next_page_token;
        }
    }

    /// One page of `ConflictService.GetConflictAssignment` (R3.27): the
    /// assignment with one page of its events.
    pub async fn get_conflict_assignment_page(
        &self,
        request: v1::GetConflictAssignmentRequest,
    ) -> Result<v1::GetConflictAssignmentResponse, ClientError> {
        self.call(
            "GetConflictAssignment",
            self.conflicts().get_conflict_assignment(request),
        )
        .await
    }

    /// The assignment `request` asks for with every page's events;
    /// `request`'s page fields are ignored.
    pub async fn get_conflict_assignment(
        &self,
        request: v1::GetConflictAssignmentRequest,
    ) -> Result<v1::GetConflictAssignmentResponse, ClientError> {
        let mut request = v1::GetConflictAssignmentRequest {
            page_size: ALL_PAGE_SIZE,
            page_token: String::new(),
            ..request
        };
        let mut whole = self.get_conflict_assignment_page(request.clone()).await?;
        while !whole.next_page_token.is_empty() {
            request.page_token = std::mem::take(&mut whole.next_page_token);
            let next = self.get_conflict_assignment_page(request.clone()).await?;
            whole.events.extend(next.events);
            whole.next_page_token = next.next_page_token;
        }
        Ok(whole)
    }

    /// `ConflictService.ClaimConflict` (R3.27): claims a queued assignment
    /// for a workspace's session.
    pub async fn claim_conflict(
        &self,
        request: v1::ClaimConflictRequest,
    ) -> Result<v1::ClaimConflictResponse, ClientError> {
        self.call("ClaimConflict", self.conflicts().claim_conflict(request))
            .await
    }

    /// `ConflictService.ReleaseConflict` (R3.27): gives an assignment up to
    /// the queue.
    pub async fn release_conflict(
        &self,
        request: v1::ReleaseConflictRequest,
    ) -> Result<v1::ReleaseConflictResponse, ClientError> {
        self.call(
            "ReleaseConflict",
            self.conflicts().release_conflict(request),
        )
        .await
    }

    /// `ConflictService.AssignConflict` (R3.27): assigns, queues,
    /// reprioritizes or takes over an assignment.
    pub async fn assign_conflict(
        &self,
        request: v1::AssignConflictRequest,
    ) -> Result<v1::AssignConflictResponse, ClientError> {
        self.call("AssignConflict", self.conflicts().assign_conflict(request))
            .await
    }
}

impl Client {
    fn identities(&self) -> IdentityServiceClient<Svc> {
        IdentityServiceClient::with_interceptor(self.channel.clone(), self.agent.clone())
    }

    /// `IdentityService.CreateIdentity` (R3.03): a new operator identity.
    pub async fn create_identity(
        &self,
        request: CreateIdentityRequest,
    ) -> Result<CreateIdentityResponse, ClientError> {
        self.call("CreateIdentity", self.identities().create_identity(request))
            .await
    }

    /// `IdentityService.GetIdentity` (R3.03): one identity by name or id;
    /// the default for an empty `name`.
    pub async fn get_identity(&self, name: &str) -> Result<GetIdentityResponse, ClientError> {
        let request = GetIdentityRequest {
            name: name.to_owned(),
        };
        self.call("GetIdentity", self.identities().get_identity(request))
            .await
    }

    /// `IdentityService.ListIdentities` (R3.03), every page.
    pub async fn list_identities_all(
        &self,
        include_revoked: bool,
    ) -> Result<Vec<Identity>, ClientError> {
        let mut request = ListIdentitiesRequest {
            include_revoked,
            page_size: 0,
            page_token: String::new(),
        };
        let mut identities = Vec::new();
        loop {
            let page = self
                .call(
                    "ListIdentities",
                    self.identities().list_identities(request.clone()),
                )
                .await?;
            identities.extend(page.identities);
            if page.next_page_token.is_empty() {
                return Ok(identities);
            }
            request.page_token = page.next_page_token;
        }
    }

    /// `IdentityService.RevokeIdentity` (R3.03).
    pub async fn revoke_identity(
        &self,
        request: RevokeIdentityRequest,
    ) -> Result<RevokeIdentityResponse, ClientError> {
        self.call("RevokeIdentity", self.identities().revoke_identity(request))
            .await
    }

    /// `IdentityService.IssueSessionKey` (R3.03) with a request built by the
    /// caller, its key and proof included. [`Client::issue_session_key`]
    /// makes both.
    pub async fn issue_session_key_request(
        &self,
        request: IssueSessionKeyRequest,
    ) -> Result<IssueSessionKeyResponse, ClientError> {
        self.call(
            "IssueSessionKey",
            self.identities().issue_session_key(request),
        )
        .await
    }

    /// A session key, generated here and certified by the daemon (R3.03,
    /// ADR 0057): a fresh ed25519 key, its proof of possession addressed to
    /// the daemon's current authority (read with `ListAuthorities`) over
    /// `identity`, `actor` and `session`, and the call. A refusal of the
    /// proof that names another current authority (a rotation in between)
    /// is retried once, signed for that one. `identity` empty is the
    /// default identity on the socket, or the token's linked identity
    /// remotely; `lifetime` `None` is the daemon's `[session_keys]
    /// lifetime`; `session` continues a session (renewal), empty for a new
    /// one. The private key never leaves this process: the returned
    /// [`SessionKey`] holds it in memory, and its PKCS#8 PEM is wiped on
    /// drop (ring's own key schedule is not). It signs with
    /// [`reef_evidence::Claim::sign`] unchanged.
    pub async fn issue_session_key(
        &self,
        identity: &str,
        actor: reef_evidence::session::Actor,
        lifetime: Option<std::time::Duration>,
        session: &str,
    ) -> Result<SessionKey, IssueError> {
        let (private_key_pem, _) =
            reef_evidence::SigningKey::generate().map_err(IssueError::Key)?;
        let private_key_pem = zeroize::Zeroizing::new(private_key_pem);
        let signing_key =
            reef_evidence::SigningKey::from_pkcs8_pem(&private_key_pem).map_err(IssueError::Key)?;
        let mut audience = self
            .list_authorities_all()
            .await?
            .into_iter()
            .find(|authority| authority.current)
            .map(|authority| authority.key_id)
            .unwrap_or_default();
        let mut retried = false;
        let mut record = loop {
            let authority = reef_evidence::KeyId::parse(&audience)
                .unwrap_or_else(|| reef_evidence::KeyId::of(&[]));
            let proof = reef_evidence::session::sign_proof(
                &signing_key,
                &reef_evidence::session::ProofFor {
                    authority: &authority,
                    identity,
                    actor,
                    session,
                },
            );
            let request = IssueSessionKeyRequest {
                public_key: signing_key.public_key().spki().to_vec(),
                proof,
                identity: identity.to_owned(),
                actor: session_actor(actor).into(),
                lifetime: lifetime.map(proto_duration),
                session_id: session.to_owned(),
            };
            match self.issue_session_key_request(request).await {
                Ok(response) => break response.key.unwrap_or_default(),
                Err(error)
                    if !retried
                        && error.reason() == reef_api::error::INVALID_SESSION_KEY_REQUEST
                        && error.metadata().get("field").map(String::as_str) == Some("proof")
                        && error
                            .metadata()
                            .get("authority_key_id")
                            .is_some_and(|current| *current != audience) =>
                {
                    retried = true;
                    audience.clone_from(&error.metadata()["authority_key_id"]);
                }
                Err(error) => return Err(error.into()),
            }
        };
        let certificate = std::mem::take(&mut record.certificate);
        Ok(SessionKey {
            signing_key,
            private_key_pem,
            certificate,
            record,
        })
    }

    /// `IdentityService.GetSessionKey` (R3.03): one key by its id or a
    /// unique prefix, with its certificate.
    pub async fn get_session_key(
        &self,
        key_id: &str,
    ) -> Result<GetSessionKeyResponse, ClientError> {
        let request = GetSessionKeyRequest {
            key_id: key_id.to_owned(),
        };
        self.call("GetSessionKey", self.identities().get_session_key(request))
            .await
    }

    /// `IdentityService.ListSessionKeys` (R3.03), every page; `request`'s
    /// page fields are ignored.
    pub async fn list_session_keys_all(
        &self,
        request: ListSessionKeysRequest,
    ) -> Result<Vec<v1::SessionKey>, ClientError> {
        let mut request = ListSessionKeysRequest {
            page_size: 0,
            page_token: String::new(),
            ..request
        };
        let mut keys = Vec::new();
        loop {
            let page = self
                .call(
                    "ListSessionKeys",
                    self.identities().list_session_keys(request.clone()),
                )
                .await?;
            keys.extend(page.keys);
            if page.next_page_token.is_empty() {
                return Ok(keys);
            }
            request.page_token = page.next_page_token;
        }
    }

    /// `IdentityService.RevokeSessionKeys` (R3.03): one key, or a session's.
    pub async fn revoke_session_keys(
        &self,
        request: RevokeSessionKeysRequest,
    ) -> Result<RevokeSessionKeysResponse, ClientError> {
        self.call(
            "RevokeSessionKeys",
            self.identities().revoke_session_keys(request),
        )
        .await
    }

    /// `IdentityService.ListAuthorities` (R3.03), every page: newest first.
    pub async fn list_authorities_all(&self) -> Result<Vec<v1::Authority>, ClientError> {
        let mut request = ListAuthoritiesRequest::default();
        let mut authorities = Vec::new();
        loop {
            let page = self
                .call(
                    "ListAuthorities",
                    self.identities().list_authorities(request.clone()),
                )
                .await?;
            authorities.extend(page.authorities);
            if page.next_page_token.is_empty() {
                return Ok(authorities);
            }
            request.page_token = page.next_page_token;
        }
    }

    /// `IdentityService.RevokeAuthority` (R3.03): a retired authority.
    pub async fn revoke_authority(
        &self,
        request: v1::RevokeAuthorityRequest,
    ) -> Result<v1::RevokeAuthorityResponse, ClientError> {
        self.call(
            "RevokeAuthority",
            self.identities().revoke_authority(request),
        )
        .await
    }

    /// `IdentityService.RotateAuthority` (R3.03).
    pub async fn rotate_authority(
        &self,
        request: RotateAuthorityRequest,
    ) -> Result<RotateAuthorityResponse, ClientError> {
        self.call(
            "RotateAuthority",
            self.identities().rotate_authority(request),
        )
        .await
    }
}

/// `actor` in the API.
pub fn session_actor(actor: reef_evidence::session::Actor) -> v1::SessionActor {
    use reef_evidence::session::Actor;
    match actor {
        Actor::Human => v1::SessionActor::Human,
        Actor::Agent => v1::SessionActor::Agent,
        Actor::Executor => v1::SessionActor::Executor,
    }
}

/// A session key this process generated and the daemon certified (R3.03).
/// Its private half lives only here: Reef never writes it anywhere, and
/// its PKCS#8 text is wiped from memory on drop.
pub struct SessionKey {
    /// The private key, which signs claims ([`reef_evidence::Claim::sign`]).
    pub signing_key: reef_evidence::SigningKey,
    /// The private key as PKCS#8 PEM, for a caller that hands the key on
    /// (`reef identity session issue -o json`). A credential, wiped from
    /// memory on drop; `signing_key`'s own copy, inside ring, is not.
    pub private_key_pem: zeroize::Zeroizing<String>,
    /// The certificate: the canonical DSSE envelope the daemon's authority
    /// signed.
    pub certificate: Vec<u8>,
    /// The daemon's record of the key, without the certificate's bytes.
    pub record: v1::SessionKey,
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKey")
            .field("key_id", &self.record.key_id)
            .field("identity", &self.record.identity)
            .finish_non_exhaustive()
    }
}

/// Failure of [`Client::issue_session_key`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IssueError {
    /// No key could be generated: the system's random number generator
    /// failed.
    #[error(transparent)]
    Key(reef_evidence::KeyError),
    /// The daemon refused the call.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// The chunk [`Client::upload_artifact_file`] sends: 1 MiB, the most a
/// daemon accepts.
pub const ARTIFACT_CHUNK: usize = 1 << 20;

/// What [`Client::upload_artifact_file`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactUpload {
    /// The artifact as stored.
    pub artifact: Artifact,
    /// Whether this upload stored it; false when it was stored already.
    pub created: bool,
    /// The bytes this upload sent: 0 when they were stored already.
    pub sent: u64,
}

/// Failure of [`Client::upload_artifact_file`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UploadError {
    /// The file could not be read.
    #[error("read {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file changed while it was uploaded: shorter than when it was
    /// hashed.
    #[error("{} changed while it was uploaded", path.display())]
    Changed {
        /// The file.
        path: PathBuf,
    },
    /// The daemon refused a call.
    #[error(transparent)]
    Client(#[from] ClientError),
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

/// The largest page of `ListEvidence` (R3.01).
const EVIDENCE_PAGE_SIZE: i32 = 200;

/// The largest page of `ListProvenance` (R3.04).
const PROVENANCE_PAGE_SIZE: i32 = 1000;

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
            let detail = v1::ErrorDetail {
                reason: reason.to_owned(),
                ..v1::ErrorDetail::default()
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
