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
use reef_api::v1::{
    AddOidcMatchRequest, AddOidcMatchResponse, RegisterSigstoreKeyRequest,
    RegisterSigstoreKeyResponse, RevokeOidcMatchRequest, RevokeOidcMatchResponse,
};
use reef_api::v1::{
    GetApprovalRequest, GetApprovalResponse, ListPasskeysRequest, ListPasskeysResponse,
    RegisterPasskeyRequest, RegisterPasskeyResponse, RemovePasskeyRequest, RemovePasskeyResponse,
    RequestApprovalRequest, RequestApprovalResponse, ResetPasskeysRequest, ResetPasskeysResponse,
    ResolveApprovalRequest, ResolveApprovalResponse, StartPasskeyRegistrationRequest,
    StartPasskeyRegistrationResponse, resolve_approval_request,
};
use reef_evidence::session::{IdentityKind, is_name};
use reef_evidence::webauthn::{self, PasskeyChange};
use tonic::{Request, Response, Status};

use super::convert::timestamp;
use super::page::{PageRequest, newest_first, oldest_first};
use super::reading::bad_token;
use crate::daemon::AppState;
use crate::db::{
    AuthorityRow, IdentityRow, InTheWay, OidcMatchRow, PasskeyRow, RemoteTokenRow, SessionKeyFilter,
};
use crate::error::status;
use crate::identity::approvals::{
    Approval, ApprovalRequest, Requester, Resolution, Resolved, State,
};
use crate::identity::issue::Channel;
use crate::identity::passkeys::{ALGORITHMS, RP_ID};
use crate::identity::read::{
    issuer_kind_message, kind_message, revocation_row_message, session_key_message,
};
use crate::identity::sigstore::match_problem;
use crate::identity::{IdentityError, audit, display_name_problem, email_problem};
use crate::model::Timestamp;
use crate::service::evidence::{EvidenceError, link_of};

const LIST_SESSION_KEYS: &str = "/reef.api.v1.IdentityService/ListSessionKeys";

/// Serves `IdentityService`.
#[derive(Debug)]
pub struct IdentityHandler {
    state: Arc<AppState>,
}

impl IdentityHandler {
    /// `approval` as a message, with its identity's live passkeys.
    fn approval_message(&self, approval: &Approval) -> Result<v1::Approval, Status> {
        let live = self
            .state
            .db
            .passkeys(Some(&approval.identity.id), false)
            .map_err(status)?;
        Ok(approval_message(
            approval,
            live.into_iter().map(|row| row.credential_id).collect(),
        ))
    }

    /// `row` as a message, with its identity's name.
    fn passkey_message(&self, row: &PasskeyRow) -> Result<v1::Passkey, Status> {
        let identity = self.state.db.identity(&row.identity_id).map_err(status)?;
        Ok(passkey_message(row, identity.as_ref()))
    }

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
        let mut matches: HashMap<String, Vec<v1::OidcMatch>> = HashMap::new();
        for row in self
            .state
            .db
            .oidc_matches_of(&rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>())
            .map_err(status)?
        {
            matches
                .entry(row.identity_id.clone())
                .or_default()
                .push(oidc_message(&row));
        }
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
                oidc_matches: matches.remove(&row.id).unwrap_or_default(),
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
        kind: issuer_kind_message(
            reef_evidence::session::IssuerKind::parse(&row.kind)
                .unwrap_or(reef_evidence::session::IssuerKind::ReefDaemon),
        )
        .into(),
    }
}

/// An OIDC match in the API (R3.42).
fn oidc_message(row: &OidcMatchRow) -> v1::OidcMatch {
    v1::OidcMatch {
        id: row.id.clone(),
        identity_id: row.identity_id.clone(),
        issuer: row.issuer.clone(),
        subject: row.subject.clone(),
        created_at: Some(timestamp(row.created_at)),
        revocation: row.revocation.as_ref().map(revocation_row_message),
    }
}

