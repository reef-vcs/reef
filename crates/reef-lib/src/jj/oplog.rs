//! Reading the operation log.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use jj_lib::backend::Timestamp;
use jj_lib::op_store::OpStoreError;
use jj_lib::op_walk::walk_ancestors;

use super::repo::{OperationIdHex, Repo};

/// One operation in the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpEntry {
    /// The operation's id.
    pub id: OperationIdHex,
    /// Parent operations; more than one for a merge of divergent heads.
    pub parents: Vec<OperationIdHex>,
    /// The operation's description, such as `snapshot working copy`.
    pub description: String,
    /// When the operation started.
    pub start: SystemTime,
    /// When the operation ended.
    pub end: SystemTime,
    /// Host recorded by the writer; empty for operations Reef writes until the
    /// daemon sets `operation.hostname`.
    pub hostname: String,
    /// User recorded by the writer; empty for operations Reef writes until the
    /// daemon sets `operation.username`.
    pub username: String,
    /// Whether the operation only snapshotted the working copy.
    pub is_snapshot: bool,
    /// The workspace the operation ran in, if recorded.
    pub workspace: Option<String>,
}

/// Failure to read the operation log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpLogError {
    /// An operation could not be read from the op store.
    #[error("failed to read the operation log")]
    OpStore(#[from] OpStoreError),
}

impl Repo {
    /// Up to `limit` operations from the loaded head back through its
    /// ancestors, newest first by end time.
    pub async fn op_log(&self, limit: usize) -> Result<Vec<OpEntry>, OpLogError> {
        let mut ops =
            std::pin::pin!(walk_ancestors(std::slice::from_ref(self.repo.operation())).take(limit));
        let mut entries = Vec::new();
        while let Some(op) = ops.next().await {
            let op = op?;
            let metadata = op.metadata();
            entries.push(OpEntry {
                id: OperationIdHex::of(op.id()),
                parents: op.parent_ids().iter().map(OperationIdHex::of).collect(),
                description: metadata.description.clone(),
                start: system_time(metadata.time.start),
                end: system_time(metadata.time.end),
                hostname: metadata.hostname.clone(),
                username: metadata.username.clone(),
                is_snapshot: metadata.is_snapshot,
                workspace: metadata
                    .workspace_name
                    .as_ref()
                    .map(|name| name.as_str().to_owned()),
            });
        }
        Ok(entries)
    }
}

fn system_time(timestamp: Timestamp) -> SystemTime {
    let millis = timestamp.timestamp.0;
    let offset = Duration::from_millis(millis.unsigned_abs());
    if millis >= 0 {
        UNIX_EPOCH + offset
    } else {
        UNIX_EPOCH - offset
    }
}
