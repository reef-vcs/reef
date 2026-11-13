//! `reef land` and `reef landing` (R3.13, ADR 0064; R3.14, ADR 0067): the
//! landing service. `reef land` asks to land a proposed change: the gate
//! judges it with the policy at its destination's head, and a change it
//! allows is queued and landed onto the bookmark in a batch.
//! `reef landing list|show` read the records, and `reef landing
//! batches|batch` the batches. `reef landing push|pushes` push a
//! destination's landed history to its git remote now, and show each
//! pushing destination's state (R3.17, ADR 0077).

use std::io::{self, Write};
use std::time::Duration;

use reef_api::v1::{
    self, GetBatchRequest, GetLandingRequest, LandChangeRequest, ListBatchesRequest,
    ListLandingsRequest, PushLandedRequest,
};
use reef_client::json::{reverse_hex, rfc3339};
use reef_error::{Advice, ErrorKind};
use reef_policy::{Decision, Evaluation, RuleKind, Status};
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
    /// List batches, newest first (R3.14).
    #[command(after_help = "Example: reef landing batches --open")]
    Batches(BatchesArgs),
    /// Show a batch: by id, or the latest a change was in; its members and
    /// trials.
    #[command(after_help = "Example: reef landing batch @-")]
    Batch(BatchArgs),
    /// Push a destination's landed history to its git remote now.
    ///
    /// Its policy's `[landing] push` names the remote (R3.17).
    #[command(after_help = "Example: reef landing push main")]
    Push(PushArgs),
    /// Show each pushing destination's push state.
    ///
    /// pushed, pending, retrying, diverged, refused or disabled (R3.17).
    #[command(after_help = "Example: reef landing pushes")]
    Pushes(PushesArgs),
}

