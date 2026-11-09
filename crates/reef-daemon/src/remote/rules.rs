//! What a remote caller with a `write` token may do with each WRITE
//! method (R2.08, ADR 0049), beyond what its scope allows. The gate sees
//! only encoded bytes, so handlers check these, through [`check_create`]
//! and [`check_owned`]. [`TABLE`] names every `ACCESS_WRITE` method, and a
//! test fails when one is missing, so a new WRITE RPC is classified before
//! it ships. Callers on the socket are not affected.

use tonic::Status;

use super::{Caller, RemoteError};
use crate::error::status;
use crate::model::WorkspaceRecord;

/// A WRITE method's rule for remote callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rule {
    /// `path` must be empty: the workspace goes under the daemon's root.
    /// The new row records the caller's token.
    NoPath,
    /// The workspace must have been created by the caller's token.
    OwnWorkspace,
    /// Nothing beyond the scope: a write token writes changes, snapshots,
    /// presence, symbol uploads, evidence and artifacts, and submits jobs,
    /// in every registered repository.
    None,
    /// Session keys (R3.03): only for the operator identity the caller's
    /// token is linked to, never `human`, and none for a token with no
    /// link. The handler resolves the identity from the token
    /// (`crate::identity::issue`).
    LinkedIdentity,
    /// Job cancellation (R3.05): only jobs the caller's token submitted.
    OwnJob,
    /// A claim (R3.05): only a token linked to an operator identity, which
    /// the job records.
    LinkedToken,
    /// A claim's updates and completion (R3.05): only through the token
    /// that claimed the job, even holding its claim token.
    OwnClaim,
    /// A lifecycle transition (R3.09), decided by the handler per target:
    /// any token proposes a draft; only the token that proposed a change
    /// proposes it again, withdraws or supersedes it; nobody declines
    /// (`crate::lifecycle::transition`). An undo of a transition needs the
    /// token that recorded it (`crate::lifecycle::undo`).
    Lifecycle,
}

/// Every `ACCESS_WRITE` method and its rule.
pub(crate) const TABLE: &[(&str, Rule)] = &[
    (
        "/reef.api.v1.WorkspaceService/CreateWorkspace",
        Rule::NoPath,
    ),
    (
        "/reef.api.v1.WorkspaceService/ReleaseWorkspace",
        Rule::OwnWorkspace,
    ),
    (
        "/reef.api.v1.WorkspaceService/MoveWorkspace",
        Rule::OwnWorkspace,
    ),
    (
        "/reef.api.v1.WorkspaceService/RenewLease",
        Rule::OwnWorkspace,
    ),
    (
        "/reef.api.v1.WorkspaceService/SnapshotWorkspace",
        Rule::None,
    ),
    ("/reef.api.v1.ChangeService/DescribeChange", Rule::None),
    ("/reef.api.v1.ChangeService/AbandonChange", Rule::None),
    (
        "/reef.api.v1.ChangeService/TransitionChange",
        Rule::Lifecycle,
    ),
    ("/reef.api.v1.OperationService/UndoOperation", Rule::None),
    ("/reef.api.v1.FileService/EditFiles", Rule::None),
    ("/reef.api.v1.PresenceService/ReportPresence", Rule::None),
    ("/reef.api.v1.PresenceService/ClearPresence", Rule::None),
    ("/reef.api.v1.SymbolService/UploadScipDocuments", Rule::None),
    ("/reef.api.v1.EvidenceService/AddEvidence", Rule::None),
    ("/reef.api.v1.ArtifactService/UploadArtifact", Rule::None),
    (
        "/reef.api.v1.IdentityService/IssueSessionKey",
        Rule::LinkedIdentity,
    ),
    (
        "/reef.api.v1.ProvenanceService/DeclareProvenance",
        Rule::OwnWorkspace,
    ),
    ("/reef.api.v1.ExecutorService/SubmitJob", Rule::None),
    ("/reef.api.v1.ExecutorService/CancelJob", Rule::OwnJob),
    ("/reef.api.v1.ExecutorService/ClaimJob", Rule::LinkedToken),
    ("/reef.api.v1.ExecutorService/UpdateJob", Rule::OwnClaim),
    ("/reef.api.v1.ExecutorService/CompleteJob", Rule::OwnClaim),
];

/// `CreateWorkspace`'s rule: a remote caller cannot name the directory.
pub(crate) fn check_create(caller: Option<&Caller>, path: &str) -> Result<(), Status> {
    match caller {
        Some(_) if !path.is_empty() => Err(status(RemoteError::FieldDenied {
            method: TABLE[0].0.to_owned(),
            field: "path",
        })),
        _ => Ok(()),
    }
}

/// The release, move and renew rule: a remote caller acts only on
/// workspaces its token created.
pub(crate) fn check_owned(caller: Option<&Caller>, record: &WorkspaceRecord) -> Result<(), Status> {
    match caller {
        Some(caller) if record.remote_token_id.as_deref() != Some(caller.token_id.as_str()) => {
            Err(status(RemoteError::WorkspaceNotOwned {
                workspace: record.id.as_hex(),
            }))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use reef_api::v1::Access;

    use super::*;

    // R2.08 test 8.
    #[test]
    fn write_rules_table_is_complete() {
        let writes: BTreeSet<&str> = reef_api::methods()
            .filter(|path| reef_api::access(path) == Some(Access::Write))
            .collect();
        let table: BTreeSet<&str> = TABLE.iter().map(|(path, _)| *path).collect();
        assert_eq!(table, writes, "classify every ACCESS_WRITE method in TABLE");
        assert_eq!(table.len(), TABLE.len());
    }
}
