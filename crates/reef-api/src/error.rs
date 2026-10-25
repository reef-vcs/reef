//! Error conventions shared by every Reef crate (ADR 0009), and their
//! mapping to gRPC status codes (ADR 0017, restated by ADR 0029).
//!
//! Each module defines its own `thiserror` enums, named `<Thing>Error` and
//! `#[non_exhaustive]`, whose variants carry the identifiers a caller needs to
//! act. Every error type that crosses a crate boundary implements
//! [`Classify`], so the daemon can map any error to a gRPC status code (R1.05)
//! and the CLI to an exit code (R1.22) without matching on messages.
//!
//! The daemon's side of the mapping (`status`, which needs its error types)
//! is `reef_daemon::error`; the client's side is here: [`code_of`] and its
//! inverse [`kind_from_status`], and [`detail_of`], which reads the
//! `ErrorDetail` every daemon error carries.

use std::error::Error;
use std::fmt;
use std::io;

use prost::Message as _;
use tonic::{Code, Status};

use crate::proto::v1::ErrorDetail;

/// Coarse classification shared by every error type in the workspace.
/// Maps to gRPC status codes and CLI exit codes at the edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The thing the caller named does not exist.
    NotFound,
    /// The thing the caller asked to create already exists.
    AlreadyExists,
    /// The caller's input, or configuration it supplied, is invalid.
    InvalidArgument,
    /// The repository's state does not allow the request; retrying after the
    /// state changes may succeed.
    Conflict,
    /// A resource is temporarily unavailable; retrying later may succeed.
    Unavailable,
    /// The operating system denied access.
    PermissionDenied,
    /// Reading or writing the filesystem failed.
    Io,
    /// A bug, a corrupt repository, or a failure Reef cannot classify.
    Internal,
}

impl ErrorKind {
    /// Whether the caller can act on the error: it describes their request or
    /// their environment, not a failure inside Reef.
    pub fn is_caller_error(self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::AlreadyExists
                | Self::InvalidArgument
                | Self::Conflict
                | Self::PermissionDenied
        )
    }

    /// The CLI's exit code for a command that failed with this kind: 2 for
    /// `InvalidArgument`, 3 `NotFound`, 4 `AlreadyExists`, 5 `Conflict`, 6
    /// `Unavailable`, 7 `PermissionDenied`, 8 `Io`, 1 `Internal`.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::InvalidArgument => 2,
            Self::NotFound => 3,
            Self::AlreadyExists => 4,
            Self::Conflict => 5,
            Self::Unavailable => 6,
            Self::PermissionDenied => 7,
            Self::Io => 8,
            Self::Internal => 1,
        }
    }
}

/// Implemented by every error type that crosses a crate boundary.
pub trait Classify {
    /// The error's classification.
    fn kind(&self) -> ErrorKind;
}

/// Classifies a foreign error by the first [`io::Error`] in its source chain:
/// [`ErrorKind::PermissionDenied`] for a permission error, [`ErrorKind::Io`]
/// for any other, [`ErrorKind::Internal`] when there is none.
///
/// Used for jj-lib errors, whose variants do not say whose fault a failure is.
/// An I/O `NotFound` deep inside jj means a file of the repository is missing,
/// not that the caller named something absent, so it maps to `Io`.
pub fn classify_by_source(error: &(dyn Error + 'static)) -> ErrorKind {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(io) = error.downcast_ref::<io::Error>() {
            return match io.kind() {
                io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                _ => ErrorKind::Io,
            };
        }
        current = error.source();
    }
    ErrorKind::Internal
}

/// Displays an error followed by each of its sources, separated by `: `.
/// Logs record errors through this, because error messages do not repeat
/// their source's message. Some foreign errors do, by displaying their
/// source's text as their own; a source whose text equals the previous one's
/// is skipped.
pub struct ErrorChain<'a>(pub &'a (dyn Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut previous = self.0.to_string();
        f.write_str(&previous)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            let text = error.to_string();
            if text != previous {
                write!(f, ": {text}")?;
                previous = text;
            }
            source = error.source();
        }
        Ok(())
    }
}

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
        ErrorKind::Io | ErrorKind::Internal => Code::Internal,
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
        ErrorKind::Internal => "Internal",
    }
}

/// The kind named `name` by [`kind_name`], if any.
pub fn parse_kind(name: &str) -> Option<ErrorKind> {
    KINDS.into_iter().find(|kind| kind_name(*kind) == name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum Outer {
        #[error("failed to open {0}")]
        Open(String, #[source] Inner),
        #[error("no cause")]
        Bare,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("failed to read the state")]
    struct Inner(#[source] io::Error);

    fn open(kind: io::ErrorKind) -> Outer {
        Outer::Open("repo".into(), Inner(io::Error::new(kind, "disk says no")))
    }

    #[test]
    fn chain_joins_every_source() {
        assert_eq!(
            ErrorChain(&open(io::ErrorKind::Other)).to_string(),
            "failed to open repo: failed to read the state: disk says no"
        );
        assert_eq!(ErrorChain(&Outer::Bare).to_string(), "no cause");
    }

    #[test]
    fn chain_skips_a_source_that_repeats_its_parent() {
        #[derive(Debug, thiserror::Error)]
        #[error("{0}")]
        struct Echo(#[source] io::Error);

        let echo = Echo(io::Error::other("disk says no"));
        assert_eq!(ErrorChain(&echo).to_string(), "disk says no");
    }

    #[test]
    fn classifies_by_the_first_io_error() {
        assert_eq!(
            classify_by_source(&open(io::ErrorKind::PermissionDenied)),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_by_source(&open(io::ErrorKind::NotFound)),
            ErrorKind::Io
        );
        assert_eq!(classify_by_source(&Outer::Bare), ErrorKind::Internal);
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

    #[test]
    fn every_kind_has_a_name_and_a_code() {
        for kind in KINDS {
            assert_eq!(parse_kind(kind_name(kind)), Some(kind));
            assert_eq!(
                kind_of_code(code_of(kind)),
                if kind == ErrorKind::Io {
                    ErrorKind::Internal
                } else {
                    kind
                }
            );
        }
    }
}