/// `reef landing push`.
#[derive(Debug, clap::Args)]
struct PushArgs {
    /// The destination bookmark [default: `trunk()`'s].
    #[arg(value_name = "DEST")]
    destination: Option<String>,
    /// Return at once; the push runs in the daemon.
    #[arg(long)]
    no_wait: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef landing pushes`.
#[derive(Debug, clap::Args)]
struct PushesArgs {
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef landing batches`.
#[derive(Debug, clap::Args)]
struct BatchesArgs {
    /// Only batches onto this bookmark (repeatable).
    #[arg(long = "to", value_name = "BOOKMARK")]
    destinations: Vec<String>,
    /// Only batches not finished.
    #[arg(long)]
    open: bool,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef landing batch`.
#[derive(Debug, clap::Args)]
struct BatchArgs {
    /// A batch id (`bat_…`), or a revision naming the change whose latest
    /// batch to show.
    #[arg(value_name = "ID_OR_REV", default_value = "@")]
    target: String,
    #[command(flatten)]
    scope: ScopeArgs,
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
    /// The tree its required evidence was judged on in its batch (R3.14);
    /// `null` when none was.
    pub(crate) tested_tree_id: Option<String>,
    /// The trial commit carrying that tree; `null` when none was written.
    pub(crate) tested_commit_id: Option<String>,
    /// The evaluation over the tested tree's evidence, of which only the
    /// `[[require]]` rules count; `null` when there was none. A listing
    /// carries its decision only.
    pub(crate) tested_evaluation: Option<Evaluation>,
    /// Its position in its batch, from 1; `null` when it was never in one.
    pub(crate) batch_position: Option<u32>,
    /// Whether it landed with its description rewritten as a summary
    /// (R3.16, the destination policy's `[history] message = "summary"`).
    pub(crate) summarized: bool,
}

/// How a landing wrote its changes on the destination (R3.16).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LandingHistory {
    /// `summary` or `keep`.
    pub(crate) message: String,
    /// The trailer sets a summary appended, in order: `change`, `landing`,
    /// `agents`, `signoffs`.
    pub(crate) trailers: Vec<String>,
}

impl LandingHistory {
    /// `summary (trailers change, landing)`, or `keep`.
    fn text(&self) -> String {
        if self.trailers.is_empty() {
            self.message.clone()
        } else {
            format!("{} (trailers {})", self.message, self.trailers.join(", "))
        }
    }
}

/// Whether a landed landing reached its destination's git remote (R3.17).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LandingPush {
    /// `pushed` once a push carried it; else its destination's state:
    /// `pending`, `retrying`, `diverged`, `refused` or `disabled`.
    pub(crate) state: String,
    /// The git remote.
    pub(crate) git_remote: String,
    /// The remote branch.
    pub(crate) branch: String,
    /// When a push first carried it.
    pub(crate) pushed_at: Option<String>,
    /// That push's operation.
    pub(crate) operation_id: Option<String>,
    /// Not pushed: why.
    pub(crate) reason: Option<String>,
    /// Not pushed: what to do about it.
    pub(crate) hint: Option<String>,
}

impl LandingPush {
    /// `origin/main (pushed)`, or `origin/main: retrying (GIT_UNREACHABLE)`.
    fn text(&self) -> String {
        let place = format!("{}/{}", self.git_remote, self.branch);
        match &self.reason {
            Some(reason) if self.state != "pushed" => {
                format!("{place}: {} ({reason})", self.state)
            }
            _ => format!("{place} ({})", self.state),
        }
    }
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
    /// `amended`, `destination`, `busy`, `failed`, `checks`,
    /// `unavailable` or `empty`.
    pub(crate) reason: Option<String>,
    /// The batch it is, or was last, in (R3.14).
    pub(crate) batch_id: Option<String>,
    /// How it wrote its changes on the destination (R3.16); `null` until
    /// it lands.
    pub(crate) history: Option<LandingHistory>,
    /// Whether it reached the destination's git remote (R3.17); `null`
    /// unless it landed onto a destination that pushes.
    pub(crate) push: Option<LandingPush>,
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
    /// The push's state, when the destination pushes (R3.17).
    #[serde(skip_serializing_if = "Option::is_none")]
    push: Option<&'a str>,
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
            push: landing.push.as_ref().map(|push| push.state.as_str()),
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
        batch_id: some(&message.batch_id),
        history: message.history.as_ref().map(|history| LandingHistory {
            message: history.message.clone(),
            trailers: history.trailers.clone(),
        }),
        push: message.push.as_ref().map(|push| LandingPush {
            state: word(push.state().as_str_name(), "PUSH_STATE_").unwrap_or_default(),
            git_remote: push.git_remote.clone(),
            branch: push.branch.clone(),
            pushed_at: push.pushed_at.as_ref().map(rfc3339),
            operation_id: some(&push.operation_id),
            reason: some(&push.reason),
            hint: some(&push.hint),
        }),
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
                tested_tree_id: some(&change.tested_tree_id),
                tested_commit_id: some(&change.tested_commit_id),
                tested_evaluation: change.tested_evaluation.as_ref().map(evaluation_of),
                batch_position: (change.batch_position > 0).then_some(change.batch_position),
                summarized: change.summarized,
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
            "landing {} is still queued after {}{}",
            landing.id,
            humantime::format_duration(wait),
            landing
                .batch_id
                .as_deref()
                .map(|batch| format!(", in batch {batch} (`reef landing batch {batch}`)"))
                .unwrap_or_default()
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
        advice: advice_for(&landing),
    })
}

/// What to do about a landing that did not land, with the command to run
/// next.
fn advice_for(landing: &Landing) -> Advice {
    let words = next_words(landing);
    match (landing.reason.as_deref(), landing.batch_id.as_deref()) {
        (Some("unavailable"), _)
            if landing
                .message
                .as_deref()
                .is_some_and(|message| message.starts_with("no executor claimed")) =>
        {
            Advice::run(words, ["reef", "executor", "run"])
        }
        (Some("unavailable" | "checks"), Some(batch)) => {
            Advice::run(words, ["reef", "landing", "batch", batch])
        }
        (Some("empty"), _) if !landing.changes.is_empty() => Advice::run(
            words,
            [
                "reef",
                "proposal",
                "withdraw",
                landing.changes[0].change_id.as_str(),
            ],
        ),
        _ => Advice::run(words, ["reef", "landing", "show", landing.id.as_str()]),
    }
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
            "Rebasing onto {} conflicts: rebase the change yourself (`reef rebase --onto {}`), resolve the conflict it shows, propose it again, then land.",
            landing.destination, landing.destination
        ),
        Some("unlanded_parents") => "A parent of the change has not landed: land it first.".to_owned(),
        Some("amended") => "The change moved since it was proposed: `reef propose` it again, then land.".to_owned(),
        Some("destination") => "The destination bookmark is gone or conflicted: fix it, or propose for another with `reef propose --to`.".to_owned(),
        Some("busy") => "The destination kept moving: land again.".to_owned(),
        Some("empty") => format!(
            "Its changes are already on {}, so there is nothing to land: withdraw the proposal (`reef proposal withdraw`), or abandon the change.",
            landing.destination
        ),
        Some("checks") => {
            let failed: Vec<String> = first
                .and_then(|change| change.tested_evaluation.as_ref())
                .map(|evaluation| {
                    evaluation
                        .rules
                        .iter()
                        .flat_map(|rule| rule.failed.iter())
                        .map(|need| {
                            if need.target.is_empty() {
                                need.kind.clone()
                            } else {
                                format!("{}:{}", need.kind, need.target)
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "Required checks ({}) failed on the batch's tree with this change and passed without it: fix the change, propose it again, then land. `reef landing batch` shows the trials.",
                if failed.is_empty() { "see the batch".to_owned() } else { failed.join(", ") }
            )
        }
        Some("unavailable") => "No executor gave a verdict on the batch's tree; this is not the change's fault, so land it again. If no executor serves this repository's required evidence kinds (its trusted evidence is added some other way), set `[landing] checks = \"proposal\"` in reef-policy.toml on the destination (a policy change, which needs the owners' sign-off), and landings are judged on the proposed tree alone.".to_owned(),
        Some(_) => "The landing failed; land again.".to_owned(),
        None => "It is still queued; `reef landing show` follows it.".to_owned(),
    }
}

