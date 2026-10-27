//! The client half of the API's error model (ADR 0029): the kind of an
//! error status, and its `ErrorDetail` and advice. The daemon's `error`
//! module builds the statuses these read, and re-exports this module's
//! items for its own callers.

use reef_api::v1;
use reef_error::{Advice, ErrorKind};
use tonic::{Code, Status};

/// Every kind, in the order the API documents them.
pub const KINDS: [ErrorKind; 9] = [
    ErrorKind::NotFound,
    ErrorKind::AlreadyExists,
    ErrorKind::InvalidArgument,
    ErrorKind::Conflict,
    ErrorKind::Unavailable,
    ErrorKind::PermissionDenied,
    ErrorKind::Io,
    ErrorKind::Internal,
    ErrorKind::Unsupported,
];

/// The gRPC code for an error kind: `NotFound` → `NOT_FOUND`,
/// `AlreadyExists` → `ALREADY_EXISTS`, `InvalidArgument` →
/// `INVALID_ARGUMENT`, `Conflict` → `FAILED_PRECONDITION`, `Unavailable` →
/// `UNAVAILABLE`, `PermissionDenied` → `PERMISSION_DENIED`, `Io` and
/// `Internal` → `INTERNAL`, `Unsupported` → `UNIMPLEMENTED`.
pub fn code_of(kind: ErrorKind) -> Code {
    match kind {
        ErrorKind::NotFound => Code::NotFound,
        ErrorKind::AlreadyExists => Code::AlreadyExists,
        ErrorKind::InvalidArgument => Code::InvalidArgument,
        ErrorKind::Conflict => Code::FailedPrecondition,
        ErrorKind::Unavailable => Code::Unavailable,
        ErrorKind::PermissionDenied => Code::PermissionDenied,
        ErrorKind::Unsupported => Code::Unimplemented,
        // `ErrorKind` is non-exhaustive; a kind added later is internal
        // until it is given a code here.
        _ => Code::Internal,
    }
}

/// The kind as `ErrorDetail.kind`.
pub fn proto_kind(kind: ErrorKind) -> v1::ErrorKind {
    match kind {
        ErrorKind::NotFound => v1::ErrorKind::NotFound,
        ErrorKind::AlreadyExists => v1::ErrorKind::AlreadyExists,
        ErrorKind::InvalidArgument => v1::ErrorKind::InvalidArgument,
        ErrorKind::Conflict => v1::ErrorKind::Conflict,
        ErrorKind::Unavailable => v1::ErrorKind::Unavailable,
        ErrorKind::PermissionDenied => v1::ErrorKind::PermissionDenied,
        ErrorKind::Io => v1::ErrorKind::Io,
        ErrorKind::Unsupported => v1::ErrorKind::Unsupported,
        _ => v1::ErrorKind::Internal,
    }
}

/// The kind an `ErrorDetail.kind` names; unspecified, and a value this
/// build does not know, is `Internal`.
pub fn kind_of_proto(kind: v1::ErrorKind) -> ErrorKind {
    match kind {
        v1::ErrorKind::NotFound => ErrorKind::NotFound,
        v1::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
        v1::ErrorKind::InvalidArgument => ErrorKind::InvalidArgument,
        v1::ErrorKind::Conflict => ErrorKind::Conflict,
        v1::ErrorKind::Unavailable => ErrorKind::Unavailable,
        v1::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        v1::ErrorKind::Io => ErrorKind::Io,
        v1::ErrorKind::Unsupported => ErrorKind::Unsupported,
        v1::ErrorKind::Internal | v1::ErrorKind::Unspecified => ErrorKind::Internal,
    }
}

/// The kind of an error status: the `ErrorDetail`'s when the details carry
/// one, else the inverse of [`code_of`] (`INTERNAL`, `UNKNOWN`, and every
/// other code → `Internal`).
pub fn kind_from_status(status: &Status) -> ErrorKind {
    detail_of(status).map_or_else(
        || kind_of_code(status.code()),
        |detail| kind_of_proto(detail.kind()),
    )
}

/// The `ErrorDetail` in a status's details, if it carries one.
pub fn detail_of(status: &Status) -> Option<v1::ErrorDetail> {
    reef_api::error::detail(status)
}

/// An error kind's name in `snake_case`, as the CLI's JSON error object's
/// `kind` prints it.
pub fn kind_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::InvalidArgument => "invalid_argument",
        ErrorKind::Conflict => "conflict",
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::Io => "io",
        ErrorKind::Unsupported => "unsupported",
        _ => "internal",
    }
}

/// The kind [`kind_text`] names; any other text is `Internal`.
pub fn kind_from_text(text: &str) -> ErrorKind {
    KINDS
        .into_iter()
        .find(|kind| kind_text(*kind) == text)
        .unwrap_or(ErrorKind::Internal)
}

/// The advice an error status carries, if any.
pub fn advice_from_detail(detail: &v1::ErrorDetail) -> Option<Advice> {
    if detail.hint.is_empty() && detail.next.is_empty() {
        return None;
    }
    Some(Advice {
        hint: detail.hint.clone(),
        next: detail.next.clone(),
    })
}

fn kind_of_code(code: Code) -> ErrorKind {
    match code {
        Code::NotFound => ErrorKind::NotFound,
        Code::AlreadyExists => ErrorKind::AlreadyExists,
        Code::InvalidArgument => ErrorKind::InvalidArgument,
        Code::FailedPrecondition => ErrorKind::Conflict,
        Code::Unavailable => ErrorKind::Unavailable,
        Code::PermissionDenied => ErrorKind::PermissionDenied,
        Code::Unimplemented => ErrorKind::Unsupported,
        _ => ErrorKind::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip() {
        for kind in KINDS {
            assert_eq!(kind_from_text(kind_text(kind)), kind);
        }
        assert_eq!(kind_from_text("nope"), ErrorKind::Internal);
    }
}
