//! `reef conflict queue|assignments|assignment|claim|release|assign|take`
//! (R3.27, ADR 0068): who resolves each conflict a change introduces.
//!
//! An assignment is the daemon's: the change's originating agent by
//! default, the resolver queue when that agent's session has ended, a
//! resolver that claimed it, or a person who took it over. Claims are held
//! by a workspace's session, so `claim --next` outside a workspace creates
//! one editing the conflicted change first.

use std::io::{self, Write};
use std::path::PathBuf;

use reef_api::v1::{
    self, AssignConflictRequest, ClaimConflictRequest, CreateWorkspaceRequest,
    GetConflictAssignmentRequest, ListConflictAssignmentsRequest, ReleaseConflictRequest,
};
use reef_client::default_owner;
use reef_client::json::{self, ConflictAssignment, ConflictAssignmentEvent, reverse_hex};
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::AdviceCtx;
use super::conflict::clean;
use super::out::{TextRender, TextRow};
use super::scope::{self, Scope, ScopeArgs};
use super::{CliError, Ctx, Global, RepoArg};

/// Tries `claim --next` makes when another resolver wins the race.
const CLAIM_TRIES: usize = 3;

/// `reef conflict queue`.
#[derive(Debug, clap::Args)]
pub(crate) struct QueueArgs {
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict assignments`.
#[derive(Debug, clap::Args)]
pub(crate) struct AssignmentsArgs {
    /// Only those your workspace's session owns.
    #[arg(long)]
    mine: bool,
    /// Only these states: assigned, queued, taken, unowned, closed (repeatable).
    #[arg(long = "state", value_name = "STATE", value_parser = ["assigned", "queued", "taken", "unowned", "closed"])]
    states: Vec<String>,
    /// Only this change's (a change id prefix, repeatable).
    #[arg(long = "change", value_name = "CHANGE")]
    changes: Vec<String>,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict assignment`.
#[derive(Debug, clap::Args)]
pub(crate) struct AssignmentArgs {
    /// An assignment id (asg_…), or a revision naming its change.
    #[arg(value_name = "ASSIGNMENT")]
    assignment: String,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict claim`.
#[derive(Debug, clap::Args)]
pub(crate) struct ClaimArgs {
    /// An assignment id (asg_…), or a revision naming its change [default: the queue's head].
    #[arg(value_name = "ASSIGNMENT", conflicts_with = "next")]
    assignment: Option<String>,
    /// The queue's head.
    #[arg(long)]
    next: bool,
    /// Refuse if the conflict is no longer this one (its cfl_ id).
    #[arg(long = "expect", value_name = "CONFLICT_ID")]
    expect: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict release`.
#[derive(Debug, clap::Args)]
pub(crate) struct ReleaseArgs {
    /// An assignment id (asg_…), or a revision [default: the one your workspace owns].
    #[arg(value_name = "ASSIGNMENT")]
    assignment: Option<String>,
    /// Why, for whoever takes it next.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict assign`.
#[derive(Debug, clap::Args)]
pub(crate) struct AssignArgs {
    /// An assignment id (asg_…), or a revision naming its change.
    #[arg(value_name = "ASSIGNMENT")]
    assignment: String,
    /// Assign it to this workspace's session (an id, prefix or path).
    #[arg(long = "to", value_name = "WS", conflicts_with = "queue")]
    to: Option<String>,
    /// Put it in the resolver queue.
    #[arg(long)]
    queue: bool,
    /// Its priority in the queue, -100 to 100 (higher first).
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    priority: Option<i32>,
    /// Why.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
    /// Refuse if the conflict is no longer this one (its cfl_ id).
    #[arg(long = "expect", value_name = "CONFLICT_ID")]
    expect: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict take`.
#[derive(Debug, clap::Args)]
pub(crate) struct TakeArgs {
    /// An assignment id (asg_…), or a revision naming its change.
    #[arg(value_name = "ASSIGNMENT")]
    assignment: String,
    /// Why.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Who holds `row`, in a few words.
fn owner_text(row: &ConflictAssignment) -> String {
    let owner = row.owner.as_ref();
    let agent = owner
        .and_then(|o| o.agent.as_deref())
        .map(|agent| format!("agent {}", clean(agent)));
    let workspace = owner
        .and_then(|o| o.workspace_id.as_deref())
        .map(|ws| format!("ws {}", short(ws)));
    let who = [agent, workspace]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    match row.state.as_deref() {
        Some("taken") => format!(
            "taken by {}",
            clean(
                owner
                    .and_then(|o| o.identity.as_deref())
                    .unwrap_or("a person")
            )
        ),
        Some("unowned") => "nobody".to_owned(),
        Some("queued") if who.is_empty() => "-".to_owned(),
        Some("queued") => format!("(was {who})"),
        Some("assigned") if owner.is_some_and(|o| !o.live) => format!("{who} (ended)"),
        _ if who.is_empty() => "-".to_owned(),
        _ => who,
    }
}

/// Who resolves a conflict, as `reef conflict list` and `show` print it.
pub(crate) fn resolver_text(row: &ConflictAssignment) -> String {
    match row.state.as_deref() {
        Some("assigned") => owner_text(row),
        _ => state_text(row),
    }
}

fn state_text(row: &ConflictAssignment) -> String {
    match (row.state.as_deref(), row.queue_position) {
        (Some("queued"), Some(position)) => format!("queued #{position}"),
        (state, _) => state.unwrap_or("-").to_owned(),
    }
}

impl TextRow for ConflictAssignment {
    const HEADER: &'static [&'static str] = &[
        "ASSIGNMENT",
        "CHANGE",
        "STATE",
        "REASON",
        "OWNER",
        "PRIORITY",
        "CLAIMS",
        "FILES",
    ];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            short(&self.change_id_reverse_hex),
            state_text(self),
            self.reason.clone().unwrap_or_else(|| "-".to_owned()),
            owner_text(self),
            self.priority.to_string(),
            self.claims.to_string(),
            format!("{} ({} new)", self.files_total, self.introduced),
        ]
    }
}

/// What brief mode prints of an assignment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefAssignment<'a> {
    id: &'a str,
    change_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
    priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<u32>,
    conflict_id: &'a str,
}

impl<'a> From<&'a ConflictAssignment> for BriefAssignment<'a> {
    fn from(row: &'a ConflictAssignment) -> Self {
        let owner = row.owner.as_ref();
        Self {
            id: &row.id,
            change_id: &row.change_id_reverse_hex,
            state: row.state.as_deref(),
            agent: owner.and_then(|o| o.agent.as_deref()),
            workspace_id: owner.and_then(|o| o.workspace_id.as_deref()),
            priority: row.priority,
            queue_position: row.queue_position,
            conflict_id: &row.conflict_id,
        }
    }
}

/// `reef conflict assignment`'s answer: the assignment and its events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct AssignmentShown {
    /// The assignment.
    pub(crate) assignment: ConflictAssignment,
    /// Its events, oldest first.
    pub(crate) events: Vec<ConflictAssignmentEvent>,
}

fn render_assignment(row: &ConflictAssignment, w: &mut dyn Write) -> io::Result<()> {
    writeln!(
        w,
        "assignment {}  change {}  {}  ({})",
        row.id,
        short(&row.change_id_reverse_hex),
        state_text(row),
        row.reason.as_deref().unwrap_or("-")
    )?;
    writeln!(w, "  owner     {}", owner_text(row))?;
    writeln!(
        w,
        "  conflict  {}  {} files ({} introduced)",
        row.conflict_id, row.files_total, row.introduced
    )?;
    writeln!(
        w,
        "  priority  {}  claims {}  events {}",
        row.priority, row.claims, row.events
    )?;
    if let Some(after) = &row.queue_after {
        writeln!(w, "  queued after {after} unless its owner is followed")?;
    }
    if let Some(closed) = &row.closed_at {
        writeln!(
            w,
            "  closed {closed}{}",
            row.closed_commit_id
                .as_deref()
                .map(|commit| format!(" at {}", short(commit)))
                .unwrap_or_default()
        )?;
    }
    Ok(())
}

impl TextRender for AssignmentShown {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        render_assignment(&self.assignment, w)?;
        for event in &self.events {
            let owner = event
                .owner
                .as_ref()
                .and_then(|o| {
                    o.agent
                        .as_deref()
                        .or(o.workspace_id.as_deref())
                        .or(o.identity.as_deref())
                })
                .map(|who| format!("  -> {}", clean(who)))
                .unwrap_or_default();
            let by = match (event.by_actor.as_deref(), event.by_agent.as_deref()) {
                (Some(actor), Some(agent)) => format!("  by {actor} {}", clean(agent)),
                (Some(actor), None) => format!("  by {actor}"),
                _ => String::new(),
            };
            writeln!(
                w,
                "{:>4}  {}  {} -> {}  {}{owner}{by}",
                event.seq,
                event.at.as_deref().unwrap_or("-"),
                event.from.as_deref().unwrap_or("-"),
                event.to.as_deref().unwrap_or("-"),
                event.reason.as_deref().unwrap_or("-"),
            )?;
            if let Some(note) = &event.note {
                writeln!(w, "      {}", clean(note))?;
            }
        }
        Ok(())
    }
}

/// What `claim`, `release`, `assign` and `take` print.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct AssignmentChanged {
    /// The assignment afterwards.
    pub(crate) assignment: ConflictAssignment,
    /// The operation that announced it; `null` when nothing changed.
    pub(crate) operation_id: Option<String>,
    /// The workspace `claim` created for it, if it created one.
    pub(crate) workspace: Option<json::Workspace>,
}

impl TextRender for AssignmentChanged {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        render_assignment(&self.assignment, w)?;
        if let Some(workspace) = &self.workspace {
            writeln!(w, "  workspace {} at {}", workspace.id, workspace.path)?;
        }
        if self.assignment.state.as_deref() == Some("assigned") {
            writeln!(
                w,
                "next: reef conflict show -r {} --hunks",
                short(&self.assignment.change_id_reverse_hex)
            )?;
        }
        Ok(())
    }
}

impl AssignmentChanged {
    fn of(
        assignment: Option<&v1::ConflictAssignment>,
        operation: Option<&v1::Operation>,
        workspace: Option<json::Workspace>,
    ) -> Result<AssignmentChanged, CliError> {
        let assignment = assignment.ok_or_else(|| super::repo::missing("assignment"))?;
        Ok(AssignmentChanged {
            assignment: ConflictAssignment::from(assignment),
            operation_id: operation.map(|op| op.id.clone()),
            workspace,
        })
    }
}

fn print_changed(ctx: &Ctx, changed: &AssignmentChanged) -> Result<(), CliError> {
    ctx.out.object_with(changed, |changed| {
        BriefAssignment::from(&changed.assignment)
    })
}

fn reason_of(text: Option<String>) -> String {
    text.unwrap_or_default()
}

pub(crate) async fn queue(ctx: &Ctx, args: QueueArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    list_states(
        ctx,
        &scope,
        vec![v1::ConflictAssignmentState::Queued.into()],
        false,
        Vec::new(),
        args.limit,
    )
    .await
}

pub(crate) async fn assignments(ctx: &Ctx, args: AssignmentsArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    if args.mine && scope.workspace.is_none() {
        return Err(CliError::Usage(
            "--mine needs a workspace: run it inside one, or pass --workspace".to_owned(),
        ));
    }
    let states = args
        .states
        .iter()
        .map(|state| {
            i32::from(match state.as_str() {
                "assigned" => v1::ConflictAssignmentState::Assigned,
                "queued" => v1::ConflictAssignmentState::Queued,
                "taken" => v1::ConflictAssignmentState::Taken,
                "unowned" => v1::ConflictAssignmentState::Unowned,
                _ => v1::ConflictAssignmentState::Closed,
            })
        })
        .collect();
    list_states(ctx, &scope, states, args.mine, args.changes, args.limit).await
}

async fn list_states(
    ctx: &Ctx,
    scope: &Scope,
    states: Vec<i32>,
    mine: bool,
    change_ids: Vec<String>,
    limit: usize,
) -> Result<(), CliError> {
    let advice = AdviceCtx::new(scope);
    let found = ctx
        .call(Box::pin(ctx.client.list_conflict_assignments(
            ListConflictAssignmentsRequest {
                repo: scope.repo_id.clone(),
                workspace: if mine {
                    scope.workspace_id()
                } else {
                    String::new()
                },
                states,
                change_ids,
                ..ListConflictAssignmentsRequest::default()
            },
            limit,
        )))
        .await
        .map_err(|error| advice.refine(error))?;
    if found.more {
        ctx.out.note(&format!(
            "reef: more assignments than --limit {limit}; raise it, or 0 for all"
        ));
    }
    let listed: Vec<ConflictAssignment> =
        found.items.iter().map(ConflictAssignment::from).collect();
    ctx.out.list_with(&listed, |listed| {
        listed.iter().map(BriefAssignment::from).collect::<Vec<_>>()
    })
}

pub(crate) async fn assignment(ctx: &Ctx, args: AssignmentArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let found = ctx
        .call(Box::pin(ctx.client.get_conflict_assignment(
            GetConflictAssignmentRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                assignment: args.assignment,
                ..GetConflictAssignmentRequest::default()
            },
        )))
        .await
        .map_err(|error| advice.refine(error))?;
    let shown = AssignmentShown {
        assignment: ConflictAssignment::from(
            found
                .assignment
                .as_ref()
                .ok_or_else(|| super::repo::missing("assignment"))?,
        ),
        events: found
            .events
            .iter()
            .map(ConflictAssignmentEvent::from)
            .collect(),
    };
    ctx.out
        .object_with(&shown, |shown| BriefAssignment::from(&shown.assignment))
}

pub(crate) async fn claim(ctx: &Ctx, global: &Global, args: ClaimArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let assignment = if args.next {
        String::new()
    } else {
        args.assignment.unwrap_or_default()
    };
    let expected = args.expect.unwrap_or_default();
    if let Some(workspace) = &scope.workspace {
        let claimed = ctx
            .call(Box::pin(ctx.client.claim_conflict(ClaimConflictRequest {
                repo: scope.repo_id.clone(),
                workspace: workspace.id.clone(),
                assignment,
                expected_conflict_id: expected,
            })))
            .await
            .map_err(|error| advice.refine(error))?;
        let changed = AssignmentChanged::of(
            claimed.assignment.as_ref(),
            claimed.operation.as_ref(),
            None,
        )?;
        if claimed
            .change
            .as_ref()
            .is_some_and(|change| !change.workspace_ids.contains(&workspace.id))
        {
            ctx.out.note(&format!(
                "reef: this workspace is not on the conflicted change; `reef ws create --change {}` makes one that is",
                short(&changed.assignment.change_id_reverse_hex)
            ));
        }
        return print_changed(ctx, &changed);
    }
    // Outside a workspace: make one on the change, then claim for it.
    let tries = if assignment.is_empty() {
        CLAIM_TRIES
    } else {
        1
    };
    let mut last_error = None;
    for _ in 0..tries {
        let target = pick(ctx, &scope, &assignment).await?;
        let expected = if expected.is_empty() {
            target.conflict_id.clone()
        } else {
            expected.clone()
        };
        let workspace = match create_on(ctx, global, &scope, &target.change_id).await {
            Ok(workspace) => workspace,
            // The change went away (resolved, abandoned) since it was
            // picked: the next one, if picking the head.
            Err(error) if tries > 1 && lost_race(&error, true) => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let claimed = ctx
            .call(Box::pin(ctx.client.claim_conflict(ClaimConflictRequest {
                repo: scope.repo_id.clone(),
                workspace: workspace.id.clone(),
                assignment: target.id.clone(),
                expected_conflict_id: expected,
            })))
            .await;
        match claimed {
            Ok(claimed) => {
                let changed = AssignmentChanged::of(
                    claimed.assignment.as_ref(),
                    claimed.operation.as_ref(),
                    Some(json::Workspace::from(&workspace)),
                )?;
                return print_changed(ctx, &changed);
            }
            Err(error) => {
                // Another resolver won, or the conflict moved: give the
                // workspace back and try the next.
                let _ = ctx
                    .client
                    .release_workspace(&scope.repo_id, &workspace.id, false, false)
                    .await;
                let lost = lost_race(&error, false);
                last_error = Some(error);
                if !lost {
                    break;
                }
            }
        }
    }
    Err(advice.refine(last_error.ok_or_else(|| super::repo::missing("assignment"))?))
}

/// Whether `error` means another resolver, or a resolution, got to the
/// picked conflict first; with `creating`, a workspace's revision that no
/// longer exists too.
fn lost_race(error: &CliError, creating: bool) -> bool {
    use reef_api::error as reason;
    match error.reason() {
        reason::ASSIGNMENT_HELD
        | reason::CONFLICT_CHANGED
        | reason::ASSIGNMENT_CLOSED
        | reason::ASSIGNMENT_NOT_FOUND => true,
        reason::REVISION_NOT_FOUND => creating,
        _ => false,
    }
}

/// The assignment `text` names, or the queue's head when empty.
async fn pick(ctx: &Ctx, scope: &Scope, text: &str) -> Result<v1::ConflictAssignment, CliError> {
    let advice = AdviceCtx::new(scope);
    if text.is_empty() {
        let found = ctx
            .call(Box::pin(ctx.client.list_conflict_assignments(
                ListConflictAssignmentsRequest {
                    repo: scope.repo_id.clone(),
                    states: vec![v1::ConflictAssignmentState::Queued.into()],
                    ..ListConflictAssignmentsRequest::default()
                },
                1,
            )))
            .await
            .map_err(|error| advice.refine(error))?;
        return found.items.into_iter().next().ok_or_else(|| {
            CliError::Usage(
                "the resolver queue is empty: nothing waits for a resolver (reef conflict assignments)"
                    .to_owned(),
            )
        });
    }
    let found = ctx
        .call(Box::pin(ctx.client.get_conflict_assignment(
            GetConflictAssignmentRequest {
                repo: scope.repo_id.clone(),
                assignment: text.to_owned(),
                page_size: 1,
                ..GetConflictAssignmentRequest::default()
            },
        )))
        .await
        .map_err(|error| advice.refine(error))?;
    found
        .assignment
        .ok_or_else(|| super::repo::missing("assignment"))
}

/// A workspace editing `change_id`, declared from the environment as `ws
/// create` declares one.
async fn create_on(
    ctx: &Ctx,
    global: &Global,
    scope: &Scope,
    change_id: &str,
) -> Result<v1::Workspace, CliError> {
    let flags = super::provenance::SessionFlags::default();
    let state_dir: Option<PathBuf> = global.state_dir().ok();
    let request = CreateWorkspaceRequest {
        repo: scope.repo_id.clone(),
        owner: default_owner(),
        purpose: format!("resolve {}", short(&reverse_hex(change_id))),
        start: Some(v1::create_workspace_request::Start::Change(
            change_id.to_owned(),
        )),
        provenance: flags.declaration(state_dir.as_deref())?,
        ..CreateWorkspaceRequest::default()
    };
    let repo = RepoArg::parse(&scope.repo_id)?;
    let response = super::ws::create_declared(ctx, &repo, request, &flags).await?;
    response
        .workspace
        .ok_or_else(|| super::repo::missing("workspace"))
}

pub(crate) async fn release(ctx: &Ctx, args: ReleaseArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let released = ctx
        .call(Box::pin(ctx.client.release_conflict(
            ReleaseConflictRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                assignment: args.assignment.unwrap_or_default(),
                reason: reason_of(args.reason),
            },
        )))
        .await
        .map_err(|error| advice.refine(error))?;
    let changed = AssignmentChanged::of(
        released.assignment.as_ref(),
        released.operation.as_ref(),
        None,
    )?;
    print_changed(ctx, &changed)
}

pub(crate) async fn assign(ctx: &Ctx, args: AssignArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let target = match (args.to, args.queue) {
        (Some(workspace), _) => Some(v1::assign_conflict_request::Target::Workspace(
            scope::workspace_arg(&workspace)?,
        )),
        (None, true) => Some(v1::assign_conflict_request::Target::Queue(true)),
        (None, false) => None,
    };
    if target.is_none() && args.priority.is_none() {
        return Err(CliError::Usage(
            "say where it goes (--to WS or --queue), or give --priority".to_owned(),
        ));
    }
    send_assign(
        ctx,
        &advice,
        AssignConflictRequest {
            repo: scope.repo_id.clone(),
            assignment: args.assignment,
            expected_conflict_id: args.expect.unwrap_or_default(),
            target,
            priority: args.priority,
            reason: reason_of(args.reason),
            caller_workspace: scope.workspace_id(),
            ..AssignConflictRequest::default()
        },
    )
    .await
}

pub(crate) async fn take(ctx: &Ctx, args: TakeArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    send_assign(
        ctx,
        &advice,
        AssignConflictRequest {
            repo: scope.repo_id.clone(),
            assignment: args.assignment,
            target: Some(v1::assign_conflict_request::Target::Take(true)),
            reason: reason_of(args.reason),
            caller_workspace: scope.workspace_id(),
            ..AssignConflictRequest::default()
        },
    )
    .await
}

async fn send_assign(
    ctx: &Ctx,
    advice: &AdviceCtx,
    request: AssignConflictRequest,
) -> Result<(), CliError> {
    let assigned = ctx
        .call(Box::pin(ctx.client.assign_conflict(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let changed = AssignmentChanged::of(
        assigned.assignment.as_ref(),
        assigned.operation.as_ref(),
        None,
    )?;
    print_changed(ctx, &changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str) -> ConflictAssignment {
        ConflictAssignment {
            id: "asg_0123456789abcdef".into(),
            change_id: "00".into(),
            change_id_reverse_hex: "zz".into(),
            commit_id: "c".into(),
            conflict_id: "cfl_1".into(),
            first_conflict_id: "cfl_1".into(),
            introduced: 1,
            files_total: 1,
            state: Some(state.into()),
            reason: Some("default".into()),
            owner: Some(json::ConflictOwner {
                session_id: Some("ses_1".into()),
                workspace_id: Some("7d73d9d4a6c97d2d".into()),
                workspace_owner: None,
                identity: Some("alice".into()),
                actor: Some("agent".into()),
                agent: Some("bot\u{1b}[31m".into()),
                client_session: None,
                live: true,
                ended_at: None,
            }),
            priority: 0,
            queued_at: None,
            queue_position: (state == "queued").then_some(2),
            queue_after: None,
            claims: 0,
            events: 1,
            opened_at: None,
            updated_at: None,
            closed_at: None,
            closed_commit_id: None,
            resolved_session_id: None,
        }
    }

    #[test]
    fn owners_read_as_words_and_escape() {
        assert_eq!(
            owner_text(&row("assigned")),
            "agent bot\\u{1b}[31m ws 7d73d9d4a6c9"
        );
        assert_eq!(
            owner_text(&row("queued")),
            "(was agent bot\\u{1b}[31m ws 7d73d9d4a6c9)"
        );
        assert_eq!(owner_text(&row("taken")), "taken by alice");
        assert_eq!(owner_text(&row("unowned")), "nobody");
        assert_eq!(state_text(&row("queued")), "queued #2");
    }
}
