//! `reef.api.v1.OperationService`: `ListOperations` and `GetOperation`
//! (R2.02), `UndoOperation` (R2.03, through
//! [`crate::workspace::Manager::rewrite`]), and `SubscribeOperations`
//! (R2.09), which resolves its start here and streams from a
//! [`crate::subscribe`] session.
//!
//! Operation reads never reload the repository and never merge concurrent
//! operation heads: they walk from the heads the op heads store lists. A
//! page token pins those heads, and its cursor is `<examined>:<id>`: how
//! many operations the walk has taken so far and the last one's id, which
//! the next page checks after skipping as many.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use futures::Stream;
use prost::Message as _;
use reef_api::v1::operation_service_server::OperationService;
use reef_api::v1::subscribe_operations_request::Start as StartAt;
use reef_api::v1::{
    self, GetOperationRequest, GetOperationResponse, ListOperationsRequest, ListOperationsResponse,
    SubscribeOperationsRequest, SubscribeOperationsResponse, UndoOperationRequest,
    UndoOperationResponse,
};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::effects::OpEffects;
use reef_lib::jj::oplog::{self, Frontier, OpEntry, OpLookupError};
use reef_lib::jj::{OperationIdHex, ReadonlyRepo};
use tonic::{Request, Response, Status};

use super::convert;
use super::op_filter::OpMatcher;
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{Records, bad_token, target};
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{RepoId, WorkspaceState};
use crate::read::{PinnedGone, Reads, Stop};
use crate::subscribe::feed::LogSource;
use crate::subscribe::session::{self, Position};
use crate::subscribe::{SubscribeError, cursor};

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
    filter: OpMatcher,
    ended_after: Option<SystemTime>,
    ended_before: Option<SystemTime>,
    owners: HashMap<String, String>,
    include_effects: bool,
    stop: Stop,
}

