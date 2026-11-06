//! `reef.api.v1.RemoteService` (R2.08, ADR 0049): issuing and ending the
//! remote listener's tokens. Every RPC is ADMIN, so only the socket serves
//! them; a token cannot mint, rotate or revoke tokens, itself included.
//!
//! A token may be linked to one live operator identity when it is created
//! (R3.03, ADR 0057): the only identity it then gets session keys for.
//! Rotation keeps the link, and no RPC changes it.

use std::collections::HashMap;
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
use crate::identity::IdentityError;
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

    /// Each identity's name, by id, for the tokens' links.
    fn identity_names(&self) -> Result<HashMap<String, String>, Status> {
        Ok(self
            .state
            .db
            .identities()
            .map_err(status)?
            .into_iter()
            .map(|row| (row.id, row.name))
            .collect())
    }

    /// The live identity `name` links a new token to; `None` for an empty
    /// one.
    fn linked_identity(&self, name: &str) -> Result<Option<String>, Status> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        if !reef_evidence::session::is_name(name) {
            return Err(status(IdentityError::InvalidName {
                name: name.to_owned(),
            }));
        }
        let row = self
            .state
            .db
            .identity(name)
            .map_err(status)?
            .filter(|row| row.name == name)
            .ok_or_else(|| {
                status(IdentityError::NotFound {
                    name: name.to_owned(),
                })
            })?;
        if let Some(revocation) = &row.revocation {
            return Err(status(IdentityError::Revoked {
                name: row.name,
                revoked_at: revocation.revoked_at,
            }));
        }
        Ok(Some(row.id))
    }
}

/// A row as a message, without its secret; `names` gives its linked
/// identity's name.
fn token_message(row: &RemoteTokenRow, names: &HashMap<String, String>) -> v1::RemoteToken {
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
        identity: row
            .identity_id
            .as_ref()
            .and_then(|id| names.get(id))
            .cloned()
            .unwrap_or_default(),
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
        let identity_id = self.linked_identity(&request.identity)?;
        let (row, secret) = self
            .tokens()
            .create(&request.name, scope, expires_in, identity_id)
            .map_err(status)?;
        audit::token_changed("create", &row.id, &row.name, row.scope.as_str());
        Ok(Response::new(CreateRemoteTokenResponse {
            token: Some(token_message(&row, &self.identity_names()?)),
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
        let names = self.identity_names()?;
        Ok(Response::new(ListRemoteTokensResponse {
            tokens: rows.iter().map(|row| token_message(row, &names)).collect(),
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
            token: Some(token_message(&row, &self.identity_names()?)),
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
            token: Some(token_message(&row, &self.identity_names()?)),
        }))
    }
}
