//! The gRPC services, one module per service of `reef.api.v1` (in the
//! `reef-api` crate's `proto/`), plus [`compat`], the frozen v0
//! `Daemon/Status`. Handlers convert between the generated messages and the
//! daemon's own types (in `convert`) and end in
//! `.map_err(crate::error::status)`.
//!
//! The read RPCs (`status`, `changes`, `diff`, `operations`, and the file
//! reads) run on [`crate::read::Reads`] (R2.02, ADR 0030); the writes
//! (`changes`, undo in `operations`, and `EditFiles`) run through
//! [`crate::workspace::Manager::rewrite`] (R2.03, R2.04). Every declared
//! RPC is implemented; [`crate::error::not_implemented`] stays for RPCs
//! later revisions declare ahead of their tickets.

pub mod changes;
pub mod compat;
pub mod daemon;
pub mod diff;
pub mod files;
pub mod maintenance;
pub mod operations;
pub mod page;
pub mod presence;
pub mod repositories;
pub mod status;
pub mod stores;
pub mod workspaces;

pub(crate) mod convert;
pub(crate) mod op_filter;
pub(crate) mod reading;

/// An entry kind as `reef.api.v1.EntryType` names it, for error metadata
/// (R2.04).
pub(crate) fn entry_type_name(kind: reef_lib::jj::diff::EntryKind) -> &'static str {
    convert::entry_type(Some(kind)).as_str_name()
}

use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::OperationIdHex;
use tonic::Status;
use tonic::metadata::MetadataMap;

use crate::daemon::AppState;
use crate::error::status;
use crate::model::RepoId;

/// A `reef-agent` request metadata value that is not visible ASCII or is
/// too long: reason `INVALID_AGENT` (R2.03).
#[derive(Debug, thiserror::Error)]
#[error("the {key} request metadata must be visible ASCII of at most {max} bytes", key = reef_api::AGENT_METADATA, max = reef_api::AGENT_MAX_BYTES)]
pub struct InvalidAgent;

impl Classify for InvalidAgent {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// The agent a request names in its `reef-agent` metadata, trimmed (R2.03);
/// `None` when absent or empty. Every handler that may record an operation
/// reads it first, before doing anything else.
pub(crate) fn agent_of(metadata: &MetadataMap) -> Result<Option<String>, Status> {
    let Some(value) = metadata.get(reef_api::AGENT_METADATA) else {
        return Ok(None);
    };
    let text = value.to_str().map_err(|_| status(InvalidAgent))?.trim();
    if text.len() > reef_api::AGENT_MAX_BYTES
        || !text.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err(status(InvalidAgent));
    }
    Ok(Some(text.to_owned()).filter(|text| !text.is_empty()))
}

/// The operation `op_id` of `repo` that a handler just caused, read back in
/// full with its effects (R2.02); `None` when it recorded none (`op_id`
/// empty or absent). Effects that cannot be computed are left unset.
///
/// The write has happened by then, so a read-back that fails never fails
/// the RPC: it is logged, and the operation is reported with what the
/// handler knows (its id, kind, workspace and agent) instead of jj's
/// record.
pub(in crate::service) async fn written_operation(
    state: &AppState,
    repo: RepoId,
    op_id: Option<&str>,
    written: convert::Written<'_>,
) -> Result<Option<v1::Operation>, Status> {
    let Some(op_id) = op_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let id: OperationIdHex = op_id.parse().map_err(status)?;
    match state.manager.operation(repo, id).await {
        Ok(entry) => {
            let mut operation = convert::operation(&entry, repo, written);
            if let Ok(handle) = state.manager.handle(repo) {
                operation.effects = reading::effects_of(state, &handle, None, op_id).await;
            }
            Ok(Some(operation))
        }
        Err(error) => {
            tracing::warn!(
                op_id,
                error = %ErrorChain(&error),
                "cannot read back the operation just written; reporting it by id"
            );
            Ok(Some(convert::unread_operation(op_id, repo, written)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(value: &str) -> MetadataMap {
        let mut metadata = MetadataMap::new();
        metadata.insert(reef_api::AGENT_METADATA, value.parse().unwrap());
        metadata
    }

    fn reason(error: &Status) -> String {
        crate::error::detail_of(error).unwrap().reason
    }

    #[test]
    fn agent_is_trimmed_and_optional() {
        assert_eq!(agent_of(&MetadataMap::new()).unwrap(), None);
        assert_eq!(agent_of(&with("  ")).unwrap(), None);
        assert_eq!(
            agent_of(&with(" planner-1 ")).unwrap().as_deref(),
            Some("planner-1")
        );
        let longest = "a".repeat(reef_api::AGENT_MAX_BYTES);
        assert_eq!(agent_of(&with(&longest)).unwrap(), Some(longest));
    }

    #[test]
    fn bad_agents_are_invalid_agent() {
        let long = "a".repeat(reef_api::AGENT_MAX_BYTES + 1);
        let error = agent_of(&with(&long)).unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert_eq!(reason(&error), reef_api::error::INVALID_AGENT);
        // Not visible ASCII: tonic accepts the bytes, `to_str` does not.
        let mut metadata = MetadataMap::new();
        metadata.insert(
            reef_api::AGENT_METADATA,
            tonic::metadata::MetadataValue::try_from("agent\u{e9}".as_bytes()).unwrap(),
        );
        let error = agent_of(&metadata).unwrap_err();
        assert_eq!(reason(&error), reef_api::error::INVALID_AGENT);
    }
}
