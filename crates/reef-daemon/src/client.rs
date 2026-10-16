//! The daemon client the CLI and tests use. It wraps the generated clients,
//! one method per RPC, so nothing else depends on their shape.

use std::io;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use reef_lib::error::{Classify, ErrorKind};
use tokio::net::UnixStream;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::proto::v0::daemon_client::DaemonClient;
use crate::proto::v0::{StatusRequest, StatusResponse};

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
    /// The daemon answered with an error.
    #[error("{}", .0.message())]
    Rpc(#[from] tonic::Status),
}

impl Classify for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Connect { .. } => ErrorKind::Unavailable,
            // The inverse of `service::code_of`.
            Self::Rpc(status) => match status.code() {
                Code::NotFound => ErrorKind::NotFound,
                Code::AlreadyExists => ErrorKind::AlreadyExists,
                Code::InvalidArgument => ErrorKind::InvalidArgument,
                Code::FailedPrecondition => ErrorKind::Conflict,
                Code::Unavailable => ErrorKind::Unavailable,
                Code::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::Internal,
            },
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
}
