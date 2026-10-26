//! Errors at the API edge (ADR 0017, ADR 0029): every handler's error
//! becomes a [`tonic::Status`] whose code comes from the error's
//! [`ErrorKind`], whose message is its [`ErrorChain`], and whose details
//! carry a `google.rpc.Status` holding one `reef.api.v1.ErrorDetail { kind,
//! reason, chain, hint, next, metadata }` (see [`reef_api::error`]), so
//! clients get the kind back without parsing messages. [`kind_from_status`]
//! is the client's inverse.
//!
//! `hint` and `next` (R1.26, ADR 0025) are the error's [`Advice`]: how to
//! fix it, when the daemon knows. They come from [`Advise::advice`] on the
//! errors that have a known fix, read here and nowhere else, so no handler
//! formats a fix into a message. `reason` comes from [`reason_of`] the same
//! way.
//!
//! [`not_implemented`], [`unknown_method`] and [`v0_client`] are the three
//! answers to a request this daemon cannot serve as asked (R2.01).

use std::error::Error;

use prost::Message as _;
use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use tonic::{Code, Status};

use crate::backend::BaseCacheError;
use crate::db::DbError;
use crate::lease::LeaseError;
use crate::maintenance::backup::{BackupError, ManifestError};
use crate::maintenance::gc::GcRunError;
use crate::maintenance::quiesce::QuiesceError;
use crate::maintenance::restore::RestoreError;
use crate::repo::RegisterError;
use crate::service::page::{PageError, PageTokenError};
use crate::snapshot::index::ReindexError;
use crate::store::StoreError;
use crate::workspace::ManagerError;

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
    pub fn from_detail(detail: &v1::ErrorDetail) -> Option<Advice> {
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
/// delegating, and is in [`advice_of`]'s list: a transparent variant's
/// `source()` skips the error it wraps.
pub trait Advise {
    /// The fix, when this error has a known one.
    fn advice(&self) -> Option<Advice>;
}

/// The advice for `error`: the first error in its source chain that is one
/// of the daemon's [`Advise`] types and has advice. Wrapping variants
/// delegate to what they wrap, since a `#[error(transparent)]` variant's
/// source is its inner error's source, not the inner error.
pub fn advice_of(error: &(dyn Error + 'static)) -> Option<Advice> {
    let mut current = Some(error);
    while let Some(error) = current {
        let advice = if let Some(error) = error.downcast_ref::<ManagerError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<StoreError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<LeaseError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<RegisterError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<DbError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<BaseCacheError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<ReindexError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<GcRunError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<BackupError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<QuiesceError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<RestoreError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<ManifestError>() {
            error.advice()
        } else {
            None
        };
        if advice.is_some() {
            return advice;
        }
        current = error.source();
    }
    None
}

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

/// The name 0.1's `reef.daemon.v0.ErrorDetail.kind` carried: the variant's
/// name. Only [`v0_client`] still writes one.
pub fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "NotFound",
        ErrorKind::AlreadyExists => "AlreadyExists",
        ErrorKind::InvalidArgument => "InvalidArgument",
        ErrorKind::Conflict => "Conflict",
        ErrorKind::Unavailable => "Unavailable",
        ErrorKind::PermissionDenied => "PermissionDenied",
        ErrorKind::Io => "Io",
        ErrorKind::Unsupported => "Unsupported",
        _ => "Internal",
    }
}

