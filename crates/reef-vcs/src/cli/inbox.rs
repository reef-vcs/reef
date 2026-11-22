//! `reef inbox` (R3.33, ADR 0088): what needs you now, what you watch, and
//! what landed since you last looked, across every registered repository.
//! It reads; it never moves the mark, which the web UI moves once a person
//! has looked. `reef inbox watch` (#372) prints the inbox, then every
//! change to it, from `WatchInbox`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::Duration;

use reef_api::v1::{self, GetInboxRequest, WatchInboxRequest};
use reef_client::ClientError;
use reef_client::json::{reverse_hex, rfc3339 as time};
use schemars::JsonSchema;
use serde::Serialize;

use super::conflict::clean;
use super::out::TextRender;
use super::stream::{Backoff, Stopped, run_until_stopped, until};
use super::{CliError, Ctx};

/// How long `inbox watch` watches by default.
const WATCH_FOR: Duration = Duration::from_secs(30);

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct InboxArgs {
    #[command(subcommand)]
    command: Option<InboxCommand>,
    /// Only these repositories (id, id prefix, or path); repeat for more
    /// [default: every registered one].
    #[arg(long, value_name = "REPO")]
    repo: Vec<String>,
}

#[derive(Debug, clap::Subcommand)]
enum InboxCommand {
    /// Print the inbox, then every change to it (JSON lines on a pipe).
    #[command(
        after_help = "Example: reef inbox watch --follow   # one line per repository that changed"
    )]
    Watch(WatchArgs),
}

/// `reef inbox watch`.
#[derive(Debug, clap::Args)]
struct WatchArgs {
    /// Only this repository (id, id prefix, or path); repeatable [default: every one, and every one registered later].
    #[arg(long, value_name = "REPO")]
    repo: Vec<String>,
    /// Stop after printing this many lines.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    count: Option<u64>,
    /// Stop after this many seconds [default: 30]; 0 prints the inbox only.
    #[arg(long = "for", value_name = "SECONDS", conflicts_with = "follow")]
    seconds: Option<u64>,
    /// Keep watching until interrupted or --count.
    #[arg(long)]
    follow: bool,
}

/// One item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Item {
    /// Stable across reads: `<object>:<repository id>:<object id>`.
    key: String,
    /// The repository.
    repo_id: String,
    /// The bucket it is in: `needs_you_now` or `watching`.
    bucket: String,
    /// Why it is in its bucket, such as `help` or `signoff`.
    reason: String,
    /// The daemon's sentence for it.
    headline: String,
    /// A proposal's description, first line; null for other items.
    subject: Option<String>,
    /// When it began to need its bucket, RFC 3339.
    since: Option<String>,
    /// The change it is about, in reverse hex; null for none.
    change: Option<String>,
    /// A command that acts on it; null for none.
    next: Option<String>,
    /// A help request's question or an instruction's text, as the agent or
    /// person wrote it.
    text: Option<String>,
}

/// One landed change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Landing {
    /// The change, in reverse hex.
    change: String,
    /// Its description's first line.
    subject: String,
    /// The agent that proposed it; null for none.
    agent: Option<String>,
    /// When it landed, RFC 3339.
    landed_at: Option<String>,
}

/// What landed in one repository since the mark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Digest {
    /// The repository.
    repo_id: String,
    /// How many changes landed since.
    count: u64,
    /// Since when, RFC 3339: the mark's time, or 24 hours ago with none.
    since: Option<String>,
    /// Whether a mark set the start.
    from_mark: bool,
    /// The newest, at most 10.
    newest: Vec<Landing>,
}

/// A repository the daemon could not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Failed {
    /// The repository.
    repo_id: String,
    /// The error's reason.
    reason: String,
    /// What went wrong.
    message: String,
}

/// `reef inbox`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Inbox {
    /// What needs you now, newest first.
    needs_you_now: Vec<Item>,
    /// How many there are, over every repository.
    needs_you_now_total: u64,
    /// What you watch, newest first.
    watching: Vec<Item>,
    /// How many there are.
    watching_total: u64,
    /// What landed since you last looked, per repository with any.
    landed: Vec<Digest>,
    /// Repositories that could not be read.
    failed: Vec<Failed>,
}

