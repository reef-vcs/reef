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

use std::collections::BTreeMap;
use std::error::Error;

use prost::Message as _;
use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::diff::{EntryKind, PathError};
use reef_lib::jj::files::{EditError, EditProblem, FileError};
use reef_lib::jj::oplog::OpLookupError;
use reef_lib::jj::revset::RevisionError;
use reef_lib::jj::rewrite::RewriteError;
use reef_lib::jj::undo::UndoError;
use tonic::{Code, Status};

use crate::backend::BaseCacheError;
use crate::db::DbError;
use crate::lease::LeaseError;
use crate::maintenance::backup::{BackupError, ManifestError};
use crate::maintenance::gc::GcRunError;
use crate::maintenance::quiesce::QuiesceError;
use crate::maintenance::restore::RestoreError;
use crate::repo::RegisterError;
use crate::service::InvalidAgent;
use crate::service::operations::InvalidChangeFilter;
use crate::service::page::{PageError, PageTokenError};
use crate::snapshot::index::ReindexError;
use crate::store::StoreError;
use crate::workspace::ManagerError;

pub use reef_client::error::{
    KINDS, advice_from_detail, code_of, detail_of, kind_from_status, kind_of_proto, proto_kind,
};
pub use reef_error::Advice;

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
        } else if let Some(error) = error.downcast_ref::<crate::read::ReadError>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<crate::read::PinnedGone>() {
            error.advice()
        } else if let Some(error) = error.downcast_ref::<RevisionError>() {
            revision_advice(error)
        } else if let Some(error) = error.downcast_ref::<RewriteError>() {
            rewrite_advice(error)
        } else if let Some(error) = error.downcast_ref::<UndoError>() {
            undo_advice(error)
        } else if let Some(error) = error.downcast_ref::<FileError>() {
            file_advice(error)
        } else if let Some(error) = error.downcast_ref::<EditError>() {
            edit_advice(error)
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
    reason_and_metadata(error).map(|(reason, _)| reason)
}

