//! `reef.daemon.v1.Presence`: subscribe, report (R2.10). Stubs until then.

use reef_api::proto::v1::presence_server::Presence;
use reef_api::proto::v1::{
    Frame, PresenceUpdate, ReportPresenceResponse, SubscribePresenceRequest,
};
use tonic::{Request, Response, Status, Streaming};

use super::stubs::{StubStream, stub};

/// Serves `Presence` (R2.10).
#[derive(Debug, Default)]
pub struct PresenceService;

#[tonic::async_trait]
impl Presence for PresenceService {
    type SubscribeStream = StubStream<Frame>;

    async fn subscribe(
        &self,
        _: Request<SubscribePresenceRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(stub("reef.daemon.v1.Presence/Subscribe"))
    }

    async fn report(
        &self,
        _: Request<Streaming<PresenceUpdate>>,
    ) -> Result<Response<ReportPresenceResponse>, Status> {
        Err(stub("reef.daemon.v1.Presence/Report"))
    }
}
