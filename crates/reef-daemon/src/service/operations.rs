//! `reef.api.v1.OperationService`: declared in R2.01, implemented by R2.02,
//! R2.03 and R2.09. Until then every RPC answers `UNIMPLEMENTED` with reason
//! `NOT_IMPLEMENTED`.

use std::pin::Pin;

use futures::Stream;
use reef_api::v1::operation_service_server::OperationService;
use reef_api::v1::{
    GetOperationRequest, GetOperationResponse, ListOperationsRequest, ListOperationsResponse,
    SubscribeOperationsRequest, SubscribeOperationsResponse, UndoOperationRequest,
    UndoOperationResponse,
};
use tonic::{Request, Response, Status};

use crate::error::not_implemented;

/// Serves `OperationService`.
#[derive(Debug, Default)]
pub struct OperationsHandler;

type SubscribeOperationsStream =
    Pin<Box<dyn Stream<Item = Result<SubscribeOperationsResponse, Status>> + Send>>;

#[tonic::async_trait]
impl OperationService for OperationsHandler {
    type SubscribeOperationsStream = SubscribeOperationsStream;

    async fn list_operations(
        &self,
        _request: Request<ListOperationsRequest>,
    ) -> Result<Response<ListOperationsResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/ListOperations",
        ))
    }

    async fn get_operation(
        &self,
        _request: Request<GetOperationRequest>,
    ) -> Result<Response<GetOperationResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/GetOperation",
        ))
    }

    async fn undo_operation(
        &self,
        _request: Request<UndoOperationRequest>,
    ) -> Result<Response<UndoOperationResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/UndoOperation",
        ))
    }

    async fn subscribe_operations(
        &self,
        _request: Request<SubscribeOperationsRequest>,
    ) -> Result<Response<Self::SubscribeOperationsStream>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/SubscribeOperations",
        ))
    }
}