/// `reef landing`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        LandingCommand::List(args) => Box::pin(list(ctx, args)).await,
        LandingCommand::Show(args) => Box::pin(show(ctx, args)).await,
        LandingCommand::Batches(args) => Box::pin(batches(ctx, args)).await,
        LandingCommand::Batch(args) => Box::pin(batch(ctx, args)).await,
        LandingCommand::Push(args) => Box::pin(push(ctx, args)).await,
        LandingCommand::Pushes(args) => Box::pin(pushes(ctx, args)).await,
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
    #[expect(clippy::too_many_lines, reason = "one line per field, in order")]
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
            if change.summarized {
                words.push_str(" (summarized)");
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
            if let Some(tested) = &change.tested_tree_id {
                let commit = change
                    .tested_commit_id
                    .as_ref()
                    .map(|commit| format!(" (trial commit {})", short(commit)))
                    .unwrap_or_default();
                pairs.push(("tested", format!("tree {}{commit}", short(tested))));
            }
        }
        if let Some(batch) = &self.batch_id {
            let position = self
                .changes
                .first()
                .and_then(|change| change.batch_position)
                .map(|position| format!(" (position {position})"))
                .unwrap_or_default();
            pairs.push(("batch", format!("{batch}{position}")));
        }
        if let Some(history) = &self.history {
            pairs.push(("history", history.text()));
        }
        if let Some(push) = &self.push {
            pairs.push(("push", push.text()));
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
            if matches!(self.reason.as_deref(), Some("checks" | "unavailable"))
                && let Some(evaluation) = &change.tested_evaluation
                && !evaluation.rules.is_empty()
            {
                writeln!(w)?;
                writeln!(w, "on the batch's tested tree:")?;
                let mut required = evaluation.clone();
                required.rules.retain(|rule| rule.kind == RuleKind::Require);
                render_rules(w, &required)?;
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

/// One member of a batch.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BatchMember {
    /// Its position in the chain, from 1.
    pub(crate) position: u32,
    /// Its landing.
    pub(crate) landing_id: String,
    /// The change, in hex.
    pub(crate) change_id: String,
    /// The same, in jj's reverse hex.
    pub(crate) change_id_reverse_hex: String,
    /// The commit it was queued at.
    pub(crate) queued_commit_id: String,
    /// The chain's tree with it applied.
    pub(crate) planned_tree_id: String,
    /// `landed`, `culprit`, `requeued`, `dequeued`, `left` or
    /// `unavailable`; `null` while undecided.
    pub(crate) result: Option<String>,
}

/// One tested prefix of a batch.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BatchTrial {
    /// How many members its tree has, from the first.
    pub(crate) prefix: u32,
    /// The tested tree.
    pub(crate) tree_id: String,
    /// The trial commit carrying it; `null` when no executor run was
    /// needed.
    pub(crate) commit_id: Option<String>,
    /// `passed`, `failed`, `unavailable` or `cancelled`; `null` while open.
    pub(crate) result: Option<String>,
    /// The executor jobs asked for its missing evidence.
    pub(crate) job_ids: Vec<String>,
    /// For a failed trial the failed pairs, for an unavailable one those
    /// still missing: `kind` or `kind:target`.
    pub(crate) needs: Vec<String>,
    /// How many there are.
    pub(crate) needs_total: u32,
    /// How many times its jobs were asked for.
    pub(crate) attempts: u32,
}

