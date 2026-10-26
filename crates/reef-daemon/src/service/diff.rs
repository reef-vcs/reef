//! `reef.api.v1.DiffService`: declared in R2.01, implemented by R2.02. Until
//! then every RPC answers `UNIMPLEMENTED` with reason `NOT_IMPLEMENTED`.

use reef_api::v1::diff_service_server::DiffService;
use reef_api::v1::{GetDiffRequest, GetDiffResponse};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `DiffService`.
#[derive(Debug, Default)]
pub struct DiffHandler;

#[tonic::async_trait]
impl DiffService for DiffHandler {
    async fn get_diff(
        &self,
        _request: Request<GetDiffRequest>,
    ) -> Result<Response<GetDiffResponse>, Status> {
        Err(not_implemented("/reef.api.v1.DiffService/GetDiff"))
    }
}
