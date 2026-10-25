//! `reef.daemon.v1.Symbols`: index, ingest, lookup (R2.14). Stubs until then.

use reef_api::proto::v1::symbols_server::Symbols;
use reef_api::proto::v1::{
    IndexSymbolsRequest, IndexSymbolsResponse, IngestSymbolsChunk, IngestSymbolsResponse,
    LookupSymbolsRequest, LookupSymbolsResponse,
};
use tonic::{Request, Response, Status, Streaming};

use super::stubs::stub;

/// Serves `Symbols` (R2.14).
#[derive(Debug, Default)]
pub struct SymbolsService;

#[tonic::async_trait]
impl Symbols for SymbolsService {
    async fn index(
        &self,
        _: Request<IndexSymbolsRequest>,
    ) -> Result<Response<IndexSymbolsResponse>, Status> {
        Err(stub("reef.daemon.v1.Symbols/Index"))
    }

    async fn ingest(
        &self,
        _: Request<Streaming<IngestSymbolsChunk>>,
    ) -> Result<Response<IngestSymbolsResponse>, Status> {
        Err(stub("reef.daemon.v1.Symbols/Ingest"))
    }

    async fn lookup(
        &self,
        _: Request<LookupSymbolsRequest>,
    ) -> Result<Response<LookupSymbolsResponse>, Status> {
        Err(stub("reef.daemon.v1.Symbols/Lookup"))
    }
}
