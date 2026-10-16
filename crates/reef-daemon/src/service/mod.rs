//! The gRPC services, one module per service in `proto/`.
//!
//! Handlers return [`tonic::Status`] built by [`status_of`], which maps an
//! error's [`ErrorKind`] to a code by the Phase 1 table. R1.05 adds the
//! `ErrorDetail` message to the status details.

use std::error::Error;

use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use tonic::{Code, Status};

pub mod daemon;

/// The gRPC code for an error kind.
pub fn code_of(kind: ErrorKind) -> Code {
    match kind {
        ErrorKind::NotFound => Code::NotFound,
        ErrorKind::AlreadyExists => Code::AlreadyExists,
        ErrorKind::InvalidArgument => Code::InvalidArgument,
        ErrorKind::Conflict => Code::FailedPrecondition,
        ErrorKind::Unavailable => Code::Unavailable,
        ErrorKind::PermissionDenied => Code::PermissionDenied,
        _ => Code::Internal,
    }
}

/// A status for `error`: its kind's code, and its chain as the message.
/// Errors that are not the caller's are logged here, once.
pub fn status_of<E: Classify + Error + 'static>(error: &E) -> Status {
    let kind = error.kind();
    let message = ErrorChain(error).to_string();
    if !kind.is_caller_error() {
        tracing::error!(error = %message, ?kind, "request failed");
    }
    Status::new(code_of(kind), message)
}
