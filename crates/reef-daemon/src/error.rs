//! Errors at the API edge (ADR 0017): every handler's error becomes a
//! [`tonic::Status`] whose code comes from the error's [`ErrorKind`], whose
//! message is its [`ErrorChain`], and whose details carry a
//! `reef.daemon.v0.ErrorDetail { kind, chain }` so clients get the kind
//! back without parsing messages. [`kind_from_status`] is the client's
//! inverse.

use std::error::Error;

use prost::Message as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use tonic::{Code, Status};

use crate::proto::v0::ErrorDetail;

/// Every kind, in the order the API documents them.
pub const KINDS: [ErrorKind; 8] = [
    ErrorKind::NotFound,
    ErrorKind::AlreadyExists,
    ErrorKind::InvalidArgument,
    ErrorKind::Conflict,
    ErrorKind::Unavailable,
    ErrorKind::PermissionDenied,
    ErrorKind::Io,
    ErrorKind::Internal,
];

/// The gRPC code for an error kind: `NotFound` → `NOT_FOUND`,
/// `AlreadyExists` → `ALREADY_EXISTS`, `InvalidArgument` →
/// `INVALID_ARGUMENT`, `Conflict` → `FAILED_PRECONDITION`, `Unavailable` →
/// `UNAVAILABLE`, `PermissionDenied` → `PERMISSION_DENIED`, `Io` and
/// `Internal` → `INTERNAL`.
pub fn code_of(kind: ErrorKind) -> Code {
    match kind {
        ErrorKind::NotFound => Code::NotFound,
        ErrorKind::AlreadyExists => Code::AlreadyExists,
        ErrorKind::InvalidArgument => Code::InvalidArgument,
        ErrorKind::Conflict => Code::FailedPrecondition,
        ErrorKind::Unavailable => Code::Unavailable,
        ErrorKind::PermissionDenied => Code::PermissionDenied,
        // `ErrorKind` is non-exhaustive; a kind added later is internal
        // until it is given a code here.
        _ => Code::Internal,
    }
}

/// The name `ErrorDetail.kind` carries: the variant's name.
pub fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "NotFound",
        ErrorKind::AlreadyExists => "AlreadyExists",
        ErrorKind::InvalidArgument => "InvalidArgument",
        ErrorKind::Conflict => "Conflict",
        ErrorKind::Unavailable => "Unavailable",
        ErrorKind::PermissionDenied => "PermissionDenied",
        ErrorKind::Io => "Io",
        _ => "Internal",
    }
}

/// The kind named `name` by [`kind_name`], if any.
pub fn parse_kind(name: &str) -> Option<ErrorKind> {
    KINDS.into_iter().find(|kind| kind_name(*kind) == name)
}

/// A status for `error` classified as `kind`: the kind's code, the error's
/// chain as the message, and an `ErrorDetail` in the details. Errors that
/// are not the caller's are logged here, once.
pub fn status_from(error: &(dyn Error + 'static), kind: ErrorKind) -> Status {
    let message = ErrorChain(error).to_string();
    if !kind.is_caller_error() {
        tracing::error!(error = %message, ?kind, "request failed");
    }
    let detail = ErrorDetail {
        kind: kind_name(kind).to_owned(),
        chain: chain(error),
    };
    Status::with_details(code_of(kind), message, detail.encode_to_vec().into())
}

/// [`status_from`] with the error's own kind. Handlers end in
/// `.map_err(status)`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "by value so handlers can write `.map_err(status)`"
)]
pub fn status<E: Classify + Error + 'static>(error: E) -> Status {
    let kind = error.kind();
    status_from(&error, kind)
}

/// The kind of an error status: the `ErrorDetail`'s when the details carry
/// one, else the inverse of [`code_of`] (`INTERNAL`, `UNKNOWN`, and every
/// other code → `Internal`).
pub fn kind_from_status(status: &Status) -> ErrorKind {
    detail_of(status)
        .and_then(|detail| parse_kind(&detail.kind))
        .unwrap_or_else(|| kind_of_code(status.code()))
}

/// The `ErrorDetail` in a status's details, if it carries one.
pub fn detail_of(status: &Status) -> Option<ErrorDetail> {
    let details = status.details();
    if details.is_empty() {
        return None;
    }
    ErrorDetail::decode(details).ok()
}

