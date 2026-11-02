//! `reef mcp` (R2.13, ADR 0035): Reef's API as MCP tools, over stdio, on
//! [`reef_client::Client`]. Each tool is named after the RPC it calls and
//! returns the JSON `reef -o json` prints for that message ([`tools`]).
//!
//! What the tools share is here: the daemon connection, made on the first
//! call and remade once when the daemon goes away; the scope rule, applied
//! per call with no state between calls, since a harness may route several
//! agents through one server; the leases of the workspaces the server
//! creates ([`leases`]); the presence entries that say which tool was
//! called where ([`presence`], R2.11); and errors, which are tool results carrying the
//! CLI's error object with the daemon's `hint` and `next`, never JSON-RPC
//! errors, so the model sees them. Stdout carries only the protocol; notes
//! and logs go to stderr.

mod args;
mod leases;
mod presence;
mod results;
mod tools;
mod wait;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use reef_api::v1;
use reef_client::json::ErrorObject;
use reef_client::{ClientError, default_owner};
use reef_error::{Advice, Classify as _, ErrorKind};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;

use crate::cli::scope::{self, Scope, ScopeArgs};
use crate::cli::{self, CliError, Ctx, Global, Out, error_object};
use leases::Leases;
use presence::Activity;
use tools::{TOOLS, ToolDef};

/// What the server tells a client about itself, at most 1,000 bytes.
const INSTRUCTIONS: &str = include_str!("instructions.md");

/// The reason of a call that names no repository when the server was
/// started outside every checkout. The daemon never sends it.
pub(crate) const REPOSITORY_REQUIRED: &str = "REPOSITORY_REQUIRED";

/// A tool's failure: a command's error, or one of the server's own checks,
/// already in the error object's form.
pub(super) enum ToolError {
    Cli(CliError),
    Own(Box<ErrorObject>),
}

impl From<CliError> for ToolError {
    fn from(error: CliError) -> Self {
        Self::Cli(error)
    }
}

impl ToolError {
    /// One of the server's own `invalid_argument` errors.
    fn invalid(reason: &str, message: impl Into<String>, hint: &str) -> Self {
        Self::Own(Box::new(ErrorObject {
            kind: "invalid_argument".to_owned(),
            reason: reason.to_owned(),
            message: message.into(),
            hint: hint.to_owned(),
            ..ErrorObject::default()
        }))
    }

    fn object(&self) -> ErrorObject {
        match self {
            Self::Cli(error) => error_object(error),
            Self::Own(object) => (**object).clone(),
        }
    }
}

/// The server: one per `reef mcp` process.
pub(crate) struct ReefServer {
    global: Global,
    out: Out,
    /// The connection, made on the first call that needs the daemon.
    ctx: Mutex<Option<Arc<Ctx>>>,
    leases: Leases,
    /// Shared with [`serve`], which clears the entries at exit.
    activity: Arc<Activity>,
    /// Whether the launch environment names a repository: `REEF_WORKSPACE`,
    /// or a git or jj checkout around the current directory. When it does
    /// not, a call that names none is [`REPOSITORY_REQUIRED`].
    launch_scope: bool,
}

/// What a tool gets: the server, the connection, and the request.
pub(super) struct Call<'a> {
    pub(super) server: &'a ReefServer,
    /// The tool's name, which its presence entry says.
    pub(super) tool: &'static str,
    pub(super) ctx: &'a Ctx,
    pub(super) request: &'a RequestContext<RoleServer>,
}

