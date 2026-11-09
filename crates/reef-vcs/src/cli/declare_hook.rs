//! `reef provenance declare --hook` (R3.44): what a harness's prompt hook
//! runs. The hook event comes on stdin as JSON and `--at FIELD=POINTER`
//! says where its fields are, so the command knows no harness: the
//! pointers are facts in the agent table, written into the plugin's hooks.
//!
//! The instruction's text is hashed as it is read and dropped; only its
//! SHA-256 goes anywhere. The declaration goes to the event's workspace and
//! to every active workspace of the same client session, and into the
//! session's note, which the workspaces the session creates next declare
//! from (`ambient`).
//!
//! It never fails the prompt it runs on: it exits 0 and prints nothing
//! (a prompt hook's stdout is context the model sees, and exit 2 blocks the
//! prompt), never starts a daemon, and gives up after two seconds. What
//! goes wrong goes to the debug log, by field name, never with the event.

use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, DeclareProvenanceRequest, ListWorkspacesRequest, ProvenanceDeclaration, WorkspaceState,
};
use reef_error::Classify as _;
use tokio::task::JoinSet;

use super::ambient::{self, Ambient, Note};
use super::provenance::DeclareArgs;
use super::{CliError, Ctx, Global, Out, connect, scope};

/// The whole command's deadline, connecting included.
const DEADLINE: Duration = Duration::from_secs(2);

/// Runs the hook. Always `Ok`: a failure is logged, never reported.
pub(crate) async fn run(global: &Global, args: DeclareArgs) -> Result<(), CliError> {
    let global = Global {
        no_autostart: true,
        ..global.clone()
    };
    match tokio::time::timeout(DEADLINE, hook(&global, &args)).await {
        Ok(Ok(declared)) => {
            tracing::debug!(target: "reef", declared, "provenance hook");
            Ok(())
        }
        Ok(Err(error)) => {
            tracing::debug!(target: "reef", kind = ?error.kind(), reason = error.reason(), "provenance hook");
            Ok(())
        }
        Err(_) => {
            tracing::debug!(target: "reef", "provenance hook: out of time");
            Ok(())
        }
    }
}

/// Declares; the number of workspaces declared for.
async fn hook(global: &Global, args: &DeclareArgs) -> Result<usize, CliError> {
    let event = if args.reads_stdin() {
        let event = super::hook::read_event().await?;
        if !event.is_object() {
            return Err(CliError::Usage(
                "the hook event is not one JSON object".to_owned(),
            ));
        }
        Some(event)
    } else {
        None
    };
    // Hashed here; the event, and the text in it, end with this call.
    let declaring = args.declaring(event, true)?;
    let mut declaration = declaring.declaration;
    let ambient = Ambient::current();
    declaration.harness = declaration.harness.or(ambient.harness);
    declaration.client_session = declaration.client_session.or(ambient.client_session);
    declaration.model = declaration.model.or(ambient.model);
    if let Some(session) = declaration.client_session.clone() {
        // Before the daemon: a creation that starts it later still finds
        // the note.
        write_note(global, session, &declaration).await;
    }
    if declaration == ProvenanceDeclaration::default() {
        return Ok(0);
    }
    let out = Out::detect(Some(super::Output::Json), true);
    let ctx = connect(global, &out).await?;
    let targets = targets(&ctx, args, declaring.workspace, &declaration).await;
    let mut calls = JoinSet::new();
    let ctx = std::sync::Arc::new(ctx);
    for ws in targets {
        let ctx = std::sync::Arc::clone(&ctx);
        let request = DeclareProvenanceRequest {
            repo: ws.repo_id.clone(),
            workspace: ws.id.clone(),
            declaration: Some(declaration.clone()),
            no_snapshot: args.no_snapshot,
        };
        calls.spawn(async move { ctx.call(ctx.client.declare_provenance(request)).await });
    }
    let mut declared = 0;
    while let Some(done) = calls.join_next().await {
        match done {
            Ok(Ok(_)) => declared += 1,
            Ok(Err(error)) => {
                tracing::debug!(target: "reef", reason = error.reason(), "provenance hook: a workspace refused");
            }
            Err(error) => tracing::debug!(target: "reef", %error, "provenance hook"),
        }
    }
    Ok(declared)
}

/// Merges the declaration into the session's note, on a thread of its own
/// so the deadline can leave a file system that hangs. A failure is logged.
async fn write_note(global: &Global, session: String, declaration: &ProvenanceDeclaration) {
    let note = Note {
        instruction_sha256: declaration.instruction_sha256.clone(),
        model: declaration.model.clone(),
        harness: declaration.harness.clone(),
    };
    let state_dir = match global.state_dir() {
        Ok(state_dir) => state_dir,
        Err(error) => {
            tracing::debug!(target: "reef", reason = error.reason(), "provenance hook: no state directory");
            return;
        }
    };
    let (send, written) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let dir = ambient::notes_dir(&state_dir);
        let _ = send.send(ambient::write_note(&dir, &session, note, SystemTime::now()));
    });
    if let Ok(Err(error)) = written.await {
        tracing::debug!(target: "reef", reason = error.reason(), "provenance hook: the session note");
    }
}

/// The workspaces to declare for: `--workspace`, else the event's
/// workspace (its `cwd`, say), else `REEF_WORKSPACE`, else the current
/// directory's, the first that is a workspace (a flag wins over a pointer,
/// ADR 0053); then every active workspace of the client session. Each
/// lookup is best effort. A remote daemon cannot see this machine's paths,
/// so they are not asked about.
async fn targets(
    ctx: &Ctx,
    args: &DeclareArgs,
    pointed: Option<String>,
    declaration: &ProvenanceDeclaration,
) -> Vec<v1::Workspace> {
    let mut targets: Vec<v1::Workspace> = Vec::new();
    let remote = super::remote_mode();
    let current = (!remote)
        .then(|| scope::current_dir().ok())
        .flatten()
        .map(|dir| dir.display().to_string());
    let named = [
        args.scope_workspace(),
        pointed.filter(|text| !(remote && text.starts_with('/'))),
        scope::env_workspace(),
        current,
    ];
    for text in named.into_iter().flatten() {
        match scope::find_workspace(ctx, &text).await {
            Ok(Some(ws)) => {
                targets.push(ws);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(target: "reef", reason = error.reason(), "provenance hook: a workspace lookup");
            }
        }
    }
    if let Some(session) = &declaration.client_session {
        match session_workspaces(ctx, session).await {
            Ok(found) => {
                for ws in found {
                    if !targets.iter().any(|target| target.id == ws.id) {
                        targets.push(ws);
                    }
                }
            }
            Err(error) => {
                tracing::debug!(target: "reef", reason = error.reason(), "provenance hook: the session's workspaces");
            }
        }
    }
    targets
}

/// The active workspaces whose provenance session declared `session` as
/// its client session, in every repository.
async fn session_workspaces(ctx: &Ctx, session: &str) -> Result<Vec<v1::Workspace>, CliError> {
    let workspaces = ctx
        .call(ctx.client.list_workspaces(ListWorkspacesRequest {
            states: vec![WorkspaceState::Active.into()],
            ..ListWorkspacesRequest::default()
        }))
        .await?;
    Ok(workspaces
        .into_iter()
        .filter(|ws| {
            ws.provenance
                .as_ref()
                .and_then(|session| session.context.as_ref())
                .is_some_and(|context| context.client_session == session)
        })
        .collect())
}