fn reason_name(reason: v1::InboxReason) -> &'static str {
    match reason {
        v1::InboxReason::Unspecified => "unspecified",
        v1::InboxReason::Help => "help",
        v1::InboxReason::Signoff => "signoff",
        v1::InboxReason::ConflictUnowned => "conflict_unowned",
        v1::InboxReason::ConflictBlocked => "conflict_blocked",
        v1::InboxReason::Returned => "returned",
        v1::InboxReason::NoExecutor => "no_executor",
        v1::InboxReason::Attention => "attention",
        v1::InboxReason::Confirm => "confirm",
        v1::InboxReason::InReview => "in_review",
        v1::InboxReason::InFlight => "in_flight",
    }
}

fn bucket_name(bucket: v1::InboxBucket) -> &'static str {
    match bucket {
        v1::InboxBucket::Unspecified => "unspecified",
        v1::InboxBucket::NeedsYouNow => "needs_you_now",
        v1::InboxBucket::Watching => "watching",
    }
}

fn some(text: String) -> Option<String> {
    Some(text).filter(|text| !text.is_empty())
}

impl From<&v1::InboxItem> for Item {
    fn from(item: &v1::InboxItem) -> Self {
        let text = match &item.record {
            Some(v1::inbox_item::Record::Comment(comment)) => comment
                .help
                .as_ref()
                .map(|help| help.question.clone())
                .filter(|question| !question.is_empty())
                .or_else(|| some(comment.text.clone())),
            _ => None,
        };
        let subject = match &item.record {
            Some(v1::inbox_item::Record::Proposal(entry)) => entry
                .change
                .as_ref()
                .and_then(|change| change.description.lines().next())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
            _ => None,
        };
        Item {
            key: item.key.clone(),
            subject,
            repo_id: item.repo_id.clone(),
            bucket: bucket_name(item.bucket()).to_owned(),
            reason: reason_name(item.reason()).to_owned(),
            headline: item.headline.clone(),
            since: item.since.as_ref().map(time),
            change: some(item.change_id.clone()).map(|hex| reverse_hex(&hex)),
            next: (!item.next.is_empty()).then(|| item.next.join(" ")),
            text,
        }
    }
}

impl Digest {
    fn new(repo_id: &str, landed: &v1::LandedDigest) -> Digest {
        Digest {
            repo_id: repo_id.to_owned(),
            count: landed.count,
            since: landed.since.as_ref().map(time),
            from_mark: landed.from_mark,
            newest: landed
                .newest
                .iter()
                .map(|landing| Landing {
                    change: reverse_hex(&landing.change_id),
                    subject: landing.subject.clone(),
                    agent: some(landing.agent.clone()),
                    landed_at: landing.landed_at.as_ref().map(time),
                })
                .collect(),
        }
    }
}

impl Inbox {
    fn from_response(response: &v1::GetInboxResponse) -> Self {
        let mut inbox = Inbox {
            needs_you_now: Vec::new(),
            needs_you_now_total: 0,
            watching: Vec::new(),
            watching_total: 0,
            landed: Vec::new(),
            failed: Vec::new(),
        };
        for section in &response.sections {
            if let Some(error) = &section.error {
                inbox.failed.push(Failed {
                    repo_id: section.repo_id.clone(),
                    reason: error.reason.clone(),
                    message: error.message.clone(),
                });
                continue;
            }
            inbox
                .needs_you_now
                .extend(section.needs_you_now.iter().map(Item::from));
            inbox.needs_you_now_total += u64::from(section.needs_you_now_total);
            inbox
                .watching
                .extend(section.watching.iter().map(Item::from));
            inbox.watching_total += u64::from(section.watching_total);
            if let Some(landed) = section.landed.as_ref().filter(|landed| landed.count > 0) {
                inbox.landed.push(Digest::new(&section.repo_id, landed));
            }
        }
        let newest = |a: &Item, b: &Item| b.since.cmp(&a.since).then_with(|| a.key.cmp(&b.key));
        inbox.needs_you_now.sort_by(newest);
        inbox.watching.sort_by(newest);
        inbox
    }
}

