//! `reef ws provider create | attach | release` (R2.26, ADR 0053): the
//! workspace provider contract, for a program that isolates its sessions
//! in directories and asks Reef for them instead of making its own.
//!
//! Each command takes its fields as flags, or as one JSON object on stdin
//! keyed by the same names (`--stdin`), with `--at FIELD=POINTER` taking a
//! field from anywhere in that object (RFC 6901), so a program's own event
//! maps onto the contract without code. A flag wins over a pointer, a
//! pointer over the object's own key, and a key over the default. Each
//! prints one object (`contract: 1`), or with `-o text` the path alone.
//!
//! `hold_while` names the holder, a process whose life the workspace's
//! lease follows: a detached `reef ws renew --follow` renews it while the
//! holder runs and releases nothing when it ends, so the daemon reclaims
//! the workspace, with a final snapshot, once the lease and its grace run
//! out (R2.13's rule).

use std::io;
use std::time::Duration;

use reef_api::v1::{
    self, CreateWorkspaceRequest, GetWorkspaceRequest, ListWorkspacesRequest, MissingStores,
    WorkspaceState, create_workspace_request,
};
use reef_client::json::{self, Lease};
use reef_client::{default_owner, json::reverse_hex};
use reef_error::{Advice, Classify as _};
use serde::Serialize;

use super::holder::{self, HoldWhile, Holder};
use super::input::{Fields, Input, Rules};
use super::out::TextRender;
use super::repo::missing;
use super::scope::is_workspace_not_found;
use super::ws::{lease_length, note_default_start};
use super::ws_run::attach_rule;
use super::{CREATE_TIMEOUT, CliError, Ctx, Global, RepoArg};

/// A field the provider refused: a wrong type, a bad name, stdin that is
/// not one JSON object.
pub(crate) const PROVIDER_INPUT_INVALID: &str = "PROVIDER_INPUT_INVALID";
/// The contract's version: changes only for an incompatible change.
const CONTRACT: u32 = 1;
/// The longest name.
const NAME_MAX: usize = 64;

/// `reef ws provider`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ProviderCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ProviderCommand {
    /// Create a workspace held while a process runs; print it.
    #[command(
        after_help = "Example: reef ws provider create --name fix-parser --hold-while parent -o text"
    )]
    Create(CreateArgs),
    /// Hold an existing workspace for a new process, as after a restart.
    #[command(
        after_help = "Example: reef ws provider attach --workspace \"$PWD\" --hold-while parent"
    )]
    Attach(AttachArgs),
    /// Snapshot and release a workspace; by id, again is not an error.
    #[command(after_help = "Example: reef ws provider release --workspace 3f2a")]
    Release(ReleaseArgs),
}

/// `ws provider create`.
#[derive(Debug, clap::Args)]
struct CreateArgs {
    #[command(flatten)]
    input: Input,
    /// The repository (id, prefix, or path) [default: the current directory's].
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
    /// A name: the same name again returns the same workspace while it lives.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    /// What it is for [default: the name].
    #[arg(long, value_name = "TEXT")]
    purpose: Option<String>,
    /// Who holds it (default `<user>@<host>:<name>`).
    #[arg(long, value_name = "OWNER")]
    owner: Option<String>,
    /// Edit this revision.
    #[arg(long, value_name = "REV", conflicts_with = "new_on")]
    change: Option<String>,
    /// Start a new change on this revision [default: as `ws create`].
    #[arg(long, value_name = "REV")]
    new_on: Option<String>,
    /// The lease length, such as 30m [default: the daemon's].
    #[arg(long, value_name = "DURATION")]
    lease: Option<String>,
    /// Hold it while this process runs: a pid, or `parent` (the nearest non-shell ancestor).
    #[arg(long, value_name = "PID|parent")]
    hold_while: Option<String>,
}

/// `ws provider attach`.
#[derive(Debug, clap::Args)]
struct AttachArgs {
    #[command(flatten)]
    input: Input,
    /// The workspace: id, prefix, or a path inside it.
    #[arg(long, value_name = "WS")]
    workspace: Option<String>,
    /// The owner it must have [default: this user on this machine].
    #[arg(long, value_name = "OWNER")]
    owner: Option<String>,
    /// Hold it while this process runs: a pid, or `parent`.
    #[arg(long, value_name = "PID|parent")]
    hold_while: Option<String>,
}

