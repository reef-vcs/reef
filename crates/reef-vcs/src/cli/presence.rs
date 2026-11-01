//! `reef presence watch` (R2.10): what agents are doing now, from
//! `SubscribePresence`, as one event per line. It ends by itself unless
//! `--follow`, as `op watch` does (ADR 0025). Presence is a fleet view, so
//! it watches every repository unless `--repo` names some.
//!
//! Across a daemon restart it resubscribes and stays a correct stream of
//! changes: the new set replaces what it held, so it prints what is new or
//! changed, and an end (with no reason) for each entry that is gone.
//!
//! `reef presence report` and `clear` (R2.11) let any process report an
//! entry of its own. The entry outlives the process until its TTL runs
//! out; its id, printed by the report, is what a later process passes to
//! replace or clear it, directly (`--id`) or through a file (`--id-file`).
//!
//! `reef presence hook` (R2.25) is in [`super::hook`].

use std::collections::{HashMap, HashSet};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1::{
    self, ReportPresenceRequest, SubscribePresenceRequest, SubscribePresenceResponse,
};
use reef_client::json::{Presence, PresenceEvent};
use reef_client::{ClientError, default_owner, presence};
use serde::Serialize;
use tokio::time::Instant;

use super::advice::AdviceCtx;
use super::render_vcs::{PresenceLine, Reported};
use super::scope::{self, ScopeArgs, workspace_arg};
use super::stream::{Backoff, Stopped, run_until_stopped, until};
use super::ws::lease_length;
use super::{CliError, Ctx};

/// How long `presence watch` runs without `--for` or `--follow`.
const WATCH_FOR: Duration = Duration::from_secs(30);

/// `reef presence`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: PresenceCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PresenceCommand {
    /// Print presence as it changes (JSON lines on a pipe).
    #[command(after_help = "Example: reef presence watch --for 0")]
    Watch(WatchArgs),
    /// Report what you are doing, for presence watch (prints the entry's id).
    #[command(
        after_help = "Example: reef presence report --id-file .reef-presence \"running the tests\""
    )]
    Report(ReportArgs),
    /// End presence entries you reported.
    #[command(after_help = "Example: reef presence clear --id-file .reef-presence")]
    Clear(ClearArgs),
    /// Declare edits from a harness's hook event on stdin; never fails.
    #[command(
        after_help = "Example: reef presence hook edit --path-at /tool_input/file_path < event.json"
    )]
    Hook(super::hook::Args),
}

impl Args {
    /// Whether this is `presence hook`, which connects by itself, without
    /// starting a daemon.
    pub(crate) fn is_hook(&self) -> bool {
        matches!(self.command, PresenceCommand::Hook(_))
    }

    /// The hook's arguments, if this is `presence hook`.
    pub(crate) fn into_hook(self) -> Option<super::hook::Args> {
        match self.command {
            PresenceCommand::Hook(args) => Some(args),
            _ => None,
        }
    }
}

/// `reef presence report`. Exactly one of TEXT, `--path`, or `--phase` and
/// `--done` says what the entry is.
#[derive(Debug, clap::Args)]
struct ReportArgs {
    /// What you are doing: an activity entry.
    #[arg(value_name = "TEXT")]
    text: Option<String>,
    /// A file you are editing: an editing entry; repeatable.
    #[arg(long = "path", value_name = "PATH")]
    paths: Vec<String>,
    /// The phase of a task: a progress entry.
    #[arg(long, value_name = "NAME")]
    phase: Option<String>,
    /// Units done: a progress entry.
    #[arg(long, value_name = "N")]
    done: Option<u64>,
    /// Units in all, with --phase or --done.
    #[arg(long, value_name = "N")]
    total: Option<u64>,
    /// What a unit is, such as files, with --phase or --done.
    #[arg(long, value_name = "UNIT")]
    unit: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
    /// How long it lasts unless reported again [default: 30s; at most 10m].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    ttl: Option<Duration>,
    /// Replace this entry, an id a report printed.
    #[arg(long, value_name = "ID", conflicts_with = "id_file")]
    id: Option<String>,
    /// Replace the entry whose id FILE holds, if any, and write the id to it.
    #[arg(long, value_name = "FILE")]
    id_file: Option<PathBuf>,
}

