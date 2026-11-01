//! The remote listener's audit log (R2.08, ADR 0049): lines in the
//! daemon's log under the target `reef_daemon::remote::audit`, at `info`,
//! for every remote WRITE call (with its outcome), every refusal, every
//! token created, rotated or revoked, and every address put in backoff.
//! Token refusals from one address are logged at most once per
//! `refusal_log_every`, with a count. No line ever holds a secret.

use std::net::SocketAddr;

use reef_lib::error::ErrorChain;
use tonic::codegen::http::HeaderMap;

use super::failures::Logged;
use super::{Caller, Refusal, RemoteError};

/// A token refusal, with how many since the address's last line.
pub(crate) fn token_refused(peer: SocketAddr, method: &str, state: Refusal, logged: &Logged) {
    tracing::info!(
        target: "reef_daemon::remote::audit",
        peer = %peer,
        method,
        state = state.as_str(),
        refusal_count = logged.count,
        "remote token refused"
    );
    if let Some(backoff) = logged.backoff {
        tracing::info!(
            target: "reef_daemon::remote::audit",
            peer = %peer,
            backoff_ms = backoff.as_millis(),
            "remote address in backoff after repeated token refusals"
        );
    }
}

/// A call refused after its token was verified.
pub(crate) fn refused(
    token_id: &str,
    token_name: &str,
    peer: SocketAddr,
    method: &str,
    error: &RemoteError,
) {
    tracing::info!(
        target: "reef_daemon::remote::audit",
        token_id,
        token_name,
        peer = %peer,
        method,
        error = %ErrorChain(error),
        "remote call refused"
    );
}

/// A token created, rotated or revoked over the socket.
pub(crate) fn token_changed(action: &'static str, token_id: &str, token_name: &str, scope: &str) {
    tracing::info!(
        target: "reef_daemon::remote::audit",
        action,
        token_id,
        token_name,
        scope,
        "remote token changed"
    );
}

/// A WRITE call in flight, logged when its status is known, or as
/// `Cancelled` if the call is dropped first (the write may still finish).
#[derive(Debug)]
pub(crate) struct Write {
    info: Option<WriteInfo>,
}

#[derive(Debug)]
struct WriteInfo {
    token_id: String,
    token_name: String,
    peer: SocketAddr,
    method: String,
}

impl Write {
    pub(crate) fn begin(caller: &Caller, method: &str) -> Write {
        Write {
            info: Some(WriteInfo {
                token_id: caller.token_id.clone(),
                token_name: caller.token_name.clone(),
                peer: caller.peer,
                method: method.to_owned(),
            }),
        }
    }

    /// Logs the call with the status in `trailers` (or the headers of a
    /// trailers-only answer); none there is `Unknown`, a dropped stream.
    pub(crate) fn finish(mut self, trailers: &HeaderMap) {
        let code = trailers
            .get("grpc-status")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i32>().ok())
            .map_or_else(
                || "Unknown".to_owned(),
                |code| format!("{:?}", tonic::Code::from_i32(code)),
            );
        let reason = tonic::Status::from_header_map(trailers)
            .and_then(|status| reef_api::error::detail(&status))
            .map(|detail| detail.reason)
            .unwrap_or_default();
        if let Some(info) = self.info.take() {
            info.log(&code, &reason);
        }
    }
}

impl Drop for Write {
    fn drop(&mut self) {
        if let Some(info) = self.info.take() {
            info.log("Cancelled", "");
        }
    }
}

impl WriteInfo {
    fn log(&self, code: &str, reason: &str) {
        tracing::info!(
            target: "reef_daemon::remote::audit",
            token_id = %self.token_id,
            token_name = %self.token_name,
            peer = %self.peer,
            method = %self.method,
            code = %code,
            reason,
            "remote write"
        );
    }
}