/// `ws provider release`.
#[derive(Debug, clap::Args)]
struct ReleaseArgs {
    #[command(flatten)]
    input: Input,
    /// The workspace: id, prefix, or a path inside it.
    #[arg(long, value_name = "WS")]
    workspace: Option<String>,
    /// Release for this holder only: kept while another holder runs.
    #[arg(long, value_name = "PID|parent")]
    hold_while: Option<String>,
}

const CREATE_FIELDS: &[&str] = &[
    "repo",
    "name",
    "purpose",
    "owner",
    "change",
    "new_on",
    "lease",
    "hold_while",
];
const ATTACH_FIELDS: &[&str] = &["workspace", "owner", "hold_while"];
const RELEASE_FIELDS: &[&str] = &["workspace", "hold_while"];

/// A created or attached workspace.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.Provided")]
pub(crate) struct Provided {
    /// The contract's version, 1.
    contract: u32,
    workspace_id: String,
    repo_id: String,
    /// The workspace's directory.
    path: String,
    change_id: String,
    owner: String,
    purpose: String,
    lease: Option<Lease>,
    /// The process holding it, and the follower renewing its lease.
    holder: Option<ProvidedHolder>,
    /// Whether this is a workspace that already existed.
    attached: bool,
    /// The revision a new workspace started on when none was given.
    start: Option<String>,
}

/// Who holds a provided workspace.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.ProvidedHolder")]
pub(crate) struct ProvidedHolder {
    pid: i32,
    follower_pid: u32,
}

impl TextRender for Provided {
    fn render(&self, w: &mut dyn io::Write) -> io::Result<()> {
        writeln!(w, "{}", self.path)
    }
}

/// A released workspace, or one that already was.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.ProviderReleased")]
pub(crate) struct ProviderReleased {
    /// The contract's version, 1.
    contract: u32,
    workspace_id: String,
    /// False when it had already been released or reclaimed.
    released: bool,
    /// Its state now.
    state: String,
    snapshot_taken: bool,
    final_op_id: Option<String>,
    /// Other processes that still hold it, when a release for one holder
    /// kept it (`released: false`, state `active`); 0 otherwise.
    held_by: u32,
}

pub(crate) async fn run(ctx: &Ctx, global: &Global, args: Args) -> Result<(), CliError> {
    match args.command {
        ProviderCommand::Create(args) => Box::pin(create(ctx, global, args)).await,
        ProviderCommand::Attach(args) => attach(ctx, global, args).await,
        ProviderCommand::Release(args) => release(ctx, global, args).await,
    }
}

/// The provider's fields: `hold_while` may be a pid.
const fn rules(known: &'static [&'static str]) -> Rules {
    Rules {
        known,
        numbers: &["hold_while"],
        invalid,
    }
}

fn invalid(field: &'static str, message: String) -> CliError {
    CliError::Input {
        reason: PROVIDER_INPUT_INVALID,
        message,
        field,
        advice: Advice::hint(
            "Give each field as a flag, or as a key of one JSON object on stdin (--stdin); `reef schema cli ws provider` lists them.",
        ),
    }
}

/// A name as the owner holds it: ASCII letters, digits, `.`, `_` and
/// `-`, at most [`NAME_MAX`] long, not all digits and not `mcp` (which
/// would collide with `ws run`'s and `reef mcp`'s owners). Any other name
/// is made one, the same way every time so a repeated name finds the same
/// workspace: other characters become `-`, and a short hash of the name
/// as given is added. Only an empty name is refused.
fn name_slug(name: &str) -> Result<String, CliError> {
    if name.is_empty() {
        return Err(invalid("name", "name is empty".to_owned()));
    }
    let fits = name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !name.bytes().all(|byte| byte.is_ascii_digit())
        && name != "mcp";
    if fits {
        return Ok(name.to_owned());
    }
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(NAME_MAX - 9)
        .collect();
    let stem = mapped.trim_matches('-');
    let stem = if stem.is_empty() { "name" } else { stem };
    // FNV-1a: stable across versions and machines, which a std hasher is not.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok(format!("{stem}-{:08x}", hash & 0xffff_ffff))
}