impl Call<'_> {
    /// The scope `repo` and `workspace` name, else the launch scope
    /// (R2.27's rule), and the lease of a workspace the server owns kept
    /// alive. The call is reported as the scope's presence. Only the launch scope registers a checkout implicitly: a
    /// `repo` path is a sandboxed agent's choice, and registering it would
    /// have the daemon, outside the sandbox, write `.jj` there and later
    /// run git on its `.git/config` (S-07, S-13).
    pub(super) async fn scope(
        &self,
        repo: Option<&String>,
        workspace: Option<&String>,
    ) -> Result<Scope, ToolError> {
        let given = repo.is_some() || workspace.is_some();
        let args = ScopeArgs {
            workspace: workspace.cloned(),
            repo: repo.cloned(),
        };
        let unregistering;
        let ctx = if repo.is_some() && self.ctx.auto_register {
            unregistering = Ctx {
                client: self.ctx.client.clone(),
                out: self.ctx.out.clone(),
                timeout: self.ctx.timeout,
                timeout_flag: self.ctx.timeout_flag,
                auto_register: false,
                agent: self.ctx.agent.clone(),
            };
            &unregistering
        } else {
            self.ctx
        };
        let scope = match scope::resolve(ctx, &args).await {
            Err(error) if !given && !self.server.launch_scope && names_no_repository(&error) => {
                return Err(ToolError::invalid(
                    REPOSITORY_REQUIRED,
                    "the server was started outside every repository, and the call names none",
                    "Pass repo: an absolute path inside the repository, or its id; list_repositories lists the registered ones.",
                ));
            }
            scope => scope?,
        };
        if let Some(ws) = &scope.workspace {
            self.server.leases.keep(self.ctx, ws);
        }
        self.server.activity.called(self.ctx, self.tool, &scope);
        Ok(scope)
    }

    /// The scope's workspace, which the tool needs.
    pub(super) async fn workspace(
        &self,
        workspace: Option<&String>,
    ) -> Result<(Scope, v1::Workspace), ToolError> {
        let required = || {
            ToolError::invalid(
                reef_api::error::WORKSPACE_REQUIRED,
                "the call names no workspace, and the server was not started in one",
                "Pass workspace: its id or an absolute path inside it; list_workspaces lists them.",
            )
        };
        // Outside every checkout and workspace the launch scope is no
        // repository either, whose error asks for `repo`, which these tools
        // do not take.
        let scope = match self.scope(None, workspace).await {
            Err(ToolError::Own(error)) if error.reason == REPOSITORY_REQUIRED => {
                return Err(required());
            }
            scope => scope?,
        };
        match scope.workspace.clone() {
            Some(ws) => Ok((scope, ws)),
            None => Err(required()),
        }
    }
}

/// Whether resolving the launch scope failed because there is no repository
/// there: the current directory is in no checkout (implicit registration
/// refused it), or in none registered (`--no-register`).
fn names_no_repository(error: &CliError) -> bool {
    error.kind() == ErrorKind::NotFound
        && matches!(
            error.reason(),
            "" | reef_api::error::REPOSITORY_NOT_REGISTERED
        )
}

/// Whether `dir` or a parent holds `.git` or `.jj`.
fn in_checkout(dir: &Path) -> bool {
    dir.ancestors()
        .any(|dir| dir.join(".git").exists() || dir.join(".jj").exists())
}

/// Deserializes a tool's arguments; a mismatch is a tool error the model
/// can fix.
pub(super) fn parse<T: DeserializeOwned>(arguments: Map<String, Value>) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(arguments)).map_err(|error| {
        ToolError::invalid(
            "",
            format!("invalid arguments: {error}"),
            "See the tool's input schema.",
        )
    })
}

/// A tool's result as JSON.
pub(super) fn to_value<T: Serialize>(value: &T) -> Result<Value, ToolError> {
    serde_json::to_value(value).map_err(|error| {
        ToolError::Own(Box::new(ErrorObject {
            kind: "internal".to_owned(),
            message: format!("serialize the result: {error}"),
            ..ErrorObject::default()
        }))
    })
}

