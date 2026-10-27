//! The gRPC router: every service the daemon serves, behind
//! [`CompatLayer`].
//!
//! The daemon serves the eleven `reef.api.v1` services and, frozen,
//! `reef.daemon.v0.Daemon/Status` (R2.01, ADR 0029). A request for a method
//! it does not serve gets tonic's bare `UNIMPLEMENTED`, which a client
//! cannot tell from a bug; [`CompatLayer`] rewrites that answer so every
//! version mismatch says what it is:
//!
//! - any other `reef.daemon.v0` method, a 0.1 client: `FAILED_PRECONDITION`
//!   with a v0 `ErrorDetail` telling it to upgrade
//!   ([`crate::error::v0_client`]);
//! - anything else, a client newer than this daemon: `UNIMPLEMENTED` with
//!   reason `UNKNOWN_METHOD` ([`crate::error::unknown_method`]).
//!
//! A declared RPC's own `NOT_IMPLEMENTED` answer carries details and passes
//! through untouched.

use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use reef_api::v0::daemon_server::DaemonServer as V0DaemonServer;
use reef_api::v1::change_service_server::ChangeServiceServer;
use reef_api::v1::daemon_service_server::DaemonServiceServer;
use reef_api::v1::diff_service_server::DiffServiceServer;
use reef_api::v1::file_service_server::FileServiceServer;
use reef_api::v1::maintenance_service_server::MaintenanceServiceServer;
use reef_api::v1::operation_service_server::OperationServiceServer;
use reef_api::v1::presence_service_server::PresenceServiceServer;
use reef_api::v1::repository_service_server::RepositoryServiceServer;
use reef_api::v1::status_service_server::StatusServiceServer;
use reef_api::v1::store_service_server::StoreServiceServer;
use reef_api::v1::workspace_service_server::WorkspaceServiceServer;
use tonic::body::Body;
use tonic::codegen::http;
use tonic::transport::Server;
use tonic::transport::server::Router;
use tower::layer::util::{Identity, Stack};
use tower::{Layer, Service};

use crate::daemon::AppState;
use crate::error::{unknown_method, v0_client};
use crate::service::changes::ChangesHandler;
use crate::service::compat::V0DaemonService;
use crate::service::daemon::DaemonStatusService;
use crate::service::diff::DiffHandler;
use crate::service::files::FilesHandler;
use crate::service::maintenance::MaintenanceHandler;
use crate::service::operations::OperationsHandler;
use crate::service::presence::PresenceHandler;
use crate::service::repositories::RepositoriesService;
use crate::service::status::StatusHandler;
use crate::service::stores::StoresService;
use crate::service::workspaces::WorkspacesService;

/// Every service [`router`] registers, by full name.
pub const SERVED: [&str; 12] = [
    "reef.api.v1.ChangeService",
    "reef.api.v1.DaemonService",
    "reef.api.v1.DiffService",
    "reef.api.v1.FileService",
    "reef.api.v1.MaintenanceService",
    "reef.api.v1.OperationService",
    "reef.api.v1.PresenceService",
    "reef.api.v1.RepositoryService",
    "reef.api.v1.StatusService",
    "reef.api.v1.StoreService",
    "reef.api.v1.WorkspaceService",
    "reef.daemon.v0.Daemon",
];

/// The daemon's router type.
pub type ApiRouter = Router<Stack<CompatLayer, Identity>>;

