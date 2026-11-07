//! `reef.api.v1.IdentityService` (R3.03, ADR 0057): operator identities,
//! the daemon's issuing authorities, and the session keys they certify.
//! The work is in [`crate::identity`]; this module turns requests into its
//! calls and records into messages. Nothing here records an operation.

use std::collections::HashMap;
use std::sync::Arc;

use reef_api::v1::identity_service_server::IdentityService;
use reef_api::v1::{
    self, CreateIdentityRequest, CreateIdentityResponse, GetIdentityRequest, GetIdentityResponse,
    GetSessionKeyRequest, GetSessionKeyResponse, IssueSessionKeyRequest, IssueSessionKeyResponse,
    ListAuthoritiesRequest, ListAuthoritiesResponse, ListIdentitiesRequest, ListIdentitiesResponse,
    ListSessionKeysRequest, ListSessionKeysResponse, RevokeAuthorityRequest,
    RevokeAuthorityResponse, RevokeIdentityRequest, RevokeIdentityResponse,
    RevokeSessionKeysRequest, RevokeSessionKeysResponse, RotateAuthorityRequest,
    RotateAuthorityResponse, revoke_session_keys_request,
};
use reef_evidence::session::{IdentityKind, is_name};
use tonic::{Request, Response, Status};

use super::convert::timestamp;
use super::page::{PageRequest, newest_first};
use super::reading::bad_token;
use crate::daemon::AppState;
use crate::db::{AuthorityRow, IdentityRow, RemoteTokenRow, SessionKeyFilter};
use crate::error::status;
use crate::identity::issue::Channel;
use crate::identity::read::{kind_message, revocation_row_message, session_key_message};
use crate::identity::{IdentityError, audit, display_name_problem, email_problem};
use crate::model::Timestamp;

const LIST_SESSION_KEYS: &str = "/reef.api.v1.IdentityService/ListSessionKeys";

/// Serves `IdentityService`.
#[derive(Debug)]
pub struct IdentityHandler {
    state: Arc<AppState>,
}

impl IdentityHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> IdentityHandler {
        IdentityHandler { state }
    }

    /// The live remote tokens linked to each identity, by identity id.
    fn linked_tokens(&self) -> HashMap<String, Vec<String>> {
        let now = Timestamp::now();
        let mut linked: HashMap<String, Vec<String>> = HashMap::new();
        for row in self.state.remote.tokens().rows() {
            if let Some(id) = row.identity_id.as_ref().filter(|_| row.is_live(now)) {
                linked.entry(id.clone()).or_default().push(row.name.clone());
            }
        }
        for names in linked.values_mut() {
            names.sort();
        }
        linked
    }

    /// `rows` as messages, with their defaults, key counts and tokens.
    fn identity_messages(&self, rows: &[IdentityRow]) -> Result<Vec<v1::Identity>, Status> {
        let counts = self
            .state
            .db
            .live_session_key_counts(Timestamp::now())
            .map_err(status)?;
        let tokens = self.linked_tokens();
        let default = self.state.identity.default_id();
        Ok(rows
            .iter()
            .map(|row| v1::Identity {
                id: row.id.clone(),
                name: row.name.clone(),
                kind: kind_message(IdentityKind::parse(&row.kind)).into(),
                display_name: row.display_name.clone().unwrap_or_default(),
                email: row.email.clone().unwrap_or_default(),
                created_at: Some(timestamp(row.created_at)),
                revocation: row.revocation.as_ref().map(revocation_row_message),
                is_default: default == Some(row.id.as_str()),
                live_session_keys: counts.get(&row.id).copied().unwrap_or_default(),
                remote_token_names: tokens.get(&row.id).cloned().unwrap_or_default(),
            })
            .collect())
    }

    fn identity_message(&self, row: &IdentityRow) -> Result<v1::Identity, Status> {
        Ok(self
            .identity_messages(std::slice::from_ref(row))?
            .pop()
            .unwrap_or_default())
    }
}

/// An authority row as a message.
fn authority_message(row: &AuthorityRow) -> v1::Authority {
    v1::Authority {
        key_id: row.id.clone(),
        algorithm: row.algorithm.clone(),
        public_key: row.public_key.clone(),
        host: row.host.clone(),
        created_at: Some(timestamp(row.created_at)),
        retired_at: row.retired_at.map(timestamp),
        revocation: row.revocation.as_ref().map(revocation_row_message),
        current: row.retired_at.is_none(),
    }
}

/// The `(issued_at, key_id)` cursor of a page token.
fn parse_cursor(cursor: &str) -> Result<(Timestamp, String), Status> {
    let (at, id) = cursor.split_once(':').ok_or_else(bad_token)?;
    Ok((
        Timestamp(at.parse().map_err(|_| bad_token())?),
        id.to_owned(),
    ))
}

