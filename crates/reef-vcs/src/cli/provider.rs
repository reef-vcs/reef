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

use std::io::{self, Read as _};
use std::time::Duration;

use reef_api::v1::{
    self, CreateWorkspaceRequest, GetWorkspaceRequest, ListWorkspacesRequest, MissingStores,
    WorkspaceState, create_workspace_request,
};
use reef_client::json::{self, Lease};
use reef_client::{default_owner, json::reverse_hex};
use reef_error::{Advice, Classify as _};
use serde::Serialize;
use serde_json::Value;

use super::holder::{self, HoldWhile, Holder};
use super::out::TextRender;
use super::repo::missing;
use super::scope::is_workspace_not_found;
use super::ws::{lease_length, note_default_start};
use super::ws_run::attach_rule;
use super::{CliError, Ctx, Global, RepoArg};

/// A field the provider refused: a wrong type, a bad name, stdin that is
/// not one JSON object.
pub(crate) const PROVIDER_INPUT_INVALID: &str = "PROVIDER_INPUT_INVALID";
/// The contract's version: changes only for an incompatible change.
const CONTRACT: u32 = 1;
/// The most stdin is read.
const STDIN_MAX: u64 = 1024 * 1024;
/// The least time a create is given: a first workspace of a large
/// repository builds its base.
const CREATE_DEADLINE: Duration = Duration::from_secs(300);
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

/// Where the fields come from besides flags.
#[derive(Debug, clap::Args)]
struct Input {
    /// Read the fields as one JSON object on stdin, keyed by the flags' names (`new_on`).
    #[arg(long)]
    stdin: bool,
    /// Take FIELD from stdin's object at a JSON pointer, such as `repo=/cwd` (implies --stdin).
    #[arg(long = "at", value_name = "FIELD=POINTER")]
    at: Vec<String>,
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
    /// Who holds it [default: `<user>@<host>:<name>`].
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
const RELEASE_FIELDS: &[&str] = &["workspace"];

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
}

pub(crate) async fn run(ctx: &Ctx, global: &Global, args: Args) -> Result<(), CliError> {
    match args.command {
        ProviderCommand::Create(args) => create(ctx, global, args).await,
        ProviderCommand::Attach(args) => attach(ctx, global, args).await,
        ProviderCommand::Release(args) => release(ctx, global, args).await,
    }
}