/// The metadata of `error`'s reason, as `ErrorDetail.metadata`: the keys
/// documented with each reason.
pub fn reason_metadata(error: &(dyn Error + 'static)) -> BTreeMap<String, String> {
    reason_and_metadata(error)
        .map(|(_, metadata)| metadata)
        .unwrap_or_default()
}

/// A jj workspace name as the API reports it: the Reef workspace id of
/// `reef-<id>`, else the name.
fn workspace_of(name: &str) -> String {
    name.strip_prefix(reef_lib::jj::workspace::REEF_WORKSPACE_PREFIX)
        .unwrap_or(name)
        .to_owned()
}

/// Reason metadata from `(key, value)` pairs.
fn meta(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn revision_reason(error: &RevisionError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    Some(match error {
        RevisionError::Required => (reason::REVISION_REQUIRED, BTreeMap::new()),
        RevisionError::WorkspaceRequired { .. } => (reason::WORKSPACE_REQUIRED, BTreeMap::new()),
        RevisionError::Parse { .. } => (reason::INVALID_REVSET, BTreeMap::new()),
        RevisionError::NotFound { revision, .. } | RevisionError::Empty { revision } => (
            reason::REVISION_NOT_FOUND,
            meta(&[("revision", revision.clone())]),
        ),
        RevisionError::AmbiguousPrefix { prefix, .. } => (
            reason::AMBIGUOUS_ID_PREFIX,
            meta(&[("prefix", prefix.clone())]),
        ),
        RevisionError::Ambiguous {
            revision,
            commit_ids,
            ..
        } => (
            reason::AMBIGUOUS_REVISION,
            meta(&[
                ("revision", revision.clone()),
                (
                    "commit_ids",
                    commit_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            ]),
        ),
        RevisionError::WorkingCopyNotFound {
            workspace,
            at_operation,
        } => (
            reason::WORKING_COPY_NOT_FOUND,
            meta(&[
                ("workspace_id", workspace_of(workspace)),
                ("at_operation", at_operation.clone()),
            ]),
        ),
        _ => return None,
    })
}

fn lookup_reason(error: &OpLookupError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    let operation = |id: &str| BTreeMap::from([("operation_id".to_owned(), id.to_owned())]);
    Some(match error {
        OpLookupError::Invalid(_) => (reason::INVALID_OPERATION_ID, BTreeMap::new()),
        OpLookupError::NotFound(id) => (reason::OPERATION_NOT_FOUND, operation(id)),
        OpLookupError::Ambiguous(id) => (reason::AMBIGUOUS_OPERATION_ID, operation(id)),
        _ => return None,
    })
}

fn rewrite_reason(error: &RewriteError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    let commit = |id: &dyn std::fmt::Display| meta(&[("commit_id", id.to_string())]);
    Some(match error {
        RewriteError::InvalidCommitId(text) => (reason::INVALID_COMMIT_ID, commit(text)),
        RewriteError::InvalidDescription => (reason::INVALID_DESCRIPTION, BTreeMap::new()),
        RewriteError::Mismatch { expected, actual } => (
            reason::EXPECTED_COMMIT_MISMATCH,
            meta(&[
                ("expected_commit_id", expected.to_string()),
                ("commit_id", actual.to_string()),
            ]),
        ),
        RewriteError::Immutable(id) => (reason::IMMUTABLE_REVISION, commit(id)),
        RewriteError::Hidden(id) => (reason::HIDDEN_REVISION, commit(id)),
        _ => return None,
    })
}

fn undo_reason(error: &UndoError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    let operation = |id: &dyn std::fmt::Display| meta(&[("operation_id", id.to_string())]);
    Some(match error {
        UndoError::NotUndoable { op, .. } => (reason::OPERATION_NOT_UNDOABLE, operation(op)),
        UndoError::NotInHistory(op) => (reason::OPERATION_NOT_IN_HISTORY, operation(op)),
        UndoError::Superseded { op, .. } => (reason::OPERATION_SUPERSEDED, operation(op)),
        _ => return None,
    })
}

/// An entry type as the `type` metadata names it: the `EntryType` value.
fn type_name(kind: EntryKind) -> String {
    crate::service::entry_type_name(kind).to_owned()
}

fn file_reason(error: &FileError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    Some(match error {
        FileError::NotFound { path, commit } => (
            reason::PATH_NOT_FOUND,
            meta(&[("path", path.clone()), ("commit_id", commit.to_string())]),
        ),
        FileError::NotAFile { path, kind, commit } => (
            reason::NOT_A_FILE,
            meta(&[
                ("path", path.clone()),
                ("type", type_name(*kind)),
                ("commit_id", commit.to_string()),
            ]),
        ),
        FileError::NotADirectory { path, kind, commit } => (
            reason::NOT_A_DIRECTORY,
            meta(&[
                ("path", path.clone()),
                ("type", type_name(*kind)),
                ("commit_id", commit.to_string()),
            ]),
        ),
        // Transparent: its source skips the path error.
        FileError::Path(error) => (reason::INVALID_PATH, meta(&[("path", error.path.clone())])),
        _ => return None,
    })
}

fn edit_reason(error: &EditError) -> Option<(&'static str, BTreeMap<String, String>)> {
    use reef_api::error as reason;
    let (index, edit_path, problem) = match error {
        EditError::Edit {
            index,
            path,
            problem,
        } => (index, path, problem),
        EditError::TooMany(_) => return Some((reason::INVALID_EDIT, BTreeMap::new())),
        _ => return None,
    };
    let mut metadata = meta(&[
        ("edit_index", index.to_string()),
        ("path", problem.path().unwrap_or(edit_path).to_owned()),
    ]);
    let mut add = |key: &str, value: String| {
        metadata.insert(key.to_owned(), value);
    };
    let reason = match problem {
        EditProblem::InvalidPath(_) => reason::INVALID_PATH,
        EditProblem::Invalid(_) => reason::INVALID_EDIT,
        EditProblem::PathNotFound(_) => reason::PATH_NOT_FOUND,
        EditProblem::NotAFile { kind, .. } => {
            add("type", type_name(*kind));
            reason::NOT_A_FILE
        }
        EditProblem::NotADirectory { kind, .. } => {
            add("type", type_name(*kind));
            reason::NOT_A_DIRECTORY
        }
        EditProblem::TextNotFound(_) => reason::TEXT_NOT_FOUND,
        EditProblem::TextNotUnique { occurrences, .. } => {
            add("occurrences", occurrences.to_string());
            reason::TEXT_NOT_UNIQUE
        }
        EditProblem::FileTooLarge { size, .. } => {
            add("size", size.to_string());
            reason::FILE_TOO_LARGE
        }
        EditProblem::TooMuchContent(size) => {
            add("size", size.to_string());
            reason::FILE_TOO_LARGE
        }
        EditProblem::StoreMountPath { mount, .. } => {
            add("mount", mount.clone());
            reason::STORE_MOUNT_PATH
        }
        _ => return None,
    };
    Some((reason, metadata))
}

/// The reason of a lookup that found nothing: a repository or workspace
/// that does not exist (R2.05).
fn db_reason(error: &DbError) -> Option<(&'static str, BTreeMap<String, String>)> {
    let DbError::NotFound { what, id } = error else {
        return None;
    };
    match *what {
        "repository" => Some((
            reef_api::error::REPOSITORY_NOT_FOUND,
            meta(&[("repo", id.clone())]),
        )),
        "workspace" | "workspace containing" | "workspace path" => Some((
            reef_api::error::WORKSPACE_NOT_FOUND,
            meta(&[("workspace", id.clone())]),
        )),
        _ => None,
    }
}

/// The [`DbError`] a `#[error(transparent)]` wrapper holds: its source
/// skips it, so the chain walk never sees it.
fn wrapped_db<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a DbError> {
    if let Some(ManagerError::Db(db)) = error.downcast_ref::<ManagerError>() {
        Some(db)
    } else if let Some(LeaseError::Db(db)) = error.downcast_ref::<LeaseError>() {
        Some(db)
    } else if let Some(RegisterError::Db(db)) = error.downcast_ref::<RegisterError>() {
        Some(db)
    } else if let Some(GcRunError::Db(db)) = error.downcast_ref::<GcRunError>() {
        Some(db)
    } else if let Some(QuiesceError::Db(db)) = error.downcast_ref::<QuiesceError>() {
        Some(db)
    } else if let Some(BackupError::Db(db)) = error.downcast_ref::<BackupError>() {
        Some(db)
    } else if let Some(BaseCacheError::Db(db)) = error.downcast_ref::<BaseCacheError>() {
        Some(db)
    } else if let Some(ReindexError::Db(db)) = error.downcast_ref::<ReindexError>() {
        Some(db)
    } else if let Some(RestoreError::Db(db)) = error.downcast_ref::<RestoreError>() {
        Some(db)
    } else {
        error.downcast_ref::<DbError>()
    }
}

/// The workspace a snapshot before a read or a write failed in: reason
/// `SNAPSHOT_FAILED` (R2.27), which shadows any reason of its cause.
fn snapshot_failed(error: &(dyn Error + 'static)) -> Option<String> {
    if let Some(ManagerError::RewriteSnapshot { id, .. }) = error.downcast_ref::<ManagerError>() {
        return Some(id.as_hex());
    }
    if let Some(crate::read::ReadError::Snapshot { workspace, .. }) =
        error.downcast_ref::<crate::read::ReadError>()
    {
        return Some(workspace.as_hex());
    }
    None
}

fn reason_and_metadata(
    error: &(dyn Error + 'static),
) -> Option<(&'static str, BTreeMap<String, String>)> {
    let mut current = Some(error);
    while let Some(error) = current {
        // `PageError::Token` is transparent: its source skips the token
        // error it wraps.
        if error.downcast_ref::<PageTokenError>().is_some()
            || matches!(error.downcast_ref::<PageError>(), Some(PageError::Token(_)))
        {
            return Some((reef_api::error::INVALID_PAGE_TOKEN, BTreeMap::new()));
        }
        let found = if let Some(workspace) = snapshot_failed(error) {
            Some((
                reef_api::error::SNAPSHOT_FAILED,
                meta(&[("workspace_id", workspace)]),
            ))
        } else if let Some(ManagerError::NotRegistered(path)) = error.downcast_ref::<ManagerError>()
        {
            Some((
                reef_api::error::REPOSITORY_NOT_REGISTERED,
                meta(&[("path", path.display().to_string())]),
            ))
        } else if let Some(db) = wrapped_db(error) {
            db_reason(db)
        } else if let Some(error) = error.downcast_ref::<RevisionError>() {
            revision_reason(error)
        } else if let Some(error) = error.downcast_ref::<OpLookupError>() {
            lookup_reason(error)
        } else if let Some(error) = error.downcast_ref::<PathError>() {
            Some((
                reef_api::error::INVALID_PATH,
                BTreeMap::from([("path".to_owned(), error.path.clone())]),
            ))
        } else if error.downcast_ref::<InvalidAgent>().is_some() {
            Some((reef_api::error::INVALID_AGENT, BTreeMap::new()))
        } else if let Some(error) = error.downcast_ref::<RewriteError>() {
            rewrite_reason(error)
        } else if let Some(error) = error.downcast_ref::<UndoError>() {
            undo_reason(error)
        } else if let Some(error) = error.downcast_ref::<FileError>() {
            file_reason(error)
        } else if let Some(error) = error.downcast_ref::<EditError>() {
            edit_reason(error)
        } else {
            error.downcast_ref::<InvalidChangeFilter>().map(|error| {
                (
                    reef_api::error::INVALID_CHANGE_ID,
                    BTreeMap::from([("change".to_owned(), error.change.clone())]),
                )
            })
        };
        if found.is_some() {
            return found;
        }
        current = error.source();
    }
    None
}

/// How to fix a refused describe or abandon (R2.03).
fn rewrite_advice(error: &RewriteError) -> Option<Advice> {
    match error {
        RewriteError::Mismatch { actual, .. } => Some(Advice::hint(format!(
            "The revision now names {actual}; read it again and retry with that id."
        ))),
        RewriteError::Hidden(_) => Some(Advice::hint(
            "The commit was rewritten since it was read (a snapshot, or another write); read the change again and retry with its current commit id.",
        )),
        RewriteError::Immutable(_) => Some(Advice::hint(
            "Trunk, tags and untracked remote bookmarks are immutable; rewrite a change on top of them instead.",
        )),
        _ => None,
    }
}

/// How to fix a refused read or listing (R2.04).
fn file_advice(error: &FileError) -> Option<Advice> {
    match error {
        FileError::NotAFile { .. } => Some(Advice::hint(
            "ListFiles lists a directory; ReadFile reads the files in it.",
        )),
        FileError::NotADirectory { .. } => Some(Advice::hint(
            "It is not a directory; ReadFile reads it, and ListFiles of its parent lists it.",
        )),
        _ => None,
    }
}

/// How to fix a refused edit (R2.04).
fn edit_advice(error: &EditError) -> Option<Advice> {
    let EditError::Edit { problem, .. } = error else {
        return None;
    };
    match problem {
        EditProblem::NotAFile {
            kind: EntryKind::Directory,
            ..
        } => Some(Advice::hint(
            "Directories are not edited as one; edit or delete the files in it one by one.",
        )),
        EditProblem::NotAFile {
            kind: EntryKind::Conflict,
            ..
        } => Some(Advice::hint(
            "A conflict is resolved by writing the whole file with content.",
        )),
        EditProblem::NotAFile { .. } | EditProblem::NotADirectory { .. } => Some(Advice::hint(
            "Delete it first, in an earlier edit of the same request.",
        )),
        EditProblem::TextNotUnique { .. } => Some(Advice::hint(
            "Include more surrounding text in old_text, or set all.",
        )),
        EditProblem::TextNotFound(_) => Some(Advice::hint(
            "Read the file at the target again, and pass its commit_id as expected_commit_id.",
        )),
        EditProblem::FileTooLarge { .. } | EditProblem::TooMuchContent(_) => Some(Advice::hint(
            "Send the whole file as content, or edit it in a workspace.",
        )),
        EditProblem::StoreMountPath { .. } => Some(Advice::hint(
            "That path is the mount of a shared store declared in reef.toml; only the store's build writes it.",
        )),
        _ => None,
    }
}

/// How to fix a refused undo (R2.03).
fn undo_advice(error: &UndoError) -> Option<Advice> {
    match error {
        UndoError::Superseded { .. } => Some(Advice::hint(
            "Later operations changed what it did; undo them first, newest first (ListOperations with a change filter lists them).",
        )),
        _ => None,
    }
}

/// How to fix a revision that did not resolve (R2.02).
fn revision_advice(error: &RevisionError) -> Option<Advice> {
    match error {
        RevisionError::NotFound { candidates, .. } if !candidates.is_empty() => Some(Advice::hint(
            format!("Did you mean {}?", candidates.join(", ")),
        )),
        RevisionError::Ambiguous { commit_ids, .. } => Some(Advice::hint(format!(
            "It names several commits; name one by commit id: {}.",
            commit_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        RevisionError::WorkspaceRequired { .. } => Some(Advice::hint(
            "`@` is a workspace's working-copy commit; name the workspace in the request.",
        )),
        _ => None,
    }
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
        metadata: reason_metadata(error),
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

    /// R2.05 test 12: the three not-found reasons, with their metadata,
    /// through the transparent wrappers the handlers return.
    #[test]
    fn not_found_reasons_carry_metadata() {
        let reason = |status: &Status| {
            let detail = detail_of(status).unwrap();
            (
                detail.reason,
                detail.metadata.into_iter().collect::<Vec<_>>(),
            )
        };
        let unregistered = status(ManagerError::NotRegistered("/src/app".into()));
        assert_eq!(unregistered.code(), Code::NotFound);
        assert_eq!(
            reason(&unregistered),
            (
                reef_api::error::REPOSITORY_NOT_REGISTERED.to_owned(),
                vec![("path".to_owned(), "/src/app".to_owned())]
            )
        );
        let repo = DbError::NotFound {
            what: "repository",
            id: "abcd".into(),
        };
        assert_eq!(
            reason(&status(ManagerError::Db(repo))),
            (
                reef_api::error::REPOSITORY_NOT_FOUND.to_owned(),
                vec![("repo".to_owned(), "abcd".to_owned())]
            )
        );
        for what in ["workspace", "workspace containing", "workspace path"] {
            let ws = DbError::NotFound {
                what,
                id: "0123".into(),
            };
            assert_eq!(
                reason(&status(LeaseError::Db(ws))),
                (
                    reef_api::error::WORKSPACE_NOT_FOUND.to_owned(),
                    vec![("workspace".to_owned(), "0123".to_owned())]
                ),
                "{what}"
            );
        }
        let other = DbError::NotFound {
            what: "store",
            id: "deps".into(),
        };
        assert_eq!(detail_of(&status(other)).unwrap().reason, "");
    }

    /// R2.27 test 20: both snapshot failures carry `SNAPSHOT_FAILED` and
    /// the workspace, keep their kinds, and the write's keeps its `next`.
    #[test]
    fn snapshot_failures_have_a_reason() {
        let (ws, _) = ids();
        let cause = || {
            Box::new(ManagerError::Db(DbError::Conflict {
                what: "workspace",
                id: ws.as_hex(),
                reason: "is busy",
            }))
        };
        let read = status(crate::read::ReadError::Snapshot {
            workspace: ws,
            source: cause(),
        });
        let write = status(ManagerError::RewriteSnapshot {
            id: ws,
            source: cause(),
        });
        for status in [&read, &write] {
            let detail = detail_of(status).unwrap();
            assert_eq!(detail.reason, reef_api::error::SNAPSHOT_FAILED);
            assert_eq!(
                detail.metadata.into_iter().collect::<Vec<_>>(),
                [("workspace_id".to_owned(), ws.as_hex())]
            );
            assert_eq!(kind_from_status(status), ErrorKind::Conflict);
        }
        assert_eq!(
            advice_in(&write).next,
            ["reef", "ws", "inspect", &ws.as_hex()]
        );
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
        advice_from_detail(&detail_of(status).unwrap()).unwrap()
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
    fn write_reasons_carry_their_metadata() {
        use reef_lib::jj::rewrite::RewriteError;
        use reef_lib::jj::undo::UndoError;
        // (status, code, reason, metadata)
        type Case = (Status, Code, &'static str, Vec<(&'static str, String)>);
        let commit: reef_lib::jj::CommitIdHex = "ab".repeat(20).parse().unwrap();
        let other: reef_lib::jj::CommitIdHex = "cd".repeat(20).parse().unwrap();
        let op: reef_lib::jj::OperationIdHex = "ef".repeat(64).parse().unwrap();
        let cases: Vec<Case> = vec![
            (
                status(ManagerError::Rewrite(RewriteError::InvalidCommitId(
                    "abc".into(),
                ))),
                Code::InvalidArgument,
                reef_api::error::INVALID_COMMIT_ID,
                vec![("commit_id", "abc".into())],
            ),
            (
                status(ManagerError::Rewrite(RewriteError::InvalidDescription)),
                Code::InvalidArgument,
                reef_api::error::INVALID_DESCRIPTION,
                vec![],
            ),
            (
                status(ManagerError::Rewrite(RewriteError::Mismatch {
                    expected: commit.clone(),
                    actual: other.clone(),
                })),
                Code::FailedPrecondition,
                reef_api::error::EXPECTED_COMMIT_MISMATCH,
                vec![
                    ("expected_commit_id", commit.to_string()),
                    ("commit_id", other.to_string()),
                ],
            ),
            (
                status(ManagerError::Rewrite(RewriteError::Immutable(
                    commit.clone(),
                ))),
                Code::FailedPrecondition,
                reef_api::error::IMMUTABLE_REVISION,
                vec![("commit_id", commit.to_string())],
            ),
            (
                status(ManagerError::Rewrite(RewriteError::Hidden(commit.clone()))),
                Code::FailedPrecondition,
                reef_api::error::HIDDEN_REVISION,
                vec![("commit_id", commit.to_string())],
            ),
            (
                status(ManagerError::Undo(UndoError::NotUndoable {
                    op: op.clone(),
                    why: "it is the root operation",
                })),
                Code::InvalidArgument,
                reef_api::error::OPERATION_NOT_UNDOABLE,
                vec![("operation_id", op.to_string())],
            ),
            (
                status(ManagerError::Undo(UndoError::NotInHistory(op.clone()))),
                Code::FailedPrecondition,
                reef_api::error::OPERATION_NOT_IN_HISTORY,
                vec![("operation_id", op.to_string())],
            ),
            (
                status(ManagerError::Undo(UndoError::Superseded {
                    op: op.clone(),
                    what: "bookmark main".into(),
                })),
                Code::FailedPrecondition,
                reef_api::error::OPERATION_SUPERSEDED,
                vec![("operation_id", op.to_string())],
            ),
            (
                status(InvalidAgent),
                Code::InvalidArgument,
                reef_api::error::INVALID_AGENT,
                vec![],
            ),
        ];
        for (status, code, reason, metadata) in cases {
            assert_eq!(status.code(), code, "{reason}");
            let detail = detail_of(&status).unwrap();
            assert_eq!(detail.reason, reason);
            let expected: BTreeMap<String, String> = metadata
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            assert_eq!(detail.metadata, expected, "{reason}");
        }
        // A revision error inside a rewrite keeps R2.02's reason.
        let required = status(ManagerError::Rewrite(RewriteError::Revision(
            RevisionError::Required,
        )));
        assert_eq!(
            detail_of(&required).unwrap().reason,
            reef_api::error::REVISION_REQUIRED
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one case per reason")]
    fn file_reasons_carry_their_metadata() {
        use reef_lib::jj::files::{EditError, EditProblem, FileError};
        // (status, code, reason, metadata beyond `edit_index` and `path`)
        type Case = (
            Status,
            Code,
            &'static str,
            Vec<(&'static str, &'static str)>,
        );
        let commit: reef_lib::jj::CommitIdHex = "ab".repeat(20).parse().unwrap();
        let edit = |problem| {
            status(ManagerError::Edit(EditError::Edit {
                index: 3,
                path: "a/b".into(),
                problem,
            }))
        };
        let cases: Vec<Case> = vec![
            (
                edit(EditProblem::InvalidPath("a/b".into())),
                Code::InvalidArgument,
                reef_api::error::INVALID_PATH,
                vec![],
            ),
            (
                edit(EditProblem::Invalid("why")),
                Code::InvalidArgument,
                reef_api::error::INVALID_EDIT,
                vec![],
            ),
            (
                edit(EditProblem::PathNotFound("a/b".into())),
                Code::NotFound,
                reef_api::error::PATH_NOT_FOUND,
                vec![],
            ),
            (
                edit(EditProblem::NotAFile {
                    path: "a/b".into(),
                    kind: EntryKind::Symlink,
                }),
                Code::FailedPrecondition,
                reef_api::error::NOT_A_FILE,
                vec![("type", "ENTRY_TYPE_SYMLINK")],
            ),
            (
                edit(EditProblem::TextNotUnique {
                    path: "a/b".into(),
                    occurrences: 2,
                }),
                Code::FailedPrecondition,
                reef_api::error::TEXT_NOT_UNIQUE,
                vec![("occurrences", "2")],
            ),
            (
                edit(EditProblem::FileTooLarge {
                    path: "a/b".into(),
                    size: 99,
                }),
                Code::FailedPrecondition,
                reef_api::error::FILE_TOO_LARGE,
                vec![("size", "99")],
            ),
            (
                edit(EditProblem::StoreMountPath {
                    path: "a/b".into(),
                    mount: "a".into(),
                }),
                Code::FailedPrecondition,
                reef_api::error::STORE_MOUNT_PATH,
                vec![("mount", "a")],
            ),
        ];
        for (status, code, reason, extra) in cases {
            assert_eq!(status.code(), code, "{reason}");
            let detail = detail_of(&status).unwrap();
            assert_eq!(detail.reason, reason);
            let expected: BTreeMap<String, String> = [("edit_index", "3"), ("path", "a/b")]
                .into_iter()
                .chain(extra)
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            assert_eq!(detail.metadata, expected, "{reason}");
        }
        // The ancestor, not the edit's own path, for NOT_A_DIRECTORY.
        let ancestor = edit(EditProblem::NotADirectory {
            path: "a".into(),
            kind: EntryKind::File,
        });
        let detail = detail_of(&ancestor).unwrap();
        assert_eq!(detail.reason, reef_api::error::NOT_A_DIRECTORY);
        assert_eq!(detail.metadata["path"], "a");
        assert_ne!(detail.hint, "");
        let many = status(ManagerError::Edit(EditError::TooMany(1001)));
        let detail = detail_of(&many).unwrap();
        assert_eq!(
            (detail.reason.as_str(), detail.metadata.len()),
            (reef_api::error::INVALID_EDIT, 0)
        );
        // Reads name the commit.
        let read = status(FileError::NotAFile {
            path: "d".into(),
            kind: EntryKind::Directory,
            commit: commit.clone(),
        });
        let detail = detail_of(&read).unwrap();
        assert_eq!(read.code(), Code::FailedPrecondition);
        assert_eq!(detail.reason, reef_api::error::NOT_A_FILE);
        assert_eq!(detail.metadata["type"], "ENTRY_TYPE_DIRECTORY");
        assert_eq!(detail.metadata["commit_id"], commit.to_string());
        let path = status(FileError::Path(PathError {
            path: "../x".into(),
        }));
        assert_eq!(
            detail_of(&path).unwrap().reason,
            reef_api::error::INVALID_PATH
        );
    }

    #[test]
    fn errors_without_a_known_fix_carry_none() {
        let status = status(BackendError::RootExists("/ws".into()));
        let detail = detail_of(&status).unwrap();
        assert!(detail.hint.is_empty() && detail.next.is_empty());
        assert_eq!(advice_from_detail(&detail), None);
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