/// A web UI session in the API.
fn web_caller(session: Option<&str>) -> Option<v1::WebCaller> {
    session.map(|session| v1::WebCaller {
        session: session.to_owned(),
    })
}

/// A passkey in the API (R3.43), with its identity's name when known.
fn passkey_message(row: &PasskeyRow, identity: Option<&IdentityRow>) -> v1::Passkey {
    v1::Passkey {
        id: row.id.clone(),
        identity_id: row.identity_id.clone(),
        identity: identity
            .map(|identity| identity.name.clone())
            .unwrap_or_default(),
        label: row.label.clone(),
        fingerprint: row.fingerprint.clone(),
        credential_id: row.credential_id.clone(),
        public_key: row.public_key.clone(),
        algorithm: i32::try_from(row.algorithm).unwrap_or_default(),
        rp_id: row.rp_id.clone(),
        aaguid: row.aaguid.clone(),
        sign_count: row.sign_count,
        created_at: Some(timestamp(row.created_at)),
        created_web_session: web_caller(row.created_web_session.as_deref()),
        last_used_at: row.last_used_at.map(timestamp),
        removed_at: row.removed_at.map(timestamp),
        removed_reason: row.removed_reason.clone().unwrap_or_default(),
        removed_web_session: web_caller(row.removed_web_session.as_deref()),
    }
}

/// An approval in the API (R3.43), with the credential ids of its
/// identity's live passkeys.
fn approval_message(approval: &Approval, credential_ids: Vec<Vec<u8>>) -> v1::Approval {
    let (state, resolved_at, key, resolved_by, declined_reason) = match &approval.state {
        State::Pending { .. } => (v1::ApprovalState::Pending, None, None, None, String::new()),
        State::Approved {
            at,
            key,
            web_session,
        } => (
            v1::ApprovalState::Approved,
            Some(*at),
            Some(key.as_ref().clone()),
            Some(web_session.as_str()),
            String::new(),
        ),
        State::Declined {
            at,
            reason,
            web_session,
        } => (
            v1::ApprovalState::Declined,
            Some(*at),
            None,
            web_session.as_deref(),
            reason.clone(),
        ),
        State::Expired => (
            v1::ApprovalState::Expired,
            Some(approval.expires_at),
            None,
            None,
            String::new(),
        ),
    };
    let index = &approval.index;
    v1::Approval {
        id: approval.id.clone(),
        state: state.into(),
        identity_id: approval.identity.id.clone(),
        identity: approval.identity.name.clone(),
        key_id: approval.public_key.id().to_string(),
        session_id: approval.session_id.clone(),
        authority_key_id: approval.authority.clone(),
        approves: Some(v1::Approves {
            statement_sha256: approval.approves.statement.clone(),
            change_id: approval.approves.change.clone(),
        }),
        challenge: approval.challenge().to_vec(),
        rp_id: RP_ID.to_owned(),
        credential_ids,
        kind: index.kind.clone(),
        result: index.result.as_str().to_owned(),
        target: index.target.clone().unwrap_or_default(),
        tree_id: index.tree_id.clone(),
        repo_id: approval.resolved.repo_id.clone(),
        commit_id: approval.resolved.commit_id.clone(),
        change_id: approval.resolved.change_id.clone(),
        statement: approval.statement.clone(),
        requested_through: approval.requester.channel.name().to_owned(),
        requested_by_agent: approval.requester.agent.clone().unwrap_or_default(),
        requested_workspace: approval.requester.workspace.clone().unwrap_or_default(),
        requested_web_session: web_caller(approval.requester.channel.web_session()),
        created_at: Some(timestamp(approval.created_at)),
        expires_at: Some(timestamp(approval.expires_at)),
        resolved_at: resolved_at.map(timestamp),
        passkey: key.as_ref().and_then(|key| key.passkey.clone()),
        key,
        resolved_web_session: web_caller(resolved_by),
        declined_reason,
    }
}

