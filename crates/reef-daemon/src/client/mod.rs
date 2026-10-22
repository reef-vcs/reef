//! The daemon client the CLI and tests use. It wraps the generated clients,
//! one method per RPC, so nothing else depends on their shape.
//! [`WorkspacesApi`] is the same surface, less maintenance, as a trait, for
//! code that wants to run against a fake daemon (R1.21's heartbeater);
//! [`json`] is the stable JSON form of the responses that `reef -o json`
//! prints (R1.22).

pub mod json;

use std::io;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use reef_lib::error::{Classify, ErrorKind};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

pub use reef_lib::jj::settings::parse_byte_size;

use crate::error::{Advice, detail_of, kind_from_status};
use crate::proto::v0::daemon_client::DaemonClient;
use crate::proto::v0::maintenance_client::MaintenanceClient;
use crate::proto::v0::register_request::IfNotJj;
use crate::proto::v0::repositories_client::RepositoriesClient;
use crate::proto::v0::stores_client::StoresClient;
use crate::proto::v0::workspaces_client::WorkspacesClient;
use crate::proto::v0::{
    BackupProgress, BackupRequest, BuildEvent, BuildStoreRequest, CreateRequest, CreateResponse,
    ForgetRequest, ForgetResponse, GcRequest, GcResponse, HeartbeatRequest, HeartbeatResponse,
    InspectRequest, InspectResponse, ListRepositoriesRequest, ListStoresRequest,
    ListWorkspacesRequest, MoveRequest, MoveResponse, QuiesceRequest, QuiesceResponse,
    RegisterRequest, RegisterResponse, ReleaseRequest, ReleaseResponse, Repository, ResumeRequest,
    ResumeResponse, SnapshotRequest, SnapshotResponse, StatusRequest, StatusResponse, StoreInfo,
    Workspace,
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
    pub async fn status(&self) -> Result<StatusResponse, ClientError> {
        let mut client = DaemonClient::new(self.channel.clone());
        Ok(client.status(StatusRequest {}).await?.into_inner())
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

    /// `Workspaces.List`.
    pub async fn list_workspaces(
        &self,
        request: ListWorkspacesRequest,
    ) -> Result<Vec<Workspace>, ClientError> {
        Ok(self
            .workspaces()
            .list(request)
            .await?
            .into_inner()
            .workspaces)
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
            workspace_id: workspace.to_owned(),
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
}

/// The workspace side of the client's surface as a trait: one method per
/// `Daemon`, `Repositories`, and `Workspaces` RPC, with the same
/// signatures as [`Client`]'s (maintenance and stores are [`Client`]'s
/// alone). [`Client`] implements it; tests and
/// R1.21's heartbeater implement it over a fake.
#[tonic::async_trait]
pub trait WorkspacesApi: Send + Sync {
    /// `Daemon.Status`.
    async fn status(&self) -> Result<StatusResponse, ClientError>;
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
    async fn status(&self) -> Result<StatusResponse, ClientError> {
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
}
