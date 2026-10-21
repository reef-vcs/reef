//! `reef ws run` (R1.21): create a leased workspace, or attach to one, run
//! a command in it, keep the lease alive for as long as the command lives,
//! then take a final snapshot and release it.
//!
//! The command inherits stdio and stays in `ws run`'s process group, so an
//! interactive agent can read the terminal. `ws run`'s own messages go to
//! stderr prefixed `reef:`; `-o json` prints one object on exit and nothing
//! else on stdout. The exit code is the command's (`128 + n` if signal *n*
//! killed it, 127 or 126 if it could not be started), unless it exited 0
//! and the release failed, which exits with the daemon error's code.
//!
//! The release (or `--keep`'s snapshot) waits up to `--timeout` or
//! [`FINISH_DEADLINE`], whichever is longer: a final snapshot of a large
//! tree can outlast the usual RPC deadline, and the daemon finishes it
//! regardless.

use std::io::{self, IsTerminal as _};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitCode, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_daemon::client::{ClientError, WorkspacesApi, default_owner};
use reef_daemon::proto::v0::{CreateRequest, HeartbeatResponse, Lease, Workspace, lease};
use reef_lib::error::{Classify as _, ErrorChain, ErrorKind};
use serde::Serialize;
use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal as SignalStream, SignalKind, signal};
use tokio::time::Instant;

use super::env::reef_env;
use super::error::kind_text;
use super::repo::missing;
use super::ws::{CreateTarget, lease_millis};
use super::{CliError, Ctx, Out};

/// Each heartbeat's deadline.
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(5);
/// The longest wait between heartbeats while the daemon is unreachable.
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A second SIGINT or SIGTERM within this long of the first kills the
/// command.
const ESCALATE_WITHIN: Duration = Duration::from_secs(5);
/// A release still running after this long is explained on stderr.
const RELEASE_NOTE_AFTER: Duration = Duration::from_secs(2);
/// The least time the release, or `--keep`'s snapshot, is given.
const FINISH_DEADLINE: Duration = Duration::from_secs(600);
/// The default purpose is the command line, cut to this many characters.
const PURPOSE_MAX_CHARS: usize = 200;

/// `reef ws run`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(flatten)]
    target: CreateTarget,
    /// What the workspace is for [default: the command line].
    #[arg(long, value_name = "TEXT")]
    purpose: Option<String>,
    /// Who holds it (default `<user>@<host>:<pid>`). With --workspace, the
    /// owner the workspace must have (default: this user on this machine,
    /// `<user>@<host>` with or without a `:<pid>`).
    #[arg(long, value_name = "OWNER")]
    owner: Option<String>,
    /// The lease length [default: the daemon's lease.default].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    lease: Option<Duration>,
    /// Run in this existing workspace (id or unique prefix) instead of
    /// creating one.
    #[arg(
        long,
        value_name = "WS",
        conflicts_with_all = ["change", "new_on", "path", "lease", "purpose"]
    )]
    workspace: Option<String>,
    /// Keep the workspace when the command exits: snapshot it and let the
    /// lease run out instead of releasing it.
    #[arg(long)]
    keep: bool,
    /// Add KEY=VALUE to the command's environment, as given (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    env: Vec<(String, String)>,
    /// The command and its arguments; put `--` before a command that starts
    /// with a dash.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn parse_key_val(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("expected KEY=VALUE, got `{text}`")),
    }
}

/// How the command ended.
enum ChildEnd {
    /// It ran and exited, or was killed by a signal.
    Exited(ExitStatus),
    /// It could not be started; the code is the shell's for the reason.
    NotRun { code: u8 },
    /// Waiting for it failed; it may still be running.
    Lost,
}

impl ChildEnd {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Exited(status) => match (status.code(), status.signal()) {
                (Some(code), _) => u8::try_from(code).unwrap_or(1),
                (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(u8::MAX),
                (None, None) => 1,
            },
            Self::NotRun { code } => *code,
            Self::Lost => 1,
        }
    }

    fn signal(&self) -> Option<i32> {
        match self {
            Self::Exited(status) => status.signal(),
            _ => None,
        }
    }

    fn succeeded(&self) -> bool {
        matches!(self, Self::Exited(status) if status.success())
    }
}

