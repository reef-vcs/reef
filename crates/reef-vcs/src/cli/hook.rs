//! `reef presence hook edit` and `end` (R2.25, ADR 0052): a harness's hook
//! reports the files an agent is about to edit, as an editing entry with
//! `by_daemon: false` (R2.11's declared intent). The hook event comes on
//! stdin as JSON, and JSON Pointers say where its paths and session id are,
//! so the command knows no harness: the pointers are facts in the agent
//! table, written into the plugin's hooks.
//!
//! Each session keeps one entry, the files it last declared, replaced by
//! the next edit through an id file and lasting its TTL (5 minutes): an
//! entry cleared after the edit would last one tool call and be seen by
//! nobody. `end` clears it.
//!
//! It never fails the tool call it runs before: it exits 0, prints nothing
//! (Claude Code reads JSON on stdout as a decision and shows stderr on a
//! failure), never starts a daemon, and gives up after two seconds. What
//! goes wrong goes to the debug log.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1::{self, GetWorkspaceRequest, ReportPresenceRequest};
use reef_client::{default_owner, presence};

use super::presence::{read_id_file, remove_id_file, write_id_file};
use super::{CliError, Ctx, Global, Out, connect};

/// The most stdin a hook reads: a hook event is a few kilobytes.
const MAX_EVENT: u64 = 1 << 20;

/// The whole command's deadline, connecting included.
const DEADLINE: Duration = Duration::from_secs(2);

/// How long a declared edit lasts unless the session declares another.
const DEFAULT_TTL: Duration = Duration::from_mins(5);

/// The longest an entry may last (R2.11).
const MAX_TTL: Duration = Duration::from_mins(10);

/// `reef presence hook`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: HookCommand,
}

#[derive(Debug, clap::Subcommand)]
enum HookCommand {
    /// Declare the files a hook event's tool is about to edit.
    #[command(
        after_help = "Example: reef presence hook edit --path-at /tool_input/file_path --session-at /session_id"
    )]
    Edit(EditArgs),
    /// Clear the session's declaration, at its end.
    #[command(after_help = "Example: reef presence hook end --session-at /session_id")]
    End(EndArgs),
}

#[derive(Debug, clap::Args)]
struct EditArgs {
    /// A JSON Pointer to a path, or to an array of paths, in the event; repeatable.
    #[arg(long = "path-at", value_name = "POINTER", required = true)]
    path_at: Vec<String>,
    /// A JSON Pointer to the session's id [default: the parent process].
    #[arg(long, value_name = "POINTER")]
    session_at: Option<String>,
    /// How long the declaration lasts [default: 5m; at most 10m].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    ttl: Option<Duration>,
}

#[derive(Debug, clap::Args)]
struct EndArgs {
    /// A JSON Pointer to the session's id [default: the parent process].
    #[arg(long, value_name = "POINTER")]
    session_at: Option<String>,
}

/// Runs the hook. Always `Ok`: a failure is logged, never reported.
pub(crate) async fn run(global: &Global, args: Args) -> Result<(), CliError> {
    let global = Global {
        no_autostart: true,
        ..global.clone()
    };
    match tokio::time::timeout(DEADLINE, hook(&global, args)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(target: "reef", %error, "presence hook"),
        Err(_) => tracing::debug!(target: "reef", "presence hook: out of time"),
    }
    Ok(())
}

async fn hook(global: &Global, args: Args) -> Result<(), CliError> {
    let event = read_event().await?;
    let session_at = match &args.command {
        HookCommand::Edit(edit) => edit.session_at.as_deref(),
        HookCommand::End(end) => end.session_at.as_deref(),
    };
    let id_file = id_file(global, &session(&event, session_at))?;
    // Nothing goes to the terminal; the hook's caller reads stdout.
    let out = Out::detect(Some(super::Output::Json), true);
    match args.command {
        HookCommand::Edit(edit) => {
            let ctx = connect(global, &out).await?;
            declare(&ctx, &event, &edit, &id_file).await
        }
        HookCommand::End(_) => {
            // The file goes first, so a session that ends with no daemon
            // leaves nothing behind; its entry runs out by itself.
            let held = read_id_file(&id_file)?;
            remove_id_file(&id_file)?;
            if let Some(id) = held {
                let ctx = connect(global, &out).await?;
                // Already ended is as good as cleared.
                let _ = ctx.call(ctx.client.clear_presence(&id)).await;
            }
            Ok(())
        }
    }
}

