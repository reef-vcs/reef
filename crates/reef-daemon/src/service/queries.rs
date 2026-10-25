//! `reef.daemon.v1.Queries`: list, run (R2.16). Stubs until then.

use reef_api::proto::v1::queries_server::Queries;
use reef_api::proto::v1::{
    ListQueriesRequest, ListQueriesResponse, RunQueryRequest, RunQueryResponse,
};
use tonic::{Request, Response, Status};

use super::stubs::stub;

/// Serves `Queries` (R2.16).
#[derive(Debug, Default)]
pub struct QueriesService;

#[tonic::async_trait]
impl Queries for QueriesService {
    async fn list(
        &self,
        _: Request<ListQueriesRequest>,
    ) -> Result<Response<ListQueriesResponse>, Status> {
        Err(stub("reef.daemon.v1.Queries/List"))
    }

    async fn run(&self, _: Request<RunQueryRequest>) -> Result<Response<RunQueryResponse>, Status> {
        Err(stub("reef.daemon.v1.Queries/Run"))
    }
}
