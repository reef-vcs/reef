//! The remote listener (R2.08, ADR 0049): a TLS port that serves the API
//! and the web UI to other machines, to callers holding a token.
//!
//! Off by default; `[remote] enabled` or `--remote` turns it on, and the
//! daemon binds it at start (step 11), retrying a failed bind with backoff
//! while the socket serves. One port carries native gRPC (the CLI and
//! SDKs), gRPC-web and the embedded assets (the web UI). Reef terminates
//! TLS itself with rustls on ring (`tls`): a generated key that clients
//! pin, or files the operator supplies, reloaded when they change.
//!
//! Every request passes `gate`: the R2.18 checks for a non-loopback
//! origin, then a bearer token (`tokens`) whose scope allows the method.
//! ADMIN methods are never served. The gate puts a [`Caller`] in the
//! request's extensions, which handlers read for the WRITE rules
//! (`rules`) and to record the token on the operations they write.
//! Streams a revoked token holds end within a second (`streams`).

pub(crate) mod audit;
mod failures;
pub(crate) mod gate;
pub mod incoming;
pub mod listener;
pub(crate) mod rules;
pub(crate) mod streams;
pub mod tls;
pub mod tokens;

use std::net::SocketAddr;
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, TokenScope};
use crate::error::{Advice, Advise};

pub use listener::RemoteListener;

/// Whether the v1 method at `path` streams its response; the per-token
/// stream limit counts these.
pub fn gate_is_streaming(path: &str) -> bool {
    gate::is_streaming(path)
}

/// The remote listener's limits. Not configuration-file keys: tests
/// shrink them through `Config::remote_limits`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Open connections at most; one more is accepted and closed at once.
    pub max_connections: usize,
    /// Open connections from one IPv4 address or IPv6 /64 at most.
    pub max_connections_per_address: usize,
    /// TLS handshakes in flight at most; accepts wait for a slot.
    pub max_handshakes: usize,
    /// A handshake that takes longer is dropped.
    pub handshake_timeout: Duration,
    /// A connection with no authenticated request this long after its
    /// handshake is closed.
    pub auth_timeout: Duration,
    /// Token refusals from one address within `failure_window` that put
    /// it in backoff.
    pub failures_before_backoff: u32,
    /// See `failures_before_backoff`.
    pub failure_window: Duration,
    /// The first backoff; each repeat doubles it.
    pub backoff: Duration,
    /// The longest backoff.
    pub backoff_max: Duration,
    /// An address with no refusal for this long starts over.
    pub backoff_reset: Duration,
    /// Addresses tracked at most; the one with the oldest failure goes.
    pub tracked_addresses: usize,
    /// Requests in flight per token at most, streams included.
    pub max_requests_per_token: usize,
    /// Streams open per token at most.
    pub max_streams_per_token: usize,
    /// Supplied certificate files are checked for changes at most this
    /// often.
    pub cert_reload_interval: Duration,
    /// How often the generated certificate is checked for renewal.
    pub cert_check_interval: Duration,
    /// A generated certificate's validity.
    pub cert_validity: Duration,
    /// A generated certificate with less than this left is renewed.
    pub cert_renew_before: Duration,
    /// Live tokens at most.
    pub max_tokens: usize,
    /// Tokens' last use is written to the database at most this often.
    pub last_used_flush: Duration,
    /// The first wait before binding again after a failed bind; doubles to
    /// `bind_retry_max`.
    pub bind_retry: Duration,
    /// See `bind_retry`.
    pub bind_retry_max: Duration,
    /// Token refusals from one address are logged at most this often.
    pub refusal_log_every: Duration,
    /// How often expired tokens' streams are ended.
    pub expiry_sweep: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_connections: 256,
            max_connections_per_address: 32,
            max_handshakes: 64,
            handshake_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(30),
            failures_before_backoff: 10,
            failure_window: Duration::from_mins(1),
            backoff: Duration::from_mins(1),
            backoff_max: Duration::from_hours(1),
            backoff_reset: Duration::from_hours(1),
            tracked_addresses: 4_096,
            max_requests_per_token: 32,
            max_streams_per_token: 16,
            cert_reload_interval: Duration::from_mins(1),
            cert_check_interval: Duration::from_hours(24),
            cert_validity: Duration::from_hours(397 * 24),
            cert_renew_before: Duration::from_hours(30 * 24),
            max_tokens: 256,
            last_used_flush: Duration::from_mins(1),
            bind_retry: Duration::from_secs(1),
            bind_retry_max: Duration::from_mins(1),
            refusal_log_every: Duration::from_secs(10),
            expiry_sweep: Duration::from_secs(1),
        }
    }
}

/// Who called through the remote listener: put in a request's extensions
/// by the gate, which clients cannot set, and read by handlers.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The token's id.
    pub token_id: String,
    /// The token's name when the call was made.
    pub token_name: String,
    /// The token's scope.
    pub scope: TokenScope,
    /// The peer address.
    pub peer: SocketAddr,
    /// Cancelled when the token is revoked or expires.
    pub ended: CancellationToken,
    /// The tokens, to say which of the two ended it.
    pub(crate) tokens: std::sync::Weak<tokens::Tokens>,
}