/// `wait` as a duration; a negative one is refused.
fn wait_of(wait: Option<&prost_types::Duration>) -> Result<std::time::Duration, Status> {
    let Some(wait) = wait else {
        return Ok(std::time::Duration::ZERO);
    };
    let millis = i128::from(wait.seconds) * 1000 + i128::from(wait.nanos) / 1_000_000;
    u64::try_from(millis)
        .map(std::time::Duration::from_millis)
        .map_err(|_| Status::invalid_argument("wait must not be negative"))
}

/// The `(issued_at, key_id)` cursor of a page token.
fn parse_cursor(cursor: &str) -> Result<(Timestamp, String), Status> {
    let (at, id) = cursor.split_once(':').ok_or_else(bad_token)?;
    Ok((
        Timestamp(at.parse().map_err(|_| bad_token())?),
        id.to_owned(),
    ))
}

/// The channel of a request: the socket, the web UI's listener with its
/// session (R3.43), or the remote listener with its token's linked
/// identity.
pub(crate) fn channel_of<T>(state: &AppState, request: &Request<T>) -> Channel {
    if let Some(caller) = super::web_of(request) {
        return Channel::Web {
            session: caller.session.clone(),
        };
    }
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
        let now = Timestamp::now();
        let mut matches: Vec<OidcMatchRow> = Vec::new();
        for spec in &request.oidc {
            if let Some(problem) = match_problem(&spec.issuer, &spec.subject) {
                return Err(status(problem));
            }
            if matches
                .iter()
                .any(|row| row.issuer == spec.issuer && row.subject == spec.subject)
            {
                continue;
            }
            matches.push(OidcMatchRow {
                id: crate::identity::new_id("oidc_"),
                identity_id: String::new(),
                issuer: spec.issuer.clone(),
                subject: spec.subject.clone(),
                created_at: now,
                revocation: None,
            });
        }
        let row = IdentityRow {
            id: crate::identity::unused_identity_id(db).map_err(status)?,
            name: name.clone(),
            kind: kind.as_str().to_owned(),
            display_name: Some(request.display_name.clone()).filter(|text| !text.is_empty()),
            email: Some(email.to_owned()).filter(|text| !text.is_empty()),
            created_at: now,
            revocation: None,
        };
        match db
            .insert_identity_with_oidc(&row, &matches)
            .map_err(status)?
        {
            None => {}
            Some(InTheWay::Identity(existing)) => {
                return Err(status(IdentityError::Exists {
                    name,
                    revoked: existing.revocation.is_some(),
                }));
            }
            Some(InTheWay::Match(existing)) => {
                let holder = db
                    .identity(&existing.identity_id)
                    .map_err(status)?
                    .map_or(existing.identity_id, |row| row.name);
                return Err(status(IdentityError::MatchExists { identity: holder }));
            }
        }
        audit::identity_changed("create", &row.id, &row.name, &row.kind);
        for spec in &matches {
            let added = OidcMatchRow {
                identity_id: row.id.clone(),
                ..spec.clone()
            };
            audit::oidc_changed("add", &added, &row.name);
        }
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
            issuer_kind: match v1::IssuerKind::try_from(request.issuer_kind) {
                Ok(v1::IssuerKind::ReefDaemon) => Some("reef-daemon".to_owned()),
                Ok(v1::IssuerKind::Sigstore) => Some("sigstore".to_owned()),
                _ => None,
            },
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

    #[tracing::instrument(skip_all)]
    async fn register_sigstore_key(
        &self,
        request: Request<RegisterSigstoreKeyRequest>,
    ) -> Result<Response<RegisterSigstoreKeyResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let channel = channel_of(&self.state, &request);
        let (read, created) = self
            .state
            .identity
            .register(request.into_inner().bundle, channel, agent)
            .await
            .map_err(status)?;
        Ok(Response::new(RegisterSigstoreKeyResponse {
            key: Some(session_key_message(&read, true)),
            created,
        }))
    }

    async fn add_oidc_match(
        &self,
        request: Request<AddOidcMatchRequest>,
    ) -> Result<Response<AddOidcMatchResponse>, Status> {
        let request = request.into_inner();
        let spec = request.r#match.unwrap_or_default();
        let row = self
            .state
            .identity
            .add_match(&request.identity, &spec.issuer, &spec.subject)
            .map_err(status)?;
        Ok(Response::new(AddOidcMatchResponse {
            r#match: Some(oidc_message(&row)),
        }))
    }

    async fn revoke_oidc_match(
        &self,
        request: Request<RevokeOidcMatchRequest>,
    ) -> Result<Response<RevokeOidcMatchResponse>, Status> {
        let request = request.into_inner();
        let (row, key_ids) = self
            .state
            .identity
            .revoke_match(
                &request.id,
                request.invalid_from.as_ref(),
                request.since_created,
                &request.reason,
            )
            .map_err(status)?;
        Ok(Response::new(RevokeOidcMatchResponse {
            r#match: Some(oidc_message(&row)),
            key_ids,
        }))
    }

    async fn list_passkeys(
        &self,
        request: Request<ListPasskeysRequest>,
    ) -> Result<Response<ListPasskeysResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            "/reef.api.v1.IdentityService/ListPasskeys",
            request.page_size,
            &request.page_token,
            &ListPasskeysRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let (rows, identities) = self
            .state
            .identity
            .list_passkeys(&request.identity, request.include_removed)
            .map_err(status)?;
        let (rows, next_page_token) = page.page(rows, |row| {
            format!(
                "{}:{}",
                row.identity_id,
                oldest_first(row.created_at.0, &row.id)
            )
        });
        Ok(Response::new(ListPasskeysResponse {
            passkeys: rows
                .iter()
                .map(|row| passkey_message(row, identities.get(&row.identity_id)))
                .collect(),
            next_page_token,
        }))
    }

    async fn start_passkey_registration(
        &self,
        request: Request<StartPasskeyRegistrationRequest>,
    ) -> Result<Response<StartPasskeyRegistrationResponse>, Status> {
        let channel = channel_of(&self.state, &request);
        let started = self
            .state
            .identity
            .start_registration(&channel, &request.into_inner().identity)
            .map_err(status)?;
        let needs_approval = !started.live.is_empty();
        let add_challenge = if needs_approval {
            webauthn::passkey_change_challenge(PasskeyChange::Add {
                registration_challenge: &started.challenge,
            })
            .to_vec()
        } else {
            Vec::new()
        };
        Ok(Response::new(StartPasskeyRegistrationResponse {
            challenge: started.challenge.to_vec(),
            expires_at: Some(timestamp(started.expires_at)),
            rp_id: RP_ID.to_owned(),
            identity_id: started.identity.id.clone(),
            identity: started.identity.name.clone(),
            user_name: format!("Reef: {}", started.identity.name),
            algorithms: ALGORITHMS
                .iter()
                .map(|algorithm| i32::try_from(algorithm.cose()).unwrap_or_default())
                .collect(),
            credential_ids: started
                .live
                .into_iter()
                .map(|row| row.credential_id)
                .collect(),
            needs_approval,
            add_challenge,
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn register_passkey(
        &self,
        request: Request<RegisterPasskeyRequest>,
    ) -> Result<Response<RegisterPasskeyResponse>, Status> {
        let channel = channel_of(&self.state, &request);
        let origin = self.state.web.origin().unwrap_or_default();
        let row = self
            .state
            .identity
            .register_passkey(&channel, origin, &request.into_inner())
            .map_err(status)?;
        Ok(Response::new(RegisterPasskeyResponse {
            passkey: Some(self.passkey_message(&row)?),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn remove_passkey(
        &self,
        request: Request<RemovePasskeyRequest>,
    ) -> Result<Response<RemovePasskeyResponse>, Status> {
        let channel = channel_of(&self.state, &request);
        let origin = self.state.web.origin().unwrap_or_default();
        let row = self
            .state
            .identity
            .remove_passkey(&channel, origin, &request.into_inner())
            .map_err(status)?;
        Ok(Response::new(RemovePasskeyResponse {
            passkey: Some(self.passkey_message(&row)?),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn reset_passkeys(
        &self,
        request: Request<ResetPasskeysRequest>,
    ) -> Result<Response<ResetPasskeysResponse>, Status> {
        let request = request.into_inner();
        let (removed, revoked_key_ids) = self
            .state
            .identity
            .reset_passkeys(&request.identity, &request.reason)
            .await
            .map_err(status)?;
        Ok(Response::new(ResetPasskeysResponse {
            removed: removed
                .iter()
                .map(|row| self.passkey_message(row))
                .collect::<Result<_, _>>()?,
            revoked_key_ids,
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn request_approval(
        &self,
        request: Request<RequestApprovalRequest>,
    ) -> Result<Response<RequestApprovalResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let channel = channel_of(&self.state, &request);
        if matches!(channel, Channel::Remote { .. }) {
            return Err(status(IdentityError::PasskeyOverRemote {
                method: "RequestApproval",
            }));
        }
        let request = request.into_inner();
        let state = &self.state;
        let size = request.statement.len();
        if size > reef_evidence::MAX_ENVELOPE_BYTES {
            return Err(status(EvidenceError::from(reef_evidence::Invalid::Size(
                size,
            ))));
        }
        let statement = request.statement.clone();
        let (_, index) =
            tokio::task::spawn_blocking(move || reef_evidence::verify::check_payload(&statement))
                .await
                .map_err(|error| status(IdentityError::Task(error.to_string())))?
                .map_err(|invalid| status(EvidenceError::from(invalid)))?;
        if request.revision.trim().is_empty() {
            return Err(status(EvidenceError::Request(
                "an approval is for one change: name its revision".into(),
            )));
        }
        let target = super::reading::target(state, &request.repo, &request.workspace)?;
        let (change_id, commit_id) = link_of(
            state,
            &target,
            index.tree_id.clone(),
            request.revision.clone(),
        )
        .await?
        .unwrap_or_default();
        let approval = state
            .identity
            .request_approval(
                ApprovalRequest {
                    identity: request.identity,
                    statement: request.statement,
                    index,
                    resolved: Resolved {
                        repo_id: target.repo.id.to_string(),
                        commit_id,
                        change_id,
                    },
                    public_key: request.public_key,
                    proof: request.proof,
                },
                Requester {
                    channel,
                    agent,
                    workspace: target.workspace.as_ref().map(|ws| ws.id.as_hex()),
                },
            )
            .await
            .map_err(status)?;
        Ok(Response::new(RequestApprovalResponse {
            approval: Some(self.approval_message(&approval)?),
        }))
    }

    async fn get_approval(
        &self,
        request: Request<GetApprovalRequest>,
    ) -> Result<Response<GetApprovalResponse>, Status> {
        let request = request.into_inner();
        let wait = wait_of(request.wait.as_ref())?;
        let approval = self
            .state
            .identity
            .get_approval(&request.id, wait)
            .await
            .map_err(status)?;
        Ok(Response::new(GetApprovalResponse {
            approval: Some(self.approval_message(&approval)?),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn resolve_approval(
        &self,
        request: Request<ResolveApprovalRequest>,
    ) -> Result<Response<ResolveApprovalResponse>, Status> {
        let channel = channel_of(&self.state, &request);
        let request = request.into_inner();
        let resolution = match request.resolution {
            Some(resolve_approval_request::Resolution::Approve(assertion)) => {
                Resolution::Approve(assertion)
            }
            Some(resolve_approval_request::Resolution::Decline(reason)) => {
                Resolution::Decline(reason)
            }
            None => {
                return Err(Status::invalid_argument(
                    "resolve an approval with approve or decline",
                ));
            }
        };
        let approval = self
            .state
            .identity
            .resolve_approval(&channel, self.state.web.origin(), &request.id, resolution)
            .await
            .map_err(status)?;
        Ok(Response::new(ResolveApprovalResponse {
            approval: Some(self.approval_message(&approval)?),
        }))
    }
}