/// The router over `state`, one `add_service` per entry of [`SERVED`].
/// Built here, not generically, so `Daemon::start` stays non-generic over
/// tonic's layers.
pub fn router(state: Arc<AppState>) -> ApiRouter {
    Server::builder()
        .layer(CompatLayer)
        .add_service(ChangeServiceServer::new(ChangesHandler::new(Arc::clone(
            &state,
        ))))
        .add_service(DaemonServiceServer::new(DaemonStatusService::new(
            Arc::clone(&state),
        )))
        .add_service(DiffServiceServer::new(DiffHandler::new(Arc::clone(&state))))
        .add_service(FileServiceServer::new(FilesHandler::new(Arc::clone(
            &state,
        ))))
        .add_service(MaintenanceServiceServer::new(MaintenanceHandler::new(
            Arc::clone(&state),
        )))
        .add_service(OperationServiceServer::new(OperationsHandler::new(
            Arc::clone(&state),
        )))
        .add_service(PresenceServiceServer::new(PresenceHandler::new(
            Arc::clone(&state),
        )))
        .add_service(RepositoryServiceServer::new(RepositoriesService::new(
            Arc::clone(&state),
        )))
        .add_service(StatusServiceServer::new(StatusHandler::new(Arc::clone(
            &state,
        ))))
        .add_service(StoreServiceServer::new(StoresService::new(Arc::clone(
            &state,
        ))))
        .add_service(WorkspaceServiceServer::new(WorkspacesService::new(
            Arc::clone(&state),
        )))
        .add_service(V0DaemonServer::new(V0DaemonService::new(state)))
}

/// Rewrites a bare `UNIMPLEMENTED` into an answer that names the version
/// mismatch; see the module documentation.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompatLayer;

impl<S> Layer<S> for CompatLayer {
    type Service = Compat<S>;

    fn layer(&self, inner: S) -> Compat<S> {
        Compat { inner }
    }
}

/// [`CompatLayer`]'s service.
#[derive(Clone, Debug)]
pub struct Compat<S> {
    inner: S,
}

impl<S> Service<http::Request<Body>> for Compat<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<http::Response<Body>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let path = request.uri().path().to_owned();
        let response = self.inner.call(request);
        Box::pin(async move { Ok(rewrite(&path, response.await?)) })
    }
}

/// `response`, or the answer that replaces a bare `UNIMPLEMENTED` for
/// `path`.
fn rewrite(path: &str, response: http::Response<Body>) -> http::Response<Body> {
    let headers = response.headers();
    let bare_unimplemented = headers
        .get("grpc-status")
        .is_some_and(|code| code.as_bytes() == b"12")
        && !headers.contains_key("grpc-status-details-bin");
    if !bare_unimplemented {
        return response;
    }
    let status = if path.starts_with("/reef.daemon.v0.") {
        v0_client(path)
    } else {
        unknown_method(path)
    };
    status.into_http()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use prost::Message as _;

    use super::*;

    #[test]
    fn every_service_is_served() {
        let fds = prost_types::FileDescriptorSet::decode(reef_api::FILE_DESCRIPTOR_SET).unwrap();
        let mut expected: BTreeSet<String> = fds
            .file
            .iter()
            .filter(|file| file.package() == "reef.api.v1")
            .flat_map(|file| {
                file.service
                    .iter()
                    .map(|service| format!("reef.api.v1.{}", service.name()))
            })
            .collect();
        expected.insert("reef.daemon.v0.Daemon".to_owned());
        let served: BTreeSet<String> = SERVED.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(served, expected);
    }

    fn bare(code: &str) -> http::Response<Body> {
        let mut response = http::Response::new(Body::default());
        response
            .headers_mut()
            .insert("grpc-status", code.parse().unwrap());
        response
    }

    #[test]
    fn bare_unimplemented_is_rewritten() {
        let v0 = rewrite("/reef.daemon.v0.Workspaces/List", bare("12"));
        assert_eq!(v0.headers()["grpc-status"], "9");
        assert!(v0.headers().contains_key("grpc-status-details-bin"));
        let newer = rewrite("/reef.api.v1.WorkspaceService/Nope", bare("12"));
        assert_eq!(newer.headers()["grpc-status"], "12");
        assert!(newer.headers().contains_key("grpc-status-details-bin"));
        // Anything else passes through.
        let ok = rewrite("/reef.api.v1.WorkspaceService/Nope", bare("0"));
        assert!(!ok.headers().contains_key("grpc-status-details-bin"));
    }
}
