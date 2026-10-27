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
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_api::v1::{CreateWorkspaceRequest, LeaseState, Workspace, WorkspaceState};
use reef_client::{default_owner, json};
use reef_error::{Classify as _, ErrorChain, ErrorKind};
use serde::Serialize;
use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal as SignalStream, SignalKind, signal};
use tokio::time::Instant;

use super::env::reef_env;
use super::error::kind_text;
use super::heartbeat::{Heartbeater, heartbeats};
use super::repo::missing;
use super::ws::{CreateTarget, lease_length, note_default_start};
use super::{CliError, Ctx, Out};

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
    /// Run in this existing workspace (id, unique prefix, or path inside
    /// it) instead of creating one.
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
    /// What the default start chose (a bookmark, or `root checkout`);
    /// `None` when `--change`, `--new-on` or `--workspace` named it.
    start: Option<String>,
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
    let (ws, start) = match &args.workspace {
        Some(workspace) => (attach(ctx, workspace, args.owner.as_deref()).await?, None),
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
        start,
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

/// `WorkspaceService.CreateWorkspace` with `ws run`'s defaults: the workspace, and what
/// the default start chose, if the flags named no start.
async fn create(ctx: &Ctx, args: &Args) -> Result<(Workspace, Option<String>), CliError> {
    let request = CreateWorkspaceRequest {
        owner: args
            .owner
            .clone()
            .unwrap_or_else(|| format!("{}:{}", default_owner(), std::process::id())),
        purpose: args
            .purpose
            .clone()
            .unwrap_or_else(|| purpose_of(&args.command)),
        lease_length: lease_length(args.lease)?,
        ..args.target.request()?
    };
    let response = ctx
        .call_with_repo(&args.target.repo()?, |repo| {
            ctx.client.create_workspace(CreateWorkspaceRequest {
                repo,
                ..request.clone()
            })
        })
        .await?;
    note_default_start(ctx, &response.start);
    let start = (!response.start.is_empty()).then_some(response.start);
    let ws = response.workspace.ok_or_else(|| missing("workspace"))?;
    Ok((ws, start))
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
    let live = ws.state() == WorkspaceState::Active
        && ws
            .lease
            .as_ref()
            .is_some_and(|lease| matches!(lease.state(), LeaseState::Active | LeaseState::InGrace));
    if !live {
        return Err(conflict(format!(
            "workspace {} is {} and its lease has ended; only an active workspace can be attached",
            ws.id,
            json::state_name(ws.state)
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
        .call(ctx.client.renew_lease("", &ws.id, &lease.id))
        .await?;
    if let Some(renewed) = renewed.lease {
        *lease = renewed;
    }
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
        advice: None,
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
        .envs(reef_env(ws, ctx.client.socket(), ctx.agent.as_deref()))
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
                final_op_id: response.operation.map(|operation| operation.id),
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
            final_op_id: response.final_snapshot.map(|operation| operation.id),
            ..Outcome::default()
        },
        Err(error) => Outcome {
            error: Some(error),
            ..Outcome::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
