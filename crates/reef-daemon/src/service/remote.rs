//! `reef.daemon.v1.Remote`: access tokens (R2.08), on the socket only.
//! Stubs until then.

use reef_api::proto::v1::remote_server::Remote;
use reef_api::proto::v1::{
    IssueTokenRequest, IssueTokenResponse, ListTokensRequest, ListTokensResponse,
    RevokeTokenRequest, RevokeTokenResponse,
};
use tonic::{Request, Response, Status};

use super::stubs::stub;

/// Serves `Remote` (R2.08), on the socket only.
#[derive(Debug, Default)]
pub struct RemoteService;

#[tonic::async_trait]
impl Remote for RemoteService {
    async fn issue_token(
        &self,
        _: Request<IssueTokenRequest>,
    ) -> Result<Response<IssueTokenResponse>, Status> {
        Err(stub("reef.daemon.v1.Remote/IssueToken"))
    }

    async fn list_tokens(
        &self,
        _: Request<ListTokensRequest>,
    ) -> Result<Response<ListTokensResponse>, Status> {
        Err(stub("reef.daemon.v1.Remote/ListTokens"))
    }

    async fn revoke_token(
        &self,
        _: Request<RevokeTokenRequest>,
    ) -> Result<Response<RevokeTokenResponse>, Status> {
        Err(stub("reef.daemon.v1.Remote/RevokeToken"))
    }
}
