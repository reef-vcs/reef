//! `reef land` and `reef landing` (R3.13, ADR 0064): the landing service.
//! `reef land` asks to land a proposed change: the gate judges it with the
//! policy at its destination's head, and a change it allows is queued and
//! landed onto the bookmark. `reef landing list|show` read the records.

use std::io::{self, Write};
use std::time::Duration;

use reef_api::v1::{self, GetLandingRequest, LandChangeRequest, ListLandingsRequest};
use reef_client::json::{reverse_hex, rfc3339};
use reef_error::{Advice, ErrorKind};
use reef_policy::{Decision, Evaluation, Status};
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::AdviceCtx;
use super::changes::short;
use super::out::{TextRender, TextRow, block};
use super::policy::{PolicySource, evaluation_of, render_rules};
use super::proposal::ActorArg;
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// The longest `reef land` waits.
const MAX_WAIT: Duration = Duration::from_mins(10);

/// `reef land`.
#[derive(Debug, clap::Args)]
pub(crate) struct LandArgs {
    /// The proposed change to land.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    /// How long to wait for the landing to finish, at most 10m.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration, default_value = "2m", conflicts_with = "no_wait")]
    wait: Duration,
    /// Return once the change is queued (or refused).
    #[arg(long)]
    no_wait: bool,
    /// Who you are, as recorded [default: agent when an agent name is set, else human].
    #[arg(long, value_enum)]
    actor: Option<ActorArg>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef landing`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: LandingCommand,
}

#[derive(Debug, clap::Subcommand)]
enum LandingCommand {
    /// List landings, newest first.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef landing list --outcome blocked --outcome dequeued"
    )]
    List(ListArgs),
    /// Show a landing: by id, or a change's latest; its evaluation in full.
    #[command(after_help = "Example: reef landing show @-")]
    Show(ShowArgs),
}

/// A landing outcome, as the commands name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OutcomeArg {
    Blocked,
    Queued,
    Landed,
    Dequeued,
    Cancelled,
}

impl From<OutcomeArg> for v1::LandingOutcome {
    fn from(outcome: OutcomeArg) -> Self {
        match outcome {
            OutcomeArg::Blocked => Self::Blocked,
            OutcomeArg::Queued => Self::Queued,
            OutcomeArg::Landed => Self::Landed,
            OutcomeArg::Dequeued => Self::Dequeued,
            OutcomeArg::Cancelled => Self::Cancelled,
        }
    }
}

/// `reef landing list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Only landings onto this bookmark (repeatable).
    #[arg(long = "to", value_name = "BOOKMARK")]
    destinations: Vec<String>,
    /// Only these outcomes (repeatable).
    #[arg(long = "outcome", value_enum, value_name = "OUTCOME")]
    outcomes: Vec<OutcomeArg>,
    /// Only this change, by id prefix (repeatable).
    #[arg(long = "change", value_name = "ID")]
    changes: Vec<String>,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef landing show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// A landing id (`lnd_…`), or a revision naming the change whose latest
    /// landing to show.
    #[arg(value_name = "ID_OR_REV", default_value = "@")]
    target: String,
    /// Also print the policy text the landing evaluated.
    #[arg(long)]
    policy: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// One change of a landing.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LandedChange {
    /// The change, in hex.
    pub(crate) change_id: String,
    /// The same, in jj's reverse hex.
    pub(crate) change_id_reverse_hex: String,
    /// The commit it was queued at: the one judged.
    pub(crate) queued_commit_id: String,
    /// The commit now on the destination; `null` unless it landed.
    pub(crate) landed_commit_id: Option<String>,
    /// Whether it was rebased onto the destination.
    pub(crate) rebased: bool,
    /// Paths both the change and the destination changed since its parents.
    pub(crate) overlap: Vec<String>,
    /// How many there are.
    pub(crate) overlap_total: u32,
    /// For a conflicted landing, the conflicted paths.
    pub(crate) conflicts: Vec<String>,
    /// How many there are.
    pub(crate) conflicts_total: u32,
    /// Which policy judged it; `null` when it stopped before judging.
    pub(crate) source: Option<PolicySource>,
    /// The evaluation; `null` when it stopped before judging. A listing
    /// carries its decision only.
    pub(crate) evaluation: Option<Evaluation>,
    /// The evidence a landed change's rules used, kept for good.
    pub(crate) pinned_evidence_ids: Vec<String>,
}

