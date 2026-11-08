//! `reef job` (R3.05, ADR 0059): the requester's side of the executor
//! protocol. A job asks for checks of some evidence kinds (and targets) on
//! a tree; an executor, a process outside Reef, claims it and returns
//! signed evidence. Nothing here runs a check: executors speak the protocol
//! directly (`docs/executor-protocol.md`), and R3.06 brings `reef
//! executor`.
//!
//! A job's state says how far it got, never whether the checks passed:
//! `show`'s coverage lines and the evidence do.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, CancelJobRequest, GetJobRequest, ListJobsRequest, SubmitJobRequest,
    SubscribePresenceRequest, submit_job_request,
};
use reef_client::brief;
use reef_client::json::{self, Job, JobSubmitted, PresenceEvent};
use serde::Serialize;
use tokio::time::Instant;

use super::advice::AdviceCtx;
use super::out::{TextRender, TextRow};
use super::scope::{self, ScopeArgs};
use super::stream::Backoff;
use super::{CliError, Ctx};

/// Ids, trees and changes in text, by their first characters.
const SHORT: usize = 12;

/// `reef job`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: JobCommand,
}

#[derive(Debug, clap::Subcommand)]
enum JobCommand {
    /// Ask executors to check a revision's tree: the evidence kinds, and optionally targets.
    #[command(after_help = "Example: reef job submit --kind test --target crates/reef-lib --wait")]
    Submit(SubmitArgs),
    /// Show a job: its state, its executor, and the evidence covering each requested pair.
    #[command(after_help = "Example: reef job show job_8f3a")]
    Show(ShowArgs),
    /// List jobs, newest first; --queue for open jobs in the order executors claim them.
    #[command(visible_alias = "ls", after_help = "Example: reef job list --queue")]
    List(ListArgs),
    /// Cancel a queued or running job.
    #[command(after_help = "Example: reef job cancel job_8f3a --reason superseded")]
    Cancel(CancelArgs),
}