/// Stdin as JSON, read on a thread of its own so the deadline can leave a
/// read that never ends (the process exits without joining it).
async fn read_event() -> Result<serde_json::Value, CliError> {
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = std::io::stdin()
            .take(MAX_EVENT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(read);
    });
    let bytes = receive
        .await
        .map_err(|_| CliError::Usage("stdin was not read".to_owned()))?
        .map_err(|source| CliError::Io {
            op: "read",
            path: PathBuf::from("<stdin>"),
            source,
        })?;
    if bytes.len() as u64 > MAX_EVENT {
        return Err(CliError::Usage("the hook event is over 1 MiB".to_owned()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| CliError::Usage(format!("the hook event is not JSON: {error}")))
}

/// The session's id at `pointer`, else the parent process's id: the
/// harness that ran the hook.
fn session(event: &serde_json::Value, pointer: Option<&str>) -> String {
    pointer
        .and_then(|pointer| event.pointer(pointer))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map_or_else(
            || format!("ppid-{}", std::os::unix::process::parent_id()),
            str::to_owned,
        )
}

/// `<state>/presence/hooks/<session>.id`, with a session id that is not a
/// plain name replaced by its hash, so it cannot leave the directory.
fn id_file(global: &Global, session: &str) -> Result<PathBuf, CliError> {
    let plain = session.len() <= 64
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let name = if plain {
        session.to_owned()
    } else {
        // FNV-1a: stable across builds, unlike std's hasher.
        let hash = session
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
        format!("h{hash:016x}")
    };
    let dir = global.state_dir()?.join("presence/hooks");
    Ok(dir.join(format!("{name}.id")))
}

fn create_private_dir(dir: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| CliError::Io {
            op: "create",
            path: dir.to_owned(),
            source,
        })
}

/// The paths the event's pointers select, absolute against the current
/// directory, each with its deepest existing directory's links resolved
/// (a new file may be in a new directory) so it compares with the daemon's
/// canonical workspace paths.
fn event_paths(event: &serde_json::Value, pointers: &[String]) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut paths = Vec::new();
    for pointer in pointers {
        let found = match event.pointer(pointer) {
            Some(serde_json::Value::String(path)) => vec![path.as_str()],
            Some(serde_json::Value::Array(items)) => {
                items.iter().filter_map(serde_json::Value::as_str).collect()
            }
            _ => continue,
        };
        for path in found.into_iter().filter(|path| !path.is_empty()) {
            let path = cwd.join(path);
            let resolved = path
                .ancestors()
                .skip(1)
                .find_map(|dir| {
                    let rest = path.strip_prefix(dir).ok()?;
                    Some(dir.canonicalize().ok()?.join(rest))
                })
                .unwrap_or(path);
            if !paths.contains(&resolved) {
                paths.push(resolved);
            }
        }
    }
    paths
}

/// Reports the event's paths as this session's editing entry, in the one
/// workspace that holds them all; nothing when there is no such workspace.
async fn declare(
    ctx: &Ctx,
    event: &serde_json::Value,
    args: &EditArgs,
    id_file: &Path,
) -> Result<(), CliError> {
    let paths = event_paths(event, &args.path_at);
    let Some(first) = paths.first() else {
        return Ok(());
    };
    let dir = first.parent().unwrap_or(first);
    let Ok(response) = ctx
        .call(ctx.client.get_workspace(GetWorkspaceRequest {
            workspace: dir.display().to_string(),
            // The disk walk is the slowest part of a lookup, and a hook
            // runs before every edit.
            skip_disk_usage: true,
            ..Default::default()
        }))
        .await
    else {
        // In no workspace: nothing to declare.
        return Ok(());
    };
    let Some(ws) = response.workspace else {
        return Ok(());
    };
    let root = PathBuf::from(&ws.path);
    let mut relative = Vec::new();
    for path in &paths {
        let Ok(inside) = path.strip_prefix(&root) else {
            // Some paths are in another workspace, or in none.
            return Ok(());
        };
        relative.push(inside.to_string_lossy().into_owned());
    }
    // R2.11's longest; a hook has no one to tell a longer one is refused.
    let ttl = args.ttl.unwrap_or(DEFAULT_TTL).min(MAX_TTL);
    let held = read_id_file(id_file)?;
    let request = ReportPresenceRequest {
        presence_id: held.clone().unwrap_or_default(),
        agent: presence::agent(ctx.agent.as_deref(), Some(&ws.owner), &default_owner()),
        repo: String::new(),
        workspace: ws.id.clone(),
        change: ws.change_id.clone(),
        kind: v1::PresenceKind::Editing.into(),
        activity: String::new(),
        paths: relative,
        progress: None,
        ttl: prost_types::Duration::try_from(ttl).ok(),
    };
    let response = match ctx.call(ctx.client.report_presence(request.clone())).await {
        // The entry ran out, or is in another workspace or another agent's,
        // which an entry cannot move to: a new one, ending the old.
        Err(CliError::Daemon(error))
            if held.is_some()
                && [
                    reef_api::error::PRESENCE_NOT_FOUND,
                    reef_api::error::PRESENCE_MISMATCH,
                ]
                .contains(&error.reason()) =>
        {
            if error.reason() == reef_api::error::PRESENCE_MISMATCH
                && let Some(old) = &held
            {
                let _ = ctx.call(ctx.client.clear_presence(old)).await;
            }
            let fresh = ReportPresenceRequest {
                presence_id: String::new(),
                ..request
            };
            ctx.call(ctx.client.report_presence(fresh)).await?
        }
        response => response?,
    };
    if let Some(entry) = response.presence {
        if let Some(dir) = id_file.parent() {
            create_private_dir(dir)?;
        }
        write_id_file(id_file, &entry.id)?;
    }
    Ok(())
}
