//! `reef.daemon.v0.Repositories`: register, list, forget.

use std::path::PathBuf;
use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::daemon::AppState;
use crate::error::status;
use crate::proto::v0::repositories_server::Repositories;
use crate::proto::v0::{
    ForgetRequest, ForgetResponse, ListRepositoriesRequest, ListRepositoriesResponse,
    RegisterRequest, RegisterResponse, Repository,
};

/// Serves `Repositories`.
#[derive(Debug)]
pub struct RepositoriesService {
    state: Arc<AppState>,
}

impl RepositoriesService {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> RepositoriesService {
        RepositoriesService { state }
    }
}

#[tonic::async_trait]
impl Repositories for RepositoriesService {
    #[tracing::instrument(skip_all, fields(path = %request.get_ref().path, repo))]
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let path = PathBuf::from(request.into_inner().path);
        let (record, already_registered) = self
            .state
            .manager
            .register_repository(path)
            .await
            .map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(record.id));
        Ok(Response::new(RegisterResponse {
            repository: Some(Repository::from(&record)),
            already_registered,
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn list(
        &self,
        _request: Request<ListRepositoriesRequest>,
    ) -> Result<Response<ListRepositoriesResponse>, Status> {
        let repositories = self
            .state
            .db
            .repositories()
            .map_err(status)?
            .iter()
            .map(Repository::from)
            .collect();
        Ok(Response::new(ListRepositoriesResponse { repositories }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn forget(
        &self,
        request: Request<ForgetRequest>,
    ) -> Result<Response<ForgetResponse>, Status> {
        let manager = &self.state.manager;
        let record = manager
            .resolve_repo(&request.into_inner().repo)
            .map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(record.id));
        let forgotten = manager.forget_repository(record.id).await.map_err(status)?;
        Ok(Response::new(ForgetResponse {
            repository: Some(Repository::from(&forgotten)),
        }))
    }
}
