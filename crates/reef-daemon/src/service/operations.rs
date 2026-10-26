//! `reef.api.v1.OperationService`: `ListOperations` and `GetOperation`
//! (R2.02). `UndoOperation` (R2.03) and `SubscribeOperations` (R2.09)
//! answer `UNIMPLEMENTED` with reason `NOT_IMPLEMENTED`.
//!
//! Operation reads never reload the repository and never merge concurrent
//! operation heads: they walk from the heads the op heads store lists. A
//! page token pins those heads, and its cursor is `<examined>:<id>`: how
//! many operations the walk has taken so far and the last one's id, which
//! the next page checks after skipping as many.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use futures::Stream;
use prost::Message as _;
use reef_api::v1::operation_service_server::OperationService;
use reef_api::v1::{
    self, GetOperationRequest, GetOperationResponse, ListOperationsRequest, ListOperationsResponse,
    SubscribeOperationsRequest, SubscribeOperationsResponse, UndoOperationRequest,
    UndoOperationResponse,
};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::effects::OpEffects;
use reef_lib::jj::oplog::{self, ATTR_AGENT, OpEntry, OpLookupError};
use reef_lib::jj::{OperationIdHex, ReadonlyRepo};
use tonic::{Request, Response, Status};

use super::convert::{self, known};
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{Records, bad_token, target};
use crate::daemon::AppState;
use crate::error::{not_implemented, status};
use crate::model::{RepoId, WorkspaceState};
use crate::read::{PinnedGone, Reads, Stop};

/// Operations a page examines at most.
const MAX_EXAMINED: usize = 10_000;
/// Uncached effects a page computes at most for a `changes` filter.
const MAX_EFFECTS_COMPUTED: usize = 200;

const LIST_OPERATIONS: &str = "/reef.api.v1.OperationService/ListOperations";

/// Serves `OperationService`.
#[derive(Debug)]
pub struct OperationsHandler {
    state: Arc<AppState>,
}

impl OperationsHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> OperationsHandler {
        OperationsHandler { state }
    }
}

type SubscribeOperationsStream =
    Pin<Box<dyn Stream<Item = Result<SubscribeOperationsResponse, Status>> + Send>>;

/// A `filter.changes` value that is not a change id prefix: reason
/// `INVALID_CHANGE_ID`.
#[derive(Debug, thiserror::Error)]
#[error("{change:?} is not a change id prefix in hex or reverse hex")]
pub struct InvalidChangeFilter {
    /// The value.
    pub change: String,
}

impl Classify for InvalidChangeFilter {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// A change id prefix in hex: hex as given, reverse hex (`z` to `k`)
/// converted; `None` for anything else.
pub(super) fn change_prefix(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Some(text.to_owned());
    }
    text.bytes()
        .map(|b| match b {
            b'k'..=b'z' => char::from_digit(u32::from(b'z' - b), 16),
            _ => None,
        })
        .collect()
}

/// A `google.protobuf.Timestamp` as a [`SystemTime`]; `None` when unset or
/// out of range.
fn system_time(timestamp: Option<&prost_types::Timestamp>) -> Option<SystemTime> {
    let timestamp = timestamp?;
    let nanos = std::time::Duration::from_nanos(u64::try_from(timestamp.nanos).ok()?);
    let seconds = std::time::Duration::from_secs(timestamp.seconds.unsigned_abs());
    let whole = if timestamp.seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(seconds)?
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(seconds)?
    };
    whole.checked_add(nanos)
}

/// A request's `OperationFilter` and time bounds, resolved.
#[derive(Debug, Default)]
struct Filter {
    workspaces: HashSet<String>,
    changes: Vec<String>,
    agents: HashSet<String>,
    kinds: HashSet<i32>,
    ended_after: Option<SystemTime>,
    ended_before: Option<SystemTime>,
}

/// One operation a page returns.
struct Listed {
    entry: OpEntry,
    effects: Option<OpEffects>,
    /// Operations the walk had taken when it took this one.
    examined: usize,
}

/// What the blocking walk found.
struct WalkPage {
    listed: Vec<Listed>,
    /// Operations taken in all, skipped ones included.
    examined: usize,
    /// The last one taken.
    last: Option<String>,
    /// More operations follow.
    more: bool,
    /// The last operation skipped, to check the token against.
    last_skipped: Option<OperationIdHex>,
}

/// One page's walk, run on the blocking pool.
struct Walk {
    reads: Arc<Reads>,
    repo: RepoId,
    head: Arc<ReadonlyRepo>,
    heads: Vec<OperationIdHex>,
    skip: usize,
    limit: usize,
    filter: Filter,
    owners: HashMap<String, String>,
    include_effects: bool,
    stop: Stop,
}