/// The object `-o json` prints on exit.
#[derive(Debug, Serialize)]
struct Report {
    workspace_id: String,
    path: String,
    change_id: String,
    exit_code: u8,
    /// The signal that killed the command, if one did.
    signal: Option<i32>,
    /// `--keep`: snapshotted and left active.
    kept: bool,
    released: bool,
    /// Whether the final snapshot (or `--keep`'s) ran.
    snapshot_taken: bool,
    final_op_id: Option<String>,
    /// Why the release or `--keep`'s snapshot failed.
    error: Option<ReportError>,
}

#[derive(Debug, Serialize)]
struct ReportError {
    kind: &'static str,
    message: String,
}

/// What happened after the command exited.
#[derive(Default)]
struct Outcome {
    kept: bool,
    released: bool,
    snapshot_taken: bool,
    final_op_id: Option<String>,
    error: Option<CliError>,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode, CliError> {
    let ws = match &args.workspace {
        Some(workspace) => attach(ctx, workspace, args.owner.as_deref()).await?,
        None => create(ctx, &args).await?,
    };
    // The workspace exists from here on: a failure before the command runs
    // still releases it (or, with `--keep`, snapshots it).
    let (heartbeater, mut signals) = match prepare(ctx, &ws) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = finish(ctx, &ws, args.keep).await;
            return Err(error);
        }
    };
    let end = match spawn(ctx, &args, &ws) {
        Ok(child) => {
            let heartbeats = tokio::spawn(heartbeats(heartbeater));
            let end = supervise(child, &mut signals, &ctx.out).await;
            heartbeats.abort();
            end
        }
        Err(error) => {
            let code = if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            ctx.out.error_line(&format!(
                "reef: cannot run {}: {error}",
                args.command.first().map_or("", String::as_str)
            ));
            ChildEnd::NotRun { code }
        }
    };
    let outcome = finish(ctx, &ws, args.keep).await;
    let mut code = end.exit_code();
    if let Some(error) = &outcome.error {
        ctx.out.error_line(&format!("error: {}", ErrorChain(error)));
        if end.succeeded() {
            code = error.exit_code();
        }
    }
    let report = Report {
        workspace_id: ws.id.clone(),
        path: ws.path.clone(),
        change_id: ws.change_id.clone(),
        exit_code: code,
        signal: end.signal(),
        kept: outcome.kept,
        released: outcome.released,
        snapshot_taken: outcome.snapshot_taken,
        final_op_id: outcome.final_op_id,
        error: outcome.error.as_ref().map(|error| ReportError {
            kind: kind_text(error.kind()),
            message: ErrorChain(error).to_string(),
        }),
    };
    // The command's exit code stands even if stdout is gone.
    if let Err(error) = ctx.out.json_only(&report) {
        ctx.out
            .error_line(&format!("error: {}", ErrorChain(&error)));
    }
    Ok(ExitCode::from(code))
}

/// The heartbeater for `ws`'s lease, and the signal handlers, installed
/// before the spawn so that a signal from then on is forwarded rather than
/// ending `ws run`.
fn prepare(ctx: &Ctx, ws: &Workspace) -> Result<(Heartbeater, Signals), CliError> {
    let lease = ws.lease.as_ref().ok_or_else(|| missing("lease"))?;
    let out = ctx.out.clone();
    let notify = Box::new(move |text: &str| out.note(text));
    let heartbeater = Heartbeater::new(Arc::new(ctx.client.clone()), ws, lease, notify)?;
    Ok((heartbeater, Signals::install()?))
}