fn render_items(w: &mut dyn Write, title: &str, items: &[Item], total: u64) -> io::Result<()> {
    writeln!(w, "{title} ({total})")?;
    if items.is_empty() {
        writeln!(w, "  nothing")?;
    }
    for item in items {
        let change = item
            .change
            .as_deref()
            .map(|change| format!(" {}", super::changes::short(change)))
            .unwrap_or_default();
        let line = match &item.subject {
            Some(subject) => format!("{}{change}: {}", clean(subject), clean(&item.headline)),
            None => format!("{}{change}", clean(&item.headline)),
        };
        writeln!(w, "  {line}")?;
        if let Some(text) = &item.text {
            writeln!(w, "    {}", clean(text.lines().next().unwrap_or_default()))?;
        }
        if let Some(next) = &item.next {
            writeln!(w, "    next: {next}")?;
        }
    }
    let shown = u64::try_from(items.len()).unwrap_or(u64::MAX);
    if total > shown {
        writeln!(w, "  and {} more", total - shown)?;
    }
    Ok(())
}

impl TextRender for Inbox {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        render_items(
            w,
            "Needs you now",
            &self.needs_you_now,
            self.needs_you_now_total,
        )?;
        writeln!(w)?;
        render_items(w, "Watching", &self.watching, self.watching_total)?;
        writeln!(w)?;
        let total: u64 = self.landed.iter().map(|digest| digest.count).sum();
        writeln!(w, "Landed since you last looked ({total})")?;
        if self.landed.is_empty() {
            writeln!(w, "  nothing")?;
        }
        for digest in &self.landed {
            writeln!(
                w,
                "  {}: {} since {}",
                super::changes::short(&digest.repo_id),
                digest.count,
                digest.since.as_deref().unwrap_or("-")
            )?;
            for landing in &digest.newest {
                writeln!(
                    w,
                    "    {} {}",
                    super::changes::short(&landing.change),
                    clean(&landing.subject)
                )?;
            }
        }
        for failed in &self.failed {
            writeln!(
                w,
                "\nreef: could not read {}: {}",
                super::changes::short(&failed.repo_id),
                clean(&failed.message)
            )?;
        }
        Ok(())
    }
}

/// `reef inbox`.
pub(crate) async fn run(ctx: &Ctx, args: InboxArgs) -> Result<(), CliError> {
    if let Some(InboxCommand::Watch(args)) = args.command {
        return watch(ctx, args).await;
    }
    let response = Box::pin(ctx.call(ctx.client.get_inbox(GetInboxRequest {
        repos: args.repo,
        ..GetInboxRequest::default()
    })))
    .await?;
    ctx.out.object(&Inbox::from_response(&response))
}

/// One line of `reef inbox watch`: what changed in one repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Change {
    /// The repository.
    repo_id: String,
    /// Items that entered its inbox or changed, moved to the other bucket
    /// included, newest first.
    items: Vec<Item>,
    /// Keys of items no longer listed.
    left: Vec<String>,
    /// How many need you now in it.
    needs_you_now_total: u64,
    /// How many you watch in it.
    watching_total: u64,
    /// What landed in it since you last looked; null when it was
    /// forgotten.
    landed: Option<Digest>,
    /// Set while it cannot be read; its items stand as last printed.
    error: Option<Failed>,
    /// It was forgotten: nothing more comes for it.
    removed: bool,
    /// True from the line that completes the first inbox on; false on
    /// the lines before it.
    synced: bool,
}