/// The stdin object and the `--at` pointers into it.
#[derive(Debug, Default)]
struct Fields {
    stdin: Option<Value>,
    at: Vec<(&'static str, String)>,
}

impl Fields {
    /// Reads stdin if `--stdin` or `--at` asks for it, and checks the
    /// pointers' fields against `known`.
    fn read(input: &Input, known: &[&'static str]) -> Result<Self, CliError> {
        let stdin = if input.stdin || !input.at.is_empty() {
            let mut bytes = Vec::new();
            io::stdin()
                .take(STDIN_MAX + 1)
                .read_to_end(&mut bytes)
                .map_err(|source| CliError::Io {
                    op: "read",
                    path: "stdin".into(),
                    source,
                })?;
            Some(parse_stdin(&bytes)?)
        } else {
            None
        };
        Self::with(stdin, &input.at, known)
    }

    fn with(stdin: Option<Value>, at: &[String], known: &[&'static str]) -> Result<Self, CliError> {
        let at = at
            .iter()
            .map(|pair| {
                let (field, pointer) = pair.split_once('=').unwrap_or((pair, ""));
                let field = known.iter().find(|known| **known == field).ok_or_else(|| {
                    invalid(
                        "at",
                        format!(
                            "--at {pair}: no field `{field}`; this command takes {}",
                            known.join(", ")
                        ),
                    )
                })?;
                if !(pointer.is_empty() || pointer.starts_with('/')) {
                    return Err(invalid(
                        "at",
                        format!("--at {pair}: a JSON pointer is empty or starts with `/`"),
                    ));
                }
                Ok((*field, pointer.to_owned()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { stdin, at })
    }

    /// `field`'s value: the flag, else the pointer's, else the object's
    /// key; `None` when none gives one.
    fn get(&self, field: &'static str, flag: Option<String>) -> Result<Option<String>, CliError> {
        if flag.is_some() {
            return Ok(flag);
        }
        let Some(stdin) = &self.stdin else {
            return Ok(None);
        };
        for (_, pointer) in self.at.iter().filter(|(name, _)| *name == field) {
            if let Some(value) = text(field, stdin.pointer(pointer))? {
                return Ok(Some(value));
            }
        }
        text(field, stdin.get(field))
    }
}

/// Stdin as one JSON object of at most [`STDIN_MAX`] bytes.
fn parse_stdin(bytes: &[u8]) -> Result<Value, CliError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > STDIN_MAX {
        return Err(invalid("stdin", "stdin is over 1 MiB".to_owned()));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid("stdin", format!("stdin is not JSON: {error}")))?;
    if !value.is_object() {
        return Err(invalid("stdin", "stdin is not one JSON object".to_owned()));
    }
    Ok(value)
}

/// A field's value as text: a string, or for `hold_while` a whole
/// number; `null` and absent are none.
fn text(field: &'static str, value: Option<&Value>) -> Result<Option<String>, CliError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(Value::Number(number)) if field == "hold_while" && number.is_u64() => {
            Ok(Some(number.to_string()))
        }
        Some(_) => Err(invalid(
            field,
            format!(
                "`{field}` must be a string{}",
                if field == "hold_while" {
                    " or a pid"
                } else {
                    ""
                }
            ),
        )),
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

/// A name is ASCII letters, digits, `.`, `_` and `-`, at most
/// [`NAME_MAX`] long, not all digits and not `mcp`, which would collide
/// with `ws run`'s and `reef mcp`'s owners.
fn check_name(name: &str) -> Result<(), CliError> {
    let fits = !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !name.bytes().all(|byte| byte.is_ascii_digit())
        && name != "mcp";
    if fits {
        Ok(())
    } else {
        Err(invalid(
            "name",
            format!(
                "name {name:?} is not 1 to {NAME_MAX} ASCII letters, digits, `.`, `_` or `-`, or is all digits, or `mcp`"
            ),
        ))
    }
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
        let name = fields.get("name", args.name)?;
        if let Some(name) = &name {
            check_name(name)?;
        }
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
            purpose: fields
                .get("purpose", args.purpose)?
                .or_else(|| name.clone()),
            owner: owner_for(owner, name.as_deref(), &default_owner()),
            name,
            start,
            lease,
            hold_while: hold_while(fields.get("hold_while", args.hold_while)?)?,
        })
    }
}

async fn create(ctx: &Ctx, global: &Global, args: CreateArgs) -> Result<(), CliError> {
    let fields = Fields::read(&args.input, CREATE_FIELDS)?;
    let request = CreateRequest::from(args, &fields)?;
    // Before anything is created: a holder that is not running fails here.
    let holder = request.hold_while.map(holder::resolve).transpose()?;
    let deadline = ctx.timeout.max(CREATE_DEADLINE);
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
    let start = request.start.clone().or_else(|| {
        outer
            .as_ref()
            .map(|outer| create_workspace_request::Start::NewOn(outer.commit_id.clone()))
    });
    let create = CreateWorkspaceRequest {
        start,
        owner: request.owner.clone(),
        purpose: request.purpose.clone().unwrap_or_default(),
        lease_length: lease_length(request.lease)?,
        missing_stores: MissingStores::Fail.into(),
        ..CreateWorkspaceRequest::default()
    };
    let response = ctx
        .call_with_repo_within(deadline, &repo, |repo| {
            ctx.client.create_workspace(CreateWorkspaceRequest {
                repo,
                ..create.clone()
            })
        })
        .await?;
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

/// Starts a follower for `holder`, if any, then ends any previous
/// holder's, so a failed start leaves the workspace held as it was, and
/// prints the workspace.
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
    let previous = holder::current(&state_dir, ws);
    let holder = match (holder, previous) {
        (Some(holder), Some(previous)) if previous.holder == Some(holder) => Some(ProvidedHolder {
            pid: holder.pid,
            follower_pid: u32::try_from(previous.follower.pid).unwrap_or(0),
        }),
        (Some(holder), _) => {
            let follower_pid =
                holder::spawn_follower(global, ctx.agent.as_deref(), &ws.id, &lease.id, &holder)?;
            let started = i32::try_from(follower_pid).unwrap_or(0);
            for record in holder::read(&state_dir, &ws.id) {
                if record.follower.pid != started {
                    holder::stop(&record);
                }
            }
            Some(ProvidedHolder {
                pid: holder.pid,
                follower_pid,
            })
        }
        (None, _) => None,
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
    let fields = Fields::read(&args.input, ATTACH_FIELDS)?;
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

async fn release(ctx: &Ctx, global: &Global, args: ReleaseArgs) -> Result<(), CliError> {
    let fields = Fields::read(&args.input, RELEASE_FIELDS)?;
    let workspace = super::scope::workspace_arg(&required(&fields, "workspace", args.workspace)?)?;
    let found = ctx
        .call(ctx.client.inspect_workspace("", &workspace, false))
        .await?
        .workspace
        .ok_or_else(|| missing("workspace"))?;
    let state_dir = global.state_dir()?;
    let records = holder::read(&state_dir, &found.id);
    let response = if found.state() == WorkspaceState::Active {
        ctx.call_within(
            ctx.timeout.max(CREATE_DEADLINE),
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
    let kept = ctx
        .call(ctx.client.list_changes_page(v1::ListChangesRequest {
            repo: ws.repo_id.clone(),
            revset: format!("change_id({rev})"),
            skip_snapshot: true,
            page_size: 1,
            ..v1::ListChangesRequest::default()
        }))
        .await
        .is_ok_and(|response| !response.entries.is_empty());
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
    use serde_json::json;

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
        let fields = Fields::with(stdin, &given.input.at, CREATE_FIELDS)?;
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
            assert_eq!(field_of(&parse_stdin(&bytes).unwrap_err()), "stdin");
        }
    }

    /// R2.26 test 1, attach and release: the same order for their fields.
    #[test]
    fn attach_and_release_fields() {
        let event = json!({"cwd": "/from/pointer", "workspace": "from-key"});
        let fields = Fields::with(
            Some(event.clone()),
            &["workspace=/cwd".to_owned()],
            ATTACH_FIELDS,
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
        let fields = Fields::with(Some(event), &[], RELEASE_FIELDS).unwrap();
        assert_eq!(required(&fields, "workspace", None).unwrap(), "from-key");
        let none = Fields::with(Some(json!({})), &[], RELEASE_FIELDS).unwrap();
        assert_eq!(
            field_of(&required(&none, "workspace", None).unwrap_err()),
            "workspace"
        );
        let unknown = Fields::with(None, &["repo=/cwd".to_owned()], RELEASE_FIELDS).unwrap_err();
        assert_eq!(field_of(&unknown), "at");
    }

    /// R2.26 test 2.
    #[test]
    fn names_are_checked() {
        for good in ["bold-oak-a3f2", "agent-a1b2c3d4", "a.b_c"] {
            check_name(good).unwrap();
        }
        let long = "a".repeat(NAME_MAX + 1);
        for bad in ["", "123", "mcp", long.as_str(), "a b", "a/b"] {
            assert_eq!(field_of(&check_name(bad).unwrap_err()), "name", "{bad:?}");
        }
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
