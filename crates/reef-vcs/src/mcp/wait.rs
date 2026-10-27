//! `wait_for_operations`: "subscriptions as a wait tool" over what exists.
//! `SubscribeOperations` is declared but not implemented (R2.09), so the
//! tool polls `ListOperations` every [`POLL`], over a window that starts at
//! a baseline operation's end and moves up to the log's head while nothing
//! matches, so each poll reads only the last half second. When R2.09 lands
//! it reads the stream instead, with the same inputs and result.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reef_api::v1::{ListOperationsRequest, OperationFilter};
use reef_client::json::{self, millis_of};
use rmcp::model::ProgressNotificationParam;
use serde_json::{Map, Value};
use tokio::time::Instant;

use super::results::Waited;
use super::tools::filter;
use super::{Call, ToolError, args, parse, to_value};
use crate::cli::repo::missing;
use crate::cli::{CliError, Ctx};

/// How often the log is read.
const POLL: Duration = Duration::from_millis(500);
/// How often a waiting call with a progress token reports progress.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// `timeout_seconds` when not given, and its largest value.
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 300;

pub(super) fn run<'a>(
    call: &'a Call<'a>,
    arguments: Map<String, Value>,
) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>> {
    Box::pin(async move {
        let args: args::WaitForOperations = parse(arguments)?;
        let timeout = args.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT);
        if timeout > MAX_TIMEOUT {
            return Err(ToolError::invalid(
                "",
                format!("timeout_seconds {timeout} is over {MAX_TIMEOUT}"),
                "Wait at most 300 seconds per call, and call again with last_operation_id as after_operation.",
            ));
        }
        let scope = call.scope(args.repo.as_ref(), None).await?;
        let filter = filter(args.workspaces, args.changes, args.agents, args.kinds)?;
        let mut log = Log {
            ctx: call.ctx,
            repo: scope.repo_id,
            filter,
            include_effects: args.include_effects.unwrap_or(false),
        };
        to_value(
            &wait(
                call,
                &mut log,
                args.after_operation,
                Duration::from_secs(timeout),
            )
            .await?,
        )
    })
}

/// The repository's log, as the wait reads it.
struct Log<'a> {
    ctx: &'a Ctx,
    repo: String,
    filter: OperationFilter,
    include_effects: bool,
}

/// An operation's end, in milliseconds since the epoch.
fn ended(operation: &reef_api::v1::Operation) -> i64 {
    operation.ended_at.as_ref().map_or(0, millis_of)
}

fn timestamp(millis: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: millis.div_euclid(1000),
        nanos: i32::try_from(millis.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

impl Log<'_> {
    /// The newest operation, matching or not.
    async fn head(&self) -> Result<Option<reef_api::v1::Operation>, CliError> {
        let response = self
            .ctx
            .call(self.ctx.client.list_operations_page(ListOperationsRequest {
                repo: self.repo.clone(),
                page_size: 1,
                ..ListOperationsRequest::default()
            }))
            .await?;
        Ok(response.operations.into_iter().next())
    }

    /// Every matching operation that ended at or after `from` (and before
    /// `before`, when given), newest first. The millisecond of overlap
    /// catches operations that ended in the same millisecond as `from`'s.
    async fn since(
        &self,
        from: i64,
        before: Option<i64>,
    ) -> Result<Vec<reef_api::v1::Operation>, CliError> {
        let mut request = ListOperationsRequest {
            repo: self.repo.clone(),
            filter: Some(self.filter.clone()),
            ended_after: Some(timestamp(from - 1)),
            ended_before: before.map(timestamp),
            include_effects: self.include_effects,
            page_size: 1000,
            page_token: String::new(),
        };
        let mut operations = Vec::new();
        loop {
            let response = self
                .ctx
                .call(self.ctx.client.list_operations_page(request.clone()))
                .await?;
            operations.extend(response.operations);
            if response.next_page_token.is_empty() {
                return Ok(operations);
            }
            request.page_token = response.next_page_token;
        }
    }
}

async fn wait(
    call: &Call<'_>,
    log: &mut Log<'_>,
    after: Option<String>,
    timeout: Duration,
) -> Result<Waited, ToolError> {
    let started = Instant::now();
    let deadline = started + timeout;
    let baseline = match after {
        Some(id) => {
            let response = log
                .ctx
                .call(log.ctx.client.get_operation(&log.repo, &id))
                .await?;
            Some(response.operation.ok_or_else(|| missing("operation"))?)
        }
        None => log.head().await?,
    };
    let mut from = baseline.as_ref().map_or(0, ended);
    // What is not new: the baseline, and matching operations that ended in
    // its millisecond, which a wait from the head has already seen.
    let mut known: HashSet<String> = log
        .since(from, Some(from + 1))
        .await?
        .into_iter()
        .map(|op| op.id)
        .chain(baseline.as_ref().map(|op| op.id.clone()))
        .collect();
    let mut last = baseline.map(|op| op.id);
    let token = call.request.meta.get_progress_token();
    let mut next_progress = started + PROGRESS_EVERY;
    loop {
        let head = log.head().await?;
        let found = log.since(from, None).await?;
        let mut new: Vec<_> = found.iter().filter(|op| !known.contains(&op.id)).collect();
        if !new.is_empty() {
            new.reverse();
            return Ok(Waited {
                last_operation_id: new.last().map(|op| op.id.clone()),
                operations: new.into_iter().map(json::Operation::from).collect(),
                timed_out: false,
            });
        }
        if let Some(head) = head {
            last = Some(head.id.clone());
            // Nothing matched up to the head: the window moves up to it.
            if ended(&head) > from {
                from = ended(&head);
                known = found
                    .into_iter()
                    .filter(|op| ended(op) >= from)
                    .map(|op| op.id)
                    .chain([head.id])
                    .collect();
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Waited {
                operations: Vec::new(),
                last_operation_id: last,
                timed_out: true,
            });
        }
        if let Some(token) = &token
            && now >= next_progress
        {
            next_progress += PROGRESS_EVERY;
            let progress = ProgressNotificationParam::new(
                token.clone(),
                now.duration_since(started).as_secs_f64(),
            )
            .with_total(timeout.as_secs_f64());
            // A client that stopped listening cancels the call too.
            let _ = call.request.peer.notify_progress(progress).await;
        }
        tokio::select! {
            () = tokio::time::sleep_until((now + POLL).min(deadline)) => {}
            () = call.request.ct.cancelled() => {
                return Ok(Waited {
                    operations: Vec::new(),
                    last_operation_id: last,
                    timed_out: true,
                });
            }
        }
    }
}
