//! `wait_for_operations`: "subscriptions as a wait tool". It reads
//! `SubscribeOperations` (R2.09) from `after_operation`, or from the op
//! heads when the call arrives, until a frame brings matching operations,
//! the timeout passes, or the call is cancelled. Until R2.09 it polled
//! `ListOperations`; its inputs and result did not change.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reef_api::v1::subscribe_operations_request::Start;
use reef_api::v1::{OperationFilter, SubscribeOperationsRequest};
use reef_client::ClientError;
use reef_client::json;
use reef_error::ErrorKind;
use rmcp::model::ProgressNotificationParam;
use serde_json::{Map, Value};
use tokio::time::Instant;

use super::results::Waited;
use super::tools::filter;
use super::{Call, ToolError, args, parse, to_value};
use crate::cli::{CliError, Ctx};

/// How often a waiting call with a progress token reports progress.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// The most operations one call returns, oldest first; the next call,
/// from its `last_operation_id`, returns the rest.
const MAX_RESULTS: usize = 100;
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

async fn wait(
    call: &Call<'_>,
    log: &mut Log<'_>,
    after: Option<String>,
    timeout: Duration,
) -> Result<Waited, ToolError> {
    let started = Instant::now();
    let deadline = started + timeout;
    let request = SubscribeOperationsRequest {
        repos: vec![log.repo.clone()],
        filter: Some(log.filter.clone()),
        start: after.clone().map(Start::AfterOperation),
    };
    let mut stream = log
        .ctx
        .call(log.ctx.client.subscribe_operations(request))
        .await?;
    // Where the stream stands, matching or not: what a timed-out wait
    // returns, so passing it back sees nothing twice.
    let mut last = after;
    let token = call.request.meta.get_progress_token();
    let mut progress = tokio::time::interval_at(started + PROGRESS_EVERY, PROGRESS_EVERY);
    let timed_out = |last: Option<String>| Waited {
        operations: Vec::new(),
        last_operation_id: last,
        timed_out: true,
    };
    loop {
        tokio::select! {
            next = stream.message() => {
                let frame = match next {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        return Err(CliError::Refused {
                            kind: ErrorKind::Unavailable,
                            message: "the daemon ended the subscription".to_owned(),
                            advice: None,
                        }
                        .into());
                    }
                    Err(status) => return Err(CliError::Daemon(ClientError::from(status)).into()),
                };
                if !frame.operations.is_empty() {
                    let mut operations = frame.operations;
                    operations.truncate(MAX_RESULTS);
                    if !log.include_effects {
                        for operation in &mut operations {
                            operation.effects = None;
                        }
                    }
                    return Ok(Waited {
                        last_operation_id: operations.last().map(|op| op.id.clone()),
                        operations: operations.iter().map(json::Operation::from).collect(),
                        timed_out: false,
                    });
                }
                if let Some(head) = frame.head_operation_ids.last() {
                    last = Some(head.clone());
                }
                if timeout.is_zero() && frame.live {
                    return Ok(timed_out(last));
                }
            }
            () = tokio::time::sleep_until(deadline) => return Ok(timed_out(last)),
            () = call.request.ct.cancelled() => return Ok(timed_out(last)),
            now = progress.tick(), if token.is_some() => {
                if let Some(token) = &token {
                    let progress = ProgressNotificationParam::new(
                        token.clone(),
                        now.duration_since(started).as_secs_f64(),
                    )
                    .with_total(timeout.as_secs_f64());
                    // A client that stopped listening cancels the call too.
                    let _ = call.request.peer.notify_progress(progress).await;
                }
            }
        }
    }
}
