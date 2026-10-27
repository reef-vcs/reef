//! Reading the operation log, and recognizing the snapshot operations Reef
//! writes (R1.16).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use jj_lib::backend::{BackendError, Timestamp};
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::op_store::{OpStoreError, OperationId};
use jj_lib::op_walk::walk_ancestors;
use jj_lib::operation::Operation;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoaderError};
use jj_lib::transaction::{Transaction, TransactionCommitError};

use super::repo::{ChangeIdHex, CommitIdHex, OperationIdHex, Repo};
use crate::error::{Classify, ErrorKind, classify_by_source};

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
    /// Host recorded by the writer: the machine's hostname for operations
    /// the daemon writes, empty for settings built without a
    /// [`SettingsSpec`](super::settings::SettingsSpec).
    pub hostname: String,
    /// User recorded by the writer: the OS user for operations the daemon
    /// writes, empty for settings built without a
    /// [`SettingsSpec`](super::settings::SettingsSpec).
    pub username: String,
    /// Whether the operation only snapshotted the working copy.
    pub is_snapshot: bool,
    /// The workspace the operation ran in, if recorded.
    pub workspace: Option<String>,
    /// jj's operation attributes (`jj op log -T attributes`), verbatim.
    pub attributes: BTreeMap<String, String>,
}

/// What [`Repo::walk_op_log`] walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpWalk {
    /// Operations visited.
    pub walked: u64,
    /// The limit stopped the walk before the root operation.
    pub truncated: bool,
}

/// A snapshot operation Reef wrote, recognized by its description (the
/// operation itself has no field to mark it with; ADR 0008).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMark {
    /// The Reef workspace id, in hex: from the description, else from the
    /// jj workspace name `reef-<id>`.
    pub workspace_id: String,
    /// Which description it carries.
    pub kind: SnapshotMarkKind,
}

/// The description forms of [`is_reef_snapshot`]. Watcher and explicit
/// snapshots share one form, as do final and shutdown ones, so the daemon
/// keeps the finer kind in its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotMarkKind {
    /// `reef: snapshot workspace <id>`, or any other description on a
    /// snapshot of a `reef-<id>` jj workspace.
    Snapshot,
    /// `reef: final snapshot of workspace <id> (<reason>)`.
    Final,
    /// `reef: move workspace <id> to <change>`.
    Move,
}

/// Prefix of watcher and explicit snapshot descriptions.
const SNAPSHOT_PREFIX: &str = "reef: snapshot workspace ";
/// Prefix of final (and shutdown) snapshot descriptions.
const FINAL_PREFIX: &str = "reef: final snapshot of workspace ";
/// Prefix of move snapshot descriptions.
const MOVE_PREFIX: &str = "reef: move workspace ";
/// Prefix of the jj workspace names of Reef workspaces.
const WORKSPACE_PREFIX: &str = "reef-";

/// Whether `entry` is a snapshot operation of a Reef workspace, and of
/// which one: a snapshot operation (`is_snapshot`) with one of the daemon's
/// descriptions (R1.13), or with any other description recorded in a
/// `reef-<id>` jj workspace. The second covers a snapshot given a custom
/// description through the `Snapshot` RPC; it also covers `jj` run inside a
/// Reef workspace, whose snapshot holds that workspace's local edits as
/// much as one the daemon took. `None` for anything else, such as jj's
/// `snapshot working copy` in the user's `default` workspace.
pub fn is_reef_snapshot(entry: &OpEntry) -> Option<SnapshotMark> {
    if !entry.is_snapshot {
        return None;
    }
    described(&entry.description).or_else(|| {
        let id = entry.workspace.as_deref()?.strip_prefix(WORKSPACE_PREFIX)?;
        is_hex(id).then(|| SnapshotMark {
            workspace_id: id.to_owned(),
            kind: SnapshotMarkKind::Snapshot,
        })
    })
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The mark one of the daemon's snapshot descriptions carries.
fn described(description: &str) -> Option<SnapshotMark> {
    let (workspace_id, kind) = if let Some(id) = description.strip_prefix(SNAPSHOT_PREFIX) {
        (id, SnapshotMarkKind::Snapshot)
    } else if let Some(rest) = description.strip_prefix(FINAL_PREFIX) {
        let (id, reason) = rest.split_once(' ')?;
        let reason = reason.strip_prefix('(')?.strip_suffix(')')?;
        if reason.is_empty() {
            return None;
        }
        (id, SnapshotMarkKind::Final)
    } else {
        let (id, change) = description.strip_prefix(MOVE_PREFIX)?.split_once(" to ")?;
        if change.is_empty() {
            return None;
        }
        (id, SnapshotMarkKind::Move)
    };
    is_hex(workspace_id).then(|| SnapshotMark {
        workspace_id: workspace_id.to_owned(),
        kind,
    })
}

/// Failure to read the operation log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpLogError {
    /// An operation could not be read from the op store.
    #[error("failed to read the operation log")]
    OpStore(#[from] OpStoreError),
    /// An operation's view named a commit that could not be read.
    #[error("failed to read a commit named by the operation log")]
    Commit(#[from] BackendError),
    /// An operation id is not one of this store's.
    #[error("{0} is not an operation id of this repository")]
    InvalidOperationId(OperationIdHex),
    /// The repository could not be loaded at an operation.
    #[error("failed to load the repository at an operation")]
    Load(#[from] RepoLoaderError),
}

impl Classify for OpLogError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::OpStore(error) => classify_by_source(error),
            Self::Commit(error) => classify_by_source(error),
            Self::InvalidOperationId(_) => ErrorKind::InvalidArgument,
            Self::Load(error) => classify_by_source(error),
        }
    }
}