fn kind_of_code(code: Code) -> ErrorKind {
    match code {
        Code::NotFound => ErrorKind::NotFound,
        Code::AlreadyExists => ErrorKind::AlreadyExists,
        Code::InvalidArgument => ErrorKind::InvalidArgument,
        Code::FailedPrecondition => ErrorKind::Conflict,
        Code::Unavailable => ErrorKind::Unavailable,
        Code::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Internal,
    }
}

/// The error's message and each source's, outermost first, skipping a
/// source whose text repeats its parent's as [`ErrorChain`] does.
fn chain(error: &(dyn Error + 'static)) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        let text = error.to_string();
        if messages.last() != Some(&text) {
            messages.push(text);
        }
        current = error.source();
    }
    messages
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::backend::{BackendError, BackendKind};
    use crate::db::DbError;
    use crate::repo::RepoThreadError;

    /// One real error per kind.
    fn one_error_per_kind() -> Vec<(ErrorKind, Box<dyn Fn() -> Status>)> {
        vec![
            (
                ErrorKind::NotFound,
                Box::new(|| {
                    status(DbError::NotFound {
                        what: "workspace",
                        id: "abcd".into(),
                    })
                }),
            ),
            (
                ErrorKind::AlreadyExists,
                Box::new(|| status(BackendError::RootExists("/ws".into()))),
            ),
            (
                ErrorKind::InvalidArgument,
                Box::new(|| {
                    status(DbError::InvalidPrefix {
                        what: "repository",
                        prefix: "xy".into(),
                    })
                }),
            ),
            (
                ErrorKind::Conflict,
                Box::new(|| {
                    status(DbError::Conflict {
                        what: "repository",
                        id: "abcd".into(),
                        reason: "has active workspaces",
                    })
                }),
            ),
            (
                ErrorKind::Unavailable,
                Box::new(|| {
                    status(BackendError::Unavailable {
                        kind: BackendKind::Overlayfs,
                        reason: "not implemented until R1.09".into(),
                    })
                }),
            ),
            (
                ErrorKind::PermissionDenied,
                Box::new(|| {
                    status(BackendError::Io {
                        op: "walk",
                        path: "/ws".into(),
                        source: io::Error::from(io::ErrorKind::PermissionDenied),
                    })
                }),
            ),
            (
                ErrorKind::Io,
                Box::new(|| {
                    status(BackendError::Io {
                        op: "walk",
                        path: "/ws".into(),
                        source: io::Error::other("disk on fire"),
                    })
                }),
            ),
            (
                ErrorKind::Internal,
                Box::new(|| {
                    status(RepoThreadError::Panicked {
                        id: "00000000000000aa".parse().unwrap(),
                    })
                }),
            ),
        ]
    }

    #[test]
    fn every_kind_has_a_code_and_round_trips() {
        let cases = one_error_per_kind();
        assert_eq!(cases.len(), KINDS.len());
        for (kind, make) in cases {
            let status = make();
            assert_eq!(status.code(), code_of(kind), "{kind:?}");
            assert_eq!(kind_from_status(&status), kind, "{kind:?}");
            assert_eq!(parse_kind(kind_name(kind)), Some(kind));
            let detail = detail_of(&status).unwrap();
            assert_eq!(detail.kind, kind_name(kind));
            assert_eq!(detail.chain.join(": "), status.message());
        }
        // Io and Internal share a code; only the detail tells them apart.
        assert_eq!(code_of(ErrorKind::Io), Code::Internal);
    }

    #[test]
    fn chain_lists_each_source() {
        let error = BackendError::Io {
            op: "walk",
            path: "/ws".into(),
            source: io::Error::other("disk on fire"),
        };
        let status = status(error);
        assert_eq!(status.message(), "walk /ws: disk on fire");
        assert_eq!(
            detail_of(&status).unwrap().chain,
            ["walk /ws", "disk on fire"]
        );
    }

    #[test]
    fn statuses_without_details_fall_back_to_the_code() {
        assert_eq!(
            kind_from_status(&Status::not_found("x")),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind_from_status(&Status::failed_precondition("x")),
            ErrorKind::Conflict
        );
        assert_eq!(kind_from_status(&Status::unknown("x")), ErrorKind::Internal);
        let garbage = Status::with_details(Code::NotFound, "x", vec![0xff, 0xff].into());
        assert_eq!(kind_from_status(&garbage), ErrorKind::NotFound);
        assert_eq!(parse_kind("Nope"), None);
    }
}