/// Serves until the client closes stdin, or SIGINT or SIGTERM. Exits 0
/// then; a client that hangs up before `initialize` is not a failure.
pub(crate) async fn serve(global: Global, out: &Out) -> Result<ExitCode, CliError> {
    // Before anything is served: a bad name is a usage error.
    global.agent()?;
    let launch_scope =
        scope::env_workspace().is_some() || scope::current_dir().is_ok_and(|dir| in_checkout(&dir));
    let owner = format!("{}:mcp", default_owner());
    let activity = Arc::new(Activity::new(owner.clone()));
    let server = ReefServer {
        global,
        out: out.clone(),
        ctx: Mutex::new(None),
        leases: Leases::new(owner),
        activity: Arc::clone(&activity),
        launch_scope,
    };
    let internal = |message: String| CliError::Refused {
        kind: ErrorKind::Internal,
        message,
        advice: None,
    };
    let mut terminate = signal(SignalKind::terminate()).map_err(CliError::Signals)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(CliError::Signals)?;
    let service = tokio::select! {
        service = rmcp::serve_server(server, rmcp::transport::stdio()) => service,
        _ = terminate.recv() => return Ok(ExitCode::SUCCESS),
        _ = interrupt.recv() => return Ok(ExitCode::SUCCESS),
    };
    let service = match service {
        Ok(service) => service,
        Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => {
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => return Err(internal(format!("start the MCP server: {error}"))),
    };
    tracing::debug!(target: "reef", "serving MCP on stdio");
    let cancel = service.cancellation_token();
    let ended = tokio::select! {
        ended = service.waiting() => {
            ended.map(|_| ()).map_err(|error| internal(format!("serve MCP: {error}")))
        }
        _ = terminate.recv() => {
            cancel.cancel();
            Ok(())
        }
        _ = interrupt.recv() => {
            cancel.cancel();
            Ok(())
        }
    };
    // After the service stops: stdout is the protocol's, and the clear
    // writes nothing to it.
    activity.clear_all().await;
    ended.map(|()| ExitCode::SUCCESS)
}

impl ReefServer {
    /// The connection, made now if there is none.
    async fn ctx(&self) -> Result<Arc<Ctx>, CliError> {
        let mut slot = self.ctx.lock().await;
        if let Some(ctx) = &*slot {
            return Ok(Arc::clone(ctx));
        }
        let ctx = Arc::new(cli::connect(&self.global, &self.out).await?);
        *slot = Some(Arc::clone(&ctx));
        Ok(ctx)
    }

    /// Forgets `stale`, so the next call connects again (and starts the
    /// daemon, unless `--no-autostart`).
    async fn reconnect(&self, stale: &Arc<Ctx>) {
        let mut slot = self.ctx.lock().await;
        if slot.as_ref().is_some_and(|ctx| Arc::ptr_eq(ctx, stale)) {
            *slot = None;
        }
    }

    /// Runs `tool`, retrying once against a restarted daemon of this
    /// version while no call has succeeded (as `main` reruns a command), and
    /// a read once after the daemon went away mid-call.
    async fn run(
        &self,
        tool: &ToolDef,
        arguments: Map<String, Value>,
        request: &RequestContext<RoleServer>,
    ) -> Result<Value, ToolError> {
        let mut retried = false;
        loop {
            let ctx = self.ctx().await?;
            let call = Call {
                server: self,
                tool: tool.name,
                ctx: &ctx,
                request,
            };
            let error = match (tool.call)(&call, arguments.clone()).await {
                Ok(value) => return Ok(value),
                Err(ToolError::Cli(error)) => error,
                Err(own) => return Err(own),
            };
            match error {
                CliError::Daemon(error @ ClientError::Incompatible { .. })
                    if !retried && !reef_client::any_call_answered() =>
                {
                    cli::daemon::restart(&self.global, &self.out, error).await?;
                    self.reconnect(&ctx).await;
                }
                CliError::Daemon(error @ ClientError::Incompatible { .. }) => {
                    return Err(cli::old_daemon(&self.global, error).into());
                }
                CliError::Daemon(error) if went_away(&error) => {
                    self.reconnect(&ctx).await;
                    if retried || !tool.retries() {
                        let hint = if tool.reads() {
                            "The daemon went away during the wait; wait again, with after_operation set to the last operation you saw."
                        } else {
                            "The daemon went away during the call; check whether it took effect (get_operation, list_operations) before retrying."
                        };
                        return Err(CliError::Advised {
                            error: Box::new(error),
                            advice: Advice::hint(hint),
                        }
                        .into());
                    }
                }
                error => return Err(error.into()),
            }
            retried = true;
        }
    }
}

/// Whether a call failed because the daemon went away: `unavailable` from
/// the transport, with no Reef reason.
fn went_away(error: &ClientError) -> bool {
    matches!(error, ClientError::Rpc { kind: ErrorKind::Unavailable, reason, .. } if reason.is_empty())
}

impl ServerHandler for ReefServer {
    fn get_info(&self) -> ServerConfig {
        let mut implementation = Implementation::new("reef", reef_client::VERSION);
        implementation.title = Some("Reef".to_owned());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_instructions(INSTRUCTIONS)
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tools::list())))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = TOOLS.iter().find(|tool| tool.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("no tool named {}", request.name),
                None,
            ));
        };
        let arguments = request.arguments.unwrap_or_default();
        let result = match self.run(tool, arguments, &context).await {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => {
                let object = error.object();
                tracing::debug!(target: "reef", tool = tool.name, kind = %object.kind, reason = %object.reason, "tool failed");
                let document = reef_client::json::ErrorDocument { error: object };
                CallToolResult::structured_error(serde_json::to_value(document).unwrap_or_default())
            }
        };
        Ok(result.into())
    }
}

/// `reef schema mcp`'s part (R2.24, ADR 0050): the command that serves the
/// tools, the server's instructions, every tool as `tools/list` gives it
/// with its `outputSchema`, and the schema of an error result's structured
/// content.
pub(crate) fn schema_part() -> Value {
    serde_json::json!({
        "command": ["reef", "mcp"],
        "instructions": INSTRUCTIONS,
        "tools": tools::definitions(),
        "error_result": tools::output::<reef_client::json::ErrorDocument>(),
    })
}

/// Every tool's name and description, in `tools/list`'s order, for the
/// MCPB bundle's manifest (R2.25), so a listing cannot drift from the
/// server.
#[cfg(test)]
pub(crate) fn tool_summaries() -> Vec<(&'static str, &'static str)> {
    TOOLS
        .iter()
        .map(|def| (def.name, def.description))
        .collect()
}
