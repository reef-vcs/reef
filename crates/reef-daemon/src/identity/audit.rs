//! The identity audit log (R3.03, ADR 0057): lines in the daemon's log
//! under the target `reef_daemon::identity::audit`, at `info`, for every
//! identity created or revoked, every session key issued, every key or
//! session revoked, and every authority made or rotated. A line names the
//! identity, actor, key id, session, agent and token, and never holds a
//! private key: the daemon never sees a session key's, and the authority's
//! stays in its file and in memory. Remote issuance is also in
//! `reef_daemon::remote::audit`, as every remote write is.

use crate::db::SessionKeyRow;
use crate::model::Timestamp;

/// An identity created or revoked.
pub(crate) fn identity_changed(action: &'static str, identity_id: &str, name: &str, kind: &str) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        action,
        identity_id,
        identity = name,
        kind,
        "operator identity changed"
    );
}

/// A session key issued.
pub(crate) fn issued(key: &SessionKeyRow, identity: &str, token_name: Option<&str>) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        key_id = %key.key_id,
        identity_id = %key.identity_id,
        identity,
        actor = %key.actor,
        session_id = %key.session_id,
        agent = key.issued_by_agent.as_deref().unwrap_or_default(),
        token_id = key.remote_token_id.as_deref().unwrap_or_default(),
        token_name = token_name.unwrap_or_default(),
        authority_key_id = %key.authority_id,
        not_after = %key.not_after.to_rfc3339(),
        "session key issued"
    );
}

/// A revocation: of an identity, a session, a key, or an authority.
pub(crate) fn revoked(
    what: &'static str,
    id: &str,
    invalid_from: Timestamp,
    reason: Option<&str>,
    key_count: usize,
) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        what,
        id,
        invalid_from = %invalid_from.to_rfc3339(),
        reason = reason.unwrap_or_default(),
        key_count,
        "revoked"
    );
}

/// An authority made, at start or by rotation.
pub(crate) fn authority_changed(action: &'static str, retired: Option<&str>, key_id: &str) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        action,
        retired_key_id = retired.unwrap_or_default(),
        key_id,
        "authority key changed"
    );
}

/// A keyless key registered (R3.42): its identity, the certificate's OIDC
/// issuer and subject, and its log entry.
pub(crate) fn registered(
    key: &SessionKeyRow,
    identity: &str,
    certified: &reef_evidence::sigstore::SigstoreCertified,
    token_name: Option<&str>,
) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        key_id = %key.key_id,
        identity_id = %key.identity_id,
        identity,
        actor = %key.actor,
        session_id = %key.session_id,
        agent = key.issued_by_agent.as_deref().unwrap_or_default(),
        token_id = key.remote_token_id.as_deref().unwrap_or_default(),
        token_name = token_name.unwrap_or_default(),
        oidc_issuer = %certified.oidc_issuer,
        oidc_subject = %certified.oidc_subject,
        oidc_match_id = key.oidc_id.as_deref().unwrap_or_default(),
        log_index = certified.log_index,
        authority_key_id = %key.authority_id,
        not_after = %key.not_after.to_rfc3339(),
        "keyless session key registered"
    );
}

/// An OIDC match added (R3.42).
pub(crate) fn oidc_changed(action: &'static str, row: &crate::db::OidcMatchRow, identity: &str) {
    tracing::info!(
        target: "reef_daemon::identity::audit",
        action,
        oidc_match_id = %row.id,
        identity_id = %row.identity_id,
        identity,
        issuer = %row.issuer,
        subject = %row.subject,
        "OIDC match changed"
    );
}
