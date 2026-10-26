//! The gRPC services, one module per service of `reef.api.v1` (in the
//! `reef-api` crate's `proto/`), plus [`compat`], the frozen v0
//! `Daemon/Status`. Handlers convert between the generated messages and the
//! daemon's own types (in `convert`) and end in
//! `.map_err(crate::error::status)`.
//!
//! The modules of declared RPCs that later tickets implement (`status`,
//! `changes`, `diff`, `operations`, `files`, `presence`) answer every RPC
//! with [`crate::error::not_implemented`] for now.

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

mod convert;

use reef_api::v1;
use reef_lib::error::ErrorChain;
use reef_lib::jj::OperationIdHex;
use tonic::Status;

use crate::daemon::AppState;
use crate::error::status;
use crate::model::RepoId;

/// The operation `op_id` of `repo` that a handler just caused, read back in
/// full; `None` when it recorded none (`op_id` empty or absent).
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
        Ok(entry) => Ok(Some(convert::operation(&entry, repo, written))),
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