/// One batch (`reef landing batch`, each of `reef landing batches`).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LandingBatch {
    /// `bat_` and 16 hex digits.
    pub(crate) id: String,
    /// The destination bookmark.
    pub(crate) destination: String,
    /// Its head when the batch formed.
    pub(crate) base_commit_id: String,
    /// `testing`, `bisecting` or `finished`.
    pub(crate) state: String,
    /// For a finished batch: `landed`, `split`, `unavailable`, `reformed`,
    /// `interrupted` or `busy`.
    pub(crate) outcome: Option<String>,
    /// The members, in chain order.
    pub(crate) members: Vec<BatchMember>,
    /// The tested prefixes, in the order tested.
    pub(crate) trials: Vec<BatchTrial>,
    /// For `split`, the culprit's position.
    pub(crate) culprit_position: Option<u32>,
    /// How many members landed.
    pub(crate) landed_count: u32,
    /// The destination policy's `[landing] checks`: `batch` or `proposal`.
    pub(crate) checks: String,
    /// The operation that landed members.
    pub(crate) operation_id: Option<String>,
    /// More about how it ended.
    pub(crate) message: Option<String>,
    /// When it formed.
    pub(crate) created_at: Option<String>,
    /// When it finished.
    pub(crate) finished_at: Option<String>,
}

/// The brief view of a batch.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BriefBatch<'a> {
    id: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
    size: usize,
    landed_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    culprit_position: Option<u32>,
}