/// `reef presence clear`.
#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("ids").required(true).multiple(true).args(["id", "id_file"])))]
struct ClearArgs {
    /// An entry's id; repeatable.
    #[arg(value_name = "ID")]
    id: Vec<String>,
    /// Clear the entry whose id FILE holds, then remove FILE.
    #[arg(long, value_name = "FILE")]
    id_file: Option<PathBuf>,
}

/// `reef presence watch`.
#[derive(Debug, clap::Args)]
struct WatchArgs {
    /// Only this repository (id, id prefix, or path); repeatable [default: every one].
    #[arg(long, value_name = "REPO")]
    repo: Vec<String>,
    /// Only this workspace (id, prefix, or path); repeatable.
    #[arg(long, value_name = "WS")]
    workspace: Vec<String>,
    /// Only entries by this agent; repeatable.
    #[arg(long, value_name = "AGENT")]
    by: Vec<String>,
    /// Only entries of this kind; repeatable.
    #[arg(long, value_enum, value_name = "KIND")]
    kind: Vec<KindArg>,
    /// Stop after printing this many events.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    count: Option<u64>,
    /// Stop after this many seconds [default: 30]; 0 prints the current set.
    #[arg(long = "for", value_name = "SECONDS", conflicts_with = "follow")]
    seconds: Option<u64>,
    /// Keep watching until interrupted or --count.
    #[arg(long)]
    follow: bool,
}

/// A presence kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum KindArg {
    Activity,
    Editing,
    Lease,
    Progress,
}

impl From<KindArg> for v1::PresenceKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Activity => Self::Activity,
            KindArg::Editing => Self::Editing,
            KindArg::Lease => Self::Lease,
            KindArg::Progress => Self::Progress,
        }
    }
}

/// Runs `reef presence`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        PresenceCommand::Watch(args) => watch(ctx, args).await,
        PresenceCommand::Report(args) => report(ctx, args).await,
        PresenceCommand::Clear(args) => clear(ctx, args).await,
        PresenceCommand::Hook(_) => Ok(()),
    }
}

/// What a report says, from which of its forms was given.
fn says(args: &ReportArgs) -> Result<(v1::PresenceKind, presence::State), CliError> {
    let progress = args.phase.is_some() || args.done.is_some();
    let forms = [args.text.is_some(), !args.paths.is_empty(), progress];
    if forms.iter().filter(|given| **given).count() != 1 {
        return Err(CliError::Usage(
            "say what the entry is with exactly one of: TEXT (an activity), --path (editing), or --phase and --done (progress)"
                .to_owned(),
        ));
    }
    if !progress && (args.total.is_some() || args.unit.is_some()) {
        return Err(CliError::Usage(
            "--total and --unit go with --phase or --done".to_owned(),
        ));
    }
    let state = presence::State::default();
    Ok(if let Some(text) = &args.text {
        let state = presence::State {
            activity: text.clone(),
            ..state
        };
        (v1::PresenceKind::Activity, state)
    } else if progress {
        let state = presence::State {
            progress: Some(v1::Progress {
                phase: args.phase.clone().unwrap_or_default(),
                done: args.done.unwrap_or_default(),
                total: args.total.unwrap_or_default(),
                unit: args.unit.clone().unwrap_or_default(),
            }),
            ..state
        };
        (v1::PresenceKind::Progress, state)
    } else {
        (v1::PresenceKind::Editing, state)
    })
}

