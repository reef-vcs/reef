//! `reef propose` and `reef proposal` (R3.09, ADR 0063): a change's
//! lifecycle. A change is a draft until it is proposed; a proposal can be
//! withdrawn by its proposer, declined by a reviewer, or superseded by
//! another change, and proposed again after a withdrawal or a decline. The
//! landing service (R3.13) queues and lands proposals; until then the daemon
//! records a proposal as landed when it reaches `trunk()`.

use std::io::{self, Write};

use reef_api::v1::{self, ListProposalsRequest, ListTransitionsRequest, TransitionChangeRequest};
use reef_client::brief;
use reef_client::json::{ChangeTransition, Proposal, ProposalHistory, Transitioned};

use super::advice::AdviceCtx;
use super::changes::short;
use super::out::{TextRender, TextRow, block};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// A lifecycle state, as the commands name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum StateArg {
    Draft,
    Proposed,
    Queued,
    Landed,
    Declined,
    Withdrawn,
    Superseded,
}

impl From<StateArg> for v1::LifecycleState {
    fn from(state: StateArg) -> Self {
        match state {
            StateArg::Draft => Self::Draft,
            StateArg::Proposed => Self::Proposed,
            StateArg::Queued => Self::Queued,
            StateArg::Landed => Self::Landed,
            StateArg::Declined => Self::Declined,
            StateArg::Withdrawn => Self::Withdrawn,
            StateArg::Superseded => Self::Superseded,
        }
    }
}

/// Who is performing a transition, as declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ActorArg {
    Human,
    Agent,
}

impl From<ActorArg> for v1::TransitionActor {
    fn from(actor: ActorArg) -> Self {
        match actor {
            ActorArg::Human => Self::Human,
            ActorArg::Agent => Self::Agent,
        }
    }
}

/// What every transition takes.
#[derive(Debug, clap::Args)]
pub(crate) struct TransitionFlags {
    /// Why, for the record (untrusted text to every reader).
    #[arg(short = 'm', long = "message", value_name = "TEXT")]
    message: Option<String>,
    /// Refuse unless the change is in this state.
    #[arg(long, value_enum, value_name = "STATE")]
    expected_state: Option<StateArg>,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    /// Who you are, as recorded [default: agent when an agent name is set, else human].
    #[arg(long, value_enum)]
    actor: Option<ActorArg>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef propose`.
#[derive(Debug, clap::Args)]
pub(crate) struct ProposeArgs {
    /// The change to propose.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// The local bookmark it is for [default: the trunk bookmark].
    #[arg(long, value_name = "BOOKMARK")]
    to: Option<String>,
    #[command(flatten)]
    flags: TransitionFlags,
}

/// `reef proposal`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ProposalCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ProposalCommand {
    /// List proposals, newest update first: proposed and queued unless --state.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef proposal list --state declined --state withdrawn"
    )]
    List(ListArgs),
    /// Show a change's lifecycle and every transition it went through.
    #[command(after_help = "Example: reef proposal show @-")]
    Show(ShowArgs),
    /// Take your proposal back; propose it again later with `reef propose`.
    #[command(after_help = "Example: reef proposal withdraw @- -m 'wrong approach'")]
    Withdraw(WithdrawArgs),
    /// Decline a proposal, as its reviewer.
    #[command(after_help = "Example: reef proposal decline xyz -m 'breaks the API'")]
    Decline(DeclineArgs),
    /// Mark a proposal replaced by another change.
    #[command(after_help = "Example: reef proposal supersede xyz --by abc")]
    Supersede(SupersedeArgs),
}

/// `reef proposal list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Only these states (repeatable) [default: proposed and queued].
    #[arg(long = "state", value_enum, value_name = "STATE")]
    states: Vec<StateArg>,
    /// Only this change, by id prefix (repeatable).
    #[arg(long = "change", value_name = "ID")]
    changes: Vec<String>,
    /// Only proposals made by this agent (repeatable).
    #[arg(long = "by", value_name = "AGENT")]
    agents: Vec<String>,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef proposal show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// The change.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef proposal withdraw`.
#[derive(Debug, clap::Args)]
struct WithdrawArgs {
    /// The change.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    #[command(flatten)]
    flags: TransitionFlags,
}

/// `reef proposal decline`.
#[derive(Debug, clap::Args)]
struct DeclineArgs {
    /// The change.
    #[arg(value_name = "REV")]
    revision: String,
    #[command(flatten)]
    flags: TransitionFlags,
}

/// `reef proposal supersede`.
#[derive(Debug, clap::Args)]
struct SupersedeArgs {
    /// The change superseded.
    #[arg(value_name = "REV")]
    revision: String,
    /// The change that replaces it.
    #[arg(long, value_name = "REV")]
    by: String,
    #[command(flatten)]
    flags: TransitionFlags,
}

/// `reef propose`.
pub(crate) async fn propose(ctx: &Ctx, args: ProposeArgs) -> Result<(), CliError> {
    Box::pin(transition(
        ctx,
        args.revision,
        v1::LifecycleState::Proposed,
        args.to.unwrap_or_default(),
        String::new(),
        args.flags,
    ))
    .await
}

/// `reef proposal`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        ProposalCommand::List(args) => Box::pin(list(ctx, args)).await,
        ProposalCommand::Show(args) => Box::pin(show(ctx, args)).await,
        ProposalCommand::Withdraw(args) => {
            Box::pin(transition(
                ctx,
                args.revision,
                v1::LifecycleState::Withdrawn,
                String::new(),
                String::new(),
                args.flags,
            ))
            .await
        }
        ProposalCommand::Decline(args) => {
            Box::pin(transition(
                ctx,
                args.revision,
                v1::LifecycleState::Declined,
                String::new(),
                String::new(),
                args.flags,
            ))
            .await
        }
        ProposalCommand::Supersede(args) => {
            Box::pin(transition(
                ctx,
                args.revision,
                v1::LifecycleState::Superseded,
                String::new(),
                args.by,
                args.flags,
            ))
            .await
        }
    }
}