impl Repo {
    /// Up to `limit` operations from the loaded head back through its
    /// ancestors, newest first by end time.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn op_log(&self, limit: usize) -> Result<Vec<OpEntry>, OpLogError> {
        let mut entries = Vec::new();
        self.walk_op_log_unlogged(limit, |entry| entries.push(entry))
            .await
            .inspect_err(|error| crate::log_failure!(error))?;
        Ok(entries)
    }

    /// Calls `visit` with each of up to `limit` operations from the loaded
    /// head back through its ancestors, newest first by end time, and says
    /// whether the limit cut the walk short. Streams, so a long log is never
    /// held in memory.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display()))]
    pub async fn walk_op_log(
        &self,
        limit: usize,
        visit: impl FnMut(OpEntry),
    ) -> Result<OpWalk, OpLogError> {
        self.walk_op_log_unlogged(limit, visit)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn walk_op_log_unlogged(
        &self,
        limit: usize,
        mut visit: impl FnMut(OpEntry),
    ) -> Result<OpWalk, OpLogError> {
        // One past the limit, to tell a log of exactly `limit` operations
        // from a longer one.
        let mut ops = std::pin::pin!(
            walk_ancestors(std::slice::from_ref(self.repo.operation()))
                .take(limit.saturating_add(1))
        );
        let mut walked = 0;
        while let Some(op) = ops.next().await {
            let op = op?;
            if walked == limit {
                return Ok(OpWalk {
                    walked: u64::try_from(walked).unwrap_or(u64::MAX),
                    truncated: true,
                });
            }
            walked += 1;
            visit(entry_of(&op));
        }
        Ok(OpWalk {
            walked: u64::try_from(walked).unwrap_or(u64::MAX),
            truncated: false,
        })
    }

    /// The operation `id` (full hex), which need not be an ancestor of the
    /// loaded head.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), op_id = %id))]
    pub async fn operation(&self, id: &OperationIdHex) -> Result<OpEntry, OpLogError> {
        self.operation_unlogged(id)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn operation_unlogged(&self, id: &OperationIdHex) -> Result<OpEntry, OpLogError> {
        let op_id = OperationId::try_from_hex(id.as_str())
            .ok_or_else(|| OpLogError::InvalidOperationId(id.clone()))?;
        let operation = self.repo.loader().load_operation(&op_id).await?;
        Ok(entry_of(&operation))
    }

    /// The working-copy commit of the jj workspace `workspace` in operation
    /// `op`'s view, and its change; `None` if the view has none for it.
    #[tracing::instrument(skip_all, fields(repo = %self.workspace_root().display(), op_id = %op))]
    pub async fn wc_commit_at(
        &self,
        op: &OperationIdHex,
        workspace: &str,
    ) -> Result<Option<(CommitIdHex, ChangeIdHex)>, OpLogError> {
        self.wc_commit_at_unlogged(op, workspace)
            .await
            .inspect_err(|error| crate::log_failure!(error))
    }

    async fn wc_commit_at_unlogged(
        &self,
        op: &OperationIdHex,
        workspace: &str,
    ) -> Result<Option<(CommitIdHex, ChangeIdHex)>, OpLogError> {
        let id = OperationId::try_from_hex(op.as_str())
            .ok_or_else(|| OpLogError::InvalidOperationId(op.clone()))?;
        let operation = self.repo.loader().load_operation(&id).await?;
        let view = operation.view().await?;
        let Some(commit_id) = view.get_wc_commit_id(WorkspaceName::new(workspace)) else {
            return Ok(None);
        };
        let commit = self.repo.store().get_commit_async(commit_id).await?;
        Ok(Some((
            CommitIdHex::of(commit.id()),
            ChangeIdHex::of(commit.change_id()),
        )))
    }
}

/// The operation attribute naming what Reef wrote an operation for, one of
/// [`OpKind::as_str`]'s values. R2.03 writes it; R2.02 reads it.
pub const ATTR_KIND: &str = "reef.kind";
/// The attribute naming a snapshot's kind: `watcher`, `explicit`, `final`,
/// `move` or `shutdown`.
pub const ATTR_SNAPSHOT_KIND: &str = "reef.snapshot-kind";
/// The attribute naming the Reef workspace an operation was recorded for.
pub const ATTR_WORKSPACE: &str = "reef.workspace";
/// The attribute naming the agent that caused an operation.
pub const ATTR_AGENT: &str = "reef.agent";

/// What an operation did, as Reef classifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// A snapshot of a workspace's files.
    Snapshot,
    /// A workspace was created.
    CreateWorkspace,
    /// A workspace was released or reclaimed.
    ReleaseWorkspace,
    /// A workspace was moved to another change.
    MoveWorkspace,
    /// A change's description was set.
    Describe,
    /// A change was abandoned.
    Abandon,
    /// An operation was undone.
    Undo,
    /// Files were edited without a directory.
    Edit,
    /// Git refs were imported.
    ImportRefs,
    /// Written by another jj client.
    External,
    /// Written by Reef, and none of the above.
    Other,
}