async fn report(ctx: &Ctx, args: ReportArgs) -> Result<(), CliError> {
    let (kind, mut state) = says(&args)?;
    let ttl = match args.ttl {
        Some(ttl) if ttl.is_zero() => None,
        ttl => lease_length(ttl)?,
    };
    let held = match &args.id_file {
        Some(file) => read_id_file(file)?,
        None => args.id.clone(),
    };
    let scope = scope::resolve(ctx, &args.scope).await?;
    if kind == v1::PresenceKind::Editing {
        state.paths = scope::given_paths(&scope, &args.paths)?;
    }
    let owner = scope.workspace.as_ref().map(|ws| ws.owner.as_str());
    let request = ReportPresenceRequest {
        presence_id: held.clone().unwrap_or_default(),
        agent: presence::agent(ctx.agent.as_deref(), owner, &default_owner()),
        repo: if scope.workspace.is_some() {
            String::new()
        } else {
            scope.repo_id.clone()
        },
        workspace: scope.workspace_id(),
        change: scope
            .workspace
            .as_ref()
            .map(|ws| ws.change_id.clone())
            .unwrap_or_default(),
        kind: kind.into(),
        activity: state.activity,
        paths: state.paths,
        progress: state.progress,
        ttl,
    };
    let advice = AdviceCtx::new(&scope);
    let answer = ctx.call(ctx.client.report_presence(request.clone())).await;
    let response = match answer {
        Err(CliError::Daemon(error))
            if held.is_some() && error.reason() == reef_api::error::PRESENCE_NOT_FOUND =>
        {
            let fresh = ReportPresenceRequest {
                presence_id: String::new(),
                ..request
            };
            let response = ctx
                .call(ctx.client.report_presence(fresh))
                .await
                .map_err(|error| advice.refine(error))?;
            if let Some(entry) = &response.presence {
                ctx.out.note(&format!(
                    "reef: presence entry {} had ended; reported a new one, {}",
                    held.as_deref().unwrap_or_default(),
                    entry.id
                ));
            }
            response
        }
        response => response.map_err(|error| advice.refine(error))?,
    };
    let entry = response
        .presence
        .as_ref()
        .map(Presence::from)
        .ok_or_else(|| super::repo::missing("presence"))?;
    if let Some(file) = &args.id_file {
        write_id_file(file, &entry.id)?;
    }
    ctx.out.object(&Reported(entry))
}

/// What `presence clear` cleared.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.Cleared")]
pub(crate) struct Cleared {
    cleared: Vec<String>,
}

async fn clear(ctx: &Ctx, args: ClearArgs) -> Result<(), CliError> {
    let mut ids = args.id.clone();
    if let Some(file) = &args.id_file
        && let Some(id) = read_id_file(file)?
    {
        ids.push(id);
    }
    for id in &ids {
        ctx.call(ctx.client.clear_presence(id)).await?;
    }
    if let Some(file) = &args.id_file {
        remove_id_file(file)?;
    }
    let note = if ids.is_empty() {
        "nothing to clear".to_owned()
    } else {
        ids.iter()
            .map(|id| format!("cleared {id}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    ctx.out.done(&Cleared { cleared: ids }, &note)
}

/// The id `file` holds; `None` when it is missing or blank.
pub(super) fn read_id_file(file: &Path) -> Result<Option<String>, CliError> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|id| !id.is_empty())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CliError::Io {
            op: "read",
            path: file.to_owned(),
            source,
        }),
    }
}

/// Writes `id` and a newline to `file` through a new temporary file beside
/// it, created exclusively under a random name, so a reader never sees
/// half an id and nothing planted at a predictable name is written through.
pub(super) fn write_id_file(file: &Path, id: &str) -> Result<(), CliError> {
    let io_error = |source| CliError::Io {
        op: "write",
        path: file.to_owned(),
        source,
    };
    let parent = match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temporary
        .write_all(format!("{id}\n").as_bytes())
        .map_err(io_error)?;
    temporary
        .persist(file)
        .map(drop)
        .map_err(|error| io_error(error.error))
}

pub(super) fn remove_id_file(file: &Path) -> Result<(), CliError> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CliError::Io {
            op: "remove",
            path: file.to_owned(),
            source,
        }),
    }
}

async fn watch(ctx: &Ctx, args: WatchArgs) -> Result<(), CliError> {
    let request = SubscribePresenceRequest {
        repos: args
            .repo
            .iter()
            .map(|repo| workspace_arg(repo))
            .collect::<Result<_, _>>()?,
        workspaces: args
            .workspace
            .iter()
            .map(|workspace| workspace_arg(workspace))
            .collect::<Result<_, _>>()?,
        agents: args.by.clone(),
        kinds: args
            .kind
            .iter()
            .map(|kind| v1::PresenceKind::from(*kind).into())
            .collect(),
    };
    let seconds = args.seconds.map_or(WATCH_FOR, Duration::from_secs);
    // `--for 0` ends at the first synced frame instead.
    let set_only = args.seconds == Some(0);
    let deadline = (!args.follow && !set_only).then(|| Instant::now() + seconds);
    let mut watcher = Watcher {
        ctx,
        count: args
            .count
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX)),
        set_only,
        printed: 0,
        held: HashMap::new(),
    };
    run_until_stopped(watcher.run(request, deadline)).await?;
    Ok(())
}

