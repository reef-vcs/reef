//! `reef.daemon.v1.Changes`: log, get, diff (R2.02); describe, abandon,
//! squash (R2.03). Stubs until then.

use reef_api::proto::v1::changes_server::Changes;
use reef_api::proto::v1::{
    AbandonChangeRequest, AbandonChangeResponse, DescribeChangeRequest, DescribeChangeResponse,
    DiffRequest, DiffResponse, GetChangeRequest, GetChangeResponse, LogChangesRequest,
    LogChangesResponse, SquashChangeRequest, SquashChangeResponse,
};
use tonic::{Request, Response, Status};

use super::stubs::stub;

/// Serves `Changes` (R2.02, R2.03).
#[derive(Debug, Default)]
pub struct ChangesService;

#[tonic::async_trait]
impl Changes for ChangesService {
    async fn log(
        &self,
        _: Request<LogChangesRequest>,
    ) -> Result<Response<LogChangesResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Log"))
    }

    async fn get(
        &self,
        _: Request<GetChangeRequest>,
    ) -> Result<Response<GetChangeResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Get"))
    }

    async fn diff(&self, _: Request<DiffRequest>) -> Result<Response<DiffResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Diff"))
    }

    async fn describe(
        &self,
        _: Request<DescribeChangeRequest>,
    ) -> Result<Response<DescribeChangeResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Describe"))
    }

    async fn abandon(
        &self,
        _: Request<AbandonChangeRequest>,
    ) -> Result<Response<AbandonChangeResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Abandon"))
    }

    async fn squash(
        &self,
        _: Request<SquashChangeRequest>,
    ) -> Result<Response<SquashChangeResponse>, Status> {
        Err(stub("reef.daemon.v1.Changes/Squash"))
    }
}