impl TextRender for Change {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let repo = super::changes::short(&self.repo_id);
        if self.removed {
            return writeln!(w, "{repo} forgotten");
        }
        for item in &self.items {
            let bucket = if item.bucket == "needs_you_now" {
                "needs you now"
            } else {
                "watching"
            };
            writeln!(w, "{repo} {bucket}: {}", clean(&item.headline))?;
        }
        for key in &self.left {
            writeln!(w, "{repo} left: {key}")?;
        }
        if let Some(error) = &self.error {
            writeln!(w, "{repo} could not be read: {}", clean(&error.message))?;
        }
        if self.items.is_empty() && self.left.is_empty() && self.error.is_none() {
            let landed = self.landed.as_ref().map_or(0, |landed| landed.count);
            writeln!(
                w,
                "{repo}: {} need you now, {} watching, {landed} landed since you last looked",
                self.needs_you_now_total, self.watching_total
            )?;
        }
        Ok(())
    }
}

/// What `inbox watch` printed of one repository.
#[derive(Debug, Default)]
struct Held {
    items: BTreeMap<String, Item>,
    needs_you_now_total: u64,
    watching_total: u64,
    landed: Option<Digest>,
    error: Option<Failed>,
}

async fn watch(ctx: &Ctx, args: WatchArgs) -> Result<(), CliError> {
    let request = WatchInboxRequest { repos: args.repo };
    let seconds = args.seconds.map_or(WATCH_FOR, Duration::from_secs);
    // `--for 0` ends once the first inbox is printed.
    let first_only = args.seconds == Some(0);
    let deadline = (!args.follow && !first_only).then(|| tokio::time::Instant::now() + seconds);
    let mut watcher = Watcher {
        ctx,
        count: args
            .count
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX)),
        first_only,
        printed: 0,
        held: BTreeMap::new(),
        synced: false,
    };
    run_until_stopped(watcher.run(request, deadline)).await?;
    Ok(())
}

/// One `inbox watch`: what it printed, per repository and key, so a
/// stream opened again after the daemon went away prints only what
/// differs from what was printed.
struct Watcher<'a> {
    ctx: &'a Ctx,
    count: Option<usize>,
    first_only: bool,
    printed: usize,
    held: BTreeMap<String, Held>,
    /// A first inbox was printed whole: every later line says `synced`,
    /// across reconnects.
    synced: bool,
}

/// The line for a repository no longer read.
fn removed(repo_id: String, held: Held, synced: bool) -> Change {
    Change {
        repo_id,
        items: Vec::new(),
        left: held.items.into_keys().collect(),
        needs_you_now_total: 0,
        watching_total: 0,
        landed: None,
        error: None,
        removed: true,
        synced,
    }
}