impl Walk {
    /// Whether the filter keeps `entry`, given its effects when the filter
    /// names changes.
    fn keeps(&self, entry: &OpEntry) -> bool {
        let filter = &self.filter;
        if filter
            .ended_before
            .is_some_and(|before| entry.end >= before)
        {
            return false;
        }
        let class = oplog::classify(entry);
        if !filter.kinds.is_empty()
            && !filter
                .kinds
                .contains(&i32::from(convert::operation_kind(class.kind)))
        {
            return false;
        }
        let workspace = class.workspace_id.as_deref();
        if !filter.workspaces.is_empty()
            && !workspace.is_some_and(|id| filter.workspaces.contains(id))
        {
            return false;
        }
        if !filter.agents.is_empty() {
            let agent = entry
                .attributes
                .get(ATTR_AGENT)
                .or_else(|| workspace.and_then(|id| self.owners.get(id)));
            if !agent.is_some_and(|agent| filter.agents.contains(agent)) {
                return false;
            }
        }
        true
    }

    fn touches(&self, effects: Option<&OpEffects>) -> bool {
        effects.is_some_and(|effects| {
            self.filter
                .changes
                .iter()
                .any(|prefix| effects.touches(prefix))
        })
    }

    async fn run(self) -> Result<WalkPage, OpLookupError> {
        let mut page = WalkPage {
            listed: Vec::new(),
            examined: self.skip,
            last: None,
            more: false,
            last_skipped: None,
        };
        let mut computed = 0;
        let walk = oplog::walk_from(&self.head, &self.heads, self.skip, async |operation| {
            let budget_spent = page.examined - self.skip >= MAX_EXAMINED
                || (!self.filter.changes.is_empty() && computed == MAX_EFFECTS_COMPUTED);
            if page.listed.len() == self.limit || budget_spent || self.stop.requested() {
                page.more = true;
                return ControlFlow::Break(());
            }
            let entry = oplog::entry(operation);
            if self
                .filter
                .ended_after
                .is_some_and(|after| entry.end <= after)
            {
                // The walk emits in descending end time on one clock.
                return ControlFlow::Break(());
            }
            page.examined += 1;
            page.last = Some(entry.id.to_string());
            if !self.keeps(&entry) {
                return ControlFlow::Continue(());
            }
            let mut effects = None;
            if !self.filter.changes.is_empty() {
                computed += 1;
                effects = self.reads.effects(self.repo, &self.head, operation).await;
                if !self.touches(effects.as_ref()) {
                    return ControlFlow::Continue(());
                }
            }
            if self.include_effects && effects.is_none() {
                effects = self.reads.effects(self.repo, &self.head, operation).await;
            }
            page.listed.push(Listed {
                entry,
                effects: effects.filter(|_| self.include_effects),
                examined: page.examined,
            });
            ControlFlow::Continue(())
        })
        .await?;
        page.last_skipped = walk.last_skipped;
        Ok(page)
    }
}

/// Where a page of the log starts.
struct Start {
    heads: Vec<OperationIdHex>,
    skip: usize,
    /// The id the skip must end on; empty when nothing is skipped.
    expected: String,
}

impl Start {
    fn of_token(pinned: &str, cursor: &str) -> Result<Start, Status> {
        let heads = pinned
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<OperationIdHex>, _>>()
            .map_err(|_| bad_token())?;
        let (skip, expected) = cursor.split_once(':').ok_or_else(bad_token)?;
        Ok(Start {
            heads,
            skip: skip.parse().map_err(|_| bad_token())?,
            expected: expected.to_owned(),
        })
    }
}

