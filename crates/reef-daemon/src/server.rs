//! The gRPC router: every service the daemon serves.

use std::sync::Arc;

use tonic::transport::Server;
use tonic::transport::server::Router;
use tonic_reflection::pb::v1::server_reflection_server::{
    ServerReflection, ServerReflectionServer,
};

use crate::daemon::AppState;
use crate::service::changes::ChangesService;
use crate::service::daemon::DaemonService;
use crate::service::maintenance::MaintenanceService;
use crate::service::operations::OperationsService;
use crate::service::presence::PresenceService;
use crate::service::queries::QueriesService;
use crate::service::remote::RemoteService;
use crate::service::repositories::RepositoriesService;
use crate::service::stores::StoresService;
use crate::service::symbols::SymbolsService;
use crate::service::trees::TreesService;
use crate::service::workspaces::WorkspacesService;
use reef_api::proto::v1::changes_server::ChangesServer;
use reef_api::proto::v1::daemon_server::DaemonServer;
use reef_api::proto::v1::maintenance_server::MaintenanceServer;
use reef_api::proto::v1::operations_server::OperationsServer;
use reef_api::proto::v1::presence_server::PresenceServer;
use reef_api::proto::v1::queries_server::QueriesServer;
use reef_api::proto::v1::remote_server::RemoteServer;
use reef_api::proto::v1::repositories_server::RepositoriesServer;
use reef_api::proto::v1::stores_server::StoresServer;
use reef_api::proto::v1::symbols_server::SymbolsServer;
use reef_api::proto::v1::trees_server::TreesServer;
use reef_api::proto::v1::workspaces_server::WorkspacesServer;

/// The router over `state`, one `add_service` per service, and gRPC
/// server reflection from `reef_api::DESCRIPTOR_SET` (R2.01; the socket
/// only). Built here, not generically, so `Daemon::start` stays
/// non-generic over tonic's layers.
pub fn router(state: Arc<AppState>) -> Router {
    Server::builder()
        .add_service(reflection())
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
        .add_service(ChangesServer::new(ChangesService))
        .add_service(OperationsServer::new(OperationsService))
        .add_service(TreesServer::new(TreesService))
        .add_service(PresenceServer::new(PresenceService))
        .add_service(SymbolsServer::new(SymbolsService))
        .add_service(QueriesServer::new(QueriesService))
        .add_service(RemoteServer::new(RemoteService))
}

/// Server reflection (`grpc.reflection.v1`) over the v1 schema.
#[expect(
    clippy::expect_used,
    reason = "the set protox compiled at build time; reef-api's schema tests decode it"
)]
fn reflection() -> ServerReflectionServer<impl ServerReflection> {
    tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(reef_api::DESCRIPTOR_SET)
        .build_v1()
        .expect("the embedded descriptor set is valid")
}