impl OpKind {
    /// Every kind Reef writes in [`ATTR_KIND`] (all but `External`).
    pub const WRITTEN: [OpKind; 10] = [
        OpKind::Snapshot,
        OpKind::CreateWorkspace,
        OpKind::ReleaseWorkspace,
        OpKind::MoveWorkspace,
        OpKind::Describe,
        OpKind::Abandon,
        OpKind::Undo,
        OpKind::Edit,
        OpKind::ImportRefs,
        OpKind::Other,
    ];

    /// The [`ATTR_KIND`] value.
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Snapshot => "snapshot",
            OpKind::CreateWorkspace => "create-workspace",
            OpKind::ReleaseWorkspace => "release-workspace",
            OpKind::MoveWorkspace => "move-workspace",
            OpKind::Describe => "describe",
            OpKind::Abandon => "abandon",
            OpKind::Undo => "undo",
            OpKind::Edit => "edit",
            OpKind::ImportRefs => "import-refs",
            OpKind::External => "external",
            OpKind::Other => "other",
        }
    }

    fn from_attribute(value: &str) -> OpKind {
        OpKind::WRITTEN
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .unwrap_or(OpKind::Other)
    }
}

/// What Reef records about an operation it writes (R2.03): its
/// description and the `reef.*` attributes. Every transaction Reef commits
/// records it through this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpMeta {
    /// The operation's description.
    pub description: String,
    /// [`ATTR_KIND`].
    pub kind: OpKind,
    /// [`ATTR_SNAPSHOT_KIND`], for a snapshot.
    pub snapshot_kind: Option<String>,
    /// [`ATTR_WORKSPACE`]: the Reef workspace id the operation was for.
    pub workspace: Option<String>,
    /// [`ATTR_AGENT`]: who caused it.
    pub agent: Option<String>,
}

impl OpMeta {
    /// An operation of `kind` described by `description`, with no other
    /// attributes.
    pub fn new(kind: OpKind, description: impl Into<String>) -> OpMeta {
        OpMeta {
            description: description.into(),
            kind,
            snapshot_kind: None,
            workspace: None,
            agent: None,
        }
    }

    /// With [`ATTR_SNAPSHOT_KIND`].
    #[must_use]
    pub fn snapshot_kind(mut self, kind: impl Into<String>) -> OpMeta {
        self.snapshot_kind = Some(kind.into());
        self
    }

    /// With [`ATTR_WORKSPACE`].
    #[must_use]
    pub fn workspace(mut self, id: impl Into<String>) -> OpMeta {
        self.workspace = Some(id.into());
        self
    }

    /// With [`ATTR_AGENT`].
    #[must_use]
    pub fn agent(mut self, agent: impl Into<String>) -> OpMeta {
        self.agent = Some(agent.into());
        self
    }

    /// Sets the attributes on `tx`: [`ATTR_KIND`] unless the kind is
    /// [`OpKind::External`] (which stands for another jj client, so it
    /// writes none: tests use it to write as one), the others when present
    /// and not empty.
    pub(crate) fn apply(&self, tx: &mut Transaction) {
        if self.kind != OpKind::External {
            tx.set_attribute(ATTR_KIND.to_owned(), self.kind.as_str().to_owned());
        }
        let optional = [
            (ATTR_SNAPSHOT_KIND, &self.snapshot_kind),
            (ATTR_WORKSPACE, &self.workspace),
            (ATTR_AGENT, &self.agent),
        ];
        for (key, value) in optional {
            if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                tx.set_attribute(key.to_owned(), value.to_owned());
            }
        }
    }

    /// Applies the attributes and commits `tx` with the description.
    pub(crate) async fn commit(
        &self,
        mut tx: Transaction,
    ) -> Result<Arc<ReadonlyRepo>, TransactionCommitError> {
        self.apply(&mut tx);
        tx.commit(self.description.as_str()).await
    }
}

/// How Reef reads an operation it did not just write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpClass {
    /// What it did; `None` for the root operation, which is not classified.
    pub kind: Option<OpKind>,
    /// For a snapshot, its kind as [`ATTR_SNAPSHOT_KIND`] spells it, when
    /// the attribute or the description says (`final`, `shutdown`, `move`);
    /// `None` when not known (watcher and explicit snapshots share a
    /// description).
    pub snapshot_kind: Option<String>,
    /// The Reef workspace it was for: [`ATTR_WORKSPACE`], else jj's
    /// workspace `reef-<id>`, else the id in one of Reef's descriptions.
    pub workspace_id: Option<String>,
}

