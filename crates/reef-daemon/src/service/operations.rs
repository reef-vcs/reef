//! `reef.daemon.v1.Operations`: log, get (R2.02); undo (R2.03); subscribe
//! (R2.09); replay (R2.12). Stubs until then.

use reef_api::proto::v1::operations_server::Operations;
use reef_api::proto::v1::{
    Frame, GetOperationRequest, GetOperationResponse, LogOperationsRequest, LogOperationsResponse,
    ReplayRequest, ReplayState, SubscribeRequest, UndoRequest, UndoResponse,
};
use tonic::{Request, Response, Status};

use super::stubs::{StubStream, stub};

/// Serves `Operations` (R2.02, R2.03, R2.09, R2.12).
#[derive(Debug, Default)]
pub struct OperationsService;

#[tonic::async_trait]
impl Operations for OperationsService {
    type SubscribeStream = StubStream<Frame>;

    async fn log(
        &self,
        _: Request<LogOperationsRequest>,
    ) -> Result<Response<LogOperationsResponse>, Status> {
        Err(stub("reef.daemon.v1.Operations/Log"))
    }

    async fn get(
        &self,
        _: Request<GetOperationRequest>,
    ) -> Result<Response<GetOperationResponse>, Status> {
        Err(stub("reef.daemon.v1.Operations/Get"))
    }

    async fn undo(&self, _: Request<UndoRequest>) -> Result<Response<UndoResponse>, Status> {
        Err(stub("reef.daemon.v1.Operations/Undo"))
    }

    async fn subscribe(
        &self,
        _: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(stub("reef.daemon.v1.Operations/Subscribe"))
    }

    async fn replay(&self, _: Request<ReplayRequest>) -> Result<Response<ReplayState>, Status> {
        Err(stub("reef.daemon.v1.Operations/Replay"))
    }
}
