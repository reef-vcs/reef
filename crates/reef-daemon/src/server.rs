//! The gRPC router: every service the daemon serves.

use std::sync::Arc;

use tonic::transport::Server;
use tonic::transport::server::Router;

use crate::daemon::AppState;
use crate::service::daemon::DaemonService;
use crate::service::maintenance::MaintenanceService;
use crate::service::repositories::RepositoriesService;
use crate::service::stores::StoresService;
use crate::service::workspaces::WorkspacesService;
use reef_api::proto::v1::daemon_server::DaemonServer;
use reef_api::proto::v1::maintenance_server::MaintenanceServer;
use reef_api::proto::v1::repositories_server::RepositoriesServer;
use reef_api::proto::v1::stores_server::StoresServer;
use reef_api::proto::v1::workspaces_server::WorkspacesServer;

/// The router over `state`, one `add_service` per service. Built here, not
/// generically, so `Daemon::start` stays non-generic over tonic's layers.
pub fn router(state: Arc<AppState>) -> Router {
    Server::builder()
        .add_service(DaemonServer::new(DaemonService::new(Arc::clone(&state))))
        .add_service(RepositoriesServer::new(RepositoriesService::new(
            Arc::clone(&state),
        )))
        .add_service(WorkspacesServer::new(WorkspacesService::new(Arc::clone(
            &state,
        ))))
        .add_service(MaintenanceServer::new(MaintenanceService::new(Arc::clone(
            &state,
        ))))
        .add_service(StoresServer::new(StoresService::new(state)))
}