/// The channel of a request: the socket, or the remote listener with its
/// token's linked identity.
pub(crate) fn channel_of<T>(state: &AppState, request: &Request<T>) -> Channel {
    match super::remote_of(request) {
        None => Channel::Socket,
        Some(caller) => {
            let linked = state
                .remote
                .tokens()
                .rows()
                .into_iter()
                .find(|row: &RemoteTokenRow| row.id == caller.token_id)
                .and_then(|row| row.identity_id);
            Channel::Remote {
                token_id: caller.token_id.clone(),
                token_name: caller.token_name.clone(),
                linked,
            }
        }
    }
}

#[tonic::async_trait]
impl IdentityService for IdentityHandler {
    async fn create_identity(
        &self,
        request: Request<CreateIdentityRequest>,
    ) -> Result<Response<CreateIdentityResponse>, Status> {
        let request = request.into_inner();
        let name = request.name.trim().to_owned();
        if !is_name(&name) {
            return Err(status(IdentityError::InvalidName { name }));
        }
        let kind = match v1::IdentityKind::try_from(request.kind) {
            Ok(v1::IdentityKind::Person) => IdentityKind::Person,
            Ok(v1::IdentityKind::Ci) => IdentityKind::Ci,
            _ => {
                return Err(status(IdentityError::InvalidIdentity {
                    field: "kind",
                    message: "an identity is a person or ci".into(),
                }));
            }
        };
        if let Some(message) = display_name_problem(&request.display_name) {
            return Err(status(IdentityError::InvalidIdentity {
                field: "display_name",
                message,
            }));
        }
        let email = request.email.trim();
        if let Some(message) = (!email.is_empty()).then(|| email_problem(email)).flatten() {
            return Err(status(IdentityError::InvalidIdentity {
                field: "email",
                message,
            }));
        }
        let db = &self.state.db;
        let row = IdentityRow {
            id: crate::identity::unused_identity_id(db).map_err(status)?,
            name: name.clone(),
            kind: kind.as_str().to_owned(),
            display_name: Some(request.display_name.clone()).filter(|text| !text.is_empty()),
            email: Some(email.to_owned()).filter(|text| !text.is_empty()),
            created_at: Timestamp::now(),
            revocation: None,
        };
        if let Some(existing) = db.insert_identity(&row).map_err(status)? {
            return Err(status(IdentityError::Exists {
                name,
                revoked: existing.revocation.is_some(),
            }));
        }
        audit::identity_changed("create", &row.id, &row.name, &row.kind);
        Ok(Response::new(CreateIdentityResponse {
            identity: Some(self.identity_message(&row)?),
        }))
    }

    async fn get_identity(
        &self,
        request: Request<GetIdentityRequest>,
    ) -> Result<Response<GetIdentityResponse>, Status> {
        let row = self
            .state
            .identity
            .find(&request.into_inner().name)
            .map_err(status)?;
        Ok(Response::new(GetIdentityResponse {
            identity: Some(self.identity_message(&row)?),
        }))
    }