/// The owner a create gives: `owner`, else `<user>@<host>:<name>`, else
/// `<user>@<host>`.
fn owner_for(owner: Option<String>, name: Option<&str>, me: &str) -> String {
    owner.unwrap_or_else(|| match name {
        Some(name) => format!("{me}:{name}"),
        None => me.to_owned(),
    })
}

fn hold_while(text: Option<String>) -> Result<Option<HoldWhile>, CliError> {
    text.map(|text| {
        HoldWhile::parse(&text).ok_or_else(|| {
            invalid(
                "hold_while",
                format!("hold_while {text:?} is not a pid or `parent`"),
            )
        })
    })
    .transpose()
}

/// The create's fields, from flags, pointers and keys.
#[derive(Debug, PartialEq, Eq)]
struct CreateRequest {
    repo: Option<String>,
    name: Option<String>,
    purpose: Option<String>,
    owner: String,
    start: Option<create_workspace_request::Start>,
    lease: Option<Duration>,
    hold_while: Option<HoldWhile>,
}

impl CreateRequest {
    fn from(args: CreateArgs, fields: &Fields) -> Result<Self, CliError> {
        let given = fields.get("name", args.name)?;
        let name = given.as_deref().map(name_slug).transpose()?;
        let owner = fields.get("owner", args.owner)?;
        if name.is_some() && owner.is_some() {
            return Err(invalid(
                "owner",
                "name and owner were both given: a name is held through the owner `<user>@<host>:<name>`".to_owned(),
            ));
        }
        let change = fields.get("change", args.change)?;
        let new_on = fields.get("new_on", args.new_on)?;
        let start = match (change, new_on) {
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "new_on",
                    "change and new_on were both given".to_owned(),
                ));
            }
            (Some(change), None) => Some(create_workspace_request::Start::Change(change)),
            (None, Some(new_on)) => Some(create_workspace_request::Start::NewOn(new_on)),
            (None, None) => None,
        };
        let lease = fields
            .get("lease", args.lease)?
            .map(|text| {
                humantime::parse_duration(&text)
                    .ok()
                    .filter(|lease| !lease.is_zero())
                    .ok_or_else(|| {
                        invalid(
                            "lease",
                            format!("lease {text:?} is not a duration such as 30m"),
                        )
                    })
            })
            .transpose()?;
        Ok(Self {
            repo: fields.get("repo", args.repo)?,
            purpose: fields.get("purpose", args.purpose)?.or(given),
            owner: owner_for(owner, name.as_deref(), &default_owner()),
            name,
            start,
            lease,
            hold_while: hold_while(fields.get("hold_while", args.hold_while)?)?,
        })
    }
}