impl<'a> BriefBatch<'a> {
    pub(crate) fn of(batch: &'a LandingBatch) -> BriefBatch<'a> {
        BriefBatch {
            id: &batch.id,
            state: &batch.state,
            outcome: batch.outcome.as_deref(),
            size: batch.members.len(),
            landed_count: batch.landed_count,
            culprit_position: batch.culprit_position,
        }
    }
}

/// A need as `kind` or `kind:target`.
fn need_words(need: &v1::PolicyEvidenceNeed) -> String {
    if need.target.is_empty() {
        need.kind.clone()
    } else {
        format!("{}:{}", need.kind, need.target)
    }
}

/// `message` as the CLI's JSON.
pub(crate) fn batch_of(message: &v1::LandingBatch) -> LandingBatch {
    LandingBatch {
        id: message.id.clone(),
        destination: message.destination.clone(),
        base_commit_id: message.base_commit_id.clone(),
        state: word(message.state().as_str_name(), "BATCH_STATE_").unwrap_or_default(),
        outcome: word(message.outcome().as_str_name(), "BATCH_OUTCOME_"),
        members: message
            .members
            .iter()
            .map(|member| BatchMember {
                position: member.position,
                landing_id: member.landing_id.clone(),
                change_id: member.change_id.clone(),
                change_id_reverse_hex: reverse_hex(&member.change_id),
                queued_commit_id: member.queued_commit_id.clone(),
                planned_tree_id: member.planned_tree_id.clone(),
                result: word(member.result().as_str_name(), "BATCH_MEMBER_RESULT_"),
            })
            .collect(),
        trials: message
            .trials
            .iter()
            .map(|trial| BatchTrial {
                prefix: trial.prefix,
                tree_id: trial.tree_id.clone(),
                commit_id: some(&trial.commit_id),
                result: word(trial.result().as_str_name(), "TRIAL_RESULT_"),
                job_ids: trial.job_ids.clone(),
                needs: trial.needs.iter().map(need_words).collect(),
                needs_total: trial.needs_total,
                attempts: trial.attempts,
            })
            .collect(),
        culprit_position: (message.culprit_position > 0).then_some(message.culprit_position),
        landed_count: message.landed_count,
        checks: message.checks.clone(),
        operation_id: some(&message.operation_id),
        message: some(&message.message),
        created_at: message.created_at.as_ref().map(rfc3339),
        finished_at: message.finished_at.as_ref().map(rfc3339),
    }
}

async fn batches(ctx: &Ctx, args: BatchesArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let listed = ctx
        .call(ctx.client.list_batches(
            ListBatchesRequest {
                repo: scope.repo_id.clone(),
                destinations: args.destinations.clone(),
                open_only: args.open,
                ..ListBatchesRequest::default()
            },
            args.limit,
        ))
        .await
        .map_err(|error| advice.refine(error))?;
    if listed.more {
        ctx.out.note(&format!(
            "reef: more batches than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    let batches: Vec<LandingBatch> = listed.items.iter().map(batch_of).collect();
    ctx.out.list_with(&batches, |batches| {
        batches.iter().map(BriefBatch::of).collect::<Vec<_>>()
    })
}

async fn batch(ctx: &Ctx, args: BatchArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: (!args.target.starts_with("bat_")).then(|| args.target.clone()),
        ..AdviceCtx::new(&scope)
    };
    let (id, change) = if args.target.starts_with("bat_") {
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
        .call(ctx.client.get_batch(GetBatchRequest {
            repo: scope.repo_id.clone(),
            id,
            change,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let batch = batch_of(&response.batch.unwrap_or_default());
    ctx.out.object_with(&batch, BriefBatch::of)
}

impl TextRender for LandingBatch {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("batch", self.id.clone()),
            (
                "state",
                match &self.outcome {
                    Some(outcome) => format!("{} ({outcome})", self.state),
                    None => self.state.clone(),
                },
            ),
            ("destination", self.destination.clone()),
            ("base", short(&self.base_commit_id)),
            ("checks", self.checks.clone()),
            (
                "landed",
                format!("{} of {}", self.landed_count, self.members.len()),
            ),
        ];
        if let Some(culprit) = self.culprit_position {
            pairs.push(("culprit", format!("position {culprit}")));
        }
        if let Some(message) = &self.message {
            pairs.push(("why", message.clone()));
        }
        block(w, &pairs)?;
        writeln!(w)?;
        for member in &self.members {
            writeln!(
                w,
                "  {:>2}. {}  {}",
                member.position,
                short(&member.change_id_reverse_hex),
                member.result.as_deref().unwrap_or("-")
            )?;
        }
        for trial in &self.trials {
            writeln!(w)?;
            let commit = trial
                .commit_id
                .as_ref()
                .map(|commit| format!(", commit {}", short(commit)))
                .unwrap_or_default();
            writeln!(
                w,
                "trial: prefix {} of {}, tree {}{commit}, {}",
                trial.prefix,
                self.members.len(),
                short(&trial.tree_id),
                trial.result.as_deref().unwrap_or("testing")
            )?;
            if !trial.job_ids.is_empty() {
                writeln!(w, "  jobs: {}", trial.job_ids.join(", "))?;
            }
            if !trial.needs.is_empty() {
                let label = if trial.result.as_deref() == Some("failed") {
                    "failed"
                } else {
                    "missing"
                };
                writeln!(w, "  {label}: {}", trial.needs.join(", "))?;
            }
        }
        Ok(())
    }
}

impl TextRow for LandingBatch {
    const HEADER: &'static [&'static str] = &["BATCH", "STATE", "TO", "SIZE", "LANDED", "CREATED"];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            match &self.outcome {
                Some(outcome) => outcome.clone(),
                None => self.state.clone(),
            },
            self.destination.clone(),
            self.members.len().to_string(),
            self.landed_count.to_string(),
            self.created_at.clone().unwrap_or_else(|| "-".to_owned()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_landing_advises_withdrawing() {
        let landing = landing_of(&v1::Landing {
            id: "lnd_0000000000000001".to_owned(),
            destination: "main".to_owned(),
            outcome: v1::LandingOutcome::Dequeued.into(),
            reason: v1::LandingReason::Empty.into(),
            changes: vec![v1::LandedChange {
                change_id: "0123456789abcdef0123456789abcdef".to_owned(),
                ..v1::LandedChange::default()
            }],
            history: Some(v1::LandingHistory {
                message: "summary".to_owned(),
                trailers: vec!["change".to_owned()],
            }),
            ..v1::Landing::default()
        });
        assert_eq!(landing.reason.as_deref(), Some("empty"));
        assert_eq!(landing.history.as_ref().unwrap().message, "summary");
        let advice = advice_for(&landing);
        assert!(advice.hint.contains("already on main"), "{advice:?}");
        assert_eq!(
            advice.next,
            [
                "reef",
                "proposal",
                "withdraw",
                "0123456789abcdef0123456789abcdef"
            ]
        );
    }
}

/// One destination's push state (`reef landing pushes`, `reef landing
/// push`; R3.17).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct DestinationPush {
    /// The destination bookmark.
    pub(crate) destination: String,
    /// The git remote its policy names.
    pub(crate) git_remote: String,
    /// The remote branch: the destination's own name.
    pub(crate) branch: String,
    /// `pushed`, `pending`, `retrying`, `diverged`, `refused` or
    /// `disabled`.
    pub(crate) state: String,
    /// The destination's head as last seen.
    pub(crate) local_commit_id: Option<String>,
    /// The remote branch as last seen; `null` when absent.
    pub(crate) remote_commit_id: Option<String>,
    /// Landings the remote lacks.
    pub(crate) unpushed_count: u32,
    /// The last failure's reason constant.
    pub(crate) reason: Option<String>,
    /// Its message.
    pub(crate) message: Option<String>,
    /// Git's stderr, redacted.
    pub(crate) git_stderr: Option<String>,
    /// What to do about it.
    pub(crate) hint: Option<String>,
    /// The command that does it, as argv.
    pub(crate) next: Vec<String>,
    /// Diverged: the remote's head, `<commit> <author>: <summary>`.
    pub(crate) remote_head: Option<String>,
    /// Failed attempts since the last success.
    pub(crate) attempts: u32,
    /// Retrying: when it tries again.
    pub(crate) next_attempt_at: Option<String>,
    /// The last successful push.
    pub(crate) pushed_at: Option<String>,
    /// Its operation.
    pub(crate) operation_id: Option<String>,
}

/// The brief view of a destination's push state: its state, and what to
/// do when it is not pushed.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BriefPush<'a> {
    destination: &'a str,
    git_remote: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "is_zero")]
    unpushed_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    next: &'a [String],
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl<'a> BriefPush<'a> {
    pub(crate) fn of(push: &'a DestinationPush) -> BriefPush<'a> {
        BriefPush {
            destination: &push.destination,
            git_remote: &push.git_remote,
            state: &push.state,
            reason: push.reason.as_deref(),
            unpushed_count: push.unpushed_count,
            hint: push.hint.as_deref(),
            next: &push.next,
        }
    }
}

/// `message` as the CLI's JSON.
pub(crate) fn push_of(message: &v1::DestinationPush) -> DestinationPush {
    DestinationPush {
        destination: message.destination.clone(),
        git_remote: message.git_remote.clone(),
        branch: message.branch.clone(),
        state: word(message.state().as_str_name(), "PUSH_STATE_").unwrap_or_default(),
        local_commit_id: some(&message.local_commit_id),
        remote_commit_id: some(&message.remote_commit_id),
        unpushed_count: message.unpushed_count,
        reason: some(&message.reason),
        message: some(&message.message),
        git_stderr: some(&message.git_stderr),
        hint: some(&message.hint),
        next: message.next.clone(),
        remote_head: some(&message.remote_head),
        attempts: message.attempts,
        next_attempt_at: message.next_attempt_at.as_ref().map(rfc3339),
        pushed_at: message.pushed_at.as_ref().map(rfc3339),
        operation_id: some(&message.operation_id),
    }
}

impl DestinationPush {
    /// What its state says, in a few words: `pushed 12:03:04Z`,
    /// `retrying (GIT_UNREACHABLE; attempt 3, next 12:05:00Z)`,
    /// `diverged (origin/main at 1a2b3c4d5e6f Ana: Fix CI; 2 landings not
    /// pushed)`.
    fn detail(&self) -> String {
        let reason = self.reason.as_deref().unwrap_or("");
        match self.state.as_str() {
            "pushed" => match &self.pushed_at {
                Some(at) => format!("pushed {at}"),
                None => "pushed".to_owned(),
            },
            "retrying" => format!(
                "retrying ({reason}; attempt {}{})",
                self.attempts,
                self.next_attempt_at
                    .as_deref()
                    .map(|at| format!(", next {at}"))
                    .unwrap_or_default()
            ),
            "diverged" => format!(
                "diverged ({}/{} at {}; {} landings not pushed)",
                self.git_remote,
                self.branch,
                self.remote_head.as_deref().unwrap_or("?"),
                self.unpushed_count
            ),
            state if reason.is_empty() => state.to_owned(),
            state => format!("{state} ({reason})"),
        }
    }
}

impl TextRender for DestinationPush {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("destination", self.destination.clone()),
            ("remote", format!("{}/{}", self.git_remote, self.branch)),
            ("state", self.detail()),
        ];
        if let Some(local) = &self.local_commit_id {
            pairs.push(("local", short(local)));
        }
        if let Some(remote) = &self.remote_commit_id {
            pairs.push(("remote at", short(remote)));
        }
        if self.unpushed_count > 0 {
            pairs.push(("unpushed", self.unpushed_count.to_string()));
        }
        if let Some(message) = &self.message {
            pairs.push(("why", message.clone()));
        }
        if let Some(hint) = &self.hint {
            pairs.push(("hint", hint.clone()));
        }
        block(w, &pairs)
    }
}

impl TextRow for DestinationPush {
    const HEADER: &'static [&'static str] = &["DESTINATION", "REMOTE", "STATE"];

