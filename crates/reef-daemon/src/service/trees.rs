//! `reef.daemon.v1.Trees`: the virtual workspace (R2.04). Stubs until then.

use reef_api::proto::v1::trees_server::Trees;
use reef_api::proto::v1::{
    ApplyEditsChunk, ApplyEditsResponse, FileChunk, ListTreeRequest, ListTreeResponse,
    ReadFileRequest,
};
use tonic::{Request, Response, Status, Streaming};

use super::stubs::{StubStream, stub};

/// Serves `Trees` (R2.04).
#[derive(Debug, Default)]
pub struct TreesService;

#[tonic::async_trait]
impl Trees for TreesService {
    type ReadStream = StubStream<FileChunk>;

    async fn list(
        &self,
        _: Request<ListTreeRequest>,
    ) -> Result<Response<ListTreeResponse>, Status> {
        Err(stub("reef.daemon.v1.Trees/List"))
    }

    async fn read(
        &self,
        _: Request<ReadFileRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        Err(stub("reef.daemon.v1.Trees/Read"))
    }

    async fn apply(
        &self,
        _: Request<Streaming<ApplyEditsChunk>>,
    ) -> Result<Response<ApplyEditsResponse>, Status> {
        Err(stub("reef.daemon.v1.Trees/Apply"))
    }
}
