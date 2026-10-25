//! How to fix an error (R1.26, ADR 0025): the `hint` and `next` every
//! `ErrorDetail` may carry. The daemon's error types implement [`Advise`];
//! `reef_daemon::error::status` reads it, and clients read it back with
//! [`Advice::from_detail`].

use crate::proto::v1::ErrorDetail;

/// How to fix an error (R1.26, ADR 0025): `ErrorDetail.hint` and
/// `ErrorDetail.next`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Advice {
    /// One sentence; empty if there is only a command.
    pub hint: String,
    /// An argv an agent can run verbatim, starting `reef`; empty if there
    /// is only a hint. Never a placeholder.
    pub next: Vec<String>,
}

impl Advice {
    /// A hint with no command.
    pub fn hint(hint: impl Into<String>) -> Advice {
        Advice {
            hint: hint.into(),
            next: Vec::new(),
        }
    }

    /// A hint and the command that follows it.
    pub fn run<I, S>(hint: impl Into<String>, next: I) -> Advice
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Advice {
            hint: hint.into(),
            next: next.into_iter().map(Into::into).collect(),
        }
    }

    /// The advice an error status carries, if any.
    pub fn from_detail(detail: &ErrorDetail) -> Option<Advice> {
        if detail.hint.is_empty() && detail.next.is_empty() {
            return None;
        }
        Some(Advice {
            hint: detail.hint.clone(),
            next: detail.next.clone(),
        })
    }
}

/// An error that knows how it is fixed. Every error type with a
/// `#[error(transparent)]` variant wrapping one of these implements it too,
/// delegating, and is in `reef_daemon::error::advice_of`'s list: a
/// transparent variant's `source()` skips the error it wraps.
pub trait Advise {
    /// The fix, when this error has a known one.
    fn advice(&self) -> Option<Advice>;
}