async fn create(ctx: &Ctx, global: &Global, args: CreateArgs) -> Result<(), CliError> {
    let fields = Fields::read(&args.input, &rules(CREATE_FIELDS))?;
    let request = CreateRequest::from(args, &fields)?;
    // Before anything is created: a holder that is not running fails here.
    let holder = request.hold_while.map(holder::resolve).transpose()?;
    let deadline = ctx.timeout.max(CREATE_TIMEOUT);
    let repo = match &request.repo {
        Some(repo) => RepoArg::parse(repo)?,
        None => RepoArg::current()?,
    };
    let outer = enclosing_workspace(ctx, &repo).await?;
    let repo = match &outer {
        Some(outer) => RepoArg::parse(&outer.repo_id)?,
        None => repo,
    };
    if request.name.is_some()
        && let Some(existing) = named(ctx, &repo, &request.owner).await?
    {
        let ws = attach_rule(ctx, &existing.id, Some(&request.owner)).await?;
        return provide(ctx, global, &ws, holder, true, None);
    }
    let again = if request.name.is_some() && request.start.is_none() && outer.is_none() {
        kept_change(ctx, &repo, &request.owner).await?
    } else {
        None
    };
    let start = request.start.clone().or(again).or_else(|| {
        outer
            .as_ref()
            .map(|outer| create_workspace_request::Start::NewOn(outer.commit_id.clone()))
    });
    // What the harness's environment and its session's note declare
    // (R3.44): a subagent's workspace carries its harness and session.
    let declared = super::ambient::declaration(
        super::ambient::Ambient::current(),
        global.state_dir().ok().as_deref(),
    );
    let create = CreateWorkspaceRequest {
        start,
        owner: request.owner.clone(),
        purpose: request.purpose.clone().unwrap_or_default(),
        lease_length: lease_length(request.lease)?,
        missing_stores: MissingStores::Fail.into(),
        provenance: (declared != v1::ProvenanceDeclaration::default()).then_some(declared),
        ..CreateWorkspaceRequest::default()
    };
    let call = |create: CreateWorkspaceRequest| {
        ctx.call_with_repo_within(deadline, &repo, move |repo| {
            ctx.client.create_workspace(CreateWorkspaceRequest {
                repo,
                ..create.clone()
            })
        })
    };
    let response = match call(create.clone()).await {
        // The environment never fails a creation (R3.04).
        Err(CliError::Daemon(error))
            if create.provenance.is_some()
                && error.reason() == reef_api::error::PROVENANCE_DISABLED =>
        {
            call(CreateWorkspaceRequest {
                provenance: None,
                ..create
            })
            .await?
        }
        response => response?,
    };
    note_default_start(ctx, &response.start);
    let start = (!response.start.is_empty()).then_some(response.start);
    let ws = response.workspace.ok_or_else(|| missing("workspace"))?;
    let id = ws.id.clone();
    match provide(ctx, global, &ws, holder, false, start) {
        Ok(()) => Ok(()),
        Err(error) => {
            // A program that got an error must not have a workspace left.
            let _ = ctx
                .call(ctx.client.release_workspace("", &id, false, false))
                .await;
            Err(error)
        }
    }
}