/// `reef job submit`.
#[derive(Debug, clap::Args)]
struct SubmitArgs {
    /// The revision whose tree to check [default: the workspace's change].
    #[arg(short = 'r', long, value_name = "REV", conflicts_with = "tree")]
    revision: Option<String>,
    /// A tree by id instead of a revision.
    #[arg(long, value_name = "ID")]
    tree: Option<String>,
    /// An evidence kind to produce, such as test or lint (repeatable).
    #[arg(long = "kind", value_name = "KIND", required = true)]
    kinds: Vec<String>,
    /// A target to check, in the executor's naming (repeatable) [default: the whole check].
    #[arg(long = "target", value_name = "TARGET")]
    targets: Vec<String>,
    /// A label the executor must have, such as os=linux (repeatable).
    #[arg(long = "require", value_name = "KEY=VALUE")]
    requirements: Vec<String>,
    /// How long one attempt may run [default: the daemon's `[executor] default_wall_time`].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    wall_time: Option<Duration>,
    /// CPU time one attempt may use, enforced by the executor.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    cpu_time: Option<Duration>,
    /// Higher is claimed first.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    priority: i32,
    /// How long it may wait unclaimed [default: the daemon's `[executor] queue_timeout`].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    queue_timeout: Option<Duration>,
    /// Queue a new job even when an identical one is open.
    #[arg(long)]
    duplicate: bool,
    /// Wait until the job finishes, printing its states on stderr.
    #[arg(long)]
    wait: bool,
    /// With --wait, give up after this long (exit 6), leaving the job open.
    #[arg(long = "for", value_name = "DURATION", value_parser = humantime::parse_duration, requires = "wait")]
    wait_for: Option<Duration>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef job show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// The job: its id, or job_ and a unique prefix.
    #[arg(value_name = "ID")]
    id: String,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef job list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Only jobs in this state: queued, running, completed, cancelled or expired (repeatable).
    #[arg(long = "state", value_name = "STATE", value_parser = ["queued", "running", "completed", "cancelled", "expired"])]
    states: Vec<String>,
    /// Only jobs on this tree.
    #[arg(long, value_name = "ID")]
    tree: Option<String>,
    /// Only jobs for this revision's change.
    #[arg(long, value_name = "REV")]
    change: Option<String>,
    /// Only jobs this executor holds or last held, by name.
    #[arg(long, value_name = "NAME")]
    executor: Option<String>,
    /// Only jobs submitted since then: an RFC 3339 time, or a duration ago (2h).
    #[arg(long, value_name = "WHEN", value_parser = super::history::when)]
    since: Option<SystemTime>,
    /// Only open jobs, in the order executors claim them.
    #[arg(long)]
    queue: bool,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef job cancel`.
#[derive(Debug, clap::Args)]
struct CancelArgs {
    /// The job: its id, or job_ and a unique prefix.
    #[arg(value_name = "ID")]
    id: String,
    /// Why, kept as the job's message.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// Runs `reef job`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        JobCommand::Submit(args) => Box::pin(submit(ctx, args)).await,
        JobCommand::Show(args) => Box::pin(show(ctx, args)).await,
        JobCommand::List(args) => Box::pin(list(ctx, args)).await,
        JobCommand::Cancel(args) => Box::pin(cancel(ctx, args)).await,
    }
}

fn proto(duration: Option<Duration>) -> Option<prost_types::Duration> {
    duration.map(|duration| prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or(0),
    })
}

async fn submit(ctx: &Ctx, args: SubmitArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let source = match (&args.tree, &args.revision) {
        (Some(tree), _) => submit_job_request::Source::TreeId(tree.trim().to_owned()),
        (None, Some(revision)) => submit_job_request::Source::Revision(revision.clone()),
        (None, None) if scope.workspace.is_some() => {
            submit_job_request::Source::Revision("@".to_owned())
        }
        (None, None) => {
            return Err(CliError::Usage(
                "outside a workspace, name the tree to check: -r REV or --tree ID".to_owned(),
            ));
        }
    };
    let response = ctx
        .call(ctx.client.submit_job(SubmitJobRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            source: Some(source),
            kinds: args.kinds,
            targets: args.targets,
            budget: Some(v1::Budget {
                wall_time: proto(args.wall_time),
                cpu_time: proto(args.cpu_time),
            }),
            requirements: args.requirements,
            priority: args.priority,
            queue_timeout: proto(args.queue_timeout),
            duplicate: args.duplicate,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let mut job = Job::from(&response.job.unwrap_or_default());
    if args.wait {
        ctx.out.note(&submitted_line(&job, response.created));
        job = Box::pin(wait(ctx, &job, args.wait_for)).await?;
    }
    ctx.out.object_with(
        &Submitted {
            result: JobSubmitted {
                job,
                created: response.created,
            },
            waited: args.wait,
        },
        |submitted| brief::Job::from(&submitted.result.job),
    )
}

/// Follows the job's presence entry until it ends, printing each state
/// and phase change on stderr, then reads the job with its coverage.
/// Across a daemon restart it subscribes again; a job that finished while
/// nobody watched is read with `GetJob`.
async fn wait(ctx: &Ctx, job: &Job, timeout: Option<Duration>) -> Result<Job, CliError> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let request = SubscribePresenceRequest {
        repos: vec![job.repo_id.clone()],
        kinds: vec![v1::PresenceKind::Job.into()],
        ..SubscribePresenceRequest::default()
    };
    let until = |deadline: Option<Instant>| async move {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    let timed_out = || CliError::Refused {
        kind: reef_error::ErrorKind::Unavailable,
        message: format!(
            "job {} has not finished within {} (--for); it stays open",
            job.id,
            humantime::format_duration(timeout.unwrap_or_default())
        ),
        advice: Some(reef_error::Advice::run(
            "Follow it later, or cancel it if nobody needs it.",
            ["reef", "job", "show", job.id.as_str()],
        )),
    };
    let mut backoff = Backoff::new();
    let mut last = String::new();
    loop {
        let subscribed = tokio::select! {
            () = until(deadline) => return Err(timed_out()),
            subscribed = ctx.call(ctx.client.subscribe_presence(request.clone())) => subscribed,
        };
        match subscribed {
            Ok(mut stream) => loop {
                let next = tokio::select! {
                    () = until(deadline) => return Err(timed_out()),
                    next = stream.message() => next,
                };
                let Ok(Some(frame)) = next else {
                    break;
                };
                backoff.reset();
                for event in &frame.events {
                    let event = PresenceEvent::from(event);
                    let Some(entry) = event.presence.job.as_ref() else {
                        continue;
                    };
                    if entry.job_id != job.id {
                        continue;
                    }
                    let line = state_line(entry);
                    if line != last {
                        ctx.out.note(&line);
                        last = line;
                    }
                    if event.ended {
                        return read(ctx, &job.id).await;
                    }
                }
                // Synced: a job that finished before the subscription has
                // no entry, so read it.
                if frame.synced {
                    let now = read(ctx, &job.id).await?;
                    if !matches!(now.state.as_str(), "queued" | "running") {
                        return Ok(now);
                    }
                }
            },
            Err(error) => {
                if !backoff.retry(&error, deadline) {
                    return Err(error);
                }
            }
        }
        if !backoff.wait(deadline).await {
            return Err(timed_out());
        }
    }
}

async fn read(ctx: &Ctx, id: &str) -> Result<Job, CliError> {
    let response = ctx
        .call(ctx.client.get_job(GetJobRequest {
            repo: String::new(),
            job_id: id.to_owned(),
        }))
        .await?;
    Ok(Job::from(&response.job.unwrap_or_default()))
}

/// A `job` presence entry's state, as `--wait` prints it.
fn state_line(entry: &json::JobPresence) -> String {
    let mut line = format!("{} {}", entry.job_id, entry.state);
    if let Some(outcome) = &entry.outcome {
        let _ = write!(line, " ({outcome})");
    }
    if let Some(reason) = &entry.end_reason {
        let _ = write!(line, " ({reason})");
    }
    if let Some(executor) = &entry.executor {
        let _ = write!(line, " by {executor}, attempt {}", entry.attempt);
    }
    if let Some(progress) = &entry.progress {
        let _ = write!(line, " {}", progress_text(progress));
    }
    line
}

fn progress_text(progress: &json::Progress) -> String {
    let mut text = format!("phase {}", progress.phase.as_deref().unwrap_or("-"));
    match progress.total {
        Some(total) => {
            let _ = write!(text, " {}/{total}", progress.done);
        }
        None if progress.done > 0 => {
            let _ = write!(text, " {}", progress.done);
        }
        None => {}
    }
    if let Some(unit) = &progress.unit {
        let _ = write!(text, " {unit}");
    }
    text
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let response = ctx
        .call(ctx.client.get_job(GetJobRequest {
            repo: scope.repo_id.clone(),
            job_id: args.id,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let job = Job::from(&response.job.unwrap_or_default());
    ctx.out
        .object_with(&Shown(job), |shown| brief::Job::from(&shown.0))
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let change_id = match &args.change {
        None => String::new(),
        Some(revision) => {
            let change = ctx
                .call(ctx.client.get_change(v1::GetChangeRequest {
                    repo: scope.repo_id.clone(),
                    workspace: scope.workspace_id(),
                    revision: revision.clone(),
                    ..v1::GetChangeRequest::default()
                }))
                .await
                .map_err(|error| advice.refine(error))?;
            change.change.unwrap_or_default().change_id
        }
    };
    let states = args
        .states
        .iter()
        .filter_map(|name| json::job_state_of(name))
        .map(i32::from)
        .collect();
    let jobs = ctx
        .call(ctx.client.list_jobs(
            ListJobsRequest {
                repo: scope.repo_id.clone(),
                states,
                tree_id: args.tree.unwrap_or_default(),
                change_id,
                executor: args.executor.unwrap_or_default(),
                since: args.since.map(prost_types::Timestamp::from),
                queue_order: args.queue,
                ..ListJobsRequest::default()
            },
            args.limit,
        ))
        .await
        .map_err(|error| advice.refine(error))?;
    let listed: Vec<Job> = jobs.items.iter().map(Job::from).collect();
    if jobs.more {
        ctx.out.note(&format!(
            "reef: more jobs than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    ctx.out.list_with(&listed, |listed| {
        listed.iter().map(brief::Job::from).collect::<Vec<_>>()
    })
}

async fn cancel(ctx: &Ctx, args: CancelArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let response = ctx
        .call(ctx.client.cancel_job(CancelJobRequest {
            repo: scope.repo_id.clone(),
            job_id: args.id,
            reason: args.reason.unwrap_or_default(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let job = Job::from(&response.job.unwrap_or_default());
    ctx.out
        .object_with(&Cancelled(job), |cancelled| brief::Job::from(&cancelled.0))
}

fn short(id: &str) -> &str {
    id.get(..SHORT).unwrap_or(id)
}

/// A state with its outcome or end reason: `completed (done)`.
fn state_text(job: &Job) -> String {
    match (&job.outcome, &job.end_reason) {
        (Some(outcome), _) => format!("{} ({outcome})", job.state),
        (None, Some(reason)) => format!("{} ({reason})", job.state),
        (None, None) => job.state.clone(),
    }
}

/// `job job_8f3a… queued: test, lint on tree 4b825dc642cb (change kxyz…)`.
fn submitted_line(job: &Job, created: bool) -> String {
    let mut line = format!(
        "job {} {}: {} on tree {}",
        job.id,
        job.state,
        job.spec.kinds.join(", "),
        short(&job.spec.tree_id)
    );
    if let Some(change) = &job.spec.change_id {
        let _ = write!(line, " (change {})", short(change));
    }
    if !created {
        line.push_str(" (already open)");
    }
    line
}

fn time_of(at: Option<&str>) -> &str {
    at.unwrap_or("-")
}

fn render_job(job: &Job, w: &mut dyn Write) -> io::Result<()> {
    writeln!(
        w,
        "job {}  {}  attempt {} of {}",
        job.id,
        state_text(job),
        job.attempt,
        job.max_attempts
    )?;
    let mut spec = format!("tree {}", short(&job.spec.tree_id));
    if let Some(change) = &job.spec.change_id {
        let _ = write!(spec, "  change {}", short(change));
    }
    let _ = write!(spec, "  kinds {}", job.spec.kinds.join(", "));
    if !job.spec.targets.is_empty() {
        let _ = write!(spec, "  targets {}", job.spec.targets.len());
    }
    if let Some(ms) = job.spec.wall_time_ms {
        let _ = write!(
            spec,
            "  wall time {}",
            humantime::format_duration(Duration::from_millis(ms))
        );
    }
    writeln!(w, "{spec}")?;
    let mut life = format!(
        "requested by {} {}",
        job.requested_by_agent.as_deref().unwrap_or("-"),
        time_of(job.submitted_at.as_deref())
    );
    if let Some(executor) = &job.executor {
        let _ = write!(life, "; claimed by {}", executor.name);
        if let Some(version) = &executor.version {
            let _ = write!(life, " {version}");
        }
        if let Some(identity) = &executor.identity {
            let _ = write!(life, " ({identity})");
        }
        let _ = write!(life, " {}", time_of(job.claimed_at.as_deref()));
    }
    match (&job.finished_at, job.state.as_str()) {
        (Some(finished), _) => {
            let _ = write!(
                life,
                "; finished {finished} ({})",
                humantime::format_duration(Duration::from_millis(job.elapsed_ms))
            );
        }
        (None, "running") => {
            if let Some(progress) = &job.progress {
                let _ = write!(life, "; {}", progress_text(progress));
            }
            let _ = write!(
                life,
                "; lease until {}",
                time_of(job.lease_expires_at.as_deref())
            );
        }
        _ => {}
    }
    writeln!(w, "{life}")?;
    if let Some(message) = &job.message {
        writeln!(w, "message: {message}")?;
    }
    for entry in &job.coverage {
        let result = match (&entry.evidence_id, &entry.result) {
            (Some(id), Some(result)) => format!(
                "{result:<5} {}  {}",
                short(id),
                if entry.fresh { "fresh" } else { "reused" }
            ),
            _ => "(none yet)".to_owned(),
        };
        writeln!(
            w,
            "  {}  {}  {result}",
            entry.kind,
            entry.target.as_deref().unwrap_or("(whole)")
        )?;
    }
    Ok(())
}

/// `job submit`'s result.
#[derive(Serialize)]
struct Submitted {
    #[serde(flatten)]
    result: JobSubmitted,
    #[serde(skip)]
    waited: bool,
}

impl TextRender for Submitted {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.waited {
            render_job(&self.result.job, w)
        } else {
            writeln!(
                w,
                "{}",
                submitted_line(&self.result.job, self.result.created)
            )
        }
    }
}

/// `job show`'s job.
#[derive(Serialize)]
#[serde(transparent)]
struct Shown(Job);

impl TextRender for Shown {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        render_job(&self.0, w)
    }
}

/// `job cancel`'s job.
#[derive(Serialize)]
#[serde(transparent)]
struct Cancelled(Job);

impl TextRender for Cancelled {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "cancelled job {}", self.0.id)
    }
}

impl TextRow for Job {
    const HEADER: &'static [&'static str] =
        &["ID", "STATE", "TREE", "KINDS", "EXECUTOR", "SUBMITTED"];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            state_text(self),
            short(&self.spec.tree_id).to_owned(),
            self.spec.kinds.join(","),
            self.executor
                .as_ref()
                .map_or_else(|| "-".to_owned(), |executor| executor.name.clone()),
            self.submitted_at.clone().unwrap_or_else(|| "-".to_owned()),
        ]
    }
}