    async fn list_identities(
        &self,
        request: Request<ListIdentitiesRequest>,
    ) -> Result<Response<ListIdentitiesResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            "/reef.api.v1.IdentityService/ListIdentities",
            request.page_size,
            &request.page_token,
            &ListIdentitiesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let rows: Vec<IdentityRow> = self
            .state
            .db
            .identities()
            .map_err(status)?
            .into_iter()
            .filter(|row| request.include_revoked || row.revocation.is_none())
            .collect();
        let (rows, next_page_token) =
            page.page(rows, |row| newest_first(row.created_at.0, &row.id));
        Ok(Response::new(ListIdentitiesResponse {
            identities: self.identity_messages(&rows)?,
            next_page_token,
        }))
    }

    async fn revoke_identity(
        &self,
        request: Request<RevokeIdentityRequest>,
    ) -> Result<Response<RevokeIdentityResponse>, Status> {
        let request = request.into_inner();
        let row = self
            .state
            .identity
            .revoke_identity(
                &request.name,
                request.invalid_from.as_ref(),
                request.since_created,
                &request.reason,
            )
            .map_err(status)?;
        // Held provenance keys of the identity go now (R3.04).
        self.state
            .provenance
            .revoked(&crate::provenance::Revoked::Identity(row.id.clone()));
        Ok(Response::new(RevokeIdentityResponse {
            identity: Some(self.identity_message(&row)?),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn issue_session_key(
        &self,
        request: Request<IssueSessionKeyRequest>,
    ) -> Result<Response<IssueSessionKeyResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let channel = channel_of(&self.state, &request);
        let read = self
            .state
            .identity
            .issue(request.into_inner(), channel, agent)
            .await
            .map_err(status)?;
        Ok(Response::new(IssueSessionKeyResponse {
            key: Some(session_key_message(&read, true)),
        }))
    }

    async fn get_session_key(
        &self,
        request: Request<GetSessionKeyRequest>,
    ) -> Result<Response<GetSessionKeyResponse>, Status> {
        let identities = &self.state.identity;
        let row = identities
            .key_by_prefix(&request.into_inner().key_id)
            .map_err(status)?;
        let read = identities.read(vec![row]).await.map_err(status)?;
        Ok(Response::new(GetSessionKeyResponse {
            key: read.first().map(|read| session_key_message(read, true)),
        }))
    }

    async fn list_session_keys(
        &self,
        request: Request<ListSessionKeysRequest>,
    ) -> Result<Response<ListSessionKeysResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_SESSION_KEYS,
            request.page_size,
            &request.page_token,
            &ListSessionKeysRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let identities = &self.state.identity;
        let identity_id = match request.identity.trim() {
            "" => None,
            named => Some(identities.find(named).map_err(status)?.id),
        };
        let filter = SessionKeyFilter {
            identity_id,
            session_id: Some(request.session_id.trim().to_owned()).filter(|id| !id.is_empty()),
            live_at: (!request.include_ended).then(Timestamp::now),
        };
        let cursor = page.cursor().map(parse_cursor).transpose()?;
        let size = page.size();
        let mut rows = self
            .state
            .db
            .list_session_keys(
                &filter,
                cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                size + 1,
            )
            .map_err(status)?;
        let more = rows.len() > size;
        rows.truncate(size);
        let next_page_token = match rows.last() {
            Some(last) if more => {
                page.token_at(&format!("{}:{}", last.issued_at.0, last.key_id), "")
            }
            _ => String::new(),
        };
        let read = identities.read(rows).await.map_err(status)?;
        Ok(Response::new(ListSessionKeysResponse {
            keys: read
                .iter()
                .map(|read| session_key_message(read, false))
                .collect(),
            next_page_token,
        }))
    }

    async fn revoke_session_keys(
        &self,
        request: Request<RevokeSessionKeysRequest>,
    ) -> Result<Response<RevokeSessionKeysResponse>, Status> {
        let request = request.into_inner();
        let identities = &self.state.identity;
        let given = request.invalid_from.as_ref();
        let read = match &request.target {
            Some(revoke_session_keys_request::Target::KeyId(key_id)) => {
                identities
                    .revoke_key(key_id, given, request.since_issued, &request.reason)
                    .await
            }
            Some(revoke_session_keys_request::Target::SessionId(session_id)) => {
                identities
                    .revoke_session(
                        session_id.trim(),
                        given,
                        request.since_issued,
                        &request.reason,
                    )
                    .await
            }
            None => Err(IdentityError::InvalidRevocation {
                field: "target",
                message: "name a key_id or a session_id to revoke".into(),
            }),
        }
        .map_err(status)?;
        // Held provenance keys of the session, or the key, go now (R3.04).
        let scope = match &request.target {
            Some(revoke_session_keys_request::Target::SessionId(session_id)) => {
                crate::provenance::Revoked::Session(session_id.trim().to_owned())
            }
            _ => crate::provenance::Revoked::Key(
                read.first()
                    .map(|key| key.row.key_id.clone())
                    .unwrap_or_default(),
            ),
        };
        self.state.provenance.revoked(&scope);
        Ok(Response::new(RevokeSessionKeysResponse {
            keys: read
                .iter()
                .map(|read| session_key_message(read, false))
                .collect(),
        }))
    }

    async fn list_authorities(
        &self,
        request: Request<ListAuthoritiesRequest>,
    ) -> Result<Response<ListAuthoritiesResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            "/reef.api.v1.IdentityService/ListAuthorities",
            request.page_size,
            &request.page_token,
            &ListAuthoritiesRequest::default(),
        )
        .map_err(status)?;
        let rows = self.state.db.authorities().map_err(status)?;
        let (rows, next_page_token) =
            page.page(rows, |row| newest_first(row.created_at.0, &row.id));
        Ok(Response::new(ListAuthoritiesResponse {
            authorities: rows.iter().map(authority_message).collect(),
            next_page_token,
        }))
    }

    async fn rotate_authority(
        &self,
        request: Request<RotateAuthorityRequest>,
    ) -> Result<Response<RotateAuthorityResponse>, Status> {
        let request = request.into_inner();
        let (retired, current) = self
            .state
            .identity
            .rotate(
                request.revoke,
                request.invalid_from.as_ref(),
                &request.reason,
            )
            .await
            .map_err(status)?;
        if request.revoke {
            self.state
                .provenance
                .revoked(&crate::provenance::Revoked::Authority(retired.id.clone()));
        }
        tracing::info!(
            retired_key_id = %retired.id,
            key_id = %current.id,
            revoked = request.revoke,
            "rotated the authority key"
        );
        Ok(Response::new(RotateAuthorityResponse {
            retired: Some(authority_message(&retired)),
            current: Some(authority_message(&current)),
        }))
    }

    async fn revoke_authority(
        &self,
        request: Request<RevokeAuthorityRequest>,
    ) -> Result<Response<RevokeAuthorityResponse>, Status> {
        let request = request.into_inner();
        let row = self
            .state
            .identity
            .revoke_authority(
                &request.key_id,
                request.invalid_from.as_ref(),
                request.since_created,
                &request.reason,
            )
            .map_err(status)?;
        self.state
            .provenance
            .revoked(&crate::provenance::Revoked::Authority(row.id.clone()));
        Ok(Response::new(RevokeAuthorityResponse {
            authority: Some(authority_message(&row)),
        }))
    }
}