/// `Workspaces.Create` with `ws run`'s defaults.
async fn create(ctx: &Ctx, args: &Args) -> Result<Workspace, CliError> {
    let request = CreateRequest {
        owner: args
            .owner
            .clone()
            .unwrap_or_else(|| format!("{}:{}", default_owner(), std::process::id())),
        purpose: args
            .purpose
            .clone()
            .unwrap_or_else(|| purpose_of(&args.command)),
        lease_ms: lease_millis(args.lease)?,
        ..args.target.request()?
    };
    let response = ctx
        .call_with_repo(&args.target.repo()?, |repo| {
            ctx.client.create_workspace(CreateRequest {
                repo,
                ..request.clone()
            })
        })
        .await?;
    response.workspace.ok_or_else(|| missing("workspace"))
}

/// The command line, cut to [`PURPOSE_MAX_CHARS`].
fn purpose_of(command: &[String]) -> String {
    command.join(" ").chars().take(PURPOSE_MAX_CHARS).collect()
}

/// Looks `workspace` up and takes over its lease: it must be active with a
/// live lease, and its owner must be `owner` or, without one, this user on
/// this machine. Heartbeats once, which proves the lease can be renewed and
/// gives a grace-period lease its full length back.
async fn attach(ctx: &Ctx, workspace: &str, owner: Option<&str>) -> Result<Workspace, CliError> {
    let response = ctx
        .call(ctx.client.inspect_workspace("", workspace, false))
        .await?;
    let mut ws = response.workspace.ok_or_else(|| missing("workspace"))?;
    let live = ws.state == "active"
        && ws.lease.as_ref().is_some_and(|lease| {
            matches!(lease.state(), lease::State::Active | lease::State::InGrace)
        });
    if !live {
        return Err(conflict(format!(
            "workspace {} is {} and its lease has ended; only an active workspace can be attached",
            ws.id, ws.state
        )));
    }
    if !may_attach(&ws.owner, owner, &default_owner()) {
        return Err(conflict(format!(
            "workspace {} is held by {}; pass --owner {} to attach to it",
            ws.id, ws.owner, ws.owner
        )));
    }
    let lease = ws.lease.as_mut().ok_or_else(|| missing("lease"))?;
    let renewed = ctx
        .call(ctx.client.heartbeat_workspace("", &ws.id, &lease.id))
        .await?;
    lease.expires_at = renewed.expires_at.or(lease.expires_at);
    lease.heartbeat_every = renewed.heartbeat_every.or(lease.heartbeat_every);
    Ok(ws)
}

/// Whether a workspace held by `holder` may be attached by a `ws run`
/// given `owner`, or none, on behalf of `me` (`<user>@<host>`).
fn may_attach(holder: &str, owner: Option<&str>, me: &str) -> bool {
    match owner {
        Some(owner) => holder == owner,
        None => holder
            .strip_prefix(me)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':')),
    }
}

fn conflict(message: String) -> CliError {
    CliError::Refused {
        kind: ErrorKind::Conflict,
        message,
    }
}

/// Starts the command in the workspace with the `REEF_*` variables and
/// `--env`'s.
fn spawn(ctx: &Ctx, args: &Args, ws: &Workspace) -> io::Result<Child> {
    let (program, rest) = args
        .command
        .split_first()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    Command::new(program)
        .args(rest)
        .current_dir(&ws.path)
        .envs(reef_env(ws, ctx.client.socket()))
        .envs(args.env.iter().map(|(key, value)| (key, value)))
        // A panic in `ws run` must not kill an agent mid-write.
        .kill_on_drop(false)
        .spawn()
}

/// The signals `ws run` forwards. tokio merges deliveries of one signal
/// that arrive before it is polled, so two signals sent back to back may
/// count as one.
struct Signals {
    term: SignalStream,
    int: SignalStream,
    quit: SignalStream,
    hup: SignalStream,
}

impl Signals {
    fn install() -> Result<Self, CliError> {
        let stream = |kind| signal(kind).map_err(CliError::Signals);
        Ok(Self {
            term: stream(SignalKind::terminate())?,
            int: stream(SignalKind::interrupt())?,
            quit: stream(SignalKind::quit())?,
            hup: stream(SignalKind::hangup())?,
        })
    }
}

/// Repeated forwarded SIGINT and SIGTERM: the second within
/// [`ESCALATE_WITHIN`] of the first is a repeat.
#[derive(Debug, Default)]
struct Escalation {
    first: Option<Instant>,
}