/// Descriptions Reef writes that are not snapshots, with the kind each
/// means; each is followed by a workspace id (and, for some, more text).
const DESCRIBED: [(&str, OpKind); 4] = [
    ("reef: create workspace ", OpKind::CreateWorkspace),
    ("reef: release workspace ", OpKind::ReleaseWorkspace),
    (MOVE_PREFIX, OpKind::MoveWorkspace),
    ("reef: rematerialize workspace ", OpKind::Other),
];
/// The description of a ref import before a create.
const IMPORT_REFS: &str = "reef: import git refs";
/// The prefix of `reef_lib::jj::undo`'s description.
const UNDO_PREFIX: &str = "undo: restore to operation ";
/// The descriptions of the write RPCs (R2.03), each followed by a full
/// commit or operation id.
const WRITES: [(&str, OpKind); 5] = [
    (DESCRIBE_PREFIX, OpKind::Describe),
    (ABANDON_PREFIX, OpKind::Abandon),
    (REVERT_PREFIX, OpKind::Undo),
    (EDIT_PREFIX, OpKind::Edit),
    (NEW_CHANGE_PREFIX, OpKind::Edit),
];
/// The description of a describe, before the commit id it rewrote.
pub const DESCRIBE_PREFIX: &str = "reef: describe commit ";
/// The description of an abandon, before the commit id it abandoned.
pub const ABANDON_PREFIX: &str = "reef: abandon commit ";
/// The description of an undo, before the operation id it reverted.
pub const REVERT_PREFIX: &str = "reef: undo operation ";
/// The description of an edit that rewrote a commit (R2.04), before the
/// commit id it rewrote.
pub const EDIT_PREFIX: &str = "reef: edit commit ";
/// The description of an edit that created a change (R2.04), before its
/// parent's commit id.
pub const NEW_CHANGE_PREFIX: &str = "reef: new change on ";
/// Every description Reef writes starts so, except jj's own texts.
const REEF_PREFIX: &str = "reef: ";

/// The workspace id at the start of `rest`, up to a space or the end.
fn leading_id(rest: &str) -> Option<String> {
    let id = rest.split(' ').next()?;
    is_hex(id).then(|| id.to_owned())
}

/// Classifies `entry`: by [`ATTR_KIND`], else by the descriptions Reef
/// writes, else [`OpKind::External`]; the root operation is unclassified.
pub fn classify(entry: &OpEntry) -> OpClass {
    let workspace_id = entry
        .attributes
        .get(ATTR_WORKSPACE)
        .filter(|id| is_hex(id))
        .cloned()
        .or_else(|| {
            let id = entry.workspace.as_deref()?.strip_prefix(WORKSPACE_PREFIX)?;
            is_hex(id).then(|| id.to_owned())
        });
    if let Some(kind) = entry.attributes.get(ATTR_KIND) {
        let kind = OpKind::from_attribute(kind);
        return OpClass {
            kind: Some(kind),
            snapshot_kind: (kind == OpKind::Snapshot)
                .then(|| entry.attributes.get(ATTR_SNAPSHOT_KIND).cloned())
                .flatten(),
            workspace_id,
        };
    }
    if let Some(mark) = is_reef_snapshot(entry) {
        let snapshot_kind = match mark.kind {
            SnapshotMarkKind::Snapshot => None,
            SnapshotMarkKind::Final if entry.description.ends_with(" (shutdown)") => {
                Some("shutdown".to_owned())
            }
            SnapshotMarkKind::Final => Some("final".to_owned()),
            SnapshotMarkKind::Move => Some("move".to_owned()),
        };
        return OpClass {
            kind: Some(OpKind::Snapshot),
            snapshot_kind,
            workspace_id: workspace_id.or(Some(mark.workspace_id)),
        };
    }
    let class = |kind, id: Option<String>| OpClass {
        kind: Some(kind),
        snapshot_kind: None,
        workspace_id: workspace_id.clone().or(id),
    };
    let description = entry.description.as_str();
    for (prefix, kind) in DESCRIBED {
        if let Some(rest) = description.strip_prefix(prefix)
            && let Some(id) = leading_id(rest)
        {
            return class(kind, Some(id));
        }
    }
    if description == IMPORT_REFS {
        return class(OpKind::ImportRefs, None);
    }
    if description.starts_with(UNDO_PREFIX) {
        return class(OpKind::Undo, None);
    }
    for (prefix, kind) in WRITES {
        if description.starts_with(prefix) {
            return class(kind, None);
        }
    }
    if description.starts_with(REEF_PREFIX) {
        return class(OpKind::Other, None);
    }
    if entry.parents.is_empty() {
        return OpClass {
            kind: None,
            snapshot_kind: None,
            workspace_id,
        };
    }
    class(OpKind::External, None)
}

