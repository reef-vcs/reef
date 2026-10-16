//! The gRPC router: every service the daemon serves.

use std::sync::Arc;

use tonic::transport::Server;
use tonic::transport::server::Router;

use crate::daemon::AppState;
use crate::proto::v0::daemon_server::DaemonServer;
use crate::service::daemon::DaemonService;

/// The router over `state`, one `add_service` per service. Built here, not
/// generically, so `Daemon::start` stays non-generic over tonic's layers.
pub fn router(state: Arc<AppState>) -> Router {
    Server::builder().add_service(DaemonServer::new(DaemonService::new(state)))
}
