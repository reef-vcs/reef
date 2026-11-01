//! `reef.api.v1.RemoteService` (R2.08, ADR 0049): issuing and ending the
//! remote listener's tokens. Every RPC is ADMIN, so only the socket serves
//! them; a token cannot mint, rotate or revoke tokens, itself included.

use std::sync::Arc;
use std::time::Duration;

use reef_api::v1::remote_service_server::RemoteService;
use reef_api::v1::{
    self, CreateRemoteTokenRequest, CreateRemoteTokenResponse, ListRemoteTokensRequest,
    ListRemoteTokensResponse, RemoteScope, RevokeRemoteTokenRequest, RevokeRemoteTokenResponse,
    RotateRemoteTokenRequest, RotateRemoteTokenResponse,
};
use tonic::{Request, Response, Status};

use super::convert::timestamp;
use super::page::{PageRequest, newest_first};
use crate::daemon::AppState;
use crate::db::{RemoteTokenRow, TokenScope};
use crate::error::status;
use crate::model::Timestamp;
use crate::remote::{RemoteError, audit, tokens};

/// Serves `RemoteService`.
#[derive(Debug)]
pub struct RemoteTokensService {
    state: Arc<AppState>,
}

impl RemoteTokensService {
    /// A service over `state`'s tokens.
    pub fn new(state: Arc<AppState>) -> RemoteTokensService {
        RemoteTokensService { state }
    }

    fn tokens(&self) -> &Arc<tokens::Tokens> {
        self.state.remote.tokens()
    }
}

/// A row as a message, without its secret.
fn token_message(row: &RemoteTokenRow) -> v1::RemoteToken {
    let now = Timestamp::now();
    v1::RemoteToken {
        id: row.id.clone(),
        name: row.name.clone(),
        scope: match row.scope {
            TokenScope::Read => RemoteScope::Read,
            TokenScope::Write => RemoteScope::Write,
        }
        .into(),
        created_at: Some(timestamp(row.created_at)),
        expires_at: row.expires_at.map(timestamp),
        rotated_at: row.rotated_at.map(timestamp),
        previous_valid_until: row
            .previous_until
            .filter(|until| *until > now && row.previous_digest.is_some())
            .map(timestamp),
        revoked_at: row.revoked_at.map(timestamp),
        last_used_at: row
            .last_used_at
            .map(|at| timestamp(Timestamp(at.0 - at.0 % 60_000))),
        last_used_from: row.last_used_from.clone().unwrap_or_default(),
    }
}

/// A protobuf duration, or the error for one that is negative or absurd.
fn duration(
    field: &'static str,
    value: Option<&prost_types::Duration>,
) -> Result<Option<Duration>, Status> {
    let Some(value) = value else {
        return Ok(None);
    };
    let millis = i128::from(value.seconds) * 1000 + i128::from(value.nanos) / 1_000_000;
    u64::try_from(millis)
        .map(|millis| Some(Duration::from_millis(millis)))
        .map_err(|_| {
            status(RemoteError::InvalidDuration {
                field,
                reason: "must not be negative",
            })
        })
}

#[tonic::async_trait]
impl RemoteService for RemoteTokensService {
    async fn create_remote_token(
        &self,
        request: Request<CreateRemoteTokenRequest>,
    ) -> Result<Response<CreateRemoteTokenResponse>, Status> {
        let request = request.into_inner();
        let scope = match RemoteScope::try_from(request.scope) {
            Ok(RemoteScope::Write) => TokenScope::Write,
            _ => TokenScope::Read,
        };
        let expires_in = if request.never_expires {
            None
        } else {
            Some(
                duration("expires_in", request.expires_in.as_ref())?
                    .unwrap_or(tokens::DEFAULT_EXPIRY),
            )
        };
        let (row, secret) = self
            .tokens()
            .create(&request.name, scope, expires_in)
            .map_err(status)?;
        audit::token_changed("create", &row.id, &row.name, row.scope.as_str());
        Ok(Response::new(CreateRemoteTokenResponse {
            token: Some(token_message(&row)),
            secret,
        }))
    }

    async fn list_remote_tokens(
        &self,
        request: Request<ListRemoteTokensRequest>,
    ) -> Result<Response<ListRemoteTokensResponse>, Status> {
        let request = request.into_inner();
        let filters = ListRemoteTokensRequest {
            page_size: 0,
            page_token: String::new(),
            ..request.clone()
        };
        let page = PageRequest::new(
            "/reef.api.v1.RemoteService/ListRemoteTokens",
            request.page_size,
            &request.page_token,
            &filters,
        )
        .map_err(status)?;
        let now = Timestamp::now();
        let rows: Vec<RemoteTokenRow> = self
            .tokens()
            .rows()
            .into_iter()
            .filter(|row| request.include_ended || row.is_live(now))
            .collect();
        let (rows, next_page_token) =
            page.page(rows, |row| newest_first(row.created_at.0, &row.id));
        Ok(Response::new(ListRemoteTokensResponse {
            tokens: rows.iter().map(token_message).collect(),
            next_page_token,
        }))
    }

    async fn rotate_remote_token(
        &self,
        request: Request<RotateRemoteTokenRequest>,
    ) -> Result<Response<RotateRemoteTokenResponse>, Status> {
        let request = request.into_inner();
        let overlap = duration("overlap", request.overlap.as_ref())?.unwrap_or_default();
        let (row, secret) = self
            .tokens()
            .rotate(&request.name, overlap)
            .map_err(status)?;
        audit::token_changed("rotate", &row.id, &row.name, row.scope.as_str());
        Ok(Response::new(RotateRemoteTokenResponse {
            token: Some(token_message(&row)),
            secret,
        }))
    }

    async fn revoke_remote_token(
        &self,
        request: Request<RevokeRemoteTokenRequest>,
    ) -> Result<Response<RevokeRemoteTokenResponse>, Status> {
        let request = request.into_inner();
        let row = self.tokens().revoke(&request.name).map_err(status)?;
        audit::token_changed("revoke", &row.id, &row.name, row.scope.as_str());
        Ok(Response::new(RevokeRemoteTokenResponse {
            token: Some(token_message(&row)),
        }))
    }
}