/// One landing (`reef land`, `reef landing show`, each of `reef landing
/// list`).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct Landing {
    /// `lnd_` and 16 hex digits.
    pub(crate) id: String,
    /// The destination bookmark.
    pub(crate) destination: String,
    /// Its head when the landing read it.
    pub(crate) destination_before: Option<String>,
    /// Its head after a landing.
    pub(crate) destination_after: Option<String>,
    /// `blocked`, `queued`, `landed`, `dequeued` or `cancelled`.
    pub(crate) outcome: String,
    /// For blocked and dequeued: `policy`, `conflicted`, `unlanded_parents`,
    /// `amended`, `destination`, `busy` or `failed`.
    pub(crate) reason: Option<String>,
    /// More about the reason.
    pub(crate) message: Option<String>,
    /// The changes, in landing order.
    pub(crate) changes: Vec<LandedChange>,
    /// How many identical requests a blocked landing stands for.
    pub(crate) attempts: u32,
    /// The operation that queued the change.
    pub(crate) enqueue_operation_id: Option<String>,
    /// The operation that landed or returned it.
    pub(crate) operation_id: Option<String>,
    /// Whether GC dropped `operation_id`'s operation.
    pub(crate) operation_collected: bool,
    /// The agent that asked.
    pub(crate) agent: Option<String>,
    /// Who asked, as declared: `human`, `agent` or `daemon`.
    pub(crate) actor: Option<String>,
    /// The identity that asked.
    pub(crate) identity: Option<String>,
    /// The remote token that asked, by name.
    pub(crate) remote_token: Option<String>,
    /// When it was asked for.
    pub(crate) created_at: Option<String>,
    /// When it finished.
    pub(crate) finished_at: Option<String>,
    /// With `reef landing show --policy`, the policy text evaluated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) policy: Option<String>,
}

/// The brief view of a landing: how it ended, where, and the rules that
/// blocked it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BriefLanding<'a> {
    id: &'a str,
    outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_after: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocking: Vec<&'a str>,
}

