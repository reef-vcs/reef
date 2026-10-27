//! What one subscription reads from one repository's log (R2.09): since
//! R2.22 the broker's fallback, for a position its ring does not hold, and
//! [`deliver`], the conversion of one operation the broker's ingest and
//! this read share, so a frame's operations are the same from either.

use std::sync::Arc;

use prost::Message as _;
use reef_api::v1;
use reef_lib::jj::oplog::{self, Frontier, OpEntry, OpLookupError};
use reef_lib::jj::{Operation, OperationIdHex, ReadonlyRepo};
use tonic::Status;

use crate::daemon::AppState;
use crate::error::status;
use crate::model::RepoId;
use crate::service::convert;
use crate::service::op_filter::OpMatcher;
use crate::service::reading::Records;
use crate::subscribe::broker::Delivered;
use crate::subscribe::events::{Follow, RepoView};
use crate::subscribe::session::Source;

/// How much one read may return and compute.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    /// Operations examined, kept or not.
    pub(crate) operations: usize,
    /// Encoded bytes of the kept operations.
    pub(crate) bytes: usize,
    /// Whether the first kept operation goes in whatever its size: true
    /// when the frame has none yet, so one operation is never split.
    pub(crate) first_free: bool,
    /// Operations whose effects are computed.
    pub(crate) effects: usize,
    /// Operations the position may be behind.
    pub(crate) behind: usize,
}

/// What a read found.
#[derive(Debug)]
pub(crate) struct Batch {
    /// Kept, each after its parents, with effects.
    pub(crate) operations: Vec<v1::Operation>,
    /// The position after the last operation examined, kept or not.
    pub(crate) frontier: Frontier,
    /// The budget stopped the read before `upto`.
    pub(crate) more: bool,
}

/// Why a read failed.
#[derive(Debug)]
pub(crate) enum ReadFailure {
    /// More than the budget's `behind` operations follow the position.
    Behind,
    /// The position is gone from the log or not in its history: the log
    /// was rewritten.
    Rewritten,
    /// Anything else, as a status.
    Failed(Status),
}

/// The operations after `after` in `repo`, up to `upto` (the op heads at
/// the subscription's start, while it replays) or the current op heads,
/// that `matcher` keeps, read through `tip`'s stores on the blocking pool
/// under one of the subscriptions' permits.
pub(crate) async fn read(
    state: &Arc<AppState>,
    repo: RepoId,
    tip: Arc<ReadonlyRepo>,
    after: &Frontier,
    upto: Option<&[OperationIdHex]>,
    matcher: &Arc<OpMatcher>,
    budget: Budget,
) -> Result<Batch, ReadFailure> {
    let handle = state
        .repos
        .get(repo)
        .ok_or_else(|| ReadFailure::Failed(Status::unavailable("repository closed")))?;
    let permit = Arc::clone(&state.subscriptions.permits)
        .acquire_owned()
        .await
        .map_err(|_| ReadFailure::Failed(Status::unavailable("shutting down")))?;
    let (state2, after, upto, matcher) = (
        Arc::clone(state),
        after.clone(),
        upto.map(<[OperationIdHex]>::to_vec),
        Arc::clone(matcher),
    );
    state
        .reads
        .run_holding(&handle, permit, move || async move {
            read_blocking(&state2, repo, &tip, after, upto, &matcher, budget).await
        })
        .await
        .map_err(|error| ReadFailure::Failed(status(error)))?
}

async fn read_blocking(
    state: &AppState,
    repo: RepoId,
    tip: &Arc<ReadonlyRepo>,
    after: Frontier,
    upto: Option<Vec<OperationIdHex>>,
    matcher: &OpMatcher,
    budget: Budget,
) -> Result<Batch, ReadFailure> {
    let heads = match upto {
        Some(heads) => heads,
        None => oplog::op_heads(tip)
            .await
            .map_err(|error| ReadFailure::Failed(status(error)))?,
    };
    let range = match oplog::walk_after(tip, &heads, &after, budget.behind).await {
        Ok(range) => range,
        Err(OpLookupError::NotFound(_)) => return Err(ReadFailure::Rewritten),
        Err(error) => return Err(ReadFailure::Failed(status(error))),
    };
    if range.truncated {
        return Err(ReadFailure::Behind);
    }
    if !range.unreached.is_empty() {
        return Err(ReadFailure::Rewritten);
    }
    let mut more = range.operations.len() > budget.operations;
    let examined = &range.operations[..range.operations.len().min(budget.operations)];
    let entries: Vec<OpEntry> = examined.iter().map(oplog::entry).collect();
    let records = Records::of(state, &entries).map_err(ReadFailure::Failed)?;

    let mut batch = Batch {
        operations: Vec::new(),
        frontier: after,
        more: false,
    };
    let (mut bytes, mut computed) = (0, 0);
    for (operation, entry) in examined.iter().zip(&entries) {
        if !matcher.keeps(entry, |id| records.owner(id)) {
            batch.frontier.advance(entry);
            continue;
        }
        if computed == budget.effects {
            more = true;
            break;
        }
        computed += 1;
        let delivered = deliver(state, repo, tip, &records, operation, entry.clone()).await;
        if matcher.names_changes() && !matcher.touches(delivered.effects.as_ref()) {
            batch.frontier.advance(entry);
            continue;
        }
        let first = batch.operations.is_empty() && budget.first_free;
        if !first && bytes + delivered.encoded > budget.bytes {
            more = true;
            break;
        }
        bytes += delivered.encoded;
        batch.operations.push(delivered.message);
        batch.frontier.advance(entry);
    }
    batch.more = more;
    Ok(batch)
}

/// `operation` (whose entry is `entry`) of `repo` converted once, with its
/// effects computed over `tip`'s store through the shared cache: what a
/// frame sends and what a filter reads.
pub(crate) async fn deliver(
    state: &AppState,
    repo: RepoId,
    tip: &Arc<ReadonlyRepo>,
    records: &Records,
    operation: &Operation,
    entry: OpEntry,
) -> Delivered {
    let class = oplog::classify(&entry);
    let owner = class
        .workspace_id
        .as_deref()
        .and_then(|id| records.owner(id))
        .map(str::to_owned);
    let effects = state.reads.effects(repo, tip, operation).await;
    let mut message = records.operation(&entry, repo);
    message.effects = effects.as_ref().map(convert::effects);
    let encoded = message.encoded_len();
    Delivered {
        entry,
        class,
        owner,
        effects,
        message,
        encoded,
    }
}

/// The daemon's operation log as a session's [`Source`].
#[derive(Debug)]
pub(crate) struct LogSource {
    pub(crate) state: Arc<AppState>,
    pub(crate) matcher: Arc<OpMatcher>,
}

impl Source for LogSource {
    type Follow = Follow;

    fn repos(&self) -> Vec<RepoId> {
        self.state.events.repos()
    }

    fn view(&self, repo: RepoId) -> Option<RepoView> {
        self.state.events.view(repo)
    }

    fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.events.changes()
    }

    fn follow(&self, repo: RepoId) -> Follow {
        self.state.events.follow(repo)
    }

    async fn read(
        &self,
        repo: RepoId,
        after: &Frontier,
        upto: Option<&[OperationIdHex]>,
        budget: Budget,
    ) -> Result<Batch, ReadFailure> {
        let tip = self
            .state
            .events
            .tip(repo)
            .ok_or_else(|| ReadFailure::Failed(Status::unavailable("repository closed")))?;
        read(&self.state, repo, tip, after, upto, &self.matcher, budget).await
    }
}