impl Walk {
    /// Whether the filter keeps `entry`, given its effects when the filter
    /// names changes.
    fn keeps(&self, entry: &OpEntry) -> bool {
        if self.ended_before.is_some_and(|before| entry.end >= before) {
            return false;
        }
        self.filter
            .keeps(entry, |id| self.owners.get(id).map(String::as_str))
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
                || (self.filter.names_changes() && computed == MAX_EFFECTS_COMPUTED);
            if page.listed.len() == self.limit || budget_spent || self.stop.requested() {
                page.more = true;
                return ControlFlow::Break(());
            }
            let entry = oplog::entry(operation);
            if self.ended_after.is_some_and(|after| entry.end <= after) {
                // The walk emits in descending end time on one clock.
                return ControlFlow::Break(());
            }
            page.examined += 1;
            page.last = Some(entry.id.to_string());
            if !self.keeps(&entry) {
                return ControlFlow::Continue(());
            }
            let mut effects = None;
            if self.filter.names_changes() {
                computed += 1;
                effects = self.reads.effects(self.repo, &self.head, operation).await;
                if !self.filter.touches(effects.as_ref()) {
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
        let filter = OpMatcher::resolve(
            state,
            &target.repo.id.as_hex(),
            request.filter.unwrap_or_default(),
        )?;
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
            ended_after: system_time(request.ended_after.as_ref()),
            ended_before: system_time(request.ended_before.as_ref()),
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

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn undo_operation(
        &self,
        request: Request<UndoOperationRequest>,
    ) -> Result<Response<UndoOperationResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let write = crate::workspace::Write::Undo {
            operation: request.operation_id,
        };
        let (rewritten, operation) = super::changes::write_change(
            state,
            &target,
            agent.clone(),
            write,
            v1::OperationKind::Undo,
        )
        .await?;
        let workspace_id = target.workspace.as_ref().map(|record| record.id.as_hex());
        let owner = target
            .workspace
            .as_ref()
            .map(|record| record.owner.as_str());
        let snapshot = super::written_operation(
            state,
            target.repo.id,
            rewritten.snapshot_op_id.as_deref(),
            convert::Written {
                kind: v1::OperationKind::Snapshot,
                snapshot: Some(crate::snapshot::SnapshotKind::Explicit),
                workspace: workspace_id.as_deref(),
                agent: agent.as_deref().or(owner).unwrap_or_default(),
            },
        )
        .await?;
        Ok(Response::new(UndoOperationResponse {
            operation,
            snapshot,
            undone_operation_id: rewritten
                .undone
                .map(|id| id.to_string())
                .unwrap_or_default(),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn subscribe_operations(
        &self,
        request: Request<SubscribeOperationsRequest>,
    ) -> Result<Response<Self::SubscribeOperationsStream>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        let limits = state.config.subscribe.clone();
        let named = if request.repos.is_empty() {
            None
        } else {
            let mut ids = Vec::new();
            for repo in &request.repos {
                let id = state.manager.resolve_repo(repo).map_err(status)?.id;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            Some(ids)
        };
        let scope = match named.as_deref() {
            Some([only]) => only.as_hex(),
            _ => String::new(),
        };
        let matcher = Arc::new(OpMatcher::resolve(
            state,
            &scope,
            request.filter.unwrap_or_default(),
        )?);
        let slot = state
            .subscriptions
            .admit(limits.max_subscriptions)
            .map_err(status)?;
        let (given, after_operation) = match request.start {
            None => (BTreeMap::new(), false),
            Some(StartAt::Cursor(text)) => (cursor::decode(&text).map_err(status)?, false),
            Some(StartAt::AfterOperation(text)) => {
                let Some([repo]) = named.as_deref() else {
                    return Err(status(SubscribeError::AfterNeedsOneRepo));
                };
                let found = blocking_read(state, *repo, move |tip| async move {
                    oplog::lookup(&tip, &text).await
                })
                .await?
                .map_err(status)?;
                (BTreeMap::from([(*repo, Frontier::new([found.id]))]), true)
            }
        };
        let followed = named.clone().unwrap_or_else(|| state.events.repos());
        let mut positions = BTreeMap::new();
        for repo in followed {
            // The view before the heads: anything recorded in between
            // shows as a change.
            let Some(view) = state.events.view(repo) else {
                if named.is_some() {
                    return Err(status(SubscribeError::Forgotten(repo)));
                }
                continue;
            };
            let heads = blocking_read(
                state,
                repo,
                |tip| async move { oplog::op_heads(&tip).await },
            )
            .await?
            .map_err(status)?;
            let Some(frontier) = given.get(&repo) else {
                positions.insert(
                    repo,
                    Position {
                        frontier: Frontier::new(heads),
                        upto: None,
                        view: Some(view),
                    },
                );
                continue;
            };
            let (start, upto, behind) = (frontier.clone(), heads.clone(), limits.max_behind);
            let range = blocking_read(state, repo, move |tip| async move {
                oplog::walk_after(&tip, &upto, &start, behind).await
            })
            .await?;
            let gone = |frontier: &Frontier| match frontier.ids() {
                [only] if after_operation => SubscribeError::NotInHistory(only.clone()),
                _ => SubscribeError::Rewritten { repo },
            };
            match range {
                Ok(range) if range.truncated => {
                    return Err(status(SubscribeError::TooFarBehind {
                        repo,
                        limit: limits.max_behind,
                    }));
                }
                Ok(range) if !range.unreached.is_empty() => return Err(status(gone(frontier))),
                Ok(_) => {}
                Err(OpLookupError::NotFound(_)) => return Err(status(gone(frontier))),
                Err(error) => return Err(status(error)),
            }
            positions.insert(
                repo,
                Position {
                    frontier: frontier.clone(),
                    upto: Some(heads),
                    view: Some(view),
                },
            );
        }
        let source = LogSource {
            state: Arc::clone(state),
            matcher,
        };
        let frames = session::spawn(
            source,
            limits,
            state.shutdown.clone(),
            slot,
            session::Start { named, positions },
        );
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(frames),
        )))
    }
}

/// Runs `read` over `repo` as its thread last loaded it, on the blocking
/// pool.
async fn blocking_read<T, F, Fut>(state: &AppState, repo: RepoId, read: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce(Arc<ReadonlyRepo>) -> Fut + Send + 'static,
    Fut: Future<Output = T>,
{
    let handle = state.manager.handle(repo).map_err(status)?;
    let tip = match state.events.tip(repo) {
        Some(tip) => tip,
        None => state.reads.loaded(&handle).await.map_err(status)?,
    };
    state
        .reads
        .run(&handle, move || read(tip))
        .await
        .map_err(status)
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