impl<'a> BriefLanding<'a> {
    pub(crate) fn of(landing: &'a Landing) -> BriefLanding<'a> {
        BriefLanding {
            id: &landing.id,
            outcome: &landing.outcome,
            reason: landing.reason.as_deref(),
            destination_after: landing.destination_after.as_deref(),
            blocking: landing
                .changes
                .iter()
                .filter_map(|change| change.evaluation.as_ref())
                .flat_map(|evaluation| evaluation.rules.iter())
                .filter(|rule| matches!(rule.status, Status::Unsatisfied | Status::Error))
                .map(|rule| rule.name.as_str())
                .collect(),
        }
    }
}

fn some(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

fn word(name: &str, prefix: &str) -> Option<String> {
    let rest = name.strip_prefix(prefix).unwrap_or(name);
    (rest != "UNSPECIFIED").then(|| rest.to_ascii_lowercase())
}

/// `message` as the CLI's JSON.
pub(crate) fn landing_of(message: &v1::Landing) -> Landing {
    Landing {
        id: message.id.clone(),
        destination: message.destination.clone(),
        destination_before: some(&message.destination_before),
        destination_after: some(&message.destination_after),
        outcome: word(message.outcome().as_str_name(), "LANDING_OUTCOME_").unwrap_or_default(),
        reason: word(message.reason().as_str_name(), "LANDING_REASON_"),
        message: some(&message.message),
        changes: message
            .changes
            .iter()
            .map(|change| LandedChange {
                change_id: change.change_id.clone(),
                change_id_reverse_hex: reverse_hex(&change.change_id),
                queued_commit_id: change.queued_commit_id.clone(),
                landed_commit_id: some(&change.landed_commit_id),
                rebased: change.rebased,
                overlap: change.overlap.clone(),
                overlap_total: change.overlap_total,
                conflicts: change.conflicts.clone(),
                conflicts_total: change.conflicts_total,
                source: change.evaluation.as_ref().map(|evaluation| {
                    let source = evaluation.source.clone().unwrap_or_default();
                    PolicySource {
                        revision: source.revision,
                        commit_id: source.commit_id,
                        path: source.path,
                        blob_id: source.blob_id,
                        sha256: source.sha256,
                        version: source.version,
                        from_request: source.from_request,
                    }
                }),
                evaluation: change.evaluation.as_ref().map(evaluation_of),
                pinned_evidence_ids: change.pinned_evidence_ids.clone(),
            })
            .collect(),
        attempts: message.attempts,
        enqueue_operation_id: some(&message.enqueue_operation_id),
        operation_id: some(&message.operation_id),
        operation_collected: message.operation_collected,
        agent: some(&message.agent),
        actor: word(message.actor().as_str_name(), "TRANSITION_ACTOR_"),
        identity: some(&message.identity),
        remote_token: message
            .remote
            .as_ref()
            .map(|remote| remote.token_name.clone()),
        created_at: message.created_at.as_ref().map(rfc3339),
        finished_at: message.finished_at.as_ref().map(rfc3339),
        policy: None,
    }
}

/// `reef land`.
pub(crate) async fn land(ctx: &Ctx, args: LandArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let wait = if args.no_wait {
        Duration::ZERO
    } else {
        args.wait.min(MAX_WAIT)
    };
    let request = LandChangeRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.clone(),
        expected_commit_id: args.expected_commit.unwrap_or_default(),
        wait_ms: u32::try_from(wait.as_millis()).unwrap_or(u32::MAX),
        actor: args
            .actor
            .map_or(v1::TransitionActor::Unspecified, v1::TransitionActor::from)
            .into(),
        skip_snapshot: false,
    };
    let response = Box::pin(ctx.call_within(wait + ctx.timeout, ctx.client.land_change(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let landing = landing_of(&response.landing.unwrap_or_default());
    let refused = match landing.outcome.as_str() {
        "blocked" | "dequeued" => Some((
            ErrorKind::Conflict,
            if landing.outcome == "blocked" {
                reef_api::error::LANDING_BLOCKED
            } else {
                reef_api::error::LANDING_DEQUEUED
            },
        )),
        "queued" if !args.no_wait => {
            Some((ErrorKind::Unavailable, reef_api::error::LANDING_PENDING))
        }
        _ => None,
    };
    if refused.is_none() || !ctx.out.is_json() {
        ctx.out.object_with(&landing, BriefLanding::of)?;
    }
    let Some((kind, reason)) = refused else {
        if landing.outcome == "queued" {
            ctx.out.note(&format!(
                "reef: queued; `reef landing show {}` follows it",
                landing.id
            ));
        }
        return Ok(());
    };
    let message = match reason {
        reef_api::error::LANDING_PENDING => format!(
            "landing {} is still queued after {}",
            landing.id,
            humantime::format_duration(wait)
        ),
        _ => format!(
            "landing {} was {}: {}",
            landing.id,
            landing.outcome,
            landing.message.clone().unwrap_or_default()
        ),
    };
    Err(CliError::Precheck {
        kind,
        reason,
        message,
        advice: Advice::run(
            next_words(&landing),
            ["reef", "landing", "show", landing.id.as_str()],
        ),
    })
}

/// What to do about a landing that did not land.
fn next_words(landing: &Landing) -> String {
    let first = landing.changes.first();
    match landing.reason.as_deref() {
        Some("policy") => {
            let rule = first
                .and_then(|change| change.evaluation.as_ref())
                .and_then(Evaluation::first_blocking);
            match (first.and_then(|c| c.evaluation.as_ref()).map(|e| e.decision), rule) {
                (Some(Decision::NoPolicy), _) => "The destination has no reef-policy.toml, so nothing lands automatically: commit one (`reef policy init` writes a starter).".to_owned(),
                (Some(Decision::InvalidPolicy), _) => "The destination's reef-policy.toml does not load: `reef policy check -r 'trunk()'` lists its problems.".to_owned(),
                (_, Some(rule)) => format!("{}: {}. Add what it lacks (`reef policy eval` shows it), then land again.", rule.name, rule.message),
                _ => "A rule blocks the change; `reef policy eval` shows which.".to_owned(),
            }
        }
        Some("conflicted") => format!(
            "Rebasing onto {} conflicts: rebase the change yourself (`jj rebase -d {}`), resolve, propose it again, then land.",
            landing.destination, landing.destination
        ),
        Some("unlanded_parents") => "A parent of the change has not landed: land it first.".to_owned(),
        Some("amended") => "The change moved since it was proposed: `reef propose` it again, then land.".to_owned(),
        Some("destination") => "The destination bookmark is gone or conflicted: fix it, or propose for another with `reef propose --to`.".to_owned(),
        Some("busy") => "The destination kept moving: land again.".to_owned(),
        Some(_) => "The landing failed; land again.".to_owned(),
        None => "It is still queued; `reef landing show` follows it.".to_owned(),
    }
}

/// `reef landing`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        LandingCommand::List(args) => Box::pin(list(ctx, args)).await,
        LandingCommand::Show(args) => Box::pin(show(ctx, args)).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let listed = ctx
        .call(
            ctx.client.list_landings(
                ListLandingsRequest {
                    repo: scope.repo_id.clone(),
                    destinations: args.destinations.clone(),
                    outcomes: args
                        .outcomes
                        .iter()
                        .map(|outcome| v1::LandingOutcome::from(*outcome).into())
                        .collect(),
                    changes: args.changes.clone(),
                    ..ListLandingsRequest::default()
                },
                args.limit,
            ),
        )
        .await
        .map_err(|error| advice.refine(error))?;
    if listed.more {
        ctx.out.note(&format!(
            "reef: more landings than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    let landings: Vec<Landing> = listed.items.iter().map(landing_of).collect();
    ctx.out.list_with(&landings, |landings| {
        landings.iter().map(BriefLanding::of).collect::<Vec<_>>()
    })
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: (!args.target.starts_with("lnd_")).then(|| args.target.clone()),
        ..AdviceCtx::new(&scope)
    };
    let (id, change) = if args.target.starts_with("lnd_") {
        (args.target.clone(), String::new())
    } else {
        let change = ctx
            .call(ctx.client.get_change(v1::GetChangeRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision: args.target.clone(),
                ..v1::GetChangeRequest::default()
            }))
            .await
            .map_err(|error| advice.refine(error))?
            .change
            .unwrap_or_default();
        (String::new(), change.change_id)
    };
    let response = ctx
        .call(ctx.client.get_landing(GetLandingRequest {
            repo: scope.repo_id.clone(),
            id,
            change,
            include_policy: args.policy,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let mut landing = landing_of(&response.landing.unwrap_or_default());
    if args.policy {
        landing.policy = Some(response.policy);
    }
    ctx.out.object_with(&landing, BriefLanding::of)
}

impl TextRender for Landing {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("landing", self.id.clone()),
            (
                "outcome",
                match &self.reason {
                    Some(reason) => format!("{} ({reason})", self.outcome),
                    None => self.outcome.clone(),
                },
            ),
            ("destination", self.destination.clone()),
        ];
        if let Some(after) = &self.destination_after {
            pairs.push(("now at", short(after)));
        } else if let Some(before) = &self.destination_before {
            pairs.push(("at", short(before)));
        }
        for change in &self.changes {
            let mut words = short(&change.change_id_reverse_hex);
            if change.rebased {
                words.push_str(" (rebased)");
            }
            pairs.push(("change", words));
            if change.overlap_total > 0 {
                pairs.push((
                    "overlap",
                    format!(
                        "{} ({} paths)",
                        change.overlap.join(", "),
                        change.overlap_total
                    ),
                ));
            }
            if change.conflicts_total > 0 {
                pairs.push((
                    "conflicts",
                    format!(
                        "{} ({} paths)",
                        change.conflicts.join(", "),
                        change.conflicts_total
                    ),
                ));
            }
            if !change.pinned_evidence_ids.is_empty() {
                pairs.push(("pinned", change.pinned_evidence_ids.len().to_string()));
            }
        }
        if let Some(message) = &self.message {
            pairs.push(("why", message.clone()));
        }
        if self.attempts > 1 {
            pairs.push(("attempts", self.attempts.to_string()));
        }
        block(w, &pairs)?;
        for change in &self.changes {
            if let Some(evaluation) = &change.evaluation
                && evaluation.decision != Decision::Allow
                && !evaluation.rules.is_empty()
            {
                writeln!(w)?;
                render_rules(w, evaluation)?;
            }
        }
        if let Some(policy) = &self.policy {
            writeln!(w)?;
            write!(w, "{policy}")?;
        }
        Ok(())
    }
}

impl TextRow for Landing {
    const HEADER: &'static [&'static str] = &["LANDING", "OUTCOME", "CHANGE", "TO", "FINISHED"];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            match &self.reason {
                Some(reason) => format!("{} ({reason})", self.outcome),
                None => self.outcome.clone(),
            },
            self.changes.first().map_or_else(
                || "-".to_owned(),
                |change| short(&change.change_id_reverse_hex),
            ),
            self.destination.clone(),
            self.finished_at.clone().unwrap_or_else(|| "-".to_owned()),
        ]
    }
}