/// Failure to find an operation a request names.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpLookupError {
    /// Not lowercase hex, or empty.
    #[error("{0:?} is not an operation id: give a full id or a prefix in lowercase hex")]
    Invalid(String),
    /// No operation has this id or prefix.
    #[error("no operation {0} in this repository")]
    NotFound(String),
    /// Several operations share this prefix.
    #[error("operation id prefix {0} is ambiguous")]
    Ambiguous(String),
    /// The operation could not be read.
    #[error(transparent)]
    OpLog(#[from] OpLogError),
}

impl Classify for OpLookupError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Invalid(_) | Self::Ambiguous(_) => ErrorKind::InvalidArgument,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::OpLog(error) => error.kind(),
        }
    }
}

impl From<OpStoreError> for OpLookupError {
    fn from(error: OpStoreError) -> Self {
        OpLookupError::OpLog(error.into())
    }
}

/// What [`walk_from`] walked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedWalk {
    /// Operations taken from the walk, the skipped ones included.
    pub examined: usize,
    /// The last operation skipped, to check a page token against.
    pub last_skipped: Option<OperationIdHex>,
    /// The walk reached the root operation.
    pub ended: bool,
}

/// The operation `text` names in `repo`'s store: a full id, or a unique
/// prefix of lowercase hex. Any operation qualifies, not only ancestors of
/// the head.
pub async fn resolve_operation(
    repo: &ReadonlyRepo,
    text: &str,
) -> Result<Operation, OpLookupError> {
    let text = text.trim();
    let valid = !text.is_empty() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(OpLookupError::Invalid(text.to_owned()));
    }
    let full = 2 * repo.op_store().root_operation_id().as_bytes().len();
    let id = match text.len().cmp(&full) {
        std::cmp::Ordering::Equal => OperationId::try_from_hex(text)
            .ok_or_else(|| OpLookupError::Invalid(text.to_owned()))?,
        std::cmp::Ordering::Greater => return Err(OpLookupError::NotFound(text.to_owned())),
        std::cmp::Ordering::Less => {
            let prefix = HexPrefix::try_from_hex(text)
                .ok_or_else(|| OpLookupError::Invalid(text.to_owned()))?;
            match repo.op_store().resolve_operation_id_prefix(&prefix).await? {
                PrefixResolution::NoMatch => return Err(OpLookupError::NotFound(text.to_owned())),
                PrefixResolution::AmbiguousMatch => {
                    return Err(OpLookupError::Ambiguous(text.to_owned()));
                }
                PrefixResolution::SingleMatch(id) => id,
            }
        }
    };
    match repo.loader().load_operation(&id).await {
        Ok(operation) => Ok(operation),
        Err(OpStoreError::ObjectNotFound { .. }) => Err(OpLookupError::NotFound(text.to_owned())),
        Err(error) => Err(error.into()),
    }
}

/// [`resolve_operation`] as an [`OpEntry`].
pub async fn lookup(repo: &ReadonlyRepo, text: &str) -> Result<OpEntry, OpLookupError> {
    Ok(entry_of(&resolve_operation(repo, text).await?))
}

/// The operation `repo` is loaded at.
pub fn op_id_of(repo: &ReadonlyRepo) -> OperationIdHex {
    OperationIdHex::of(repo.op_id())
}

/// The current operation heads in `repo`'s store, sorted, read without
/// loading or merging them: more than one while concurrent operations
/// are unmerged.
pub async fn op_heads(repo: &ReadonlyRepo) -> Result<Vec<OperationIdHex>, OpLogError> {
    let heads = repo
        .op_heads_store()
        .get_op_heads()
        .await
        .map_err(|error| OpLogError::Load(error.into()))?;
    let mut heads: Vec<OperationIdHex> = heads.iter().map(OperationIdHex::of).collect();
    heads.sort();
    Ok(heads)
}