impl Watcher<'_> {
    async fn run(
        &mut self,
        request: WatchInboxRequest,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Stopped, CliError> {
        let mut backoff = Backoff::new();
        loop {
            let watched = tokio::select! {
                () = until(deadline) => return Ok(Stopped::Time),
                watched = self.ctx.call(self.ctx.client.watch_inbox(request.clone())) => watched,
            };
            let error = match watched {
                Ok(mut stream) => {
                    // What this stream's first inbox listed, until synced.
                    let mut first: Option<BTreeMap<String, BTreeSet<String>>> =
                        Some(BTreeMap::new());
                    loop {
                        let next = tokio::select! {
                            () = until(deadline) => return Ok(Stopped::Time),
                            next = stream.message() => next,
                        };
                        match next {
                            Ok(Some(frame)) => {
                                backoff.reset();
                                if let Some(stopped) = self.print(&frame, &mut first)? {
                                    return Ok(stopped);
                                }
                            }
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

    /// Prints a frame's changes; `Some` when the watch is done. `first`
    /// holds the keys the current stream's first inbox listed, by
    /// repository, until it is synced; then what was printed before and is
    /// not listed has left. A line that would repeat what was printed is
    /// not printed, and `synced` is true from the line that completes the
    /// first inbox.
    fn print(
        &mut self,
        frame: &v1::WatchInboxResponse,
        first: &mut Option<BTreeMap<String, BTreeSet<String>>>,
    ) -> Result<Option<Stopped>, CliError> {
        let mut lines = Vec::new();
        for section in &frame.sections {
            lines.extend(self.fold(section, first));
        }
        let completes = frame.synced && first.is_some();
        if completes && let Some(listed) = first.take() {
            lines.extend(self.settle(&listed));
            if let Some(last) = lines.last_mut() {
                last.synced = true;
            }
            self.synced = true;
        }
        for line in &lines {
            if let Some(stopped) = self.emit(line)? {
                return Ok(Some(stopped));
            }
        }
        if completes && self.first_only {
            return Ok(Some(Stopped::Done));
        }
        Ok(None)
    }

    /// Folds one section into what was printed; the line to print, if any.
    fn fold(
        &mut self,
        section: &v1::InboxSectionChange,
        first: &mut Option<BTreeMap<String, BTreeSet<String>>>,
    ) -> Option<Change> {
        let synced = self.synced;
        let repo = section.repo_id.clone();
        if section.removed {
            if let Some(first) = first.as_mut() {
                first.remove(&repo);
            }
            return self
                .held
                .remove(&repo)
                .map(|held| removed(repo, held, synced));
        }
        if let Some(first) = first.as_mut() {
            first
                .entry(repo.clone())
                .or_default()
                .extend(section.items.iter().map(|item| item.key.clone()));
        }
        let held = self.held.entry(repo.clone()).or_default();
        let mut items = Vec::new();
        for item in &section.items {
            let item = Item::from(item);
            // A first inbox after a reconnect repeats what was printed.
            if held.items.get(&item.key) != Some(&item) {
                held.items.insert(item.key.clone(), item.clone());
                items.push(item);
            }
        }
        let left: Vec<String> = section
            .left
            .iter()
            .filter(|key| held.items.remove(*key).is_some())
            .cloned()
            .collect();
        let landed = section
            .landed
            .as_ref()
            .map(|landed| Digest::new(&repo, landed));
        let error = section.error.as_ref().map(|error| Failed {
            repo_id: repo.clone(),
            reason: error.reason.clone(),
            message: error.message.clone(),
        });
        let totals = (
            u64::from(section.needs_you_now_total),
            u64::from(section.watching_total),
        );
        let unchanged = items.is_empty()
            && left.is_empty()
            && totals == (held.needs_you_now_total, held.watching_total)
            && landed == held.landed
            && error == held.error;
        (held.needs_you_now_total, held.watching_total) = totals;
        held.landed.clone_from(&landed);
        held.error.clone_from(&error);
        (!unchanged).then_some(Change {
            repo_id: repo,
            items,
            left,
            needs_you_now_total: totals.0,
            watching_total: totals.1,
            landed,
            error,
            removed: false,
            synced,
        })
    }

    /// The lines for what was printed before a reconnect that the new
    /// stream's first inbox, `listed`, no longer lists.
    fn settle(&mut self, listed: &BTreeMap<String, BTreeSet<String>>) -> Vec<Change> {
        let mut lines = Vec::new();
        let gone: Vec<String> = self
            .held
            .keys()
            .filter(|repo| !listed.contains_key(*repo))
            .cloned()
            .collect();
        for repo in gone {
            if let Some(held) = self.held.remove(&repo) {
                lines.push(removed(repo, held, self.synced));
            }
        }
        for (repo, held) in &mut self.held {
            let keys = listed.get(repo);
            let left: Vec<String> = held
                .items
                .keys()
                .filter(|key| !keys.is_some_and(|keys| keys.contains(*key)))
                .cloned()
                .collect();
            if left.is_empty() {
                continue;
            }
            for key in &left {
                held.items.remove(key);
            }
            lines.push(Change {
                repo_id: repo.clone(),
                items: Vec::new(),
                left,
                needs_you_now_total: held.needs_you_now_total,
                watching_total: held.watching_total,
                landed: held.landed.clone(),
                error: held.error.clone(),
                removed: false,
                synced: self.synced,
            });
        }
        lines
    }

    fn emit(&mut self, change: &Change) -> Result<Option<Stopped>, CliError> {
        if !self.ctx.out.stream_item(change)? {
            return Ok(Some(Stopped::Done));
        }
        self.printed += 1;
        Ok(self
            .count
            .is_some_and(|count| self.printed >= count)
            .then_some(Stopped::Done))
    }
}
