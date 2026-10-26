//! `reef.api.v1.ChangeService`: declared in R2.01, implemented by R2.02 and
//! R2.03. Until then every RPC answers `UNIMPLEMENTED` with reason
//! `NOT_IMPLEMENTED`.

use reef_api::v1::change_service_server::ChangeService;
use reef_api::v1::{
    AbandonChangeRequest, AbandonChangeResponse, DescribeChangeRequest, DescribeChangeResponse,
    GetChangeRequest, GetChangeResponse, ListChangesRequest, ListChangesResponse,
};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `ChangeService`.
#[derive(Debug, Default)]
pub struct ChangesHandler;

#[tonic::async_trait]
impl ChangeService for ChangesHandler {
    async fn list_changes(
        &self,
        _request: Request<ListChangesRequest>,
    ) -> Result<Response<ListChangesResponse>, Status> {
        Err(not_implemented("/reef.api.v1.ChangeService/ListChanges"))
    }

    async fn get_change(
        &self,
        _request: Request<GetChangeRequest>,
    ) -> Result<Response<GetChangeResponse>, Status> {
        Err(not_implemented("/reef.api.v1.ChangeService/GetChange"))
    }

    async fn describe_change(
        &self,
        _request: Request<DescribeChangeRequest>,
    ) -> Result<Response<DescribeChangeResponse>, Status> {
        Err(not_implemented("/reef.api.v1.ChangeService/DescribeChange"))
    }

    async fn abandon_change(
        &self,
        _request: Request<AbandonChangeRequest>,
    ) -> Result<Response<AbandonChangeResponse>, Status> {
        Err(not_implemented("/reef.api.v1.ChangeService/AbandonChange"))
    }
}