impl Escalation {
    fn repeated(&mut self, now: Instant) -> bool {
        match self.first {
            Some(first) if now.duration_since(first) <= ESCALATE_WITHIN => true,
            _ => {
                self.first = Some(now);
                false
            }
        }
    }
}

/// Waits for the command to exit, forwarding signals: SIGTERM and SIGHUP
/// always; SIGINT and SIGQUIT only when stdin is not a terminal, since a
/// terminal's `Ctrl-C` or `Ctrl-\` already reached the command through the
/// shared process group. A forwarded SIGINT or SIGTERM repeated within
/// [`ESCALATE_WITHIN`] kills it; a terminal's are the command's to handle
/// (an interactive agent may take two `Ctrl-C` to exit). SIGHUP is
/// forwarded although a hangup also reaches the group (R1.21's rule).
async fn supervise(mut child: Child, signals: &mut Signals, out: &Out) -> ChildEnd {
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw);
    let forward_tty = !io::stdin().is_terminal();
    let mut escalation = Escalation::default();
    loop {
        let (signal, forward, counts) = tokio::select! {
            status = child.wait() => {
                return match status {
                    Ok(status) => ChildEnd::Exited(status),
                    Err(error) => {
                        out.error_line(&format!("reef: lost track of the command: {error}"));
                        ChildEnd::Lost
                    }
                };
            }
            Some(()) = signals.term.recv() => (Signal::SIGTERM, true, true),
            Some(()) = signals.int.recv() => (Signal::SIGINT, forward_tty, forward_tty),
            Some(()) = signals.quit.recv() => (Signal::SIGQUIT, forward_tty, false),
            Some(()) = signals.hup.recv() => (Signal::SIGHUP, true, false),
        };
        let Some(pid) = pid else { continue };
        // ESRCH means it exited already; `wait` reports that next.
        if counts && escalation.repeated(Instant::now()) {
            out.note("reef: signalled again; killing the command");
            let _ = kill(pid, Signal::SIGKILL);
        } else if forward {
            let _ = kill(pid, signal);
        }
    }
}

/// After the command: `--keep` snapshots and leaves the lease to run out;
/// otherwise release with a final snapshot.
async fn finish(ctx: &Ctx, ws: &Workspace, keep: bool) -> Outcome {
    let deadline = ctx.timeout.max(FINISH_DEADLINE);
    if keep {
        return match ctx
            .call_within(deadline, ctx.client.snapshot_workspace("", &ws.id, ""))
            .await
        {
            Ok(response) => Outcome {
                kept: true,
                snapshot_taken: true,
                final_op_id: non_empty(&response.op_id),
                ..Outcome::default()
            },
            Err(error) => Outcome {
                kept: true,
                error: Some(error),
                ..Outcome::default()
            },
        };
    }
    let release = ctx.call_within(
        deadline,
        ctx.client.release_workspace("", &ws.id, true, false),
    );
    tokio::pin!(release);
    let result = tokio::select! {
        result = &mut release => result,
        () = tokio::time::sleep(RELEASE_NOTE_AFTER) => {
            ctx.out.note(&format!("reef: releasing workspace {}", ws.id));
            release.await
        }
    };
    match result {
        Ok(response) => Outcome {
            released: true,
            snapshot_taken: response.snapshot_taken,
            final_op_id: non_empty(&response.final_op_id),
            ..Outcome::default()
        },
        Err(error) => Outcome {
            error: Some(error),
            ..Outcome::default()
        },
    }
}

/// `value`, or `None` if it is empty, as the API spells "unset".
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Renews the lease every `heartbeat_every` until aborted, or until the
/// daemon says the lease has ended, which it reports once.
async fn heartbeats(mut heartbeater: Heartbeater) {
    loop {
        tokio::time::sleep(heartbeater.next_delay()).await;
        if let Err(error) = heartbeater.tick().await {
            heartbeater.notify(&format!(
                "reef: lease {} cannot be renewed: {}; the workspace may be reclaimed",
                heartbeater.lease,
                ErrorChain(&error)
            ));
            return;
        }
    }
}