/// One `presence watch`: what it printed, and the entries it holds live.
struct Watcher<'a> {
    ctx: &'a Ctx,
    count: Option<usize>,
    /// `--for 0`: stop at the first synced frame.
    set_only: bool,
    printed: usize,
    /// Live entries as last printed, by id.
    held: HashMap<String, Presence>,
}

impl Watcher<'_> {
    /// Subscribes and prints until it stops, resubscribing after the
    /// daemon goes away.
    async fn run(
        &mut self,
        request: SubscribePresenceRequest,
        deadline: Option<Instant>,
    ) -> Result<Stopped, CliError> {
        let mut backoff = Backoff::new();
        loop {
            let subscribed = tokio::select! {
                () = until(deadline) => return Ok(Stopped::Time),
                subscribed = self.ctx.call(self.ctx.client.subscribe_presence(request.clone())) => subscribed,
            };
            let error = match subscribed {
                Ok(mut stream) => {
                    // The entries this subscription's set holds, until it
                    // is synced.
                    let mut set = Some(HashSet::new());
                    loop {
                        let next = tokio::select! {
                            () = until(deadline) => return Ok(Stopped::Time),
                            next = stream.message() => next,
                        };
                        match next {
                            Ok(Some(frame)) => {
                                backoff.reset();
                                if let Some(stopped) = self.print(&frame, &mut set)? {
                                    return Ok(stopped);
                                }
                            }
                            // Ended without a status: the connection went.
                            Ok(None) => break None,
                            Err(status) => break Some(CliError::Daemon(ClientError::from(status))),
                        }
                    }
                }
                Err(error) => Some(error),
            };
            if let Some(error) = error
                && !backoff.retry(&error, deadline)
            {
                return Err(error);
            }
            if !backoff.wait(deadline).await {
                return Ok(Stopped::Time);
            }
        }
    }

    /// Prints a frame's events; `Some` when the watch is done. `set` holds
    /// the ids of the current set until a frame completes it; then every
    /// entry held but not in it has ended.
    fn print(
        &mut self,
        frame: &SubscribePresenceResponse,
        set: &mut Option<HashSet<String>>,
    ) -> Result<Option<Stopped>, CliError> {
        for event in &frame.events {
            let event = PresenceEvent::from(event);
            let id = event.presence.id.clone();
            if let Some(set) = set.as_mut() {
                set.insert(id.clone());
            }
            if event.ended {
                self.held.remove(&id);
            } else {
                // A resubscribed set repeats what was printed; any field
                // that differs is a change, even within one millisecond.
                let unchanged = self.held.get(&id) == Some(&event.presence);
                self.held.insert(id, event.presence.clone());
                if unchanged {
                    continue;
                }
            }
            if let Some(stopped) = self.emit(&event)? {
                return Ok(Some(stopped));
            }
        }
        if frame.synced
            && let Some(set) = set.take()
        {
            let gone: Vec<Presence> = self
                .held
                .iter()
                .filter(|(id, _)| !set.contains(*id))
                .map(|(_, presence)| presence.clone())
                .collect();
            for presence in gone {
                self.held.remove(&presence.id);
                let event = PresenceEvent {
                    ended: true,
                    end_reason: None,
                    presence,
                };
                if let Some(stopped) = self.emit(&event)? {
                    return Ok(Some(stopped));
                }
            }
            if self.set_only {
                return Ok(Some(Stopped::Done));
            }
        }
        Ok(None)
    }

    /// Prints one event; `Some` when the watch is done.
    fn emit(&mut self, event: &PresenceEvent) -> Result<Option<Stopped>, CliError> {
        if !self.ctx.out.stream_item(&PresenceLine(event.clone()))? {
            return Ok(Some(Stopped::Done));
        }
        self.printed += 1;
        Ok(self
            .count
            .is_some_and(|count| self.printed >= count)
            .then_some(Stopped::Done))
    }
}