    fn row(&self) -> Vec<String> {
        vec![
            self.destination.clone(),
            format!("{}/{}", self.git_remote, self.branch),
            self.detail(),
        ]
    }
}

/// `reef landing push`: exit 0 when pushed or up to date, 6 while it is
/// pending or retrying, 5 when diverged, refused or disabled, with the
/// state's reason, hint and next command.
async fn push(ctx: &Ctx, args: PushArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let request = PushLandedRequest {
        repo: scope.repo_id.clone(),
        destination: args.destination.clone().unwrap_or_default(),
        wait: !args.no_wait,
    };
    // A push waits at most the daemon's [publish] timeout (5 minutes by
    // default, at most an hour).
    let response = Box::pin(ctx.call_within(
        Duration::from_hours(1) + ctx.timeout,
        ctx.client.push_landed(request),
    ))
    .await
    .map_err(|error| advice.refine(error))?;
    let push = push_of(&response.push.unwrap_or_default());
    let failed = match push.state.as_str() {
        "pushed" => None,
        "pending" if args.no_wait => None,
        "pending" | "retrying" => Some(ErrorKind::Unavailable),
        _ => Some(ErrorKind::Conflict),
    };
    if failed.is_none() || !ctx.out.is_json() {
        ctx.out.object_with(&push, BriefPush::of)?;
    }
    let Some(kind) = failed else {
        return Ok(());
    };
    let reason = push
        .reason
        .as_deref()
        .map_or(reef_api::error::GIT_PUSH_FAILED, push_reason);
    let mut next = push.next.clone();
    if next.is_empty() {
        next = vec!["reef".into(), "landing".into(), "pushes".into()];
    }
    Err(CliError::Precheck {
        kind,
        reason,
        message: format!(
            "{} is not pushed to {}/{}: {}",
            push.destination,
            push.git_remote,
            push.branch,
            push.message.clone().unwrap_or_else(|| push.state.clone())
        ),
        advice: Advice {
            hint: push.hint.clone().unwrap_or_default(),
            next,
        },
    })
}

