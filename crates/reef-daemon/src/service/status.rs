//! `reef.api.v1.StatusService`: declared in R2.01, implemented by R2.02.
//! Until then every RPC answers `UNIMPLEMENTED` with reason
//! `NOT_IMPLEMENTED`.

use reef_api::v1::status_service_server::StatusService;
use reef_api::v1::{GetStatusRequest, GetStatusResponse};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `StatusService`.
#[derive(Debug, Default)]
pub struct StatusHandler;

#[tonic::async_trait]
impl StatusService for StatusHandler {
    async fn get_status(
        &self,
        _request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        Err(not_implemented("/reef.api.v1.StatusService/GetStatus"))
    }
}