/// The active workspace `repo` is a path inside, snapshotted, so a
/// create from a session in a workspace starts on its files.
async fn enclosing_workspace(ctx: &Ctx, repo: &RepoArg) -> Result<Option<v1::Workspace>, CliError> {
    let RepoArg::Path(path) = repo else {
        return Ok(None);
    };
    let request = GetWorkspaceRequest {
        repo: String::new(),
        workspace: path.display().to_string(),
        snapshot_first: true,
        skip_disk_usage: true,
        at_operation: String::new(),
    };
    match ctx.call(ctx.client.get_workspace(request)).await {
        Ok(response) => {
            let ws = response.workspace.ok_or_else(|| missing("workspace"))?;
            if ws.state() == WorkspaceState::Active {
                return Ok(Some(ws));
            }
            Err(CliError::Refused {
                kind: reef_error::ErrorKind::Conflict,
                message: format!(
                    "{} is in workspace {}, which is {}",
                    path.display(),
                    ws.id,
                    json::state_name(ws.state)
                ),
                advice: Some(Advice::run(
                    "Create from the repository instead.",
                    [
                        "reef",
                        "ws",
                        "provider",
                        "create",
                        "--repo",
                        ws.repo_id.as_str(),
                    ],
                )),
            })
        }
        Err(CliError::Daemon(error)) if is_workspace_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The active workspace in `repo` with `owner` and a live lease, the
/// newest if several; one whose lease ended is about to be reclaimed.
async fn named(ctx: &Ctx, repo: &RepoArg, owner: &str) -> Result<Option<v1::Workspace>, CliError> {
    let listed = ctx
        .call_with_repo(repo, |repo| {
            ctx.client.list_workspaces(ListWorkspacesRequest {
                repo,
                owner: owner.to_owned(),
                states: vec![WorkspaceState::Active.into()],
                ..ListWorkspacesRequest::default()
            })
        })
        .await?;
    Ok(listed
        .into_iter()
        .filter(|ws| {
            ws.owner == owner
                && ws.state() == WorkspaceState::Active
                && ws.lease.as_ref().is_some_and(|lease| {
                    matches!(
                        lease.state(),
                        v1::LeaseState::Active | v1::LeaseState::InGrace
                    )
                })
        })
        .max_by_key(|ws| ws.created_at.map(|at| (at.seconds, at.nanos))))
}

/// The change a name's last workspace left, if it ended and its change was
/// kept: a session resumed after its workspace was reclaimed starts again
/// on its own files, not on the default start.
async fn kept_change(
    ctx: &Ctx,
    repo: &RepoArg,
    owner: &str,
) -> Result<Option<create_workspace_request::Start>, CliError> {
    let listed = ctx
        .call_with_repo(repo, |repo| {
            ctx.client.list_workspaces(ListWorkspacesRequest {
                repo,
                owner: owner.to_owned(),
                ..ListWorkspacesRequest::default()
            })
        })
        .await?;
    let Some(last) = listed
        .into_iter()
        .filter(|ws| ws.owner == owner)
        .max_by_key(|ws| ws.created_at.map(|at| (at.seconds, at.nanos)))
    else {
        return Ok(None);
    };
    if matches!(
        last.state(),
        WorkspaceState::Active | WorkspaceState::Releasing
    ) || !change_kept(ctx, &last).await
    {
        return Ok(None);
    }
    Ok(Some(create_workspace_request::Start::Change(reverse_hex(
        &last.change_id,
    ))))
}

/// Whether `ws`'s change is still in the log: an empty change is dropped
/// when its workspace ends.
async fn change_kept(ctx: &Ctx, ws: &v1::Workspace) -> bool {
    ctx.call(ctx.client.list_changes_page(v1::ListChangesRequest {
        repo: ws.repo_id.clone(),
        revset: format!("change_id({})", reverse_hex(&ws.change_id)),
        skip_snapshot: true,
        page_size: 1,
        ..v1::ListChangesRequest::default()
    }))
    .await
    .is_ok_and(|response| !response.entries.is_empty())
}

/// Starts a follower for `holder`, if one is not already following it,
/// and prints the workspace. A workspace may have several holders, each
/// with its follower: it is held while any of them runs, and no holder
/// takes it from another.
fn provide(
    ctx: &Ctx,
    global: &Global,
    ws: &v1::Workspace,
    holder: Option<Holder>,
    attached: bool,
    start: Option<String>,
) -> Result<(), CliError> {
    let lease = ws.lease.as_ref().ok_or_else(|| missing("lease"))?;
    let state_dir = global.state_dir()?;
    let holder = match holder {
        Some(holder) => {
            let following = holder::read(&state_dir, &ws.id).into_iter().find(|record| {
                record.holder == Some(holder)
                    && record.lease_id == lease.id
                    && holder::alive(&record.follower)
            });
            let follower_pid = match following {
                Some(record) => u32::try_from(record.follower.pid).unwrap_or(0),
                None => holder::spawn_follower(
                    global,
                    ctx.agent.as_deref(),
                    &ws.id,
                    &lease.id,
                    &holder,
                )?,
            };
            Some(ProvidedHolder {
                pid: holder.pid,
                follower_pid,
            })
        }
        None => None,
    };
    ctx.out.object(&Provided {
        contract: CONTRACT,
        workspace_id: ws.id.clone(),
        repo_id: ws.repo_id.clone(),
        path: ws.path.clone(),
        change_id: ws.change_id.clone(),
        owner: ws.owner.clone(),
        purpose: ws.purpose.clone(),
        lease: Some(Lease::from(lease)),
        holder,
        attached,
        start,
    })
}

async fn attach(ctx: &Ctx, global: &Global, args: AttachArgs) -> Result<(), CliError> {
    let fields = Fields::read(&args.input, &rules(ATTACH_FIELDS))?;
    let workspace = required(&fields, "workspace", args.workspace)?;
    let owner = fields.get("owner", args.owner)?;
    let holder = hold_while(fields.get("hold_while", args.hold_while)?)?
        .map(holder::resolve)
        .transpose()?;
    let ws = attach_rule(
        ctx,
        &super::scope::workspace_arg(&workspace)?,
        owner.as_deref(),
    )
    .await?;
    provide(ctx, global, &ws, holder, true, None)
}

fn required(
    fields: &Fields,
    field: &'static str,
    flag: Option<String>,
) -> Result<String, CliError> {
    fields
        .get(field, flag)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(field, format!("`{field}` is required")))
}

/// A release for `caller` while other holders still run: `caller`'s
/// followers stop, and the workspace is kept for the others. `None` when
/// no other holder runs.
fn keep_for_others(
    ctx: &Ctx,
    state_dir: &std::path::Path,
    found: &v1::Workspace,
    records: &[holder::Record],
    caller: Holder,
) -> Option<Result<(), CliError>> {
    let others = records
        .iter()
        .filter(|record| {
            holder::alive(&record.follower)
                && record
                    .holder
                    .is_some_and(|holder| holder != caller && holder::alive(&holder))
        })
        .count();
    if others > 0 {
        for record in records
            .iter()
            .filter(|record| record.holder == Some(caller))
        {
            holder::stop(record);
            holder::remove(state_dir, &found.id, &record.follower);
        }
        let note = format!(
            "reef: {} kept: {others} other holder{} still hold{} it",
            found.id,
            if others == 1 { "" } else { "s" },
            if others == 1 { "s" } else { "" }
        );
        return Some(ctx.out.done(
            &ProviderReleased {
                contract: CONTRACT,
                held_by: u32::try_from(others).unwrap_or(u32::MAX),
                workspace_id: found.id.clone(),
                released: false,
                state: "active".to_owned(),
                snapshot_taken: false,
                final_op_id: None,
            },
            &note,
        ));
    }
    None
}

async fn release(ctx: &Ctx, global: &Global, args: ReleaseArgs) -> Result<(), CliError> {
    let fields = Fields::read(&args.input, &rules(RELEASE_FIELDS))?;
    let workspace = super::scope::workspace_arg(&required(&fields, "workspace", args.workspace)?)?;
    let found = ctx
        .call(ctx.client.inspect_workspace("", &workspace, false))
        .await?
        .workspace
        .ok_or_else(|| missing("workspace"))?;
    let state_dir = global.state_dir()?;
    let records = holder::read(&state_dir, &found.id);
    // For one holder: while another still runs, only this one lets go.
    // A caller that is not running is no holder of it.
    let caller = hold_while(fields.get("hold_while", args.hold_while)?)?
        .and_then(|hold_while| holder::resolve(hold_while).ok());
    if let Some(caller) = caller
        && found.state() == WorkspaceState::Active
        && let Some(kept) = keep_for_others(ctx, &state_dir, &found, &records, caller)
    {
        return kept;
    }
    let response = if found.state() == WorkspaceState::Active {
        ctx.call_within(
            // Deliberately a create's bound: the 300 s the provider's
            // release has always had.
            ctx.timeout.max(CREATE_TIMEOUT),
            ctx.client.release_workspace("", &found.id, true, false),
        )
        .await
    } else {
        Err(CliError::Refused {
            kind: reef_error::ErrorKind::Conflict,
            message: String::new(),
            advice: None,
        })
    };
    for record in &records {
        holder::stop(record);
        holder::remove(&state_dir, &found.id, &record.follower);
    }
    let response = match response {
        Ok(response) => response,
        // Releasing, released or reclaimed already, or a reclaim that
        // started first: by id, not an error (the contract).
        Err(error) if error.kind() == reef_error::ErrorKind::Conflict => {
            let now = ctx
                .call(ctx.client.inspect_workspace("", &found.id, false))
                .await?
                .workspace
                .ok_or_else(|| missing("workspace"))?;
            if now.state() == WorkspaceState::Active {
                return Err(error);
            }
            let state = json::state_name(now.state).to_owned();
            let note = format!("reef: {} was already {state}", now.id);
            return ctx.out.done(
                &ProviderReleased {
                    contract: CONTRACT,
                    held_by: 0,
                    workspace_id: now.id,
                    released: false,
                    state,
                    snapshot_taken: false,
                    final_op_id: None,
                },
                &note,
            );
        }
        Err(error) => return Err(error),
    };
    let released = json::Released::from(&response);
    let note = format!("released {}", released.id);
    ctx.out.done(
        &ProviderReleased {
            contract: CONTRACT,
            held_by: 0,
            workspace_id: released.id,
            released: true,
            state: "released".to_owned(),
            snapshot_taken: released.snapshot_taken,
            final_op_id: released.final_op_id,
        },
        &note,
    )
}

/// What to do about a workspace that can no longer be attached: wait for
/// an expired one's reclaim, or start again on an ended one's change if it
/// was kept. An empty change is dropped when its workspace ends, so then
/// on the repository.
pub(crate) async fn ended_advice(ctx: &Ctx, ws: &v1::Workspace) -> Advice {
    let fresh = || -> Vec<String> {
        ["reef", "ws", "create", "--repo", ws.repo_id.as_str()]
            .iter()
            .map(|word| (*word).to_owned())
            .collect()
    };
    if ws.state() == WorkspaceState::Active {
        return Advice::run(
            "Its lease has ended; the daemon reclaims it shortly, with a final snapshot. Start a new workspace.",
            fresh(),
        );
    }
    let rev = reverse_hex(&ws.change_id);
    let kept = change_kept(ctx, ws).await;
    if kept {
        Advice::run(
            format!(
                "Its directory is gone; its work is change {rev}. Start a new workspace on it."
            ),
            create_on(&ws.repo_id, &ws.change_id),
        )
    } else {
        Advice::run(
            "Its directory is gone, and its change was empty, so nothing was kept. Start a new workspace.",
            fresh(),
        )
    }
}

/// The `ws create` that starts again on a released workspace's change.
pub(crate) fn create_on(repo_id: &str, change_id: &str) -> Vec<String> {
    ["reef", "ws", "create", "--repo", repo_id, "--change"]
        .iter()
        .map(|word| (*word).to_owned())
        .chain(std::iter::once(reverse_hex(change_id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::{Value, json};

    use super::*;

    #[derive(Debug, clap::Parser)]
    struct Probe {
        #[command(subcommand)]
        command: ProviderCommand,
    }

    fn parsed(argv: &[&str]) -> CreateArgs {
        let probe = Probe::try_parse_from(
            std::iter::once("probe")
                .chain(std::iter::once("create"))
                .chain(argv.iter().copied()),
        )
        .unwrap();
        let ProviderCommand::Create(parsed) = probe.command else {
            unreachable!()
        };
        parsed
    }

    fn create(argv: &[&str], stdin: Option<Value>) -> Result<CreateRequest, CliError> {
        let given = parsed(argv);
        let fields = Fields::with(stdin, &given.input.at, &rules(CREATE_FIELDS))?;
        CreateRequest::from(given, &fields)
    }

    fn field_of(error: &CliError) -> String {
        assert_eq!(error.reason(), PROVIDER_INPUT_INVALID, "{error}");
        error.metadata().get("field").cloned().unwrap_or_default()
    }

    /// R2.26 test 1: a flag, then a pointer, then the object's key.
    #[test]
    fn request_precedence() {
        let event = json!({"cwd": "/from/pointer", "repo": "/from/key", "extra": [1]});
        let request = create(
            &["--repo", "/from/flag", "--at", "repo=/cwd"],
            Some(event.clone()),
        )
        .unwrap();
        assert_eq!(request.repo.as_deref(), Some("/from/flag"));
        let request = create(&["--at", "repo=/cwd"], Some(event.clone())).unwrap();
        assert_eq!(request.repo.as_deref(), Some("/from/pointer"));
        let request = create(&["--stdin"], Some(event.clone())).unwrap();
        assert_eq!(request.repo.as_deref(), Some("/from/key"));
        // A pointer that selects nothing falls through to the key, then
        // the default.
        let request = create(&["--at", "repo=/nothing"], Some(event)).unwrap();
        assert_eq!(request.repo.as_deref(), Some("/from/key"));
        let request = create(&["--at", "name=/nothing"], Some(json!({}))).unwrap();
        assert_eq!(request.name, None);
        assert_eq!(request.owner, default_owner());

        // A number as a pid, and the start.
        let request = create(
            &["--stdin"],
            Some(json!({"hold_while": 42, "new_on": "main"})),
        )
        .unwrap();
        assert_eq!(request.hold_while, Some(HoldWhile::Pid(42)));
        assert_eq!(
            request.start,
            Some(create_workspace_request::Start::NewOn("main".to_owned()))
        );

        let cases: [(&[&str], Value, &str); 6] = [
            (&["--stdin"], json!({"repo": 3}), "repo"),
            (&["--stdin"], json!({"hold_while": "12x"}), "hold_while"),
            (&["--at", "nope=/x"], json!({}), "at"),
            (
                &["--stdin"],
                json!({"change": "a", "new_on": "b"}),
                "new_on",
            ),
            (&["--stdin"], json!({"lease": "soon"}), "lease"),
            (&["--name", "a"], json!({"owner": "x"}), "owner"),
        ];
        for (argv, stdin, field) in cases {
            let error = create(argv, Some(stdin)).unwrap_err();
            assert_eq!(field_of(&error), field, "{argv:?}");
        }
        for bytes in [
            b"[1]".to_vec(),
            b"nope".to_vec(),
            vec![b' '; 1024 * 1024 + 1],
        ] {
            assert_eq!(
                field_of(
                    &super::super::input::parse_stdin(&bytes, &rules(CREATE_FIELDS)).unwrap_err()
                ),
                "stdin"
            );
        }
    }

    /// R2.26 test 1, attach and release: the same order for their fields.
    #[test]
    fn attach_and_release_fields() {
        let event = json!({"cwd": "/from/pointer", "workspace": "from-key"});
        let fields = Fields::with(
            Some(event.clone()),
            &["workspace=/cwd".to_owned()],
            &rules(ATTACH_FIELDS),
        )
        .unwrap();
        assert_eq!(
            required(&fields, "workspace", Some("flag".to_owned())).unwrap(),
            "flag"
        );
        assert_eq!(
            required(&fields, "workspace", None).unwrap(),
            "/from/pointer"
        );
        let fields = Fields::with(Some(event), &[], &rules(RELEASE_FIELDS)).unwrap();
        assert_eq!(required(&fields, "workspace", None).unwrap(), "from-key");
        let none = Fields::with(Some(json!({})), &[], &rules(RELEASE_FIELDS)).unwrap();
        assert_eq!(
            field_of(&required(&none, "workspace", None).unwrap_err()),
            "workspace"
        );
        let unknown =
            Fields::with(None, &["repo=/cwd".to_owned()], &rules(RELEASE_FIELDS)).unwrap_err();
        assert_eq!(field_of(&unknown), "at");
    }

    /// R2.26 test 2.
    #[test]
    fn names_are_checked() {
        for good in ["bold-oak-a3f2", "agent-a1b2c3d4", "a.b_c"] {
            assert_eq!(name_slug(good).unwrap(), good);
        }
        // Anything else becomes a slug with a hash of the name as given:
        // the same every time, different for names that map alike.
        let long = "a".repeat(NAME_MAX + 1);
        for odd in ["123", "mcp", long.as_str(), "a b", "fix/login", "#123", "/"] {
            let slug = name_slug(odd).unwrap();
            assert_eq!(name_slug(odd).unwrap(), slug);
            assert!(slug.len() <= NAME_MAX, "{slug}");
            assert!(
                slug.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
                "{slug}"
            );
            assert_eq!(name_slug(&slug).unwrap(), slug, "a slug is its own slug");
        }
        assert!(name_slug("fix/login").unwrap().starts_with("fix-login-"));
        assert_ne!(name_slug("a b").unwrap(), name_slug("a/b").unwrap());
        assert_eq!(field_of(&name_slug("").unwrap_err()), "name");
        let request = create(&["--name", "fix/login"], None).unwrap();
        assert_eq!(request.purpose.as_deref(), Some("fix/login"));
        assert!(request.owner.ends_with(&name_slug("fix/login").unwrap()));
        assert_eq!(owner_for(None, Some("t1"), "me@host"), "me@host:t1");
        assert_eq!(owner_for(None, None, "me@host"), "me@host");
        assert_eq!(owner_for(Some("x".to_owned()), Some("t1"), "me@host"), "x");
        let request = create(&["--name", "t1"], None).unwrap();
        assert_eq!(request.purpose.as_deref(), Some("t1"));
        assert_eq!(request.owner, format!("{}:t1", default_owner()));
    }

    #[test]
    fn create_on_uses_the_change_revset() {
        let argv = create_on("r1", "0f");
        assert_eq!(
            argv[..6],
            ["reef", "ws", "create", "--repo", "r1", "--change"]
        );
        assert_eq!(argv[6], reverse_hex("0f"));
    }
}