/// Walks the operation log from `heads` as `jj op log` does (newest end
/// time first among operations whose children were all listed), skipping
/// `skip` operations, then calling `visit` with each until it breaks.
/// Never loads a repository and never writes.
pub async fn walk_from(
    repo: &ReadonlyRepo,
    heads: &[OperationIdHex],
    skip: usize,
    mut visit: impl AsyncFnMut(&Operation) -> ControlFlow<()>,
) -> Result<PinnedWalk, OpLookupError> {
    let mut ops = Vec::with_capacity(heads.len());
    for head in heads {
        match repo.loader().load_operation(&head.to_id()).await {
            Ok(op) => ops.push(op),
            Err(OpStoreError::ObjectNotFound { .. }) => {
                return Err(OpLookupError::NotFound(head.to_string()));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut stream = std::pin::pin!(walk_ancestors(&ops));
    let mut walk = PinnedWalk {
        examined: 0,
        last_skipped: None,
        ended: true,
    };
    while let Some(op) = stream.next().await {
        let op = op?;
        walk.examined += 1;
        if walk.examined <= skip {
            walk.last_skipped = Some(OperationIdHex::of(op.id()));
            continue;
        }
        if visit(&op).await.is_break() {
            walk.ended = false;
            break;
        }
    }
    Ok(walk)
}

/// Where a reader of the operation log stands (R2.09): the operations up
/// to which it has seen every one, as a set because the log is a graph.
/// Sorted full ids, one unless the log had concurrent heads; an operation
/// is behind the frontier when it is a member or an ancestor of one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Frontier(Vec<OperationIdHex>);

impl Frontier {
    /// A frontier at `ids`.
    pub fn new(ids: impl IntoIterator<Item = OperationIdHex>) -> Frontier {
        let mut ids: Vec<OperationIdHex> = ids.into_iter().collect();
        ids.sort();
        ids.dedup();
        Frontier(ids)
    }

    /// The members, sorted.
    pub fn ids(&self) -> &[OperationIdHex] {
        &self.0
    }

    /// Moves past `entry`, whose parents are behind the frontier or among
    /// the operations already moved past: they leave, it joins. A merge
    /// collapses its parents into one member.
    pub fn advance(&mut self, entry: &OpEntry) {
        self.0.retain(|id| !entry.parents.contains(id));
        if let Err(at) = self.0.binary_search(&entry.id) {
            self.0.insert(at, entry.id.clone());
        }
    }
}

/// What [`walk_after`] found.
#[derive(Clone, Debug)]
pub struct OpRange {
    /// The operations after the frontier, each after its parents.
    pub operations: Vec<Operation>,
    /// Frontier members that are not heads and that no operation in the
    /// range has as a parent: not in the history the heads reach (dropped
    /// from the op heads, or rewritten). Empty when `truncated`.
    pub unreached: Vec<OperationIdHex>,
    /// More than the limit's operations are after the frontier;
    /// `operations` is empty.
    pub truncated: bool,
}

/// An operation ordered by end time, then id, as jj's own walks order
/// them.
struct ByEnd(Operation);

impl ByEnd {
    fn key(&self) -> (i64, &OperationId) {
        (self.0.metadata().time.end.timestamp.0, self.0.id())
    }
}

impl PartialEq for ByEnd {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for ByEnd {}

impl PartialOrd for ByEnd {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByEnd {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// Where [`walk_after`] has got to with one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Queued, not behind the frontier as far as the walk knows.
    Wanted,
    /// Behind the frontier, not walked yet: queued, or only marked.
    Unwanted { queued: bool },
    /// Behind the frontier and walked.
    Behind,
    /// Walked and kept.
    Kept,
}

/// The operations that are ancestors of `heads` (or heads) and not behind
/// `frontier`, each after its parents; with `frontier` empty, every
/// ancestor of `heads`. Walks newest end time first from the heads and the
/// frontier together, marking the frontier's ancestors as it reaches them,
/// and stops when only those are left to walk, so it reads about as many
/// operations as it returns; jj-lib's `walk_ancestors_range` does the same
/// but reads its first chunk without a bound. Past `limit` kept operations
/// it stops with `truncated`. Never loads a repository and never writes.
pub async fn walk_after(
    repo: &ReadonlyRepo,
    heads: &[OperationIdHex],
    frontier: &Frontier,
    limit: usize,
) -> Result<OpRange, OpLookupError> {
    let mut seen: HashMap<OperationId, Seen> = HashMap::new();
    let mut queue = BinaryHeap::new();
    let mut kept: HashMap<OperationId, Operation> = HashMap::new();
    let mut wanted_queued = 0_usize;
    for id in frontier.ids() {
        let op = load_named(repo, id).await?;
        seen.insert(op.id().clone(), Seen::Unwanted { queued: true });
        queue.push(ByEnd(op));
    }
    for id in heads {
        let op = load_named(repo, id).await?;
        if !seen.contains_key(op.id()) {
            seen.insert(op.id().clone(), Seen::Wanted);
            wanted_queued += 1;
            queue.push(ByEnd(op));
        }
    }
    let mut truncated = false;
    while wanted_queued > 0 {
        let Some(ByEnd(op)) = queue.pop() else { break };
        if seen.get(op.id()) == Some(&Seen::Unwanted { queued: true }) {
            seen.insert(op.id().clone(), Seen::Behind);
            for parent in op.parent_ids() {
                unwant(parent, &mut seen, &mut kept, &mut wanted_queued);
            }
        } else {
            wanted_queued -= 1;
            if kept.len() == limit {
                truncated = true;
                break;
            }
            seen.insert(op.id().clone(), Seen::Kept);
            kept.insert(op.id().clone(), op.clone());
        }
        for parent in op.parents().await? {
            match seen.get(parent.id()) {
                None => {
                    seen.insert(parent.id().clone(), Seen::Wanted);
                    wanted_queued += 1;
                    queue.push(ByEnd(parent));
                }
                // Marked through a child, not walked: walk it so its own
                // ancestors get marked.
                Some(Seen::Unwanted { queued: false }) => {
                    seen.insert(parent.id().clone(), Seen::Unwanted { queued: true });
                    queue.push(ByEnd(parent));
                }
                Some(_) => {}
            }
        }
    }
    if truncated {
        return Ok(OpRange {
            operations: Vec::new(),
            unreached: Vec::new(),
            truncated,
        });
    }
    let operations = parents_first(kept);
    let reached: HashSet<&OperationId> = operations
        .iter()
        .flat_map(|op| op.parent_ids().iter())
        .collect();
    let unreached = frontier
        .ids()
        .iter()
        .filter(|id| !heads.contains(id) && !reached.contains(&id.to_id()))
        .cloned()
        .collect();
    Ok(OpRange {
        operations,
        unreached,
        truncated,
    })
}

/// Marks `id` and, if the walk kept them, its kept ancestors as behind the
/// frontier: an operation kept before a later-ending descendant of the
/// frontier reached it (clocks out of step) is taken back.
fn unwant(
    id: &OperationId,
    seen: &mut HashMap<OperationId, Seen>,
    kept: &mut HashMap<OperationId, Operation>,
    wanted_queued: &mut usize,
) {
    let mut pending = vec![id.clone()];
    while let Some(id) = pending.pop() {
        match seen.get(&id).copied() {
            None => {
                seen.insert(id, Seen::Unwanted { queued: false });
            }
            Some(Seen::Wanted) => {
                *wanted_queued -= 1;
                seen.insert(id, Seen::Unwanted { queued: true });
            }
            Some(Seen::Kept) => {
                if let Some(op) = kept.remove(&id) {
                    pending.extend(op.parent_ids().iter().cloned());
                }
                seen.insert(id, Seen::Behind);
            }
            Some(Seen::Unwanted { .. } | Seen::Behind) => {}
        }
    }
}

/// `ops` with every operation after its parents among them, oldest end
/// time first where the graph leaves a choice.
fn parents_first(mut ops: HashMap<OperationId, Operation>) -> Vec<Operation> {
    let mut waiting: HashMap<OperationId, usize> = HashMap::new();
    let mut children: HashMap<OperationId, Vec<OperationId>> = HashMap::new();
    let mut ready = BinaryHeap::new();
    for op in ops.values() {
        let parents = op
            .parent_ids()
            .iter()
            .filter(|parent| ops.contains_key(*parent))
            .count();
        for parent in op.parent_ids().iter().filter(|p| ops.contains_key(*p)) {
            children
                .entry(parent.clone())
                .or_default()
                .push(op.id().clone());
        }
        if parents == 0 {
            ready.push(Reverse(ByEnd(op.clone())));
        } else {
            waiting.insert(op.id().clone(), parents);
        }
    }
    let mut ordered = Vec::with_capacity(ops.len());
    while let Some(Reverse(ByEnd(op))) = ready.pop() {
        for child in children.remove(op.id()).unwrap_or_default() {
            if let Some(count) = waiting.get_mut(&child) {
                *count -= 1;
                if *count == 0 {
                    waiting.remove(&child);
                    if let Some(child) = ops.remove(&child) {
                        ready.push(Reverse(ByEnd(child)));
                    }
                }
            }
        }
        ops.remove(op.id());
        ordered.push(op);
    }
    ordered
}

/// The operation `id` names, which must exist.
async fn load_named(repo: &ReadonlyRepo, id: &OperationIdHex) -> Result<Operation, OpLookupError> {
    match repo.loader().load_operation(&id.to_id()).await {
        Ok(op) => Ok(op),
        Err(OpStoreError::ObjectNotFound { .. }) => Err(OpLookupError::NotFound(id.to_string())),
        Err(error) => Err(error.into()),
    }
}

/// `repo`'s store loaded at `operation`, with that operation's view and an
/// index covering it.
pub async fn load_at(
    repo: &ReadonlyRepo,
    operation: &Operation,
) -> Result<Arc<ReadonlyRepo>, OpLogError> {
    Ok(repo.loader().load_at(operation).await?)
}

/// `operation` as an [`OpEntry`].
pub fn entry(operation: &Operation) -> OpEntry {
    entry_of(operation)
}

fn entry_of(op: &Operation) -> OpEntry {
    let metadata = op.metadata();
    OpEntry {
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
        attributes: metadata.attributes.clone(),
    }
}

/// A jj timestamp as a [`SystemTime`].
pub(crate) fn system_time(timestamp: Timestamp) -> SystemTime {
    let millis = timestamp.timestamp.0;
    let offset = Duration::from_millis(millis.unsigned_abs());
    if millis >= 0 {
        UNIX_EPOCH + offset
    } else {
        UNIX_EPOCH - offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef";

    fn entry(description: &str, is_snapshot: bool) -> OpEntry {
        in_workspace(description, is_snapshot, "default")
    }

    fn in_workspace(description: &str, is_snapshot: bool, workspace: &str) -> OpEntry {
        OpEntry {
            id: "00".parse().unwrap(),
            parents: Vec::new(),
            description: description.to_owned(),
            start: UNIX_EPOCH,
            end: UNIX_EPOCH,
            hostname: String::new(),
            username: String::new(),
            is_snapshot,
            workspace: Some(workspace.to_owned()),
            attributes: BTreeMap::new(),
        }
    }

    fn child(id: &str, parents: &[&str]) -> OpEntry {
        OpEntry {
            id: id.parse().unwrap(),
            parents: parents.iter().map(|p| p.parse().unwrap()).collect(),
            ..entry("op", false)
        }
    }

    fn ids(frontier: &Frontier) -> Vec<&str> {
        frontier.ids().iter().map(OperationIdHex::as_str).collect()
    }

    #[test]
    fn frontiers_advance() {
        let mut frontier = Frontier::new(["bb".parse().unwrap(), "aa".parse().unwrap()]);
        assert_eq!(ids(&frontier), ["aa", "bb"], "sorted");
        // A branch from an older operation adds a member.
        frontier.advance(&child("cc", &["00"]));
        assert_eq!(ids(&frontier), ["aa", "bb", "cc"]);
        frontier.advance(&child("dd", &["aa"]));
        assert_eq!(ids(&frontier), ["bb", "cc", "dd"]);
        // A merge collapses its parents.
        frontier.advance(&child("ee", &["bb", "cc", "dd"]));
        assert_eq!(ids(&frontier), ["ee"]);
        frontier.advance(&child("ee", &["bb"]));
        assert_eq!(ids(&frontier), ["ee"], "no duplicates");
    }

    fn mark(description: &str) -> Option<SnapshotMarkKind> {
        let mark = is_reef_snapshot(&entry(description, true))?;
        assert_eq!(mark.workspace_id, ID, "{description}");
        Some(mark.kind)
    }

    #[test]
    fn snapshot_descriptions_are_recognized() {
        assert_eq!(
            mark(&format!("reef: snapshot workspace {ID}")),
            Some(SnapshotMarkKind::Snapshot)
        );
        for reason in ["released", "lease expired", "shutdown"] {
            assert_eq!(
                mark(&format!(
                    "reef: final snapshot of workspace {ID} ({reason})"
                )),
                Some(SnapshotMarkKind::Final),
                "{reason}"
            );
        }
        assert_eq!(
            mark(&format!("reef: move workspace {ID} to 0a1b2c3d")),
            Some(SnapshotMarkKind::Move)
        );
    }

    #[test]
    fn other_operations_are_not_reef_snapshots() {
        for description in [
            "snapshot working copy",
            "reef: create workspace 0123456789abcdef",
            "reef: release workspace 0123456789abcdef",
            "reef: snapshot workspace ",
            "reef: snapshot workspace 0123456789abcdef extra",
            "reef: final snapshot of workspace 0123456789abcdef",
            "reef: final snapshot of workspace 0123456789abcdef ()",
            "reef: move workspace 0123456789abcdef to ",
            "reef: move workspace 0123456789abcdef",
            "custom description",
        ] {
            assert_eq!(
                is_reef_snapshot(&entry(description, true)),
                None,
                "{description:?}"
            );
        }
        // The right description on an operation that is not a snapshot.
        let description = format!("reef: snapshot workspace {ID}");
        assert_eq!(is_reef_snapshot(&entry(&description, false)), None);
        // Not a Reef workspace's name.
        for workspace in ["reef-", "reef-xyz", "agent-0123456789abcdef"] {
            let op = in_workspace("custom description", true, workspace);
            assert_eq!(is_reef_snapshot(&op), None, "{workspace}");
        }
    }

    // A custom description given through the `Snapshot` RPC, and `jj` run
    // inside a Reef workspace: the workspace name marks them.
    #[test]
    fn reef_workspace_snapshots_are_recognized_by_name() {
        let name = format!("reef-{ID}");
        for description in ["wip before refactor", "snapshot working copy"] {
            let mark = is_reef_snapshot(&in_workspace(description, true, &name)).unwrap();
            assert_eq!(mark.workspace_id, ID);
            assert_eq!(mark.kind, SnapshotMarkKind::Snapshot);
        }
        assert_eq!(
            is_reef_snapshot(&in_workspace("wip", false, &name)),
            None,
            "not a snapshot"
        );
        // The description's own form still sets the kind.
        let fin = format!("reef: final snapshot of workspace {ID} (released)");
        let mark = is_reef_snapshot(&in_workspace(&fin, true, &name)).unwrap();
        assert_eq!(mark.kind, SnapshotMarkKind::Final);
    }

    #[test]
    fn write_descriptions_classify_without_attributes() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        for (description, kind) in [
            (format!("{DESCRIBE_PREFIX}{commit}"), OpKind::Describe),
            (format!("{ABANDON_PREFIX}{commit}"), OpKind::Abandon),
            (format!("{REVERT_PREFIX}{commit}"), OpKind::Undo),
            (format!("{EDIT_PREFIX}{commit}"), OpKind::Edit),
            (format!("{NEW_CHANGE_PREFIX}{commit}"), OpKind::Edit),
        ] {
            let mut op = entry(&description, false);
            op.workspace = None;
            op.parents = vec!["01".parse().unwrap()];
            let class = classify(&op);
            assert_eq!(class.kind, Some(kind), "{description}");
            assert_eq!(class.workspace_id, None, "{description}");
        }
    }
}