/// Whether the last heartbeat reached the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Up,
    Lost,
}

/// Retry delays while the daemon is unreachable: 1, 2, 4, 8, 16, then 30 s.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
}

impl Backoff {
    fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn reset(&mut self) {
        self.failures = 0;
    }

    fn delay(&self) -> Duration {
        let doubling = self.failures.saturating_sub(1).min(5);
        Duration::from_secs(1 << doubling).min(BACKOFF_CAP)
    }
}

/// Keeps one lease alive: renews it on the daemon's `heartbeat_every`, and
/// while the daemon is unreachable retries with [`Backoff`], noting each
/// change of [`LinkState`] once, and once that the lease has run out.
struct Heartbeater {
    api: Arc<dyn WorkspacesApi>,
    workspace: String,
    lease: String,
    every: Duration,
    grace: Duration,
    /// `expires_at + grace` as of the last answer: past it, the daemon may
    /// reclaim the workspace.
    lapses_at: Option<SystemTime>,
    warned_lapsed: bool,
    backoff: Backoff,
    state: LinkState,
    notify: Box<dyn Fn(&str) + Send + Sync>,
}

impl Heartbeater {
    /// For `lease` on `ws`, with the cadence the daemon gave it.
    fn new(
        api: Arc<dyn WorkspacesApi>,
        ws: &Workspace,
        lease: &Lease,
        notify: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Self, CliError> {
        let every = lease
            .heartbeat_every
            .and_then(|every| Duration::try_from(every).ok())
            .filter(|every| !every.is_zero())
            .ok_or_else(|| missing("lease.heartbeat_every"))?;
        let grace = Duration::from_millis(lease.grace_ms);
        Ok(Self {
            api,
            workspace: ws.id.clone(),
            lease: lease.id.clone(),
            every,
            grace,
            lapses_at: lapse(lease.expires_at, grace),
            warned_lapsed: false,
            backoff: Backoff::default(),
            state: LinkState::Up,
            notify,
        })
    }

    /// How long to wait before the next [`Heartbeater::tick`].
    fn next_delay(&self) -> Duration {
        match self.state {
            LinkState::Up => self.every,
            LinkState::Lost => self.backoff.delay(),
        }
    }

    /// One heartbeat. `Ok(None)` while it fails for any reason but the
    /// lease being gone; an error when it is: `Conflict` (ended, replaced,
    /// or past its grace) or `NotFound`. A request cut off by a daemon
    /// restart fails as `Internal`, which must not stop the heartbeats.
    async fn tick(&mut self) -> Result<Option<HeartbeatResponse>, ClientError> {
        let call = self
            .api
            .heartbeat_workspace("", &self.workspace, &self.lease);
        let reason = match tokio::time::timeout(HEARTBEAT_DEADLINE, call).await {
            Ok(Ok(response)) => {
                self.renewed(&response);
                return Ok(Some(response));
            }
            Ok(Err(error)) if lease_gone(&error) => return Err(error),
            Ok(Err(error)) => ErrorChain(&error).to_string(),
            Err(_) => format!(
                "no answer within {}",
                humantime::format_duration(HEARTBEAT_DEADLINE)
            ),
        };
        self.unreachable(&reason);
        Ok(None)
    }

    fn renewed(&mut self, response: &HeartbeatResponse) {
        if self.state == LinkState::Lost {
            self.notify("reef: daemon reachable again; lease renewed");
        }
        self.state = LinkState::Up;
        self.backoff.reset();
        if let Some(every) = response
            .heartbeat_every
            .and_then(|every| Duration::try_from(every).ok())
            .filter(|every| !every.is_zero())
        {
            self.every = every;
        }
        self.lapses_at = lapse(response.expires_at, self.grace).or(self.lapses_at);
        self.warned_lapsed = false;
    }

    fn unreachable(&mut self, reason: &str) {
        if self.state == LinkState::Up {
            self.notify(&format!(
                "reef: daemon unreachable ({reason}); retrying, and the command keeps running"
            ));
        }
        self.state = LinkState::Lost;
        self.backoff.fail();
        let lapsed = self.lapses_at.is_some_and(|at| SystemTime::now() > at);
        if lapsed && !self.warned_lapsed {
            self.warned_lapsed = true;
            self.notify(&format!(
                "reef: lease {} has expired; the daemon may reclaim workspace {} and remove its directory",
                self.lease, self.workspace
            ));
        }
    }

    fn notify(&self, text: &str) {
        (self.notify)(text);
    }
}

/// Whether a heartbeat failed because the lease can no longer be renewed.
fn lease_gone(error: &ClientError) -> bool {
    matches!(error.kind(), ErrorKind::Conflict | ErrorKind::NotFound)
}

/// `expires_at + grace`. Generic over the API's timestamp type, which this
/// crate does not name.
fn lapse<T>(expires_at: Option<T>, grace: Duration) -> Option<SystemTime>
where
    SystemTime: TryFrom<T>,
{
    let expires_at = SystemTime::try_from(expires_at?).ok()?;
    expires_at.checked_add(grace)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use reef_daemon::proto::v0::register_request::IfNotJj;
    use reef_daemon::proto::v0::{
        CreateResponse, ForgetResponse, InspectResponse, ListWorkspacesRequest, RegisterResponse,
        ReleaseResponse, Repository, SnapshotResponse, StatusResponse,
    };

    use super::*;

    /// A daemon whose heartbeats answer from a script; nothing else is
    /// called.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<VecDeque<Result<HeartbeatResponse, ClientError>>>,
    }

