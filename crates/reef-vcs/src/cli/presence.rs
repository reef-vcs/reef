//! `reef presence watch` (R2.10): what agents are doing now, from
//! `SubscribePresence`, as one event per line. It ends by itself unless
//! `--follow`, as `op watch` does (ADR 0025). Presence is a fleet view, so
//! it watches every repository unless `--repo` names some.
//!
//! Across a daemon restart it resubscribes and stays a correct stream of
//! changes: the new set replaces what it held, so it prints what is new or
//! changed, and an end (with no reason) for each entry that is gone.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use reef_api::v1::{self, SubscribePresenceRequest, SubscribePresenceResponse};
use reef_client::ClientError;
use reef_client::json::{Presence, PresenceEvent};
use tokio::time::Instant;

use super::render_vcs::PresenceLine;
use super::scope::workspace_arg;
use super::stream::{Backoff, Stopped, run_until_stopped, until};
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
                let unchanged = self
                    .held
                    .get(&id)
                    .is_some_and(|held| held.updated_at_ms == event.presence.updated_at_ms);
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