/// Why a token was refused, as `REMOTE_TOKEN_INVALID`'s `state` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No token, a malformed one, an unknown id, or a wrong secret.
    Unknown,
    /// The token's own secret, after it expired.
    Expired,
    /// The token's own secret, after it was revoked.
    Revoked,
}

impl Refusal {
    /// The `state` metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Unknown => "unknown",
            Refusal::Expired => "expired",
            Refusal::Revoked => "revoked",
        }
    }
}

/// The remote listener's refusals and the token RPCs' failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RemoteError {
    /// A request without a live token.
    #[error("the daemon refused this remote token ({})", state.as_str())]
    TokenInvalid {
        /// Why.
        state: Refusal,
    },
    /// A method the token's scope does not allow.
    #[error("a {scope} token cannot call {method}, which is {access}", scope = scope.as_str())]
    AccessDenied {
        /// The method's path.
        method: String,
        /// `write`, `admin` or `unknown`.
        access: &'static str,
        /// The token's scope.
        scope: TokenScope,
    },
    /// A field remote callers cannot set.
    #[error("remote callers cannot set {field} on {method}")]
    FieldDenied {
        /// The method's path.
        method: String,
        /// The field.
        field: &'static str,
    },
    /// A release, move or renewal of a workspace this token did not
    /// create.
    #[error("workspace {workspace} was not created by this remote token")]
    WorkspaceNotOwned {
        /// The workspace id.
        workspace: String,
    },
    /// The token has its limit of calls or streams open.
    #[error("this remote token has too many {limit} open")]
    Busy {
        /// `requests` or `streams`.
        limit: &'static str,
    },
    /// No live token has the name.
    #[error("no live remote token is named {name}")]
    TokenNotFound {
        /// The name.
        name: String,
    },
    /// A live token has the name.
    #[error("a live remote token is named {name}")]
    TokenExists {
        /// The name.
        name: String,
    },
    /// The daemon holds its limit of live tokens.
    #[error("the daemon holds its limit of {limit} live remote tokens")]
    TooManyTokens {
        /// [`Limits::max_tokens`].
        limit: usize,
    },
    /// A name that is not `[a-z0-9][a-z0-9._-]{0,62}`.
    #[error("{name:?} is not a valid remote token name")]
    InvalidTokenName {
        /// The name.
        name: String,
    },
    /// A rotation overlap over 7 days, or an expiry that is not positive.
    #[error("{field} is out of range: {reason}")]
    InvalidDuration {
        /// `overlap` or `expires_in`.
        field: &'static str,
        /// Why.
        reason: &'static str,
    },
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl Classify for RemoteError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::TokenInvalid { .. }
            | Self::AccessDenied { .. }
            | Self::FieldDenied { .. }
            | Self::WorkspaceNotOwned { .. } => ErrorKind::PermissionDenied,
            Self::Busy { .. } => ErrorKind::Unavailable,
            Self::TokenNotFound { .. } => ErrorKind::NotFound,
            Self::TokenExists { .. } => ErrorKind::AlreadyExists,
            Self::TooManyTokens { .. } => ErrorKind::Conflict,
            Self::InvalidTokenName { .. } | Self::InvalidDuration { .. } => {
                ErrorKind::InvalidArgument
            }
            Self::Db(error) => error.kind(),
        }
    }
}

impl Advise for RemoteError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::TokenInvalid { state } => Some(Advice::hint(format!(
                "The daemon refused this token ({}). Ask the daemon's owner for a new one (`reef remote token create` on that machine), then `reef remote add` it again.",
                state.as_str()
            ))),
            Self::AccessDenied { method, scope, .. } => Some(Advice::hint(format!(
                "This token's scope ({}) does not allow {method}; ADMIN calls run only on the daemon's machine.",
                scope.as_str()
            ))),
            Self::FieldDenied { field, .. } => Some(Advice::hint(format!(
                "Remote callers cannot set `{field}`; the workspace goes under the daemon's workspace root."
            ))),
            Self::WorkspaceNotOwned { .. } => Some(Advice::hint(
                "Remote callers release, move and renew only workspaces their token created.",
            )),
            Self::Busy { .. } => Some(Advice::hint(
                "This token has too many calls open; close some or retry.",
            )),
            Self::TokenNotFound { name } => Some(Advice::run(
                format!("No live token is named `{name}`."),
                ["reef", "remote", "token", "list"],
            )),
            Self::TokenExists { name } => Some(Advice::run(
                format!("A live token is named `{name}`: rotate it, or revoke it first."),
                ["reef", "remote", "token", "rotate", name.as_str()],
            )),
            Self::TooManyTokens { .. } => Some(Advice::run(
                "Revoke tokens you no longer use.",
                ["reef", "remote", "token", "list"],
            )),
            Self::InvalidTokenName { .. } => Some(Advice::hint(
                "Use lowercase letters, digits, `.`, `_` and `-`, at most 63, starting with a letter or digit.",
            )),
            Self::InvalidDuration { .. } => None,
            Self::Db(error) => error.advice(),
        }
    }
}

/// `access` as error metadata names it.
pub(crate) fn access_name(access: Option<reef_api::v1::Access>) -> &'static str {
    use reef_api::v1::Access;
    match access {
        Some(Access::Read) => "read",
        Some(Access::Write) => "write",
        Some(Access::Admin) => "admin",
        _ => "unknown",
    }
}