    fn not_here<T>() -> Result<T, ClientError> {
        Err(tonic::Status::unimplemented("not in this fake").into())
    }

    #[tonic::async_trait]
    impl WorkspacesApi for Fake {
        async fn status(&self) -> Result<StatusResponse, ClientError> {
            not_here()
        }
        async fn register_repository(
            &self,
            _: &Path,
            _: IfNotJj,
        ) -> Result<RegisterResponse, ClientError> {
            not_here()
        }
        async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
            not_here()
        }
        async fn forget_repository(&self, _: &str) -> Result<ForgetResponse, ClientError> {
            not_here()
        }
        async fn create_workspace(&self, _: CreateRequest) -> Result<CreateResponse, ClientError> {
            not_here()
        }
        async fn list_workspaces(
            &self,
            _: ListWorkspacesRequest,
        ) -> Result<Vec<Workspace>, ClientError> {
            not_here()
        }
        async fn inspect_workspace(
            &self,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<InspectResponse, ClientError> {
            not_here()
        }
        async fn release_workspace(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
        ) -> Result<ReleaseResponse, ClientError> {
            not_here()
        }
        async fn snapshot_workspace(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<SnapshotResponse, ClientError> {
            not_here()
        }
        async fn heartbeat_workspace(
            &self,
            _: &str,
            workspace: &str,
            lease_id: &str,
        ) -> Result<HeartbeatResponse, ClientError> {
            assert_eq!((workspace, lease_id), ("ws", "lease"));
            self.answers.lock().unwrap().pop_front().unwrap()
        }
    }

    fn down() -> Result<HeartbeatResponse, ClientError> {
        Err(tonic::Status::unavailable("daemon is down").into())
    }

    fn renewed(every: Duration) -> HeartbeatResponse {
        HeartbeatResponse {
            lease_id: "lease".into(),
            expires_at: Some((SystemTime::now() + Duration::from_secs(90)).into()),
            heartbeat_every: Some(every.try_into().unwrap()),
        }
    }

    /// A heartbeater over `answers` whose lease expires at `expires_at`,
    /// and the notes it makes.
    fn heartbeater(
        answers: Vec<Result<HeartbeatResponse, ClientError>>,
        expires_at: SystemTime,
    ) -> (Heartbeater, Arc<Mutex<Vec<String>>>) {
        let fake = Fake {
            answers: Mutex::new(answers.into()),
        };
        let ws = Workspace {
            id: "ws".into(),
            ..Workspace::default()
        };
        let lease = Lease {
            id: "lease".into(),
            expires_at: Some(expires_at.into()),
            grace_ms: 1000,
            heartbeat_every: Some(Duration::from_secs(10).try_into().unwrap()),
            ..Lease::default()
        };
        let notes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notes);
        let notify = Box::new(move |text: &str| sink.lock().unwrap().push(text.to_owned()));
        let heartbeater = Heartbeater::new(Arc::new(fake), &ws, &lease, notify).unwrap();
        (heartbeater, notes)
    }

