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
    /// presence and symbol uploads in every registered repository.
    None,
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
    ("/reef.api.v1.OperationService/UndoOperation", Rule::None),
    ("/reef.api.v1.FileService/EditFiles", Rule::None),
    ("/reef.api.v1.PresenceService/ReportPresence", Rule::None),
    ("/reef.api.v1.PresenceService/ClearPresence", Rule::None),
    ("/reef.api.v1.SymbolService/UploadScipDocuments", Rule::None),
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