/// The specific condition an error is, as `ErrorDetail.reason`: the first
/// error in its source chain that names one. Reasons are API, documented on
/// the RPCs that return them (ADR 0029).
pub fn reason_of(error: &(dyn Error + 'static)) -> Option<&'static str> {
    let mut current = Some(error);
    while let Some(error) = current {
        // `PageError::Token` is transparent: its source skips the token
        // error it wraps.
        if error.downcast_ref::<PageTokenError>().is_some()
            || matches!(error.downcast_ref::<PageError>(), Some(PageError::Token(_)))
        {
            return Some(reef_api::error::INVALID_PAGE_TOKEN);
        }
        current = error.source();
    }
    None
}

/// A status for `error` classified as `kind`: the kind's code, the error's
/// chain as the message, and an `ErrorDetail` in the details. Errors that
/// are not the caller's are logged here, once.
pub fn status_from(error: &(dyn Error + 'static), kind: ErrorKind) -> Status {
    let message = ErrorChain(error).to_string();
    if !kind.is_caller_error() {
        tracing::error!(error = %message, ?kind, "request failed");
    }
    let advice = advice_of(error).unwrap_or_default();
    let detail = v1::ErrorDetail {
        kind: proto_kind(kind).into(),
        reason: reason_of(error).unwrap_or_default().to_owned(),
        chain: chain(error),
        hint: advice.hint,
        next: advice.next,
        metadata: std::collections::BTreeMap::new(),
    };
    reef_api::error::status(code_of(kind), message, &detail)
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
    detail_of(status).map_or_else(
        || kind_of_code(status.code()),
        |detail| kind_of_proto(detail.kind()),
    )
}

/// The `ErrorDetail` in a status's details, if it carries one.
pub fn detail_of(status: &Status) -> Option<v1::ErrorDetail> {
    reef_api::error::detail(status)
}

/// The version this daemon reports.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The answer of a declared RPC this daemon does not implement:
/// `UNIMPLEMENTED`, reason `NOT_IMPLEMENTED`, metadata `method`.
pub fn not_implemented(method: &'static str) -> Status {
    let message = format!("{method} is not implemented by this daemon (reef {VERSION})");
    tracing::warn!(method, "declared RPC called; not implemented");
    unsupported(
        message,
        reef_api::error::NOT_IMPLEMENTED,
        "A later Reef release implements it.",
        [("method", method.to_owned())],
    )
}

/// The answer to a method this daemon does not serve at all, a client newer
/// than the daemon: `UNIMPLEMENTED`, reason `UNKNOWN_METHOD`, metadata
/// `method`, `daemon_version`, `api_revision`.
pub fn unknown_method(path: &str) -> Status {
    let message = format!(
        "this daemon (reef {VERSION}, API v1 revision {}) does not serve {path}",
        reef_api::API_REVISION
    );
    tracing::warn!(method = path, "unknown method called");
    unsupported(
        message,
        reef_api::error::UNKNOWN_METHOD,
        "The client is newer than the daemon; stop the daemon (`reef daemon stop`) and the next command starts the current one.",
        [
            ("method", path.to_owned()),
            ("daemon_version", VERSION.to_owned()),
            ("api_revision", reef_api::API_REVISION.to_string()),
        ],
    )
}

fn unsupported<const N: usize>(
    message: String,
    reason: &str,
    hint: &str,
    metadata: [(&str, String); N],
) -> Status {
    let detail = v1::ErrorDetail {
        kind: v1::ErrorKind::Unsupported.into(),
        reason: reason.to_owned(),
        chain: vec![message.clone()],
        hint: hint.to_owned(),
        next: Vec::new(),
        metadata: metadata
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    };
    reef_api::error::status(Code::Unimplemented, message, &detail)
}

/// The answer to a 0.1 client's call of anything but `Daemon/Status`:
/// `FAILED_PRECONDITION` with a raw `reef.daemon.v0.ErrorDetail` of kind
/// `Conflict`, as 0.1 decodes it, telling it to upgrade. `Conflict` because
/// 0.1 knows no `Unsupported` and treats `Conflict` as the caller's to fix.
pub fn v0_client(path: &str) -> Status {
    let message =
        format!("this daemon is reef {VERSION} and serves API v1; this client speaks API v0");
    tracing::warn!(method = path, "API v0 client; told to upgrade");
    let detail = reef_api::v0::ErrorDetail {
        kind: kind_name(ErrorKind::Conflict).to_owned(),
        chain: vec![message.clone()],
        hint: format!("Upgrade reef to {VERSION} or later; `reef --version` shows this one."),
        next: Vec::new(),
    };
    Status::with_details(
        Code::FailedPrecondition,
        message,
        detail.encode_to_vec().into(),
    )
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
    use crate::model::{RepoId, WorkspaceId, WorkspaceState};
    use crate::repo::RepoThreadError;

    /// A request this daemon cannot serve as asked.
    #[derive(Debug, thiserror::Error)]
    #[error("not served")]
    struct NotServed;

    impl Classify for NotServed {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Unsupported
        }
    }

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
            (ErrorKind::Unsupported, Box::new(|| status(NotServed))),
        ]
    }

    #[test]
    fn error_kinds_round_trip() {
        let cases = one_error_per_kind();
        assert_eq!(cases.len(), KINDS.len());
        for (kind, make) in cases {
            let status = make();
            assert_eq!(status.code(), code_of(kind), "{kind:?}");
            assert_eq!(kind_from_status(&status), kind, "{kind:?}");
            assert_eq!(kind_of_proto(proto_kind(kind)), kind);
            let detail = detail_of(&status).unwrap();
            assert_eq!(detail.kind(), proto_kind(kind));
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

    fn ids() -> (WorkspaceId, RepoId) {
        (
            "bbbb000000000001".parse().unwrap(),
            "aaaa000000000001".parse().unwrap(),
        )
    }

    /// The advice a status carries, read back as a client reads it.
    fn advice_in(status: &Status) -> Advice {
        Advice::from_detail(&detail_of(status).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_store_names_its_build() {
        let (_, repo) = ids();
        let missing = StoreError::Missing {
            name: "deps".into(),
            hash: "ff".into(),
            repo,
            commit: "c0ffee".into(),
        };
        // Wrapped, as `Create` returns it: the transparent variant delegates.
        let status = status(ManagerError::Store(missing));
        assert_eq!(
            status.message(),
            "store deps has no build for lockfile hash ff"
        );
        assert_eq!(
            advice_in(&status).next,
            [
                "reef",
                "store",
                "build",
                "deps",
                "--repo",
                "aaaa000000000001",
                "--rev",
                "c0ffee"
            ]
        );
    }

    #[test]
    fn a_gone_workspace_advises_a_new_one() {
        let (ws, repo) = ids();
        let create = ["reef", "ws", "create", "--repo", "aaaa000000000001"];
        for state in [WorkspaceState::Released, WorkspaceState::Reclaimed] {
            let error = ManagerError::NotActive {
                id: ws,
                repo,
                state,
            };
            assert_eq!(advice_in(&status(error)).next, create, "{state:?}");
        }
        let reclaimed = LeaseError::Reclaimed {
            workspace: ws,
            repo,
            state: WorkspaceState::Reclaimed,
            at: None,
        };
        assert_eq!(advice_in(&status(reclaimed)).next, create);
        let expired = LeaseError::Expired {
            workspace: ws,
            repo,
            lease: "cccc000000000001".parse().unwrap(),
            grace_ended_at: crate::model::Timestamp(0),
        };
        assert_eq!(advice_in(&status(expired)).next, create);
        let releasing = ManagerError::NotActive {
            id: ws,
            repo,
            state: WorkspaceState::Releasing,
        };
        assert_eq!(advice_in(&status(releasing)).next, Vec::<String>::new());
    }

    #[test]
    fn an_unregistered_path_advises_registering_it_if_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let status = status(ManagerError::NotRegistered(dir.path().to_owned()));
        assert_eq!(status.code(), Code::NotFound);
        assert!(status.message().starts_with("repository containing "));
        assert_eq!(
            advice_in(&status).next,
            ["reef", "repo", "register", dir.path().to_str().unwrap()]
        );
        let gone = status_from_missing("/nonexistent/repo");
        assert!(
            gone.hint.starts_with("/nonexistent/repo does not exist"),
            "{gone:?}"
        );
        assert_eq!(gone.next, ["reef", "repo", "list"]);
    }

    #[test]
    fn backup_and_quiesce_refusals_carry_their_hints() {
        let busy = status(BackupError::DestNotEmpty("/b".into()));
        assert_eq!(busy.code(), Code::AlreadyExists);
        assert!(
            advice_in(&busy).hint.contains("never overwrites"),
            "{busy:?}"
        );
        let quiesced = status(QuiesceError::Quiesced {
            until: crate::model::Timestamp(1_792_175_477_042),
        });
        assert_eq!(quiesced.code(), Code::Unavailable);
        assert!(
            advice_in(&quiesced)
                .hint
                .contains("2026-10-16T18:31:17.042Z"),
            "{quiesced:?}"
        );
    }

    fn status_from_missing(path: &str) -> Advice {
        advice_in(&status(ManagerError::NotRegistered(path.into())))
    }

    #[test]
    fn lookups_advise_listing_or_more_characters() {
        let not_found = DbError::NotFound {
            what: "workspace",
            id: "abcd".into(),
        };
        assert_eq!(
            advice_in(&status(not_found)).next,
            ["reef", "ws", "list", "--all"]
        );
        let ambiguous = DbError::Ambiguous {
            what: "workspace",
            prefix: "abcd".into(),
            candidates: vec!["abcd000000000001".into(), "abcd000000000002".into()],
            truncated: false,
        };
        let advice = advice_in(&status(ManagerError::Db(ambiguous)));
        assert_eq!(advice.next, Vec::<String>::new());
        assert_eq!(
            advice.hint,
            "Give more of the id: abcd matches workspace abcd000000000001, abcd000000000002."
        );
    }

    #[test]
    fn every_db_wrapper_passes_its_advice_on() {
        let not_found = || DbError::NotFound {
            what: "workspace",
            id: "abcd".into(),
        };
        let list = ["reef", "ws", "list", "--all"];
        for status in [
            status(BaseCacheError::Db(not_found())),
            status(ReindexError::Db(not_found())),
            status(GcRunError::Db(not_found())),
            status(ManagerError::BaseCache(BaseCacheError::Db(not_found()))),
            status(LeaseError::Db(not_found())),
            status(RegisterError::Db(not_found())),
            status(StoreError::Db(not_found())),
        ] {
            assert_eq!(advice_in(&status).next, list, "{status:?}");
        }
    }

    #[test]
    fn errors_without_a_known_fix_carry_none() {
        let status = status(BackendError::RootExists("/ws".into()));
        let detail = detail_of(&status).unwrap();
        assert!(detail.hint.is_empty() && detail.next.is_empty());
        assert_eq!(Advice::from_detail(&detail), None);
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
        assert_eq!(
            kind_from_status(&Status::unimplemented("")),
            ErrorKind::Unsupported
        );
    }
}