/// `reef landing pushes`.
async fn pushes(ctx: &Ctx, args: PushesArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let listed = ctx
        .call(ctx.client.list_pushes(&scope.repo_id))
        .await
        .map_err(|error| advice.refine(error))?;
    let pushes: Vec<DestinationPush> = listed.iter().map(push_of).collect();
    if pushes.is_empty() {
        ctx.out.note(
            "reef: no destination pushes to a git remote; add `push = \"<remote>\"` to [landing] in its reef-policy.toml",
        );
    }
    ctx.out.list_with(&pushes, |pushes| {
        pushes.iter().map(BriefPush::of).collect::<Vec<_>>()
    })
}

/// A push state's reason as the constant the error carries.
fn push_reason(text: &str) -> &'static str {
    use reef_api::error as reason;
    [
        reason::GIT_UNREACHABLE,
        reason::GIT_AUTH_FAILED,
        reason::GIT_NOT_FOUND,
        reason::GIT_PUSH_FAILED,
        reason::GIT_TOO_OLD,
        reason::GIT_REMOTE_NOT_FOUND,
        reason::GIT_REMOTE_NOT_ALLOWED,
        reason::PUBLISH_TIMEOUT,
        reason::PUBLISH_IN_PROGRESS,
        reason::PUBLISH_DISABLED,
        reason::REMOTE_REJECTED,
        reason::REMOTE_POLICY_CHANGED,
        reason::BRANCH_DIVERGED,
        reason::BRANCH_MOVED,
        reason::BRANCH_CONFLICTED,
        reason::CHANGE_CONFLICTED,
        reason::RANGE_TOO_LARGE,
        reason::IDENTITY_UNSET,
        reason::DESTINATION_NOT_FOUND,
        reason::DESTINATION_CONFLICTED,
    ]
    .into_iter()
    .find(|known| *known == text)
    .unwrap_or(reason::GIT_PUSH_FAILED)
}