    // 13
    #[tokio::test]
    async fn heartbeater_backoff() {
        let mut answers: Vec<_> = (0..7).map(|_| down()).collect();
        answers.push(Ok(renewed(Duration::from_secs(7))));
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(answers, later);
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(10));
        let mut delays = Vec::new();
        for _ in 0..7 {
            assert!(heartbeater.tick().await.unwrap().is_none());
            delays.push(heartbeater.next_delay().as_secs());
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(notes.lock().unwrap().len(), 1);
        assert!(notes.lock().unwrap()[0].starts_with("reef: daemon unreachable ("));
        // A success resets to the daemon's interval, the new one it gave.
        assert!(heartbeater.tick().await.unwrap().is_some());
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(7));
        let notes = notes.lock().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(notes[1], "reef: daemon reachable again; lease renewed");
    }

    #[tokio::test]
    async fn heartbeater_warns_once_when_the_lease_lapses() {
        let past = SystemTime::now() - Duration::from_secs(90);
        let (mut heartbeater, notes) = heartbeater(vec![down(), down()], past);
        heartbeater.tick().await.unwrap();
        heartbeater.tick().await.unwrap();
        let notes = notes.lock().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[1].contains("lease lease has expired"), "{notes:?}");
    }

    #[tokio::test]
    async fn heartbeater_retries_other_failures() {
        let cut_off = Err(tonic::Status::internal("h2 protocol error").into());
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(vec![cut_off], later);
        assert!(heartbeater.tick().await.unwrap().is_none());
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(1));
        assert_eq!(notes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn heartbeater_returns_refusals() {
        let refused = Err(tonic::Status::failed_precondition("lease ended").into());
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(vec![refused], later);
        let error = heartbeater.tick().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert!(notes.lock().unwrap().is_empty());
    }

    #[test]
    fn escalation_needs_a_second_signal_within_the_window() {
        let mut escalation = Escalation::default();
        let start = Instant::now();
        assert!(!escalation.repeated(start));
        assert!(escalation.repeated(start + Duration::from_millis(100)));
        // After the window a signal starts a new one.
        let later = start + ESCALATE_WITHIN + Duration::from_secs(1);
        assert!(!escalation.repeated(later));
        assert!(escalation.repeated(later + Duration::from_secs(1)));
    }

    #[test]
    fn attach_owner_rule() {
        let me = "ann@box";
        assert!(may_attach("ann@box", None, me));
        assert!(may_attach("ann@box:4242", None, me));
        assert!(!may_attach("ann@boxer", None, me));
        assert!(!may_attach("bob@box", None, me));
        assert!(may_attach("bob@box", Some("bob@box"), me));
        assert!(!may_attach("ann@box", Some("bob@box"), me));
    }

    #[test]
    fn purpose_is_the_command_line_cut_short() {
        assert_eq!(purpose_of(&["claude".into(), "-p".into()]), "claude -p");
        let long = vec!["é".repeat(300)];
        assert_eq!(purpose_of(&long).chars().count(), PURPOSE_MAX_CHARS);
    }

    #[test]
    fn env_values_parse() {
        assert_eq!(
            parse_key_val("A=b=c").unwrap(),
            ("A".to_owned(), "b=c".to_owned())
        );
        assert_eq!(
            parse_key_val("A=").unwrap(),
            ("A".to_owned(), String::new())
        );
        assert!(parse_key_val("=x").is_err());
        assert!(parse_key_val("A").is_err());
    }
}