#[tonic::async_trait]
impl OperationService for OperationsHandler {
    type SubscribeOperationsStream = SubscribeOperationsStream;

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_operations(
        &self,
        request: Request<ListOperationsRequest>,
    ) -> Result<Response<ListOperationsResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_OPERATIONS,
            request.page_size,
            &request.page_token,
            &ListOperationsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let state = &self.state;
        let target = target(state, &request.repo, "")?;
        let wanted = request.filter.unwrap_or_default();
        let mut filter = Filter {
            agents: wanted.agents.into_iter().collect(),
            ended_after: system_time(request.ended_after.as_ref()),
            ended_before: system_time(request.ended_before.as_ref()),
            ..Filter::default()
        };
        for workspace in &wanted.workspaces {
            let record = state
                .manager
                .resolve_workspace(&target.repo.id.as_hex(), workspace)
                .map_err(status)?;
            filter.workspaces.insert(record.id.as_hex());
        }
        for change in wanted.changes {
            let prefix =
                change_prefix(&change).ok_or_else(|| status(InvalidChangeFilter { change }))?;
            filter.changes.push(prefix);
        }
        for kind in wanted.kinds {
            let kind: v1::OperationKind = known("filter.kinds", kind).map_err(status)?;
            filter.kinds.insert(kind.into());
        }
        let owners: HashMap<String, String> = state
            .manager
            .list(Some(target.repo.id), &[] as &[WorkspaceState], None)
            .map_err(status)?
            .into_iter()
            .map(|(record, _)| (record.id.as_hex(), record.owner))
            .collect();
        let head = state.reads.loaded(&target.handle).await.map_err(status)?;
        let start = if let (Some(pinned), Some(cursor)) = (page.pinned(), page.cursor()) {
            Start::of_token(pinned, cursor)?
        } else {
            let walk_head = Arc::clone(&head);
            let heads = state
                .reads
                .run(&target.handle, move || async move {
                    oplog::op_heads(&walk_head).await
                })
                .await
                .map_err(status)?
                .map_err(status)?;
            Start {
                heads,
                skip: 0,
                expected: String::new(),
            }
        };
        let pinned = start
            .heads
            .iter()
            .map(OperationIdHex::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let (stop, _guard) = state.reads.stop();
        let walk = Walk {
            reads: Arc::clone(&state.reads),
            repo: target.repo.id,
            head,
            heads: start.heads,
            skip: start.skip,
            limit: page.size(),
            filter,
            owners,
            include_effects: request.include_effects,
            stop,
        };
        let found = state
            .reads
            .run(&target.handle, move || walk.run())
            .await
            .map_err(status)?
            .map_err(|error| match error {
                OpLookupError::NotFound(_) => status(PinnedGone(error)),
                error => status(error),
            })?;
        let skipped = found.last_skipped.as_ref().map(OperationIdHex::as_str);
        if start.skip > 0 && skipped != Some(start.expected.as_str()) {
            return Err(bad_token());
        }

        let entries: Vec<OpEntry> = found
            .listed
            .iter()
            .map(|listed| listed.entry.clone())
            .collect();
        let records = Records::of(state, &entries)?;
        let mut operations = Vec::new();
        let mut bytes = 0;
        // Past the byte budget the page ends at the last operation kept;
        // otherwise at the last one the walk took.
        let mut kept = None;
        let mut trimmed = false;
        for listed in found.listed {
            let mut operation = records.operation(&listed.entry, target.repo.id);
            operation.effects = listed.effects.as_ref().map(convert::effects);
            let size = operation.encoded_len();
            if !operations.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            kept = Some((listed.examined, listed.entry.id.to_string()));
            operations.push(operation);
        }
        let cursor = if trimmed {
            kept
        } else {
            found.last.map(|last| (found.examined, last))
        };
        let next_page_token = match cursor {
            _ if !(found.more || trimmed) => String::new(),
            Some((examined, last)) => page.token_at(&format!("{examined}:{last}"), &pinned),
            None => page.token_at(&format!("{}:", start.skip), &pinned),
        };
        Ok(Response::new(ListOperationsResponse {
            operations,
            next_page_token,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_operation(
        &self,
        request: Request<GetOperationRequest>,
    ) -> Result<Response<GetOperationResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, "")?;
        let head = state.reads.loaded(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let repo = target.repo.id;
        let text = request.operation_id;
        let (entry, effects) = state
            .reads
            .run(&target.handle, move || async move {
                let operation = oplog::resolve_operation(&head, &text).await?;
                let effects = reads.effects(repo, &head, &operation).await;
                Ok::<_, OpLookupError>((oplog::entry(&operation), effects))
            })
            .await
            .map_err(status)?
            .map_err(status)?;
        let records = Records::of(state, std::slice::from_ref(&entry))?;
        let mut operation = records.operation(&entry, repo);
        operation.effects = effects.as_ref().map(convert::effects);
        Ok(Response::new(GetOperationResponse {
            operation: Some(operation),
        }))
    }

    async fn undo_operation(
        &self,
        _request: Request<UndoOperationRequest>,
    ) -> Result<Response<UndoOperationResponse>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/UndoOperation",
        ))
    }

    async fn subscribe_operations(
        &self,
        _request: Request<SubscribeOperationsRequest>,
    ) -> Result<Response<Self::SubscribeOperationsStream>, Status> {
        Err(not_implemented(
            "/reef.api.v1.OperationService/SubscribeOperations",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_prefixes_take_both_alphabets() {
        assert_eq!(change_prefix("0a1b").as_deref(), Some("0a1b"));
        assert_eq!(change_prefix("zyxk").as_deref(), Some("012f"));
        assert_eq!(change_prefix(""), None);
        assert_eq!(change_prefix("xyz9"), None, "mixed alphabets");
        assert_eq!(change_prefix("ABCD"), None);
    }

    #[test]
    fn timestamps_become_system_times() {
        let at = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 5,
        };
        assert_eq!(
            system_time(Some(&at)),
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 5))
        );
        assert_eq!(system_time(None), None);
    }
}
