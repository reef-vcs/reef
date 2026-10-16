//! Error conventions shared by every Reef crate (ADR 0009).
//!
//! Each module defines its own `thiserror` enums, named `<Thing>Error` and
//! `#[non_exhaustive]`, whose variants carry the identifiers a caller needs to
//! act. Every error type that crosses a crate boundary implements
//! [`Classify`], so the daemon can map any error to a gRPC status code (R1.05)
//! and the CLI to an exit code (R1.22) without matching on messages.

use std::error::Error;
use std::fmt;
use std::io;

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
}

impl ErrorKind {
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
}