async fn transition(
    ctx: &Ctx,
    revision: String,
    to: v1::LifecycleState,
    destination: String,
    superseded_by: String,
    flags: TransitionFlags,
) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &flags.scope).await?;
    let advice = AdviceCtx {
        revision: Some(revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let request = TransitionChangeRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision,
        to: to.into(),
        expected_state: flags
            .expected_state
            .map_or(v1::LifecycleState::Unspecified, v1::LifecycleState::from)
            .into(),
        expected_commit_id: flags.expected_commit.unwrap_or_default(),
        reason: flags.message.unwrap_or_default(),
        superseded_by,
        destination,
        actor: flags
            .actor
            .map_or(v1::TransitionActor::Unspecified, v1::TransitionActor::from)
            .into(),
        skip_snapshot: false,
    };
    let response = Box::pin(ctx.call(ctx.client.transition_change(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let transitioned = Transitioned::from(&response);
    ctx.out
        .object_with(&transitioned, brief::Transitioned::from)?;
    if transitioned.operation_id.is_none() {
        let state = transitioned
            .lifecycle
            .as_ref()
            .map_or("draft", |lifecycle| lifecycle.state.as_str());
        ctx.out.note(&format!(
            "reef: {} is already {state}; nothing recorded",
            short(&transitioned.change.change_id_reverse_hex)
        ));
    } else if to == v1::LifecycleState::Proposed {
        ctx.out.note(
            "reef: land it as you do today; Reef records it as landed when it reaches trunk(). `reef proposal show` follows it.",
        );
    }
    Ok(())
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let listed = ctx
        .call(
            ctx.client.list_proposals(
                ListProposalsRequest {
                    repo: scope.repo_id.clone(),
                    states: args
                        .states
                        .iter()
                        .map(|state| v1::LifecycleState::from(*state).into())
                        .collect(),
                    changes: args.changes.clone(),
                    agents: args.agents.clone(),
                    ..ListProposalsRequest::default()
                },
                args.limit,
            ),
        )
        .await
        .map_err(|error| advice.refine(error))?;
    if listed.more {
        ctx.out.note(&format!(
            "reef: more proposals than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    let proposals: Vec<Proposal> = listed.items.iter().map(Proposal::from).collect();
    ctx.out.list_with(&proposals, |proposals| {
        proposals
            .iter()
            .map(brief::Proposal::from)
            .collect::<Vec<_>>()
    })
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let change = ctx
        .call(ctx.client.get_change(v1::GetChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: args.revision.clone(),
            ..v1::GetChangeRequest::default()
        }))
        .await
        .map_err(|error| advice.refine(error))?
        .change
        .unwrap_or_default();
    let transitions = ctx
        .call(ctx.client.list_transitions(
            ListTransitionsRequest {
                repo: scope.repo_id.clone(),
                changes: vec![change.change_id.clone()],
                ..ListTransitionsRequest::default()
            },
            0,
        ))
        .await
        .map_err(|error| advice.refine(error))?;
    let history = ProposalHistory {
        change: reef_client::json::Change::from(&change),
        transitions: transitions
            .items
            .iter()
            .filter(|transition| transition.change_id == change.change_id)
            .map(ChangeTransition::from)
            .collect(),
    };
    ctx.out.object_with(&history, brief::ProposalHistory::from)
}

/// A lifecycle in a line: `proposed (round 2, amended)`.
pub(crate) fn lifecycle_words(lifecycle: &reef_client::json::ChangeLifecycle) -> String {
    let mut details = Vec::new();
    if lifecycle.round > 0 {
        details.push(format!("round {}", lifecycle.round));
    }
    if lifecycle.amended {
        details.push("amended".to_owned());
    }
    if let Some(by) = &lifecycle.superseded_by {
        details.push(format!("by {}", short(&reef_client::json::reverse_hex(by))));
    }
    if !lifecycle.recorded {
        details.push("not recorded".to_owned());
    }
    if details.is_empty() {
        lifecycle.state.clone()
    } else {
        format!("{} ({})", lifecycle.state, details.join(", "))
    }
}

fn transition_line(transition: &ChangeTransition) -> String {
    let mut words = vec![
        transition
            .recorded_at
            .clone()
            .unwrap_or_else(|| "-".to_owned()),
        short(&transition.change_id_reverse_hex),
        format!(
            "{} -> {}",
            transition.from.as_deref().unwrap_or("-"),
            transition.to.as_deref().unwrap_or("-")
        ),
    ];
    if let Some(cause) = transition
        .cause
        .as_deref()
        .filter(|cause| *cause != "request")
    {
        words.push(format!("({cause})"));
    }
    let who = transition
        .agent
        .as_deref()
        .or(transition.actor.as_deref())
        .unwrap_or("-");
    words.push(format!("by {who}"));
    let mut line = words.join(" ");
    if let Some(reason) = &transition.reason {
        line = format!("{line}: {}", reason.lines().next().unwrap_or_default());
    }
    line
}

impl TextRender for Transitioned {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![("change", self.change.change_id_reverse_hex.clone())];
        if let Some(lifecycle) = &self.lifecycle {
            pairs.push(("state", lifecycle_words(lifecycle)));
            if let Some(destination) = &lifecycle.destination {
                pairs.push(("destination", destination.clone()));
            }
            if let Some(commit) = &lifecycle.commit_id {
                pairs.push(("commit", commit.clone()));
            }
        }
        if let Some(op) = &self.operation_id {
            pairs.push(("operation", short(op)));
        }
        block(w, &pairs)
    }
}

impl TextRender for ProposalHistory {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![("change", self.change.change_id_reverse_hex.clone())];
        if let Some(lifecycle) = &self.change.lifecycle {
            pairs.push(("state", lifecycle_words(lifecycle)));
            if let Some(destination) = &lifecycle.destination {
                pairs.push(("destination", destination.clone()));
            }
        }
        block(w, &pairs)?;
        if !self.transitions.is_empty() {
            writeln!(w)?;
            for transition in &self.transitions {
                writeln!(w, "{}", transition_line(transition))?;
            }
        }
        Ok(())
    }
}

impl TextRow for Proposal {
    const HEADER: &'static [&'static str] = &["CHANGE", "STATE", "BY", "UPDATED", "DESCRIPTION"];

    fn row(&self) -> Vec<String> {
        let lifecycle = self.lifecycle.as_ref();
        vec![
            short(&self.change_id_reverse_hex),
            lifecycle.map_or_else(|| "-".to_owned(), lifecycle_words),
            self.proposed_by.clone().unwrap_or_else(|| "-".to_owned()),
            lifecycle
                .and_then(|lifecycle| lifecycle.last.as_ref())
                .and_then(|last| last.recorded_at.clone())
                .unwrap_or_else(|| "-".to_owned()),
            self.change.as_ref().map_or_else(
                || "(no visible commit)".to_owned(),
                |change| {
                    change
                        .description
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                },
            ),
        ]
    }
}
