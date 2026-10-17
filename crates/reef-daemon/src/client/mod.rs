//! The daemon client the CLI and tests use. It wraps the generated clients,
//! one method per RPC, so nothing else depends on their shape.
//! [`WorkspacesApi`] is the same surface as a trait, for code that wants to
//! run against a fake daemon (R1.21's heartbeater); [`json`] is the stable
//! JSON form of the responses that `reef -o json` prints (R1.22).

pub mod json;

use std::io;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use reef_lib::error::{Classify, ErrorKind};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

pub use reef_lib::jj::settings::parse_byte_size;

use crate::error::{detail_of, kind_from_status};
use crate::proto::v0::daemon_client::DaemonClient;
use crate::proto::v0::repositories_client::RepositoriesClient;
use crate::proto::v0::workspaces_client::WorkspacesClient;
use crate::proto::v0::{
    CreateRequest, CreateResponse, ForgetRequest, ForgetResponse, HeartbeatRequest,
    HeartbeatResponse, InspectRequest, InspectResponse, ListRepositoriesRequest,
    ListWorkspacesRequest, RegisterRequest, RegisterResponse, ReleaseRequest, ReleaseResponse,
    Repository, SnapshotRequest, SnapshotResponse, StatusRequest, StatusResponse, Workspace,
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
    /// `path`, which must be absolute.
    pub async fn register_repository(&self, path: &Path) -> Result<RegisterResponse, ClientError> {
        let request = RegisterRequest {
            path: path.display().to_string(),
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
        let request = InspectRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            snapshot_first,
        };
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
}

/// The client's surface as a trait: one method per RPC, with the same
/// signatures as [`Client`]'s. [`Client`] implements it; tests and
/// R1.21's heartbeater implement it over a fake.
#[tonic::async_trait]
pub trait WorkspacesApi: Send + Sync {
    /// `Daemon.Status`.
    async fn status(&self) -> Result<StatusResponse, ClientError>;
    /// `Repositories.Register`.
    async fn register_repository(&self, path: &Path) -> Result<RegisterResponse, ClientError>;
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

    async fn register_repository(&self, path: &Path) -> Result<RegisterResponse, ClientError> {
        Client::register_repository(self, path).await
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
