//! `reef inbox` (R3.33, ADR 0088): what needs you now, what you watch, and
//! what landed since you last looked, across every registered repository.
//! It reads; it never moves the mark, which the web UI moves once a person
//! has looked.

use std::io::{self, Write};

use reef_api::v1::{self, GetInboxRequest};
use reef_client::json::{reverse_hex, rfc3339 as time};
use schemars::JsonSchema;
use serde::Serialize;

use super::conflict::clean;
use super::out::TextRender;
use super::{CliError, Ctx};

#[derive(Debug, clap::Args)]
pub(crate) struct InboxArgs {
    /// Only these repositories (id, id prefix, or path); repeat for more
    /// [default: every registered one].
    #[arg(long, value_name = "REPO")]
    repo: Vec<String>,
}

/// One item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Item {
    /// Stable across reads: `<object>:<repository id>:<object id>`.
    key: String,
    /// The repository.
    repo_id: String,
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
            reason: reason_name(item.reason()).to_owned(),
            headline: item.headline.clone(),
            since: item.since.as_ref().map(time),
            change: some(item.change_id.clone()).map(|hex| reverse_hex(&hex)),
            next: (!item.next.is_empty()).then(|| item.next.join(" ")),
            text,
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
                inbox.landed.push(Digest {
                    repo_id: section.repo_id.clone(),
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
                });
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
    let response = Box::pin(ctx.call(ctx.client.get_inbox(GetInboxRequest {
        repos: args.repo,
        ..GetInboxRequest::default()
    })))
    .await?;
    ctx.out.object(&Inbox::from_response(&response))
}
