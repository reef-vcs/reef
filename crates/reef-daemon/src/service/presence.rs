//! `reef.api.v1.PresenceService`: declared in R2.01, implemented by R2.10
//! and R2.11. Until then every RPC answers `UNIMPLEMENTED` with reason
//! `NOT_IMPLEMENTED`.

use std::pin::Pin;

use futures::Stream;
use reef_api::v1::presence_service_server::PresenceService;
use reef_api::v1::{
    ClearPresenceRequest, ClearPresenceResponse, ReportPresenceRequest, ReportPresenceResponse,
    SubscribePresenceRequest, SubscribePresenceResponse,
};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `PresenceService`.
#[derive(Debug, Default)]
pub struct PresenceHandler;

type SubscribePresenceStream =
    Pin<Box<dyn Stream<Item = Result<SubscribePresenceResponse, Status>> + Send>>;

#[tonic::async_trait]
impl PresenceService for PresenceHandler {
    type SubscribePresenceStream = SubscribePresenceStream;

    async fn report_presence(
        &self,
        _request: Request<ReportPresenceRequest>,
    ) -> Result<Response<ReportPresenceResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.PresenceService/ReportPresence",
        ))
    }

    async fn clear_presence(
        &self,
        _request: Request<ClearPresenceRequest>,
    ) -> Result<Response<ClearPresenceResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.PresenceService/ClearPresence",
        ))
    }

    async fn subscribe_presence(
        &self,
        _request: Request<SubscribePresenceRequest>,
    ) -> Result<Response<Self::SubscribePresenceStream>, Status> {
        Err(not_implemented(
            "/reef.api.v1.PresenceService/SubscribePresence",
        ))
    }
}
