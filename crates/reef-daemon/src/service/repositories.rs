//! `reef.api.v1.RepositoryService`: register, get, list, forget.

use std::path::PathBuf;
use std::sync::Arc;

use reef_api::v1::repository_service_server::RepositoryService;
use reef_api::v1::{
    ForgetRepositoryRequest, ForgetRepositoryResponse, GetRepositoryRequest, GetRepositoryResponse,
    ListRepositoriesRequest, ListRepositoriesResponse, RegisterRepositoryRequest,
    RegisterRepositoryResponse, Repository,
};
use tonic::{Request, Response, Status};

use super::convert::known;
use super::page::{PageRequest, oldest_first};
use crate::daemon::AppState;
use crate::error::status;

/// Serves `RepositoryService`.
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
impl RepositoryService for RepositoriesService {
    #[tracing::instrument(skip_all, fields(path = %request.get_ref().path, repo))]
    async fn register_repository(
        &self,
        request: Request<RegisterRepositoryRequest>,
    ) -> Result<Response<RegisterRepositoryResponse>, Status> {
        let request = request.into_inner();
        let if_not_jj = known("if_not_jj", request.if_not_jj).map_err(status)?;
        let registered = self
            .state
            .manager
            .register_repository(PathBuf::from(request.path), if_not_jj)
            .await
            .map_err(status)?;
        let record = &registered.record;
        tracing::Span::current().record("repo", tracing::field::display(record.id));
        Ok(Response::new(RegisterRepositoryResponse {
            repository: Some(Repository::from(record)),
            already_registered: registered.already_registered,
            initialized_jj: registered.initialized_jj,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn get_repository(
        &self,
        request: Request<GetRepositoryRequest>,
    ) -> Result<Response<GetRepositoryResponse>, Status> {
        let record = self
            .state
            .manager
            .resolve_repo(&request.into_inner().repo)
            .map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(record.id));
        Ok(Response::new(GetRepositoryResponse {
            repository: Some(Repository::from(&record)),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn list_repositories(
        &self,
        request: Request<ListRepositoriesRequest>,
    ) -> Result<Response<ListRepositoriesResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            request.page_size,
            &request.page_token,
            &ListRepositoriesRequest::default(),
        )
        .map_err(status)?;
        let (records, next_page_token) = page
            .page(self.state.db.repositories().map_err(status)?, |record| {
                oldest_first(record.registered_at.0, &record.id.as_hex())
            });
        Ok(Response::new(ListRepositoriesResponse {
            repositories: records.iter().map(Repository::from).collect(),
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo))]
    async fn forget_repository(
        &self,
        request: Request<ForgetRepositoryRequest>,
    ) -> Result<Response<ForgetRepositoryResponse>, Status> {
        let manager = &self.state.manager;
        let record = manager
            .resolve_repo(&request.into_inner().repo)
            .map_err(status)?;
        tracing::Span::current().record("repo", tracing::field::display(record.id));
        let forgotten = manager.forget_repository(record.id).await.map_err(status)?;
        Ok(Response::new(ForgetRepositoryResponse {
            repository: Some(Repository::from(&forgotten)),
        }))
    }
}
