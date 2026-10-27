//! The `reef` command line (R1.22): every command is a call to the daemon
//! through [`reef_client::Client`], except `config show`,
//! `completions`, `guide`, `init` when it registers nothing (`--check`,
//! `--print`, `--no-register`), `daemon run`, and `restore`, which is
//! offline (R1.18). Output follows [`out`]'s
//! rules and failures exit with their kind's code ([`error`]).
//!
//! The agent-facing contract (R1.26, ADR 0025) is kept here and in
//! [`out`]: never interactive, every known fix on the error, the guide on
//! `reef` alone, `reef here`, and names that are only ever added (aliases
//! and unique prefixes resolve; nothing is renamed).

mod autostart;
mod backup;
mod config;
mod daemon;
mod env;
mod error;
mod guide;
mod helper;
mod here;
mod init;
mod out;
mod render;
mod repo;
mod restore;
mod store;
mod ws;
mod ws_move;
mod ws_run;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgAction, CommandFactory as _, Parser as _};
use reef_api::v1::IfNotJj;
use reef_client::logging::EnvLog;
use reef_client::paths::{ClientConfig, PathOverrides};
use reef_client::{Client, ClientError};
use reef_error::{Advice, ErrorKind};

pub(crate) use error::CliError;
pub(crate) use out::{Out, Output};

/// The RPC deadline when `--timeout` is not given.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// `Repositories.Register`'s deadline when `--timeout` is not given:
/// initializing jj alongside git indexes every commit of the history, which
/// takes minutes on a large one.
pub(crate) const REGISTER_TIMEOUT: Duration = Duration::from_mins(10);

/// `reef --version`: the version, then the commit and target the binary was
/// built from (`build.rs`, R1.28), so a bug report names the exact binary.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("REEF_BUILD_COMMIT"),
    ", ",
    env!("REEF_BUILD_TARGET"),
    ")"
);

/// `reef`.
#[derive(Debug, clap::Parser)]
#[command(
    name = "reef",
    version = VERSION,
    propagate_version = true,
    infer_subcommands = true,
    about = "Reef: version control built for a world where AI agents write most of the code",
    after_help = "New here? `reef guide` explains Reef in one screen; `reef here` says where you are."
)]
pub(crate) struct Cli {
    #[command(flatten)]
    global: Global,
    /// None: print the guide (ADR 0025).
    #[command(subcommand)]
    command: Option<Command>,
}

/// Flags every command takes.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct Global {
    /// Output format: `json` when stdout is not a terminal, `text` when it is.
    #[arg(
        short,
        long,
        global = true,
        value_enum,
        value_name = "FORMAT",
        env = OUTPUT_ENV
    )]
    output: Option<Output>,
    /// The daemon's state directory; overrides `REEF_STATE_DIR`.
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The daemon's socket; overrides `REEF_SOCKET`.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// The daemon's configuration file; overrides `REEF_CONFIG`.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Fail instead of starting the daemon when it is not running.
    #[arg(long, global = true)]
    no_autostart: bool,
    /// Fail instead of registering a repository that a path names (and
    /// initializing jj alongside git there) when it is not registered;
    /// overrides `[repo] auto_register`.
    #[arg(long, global = true)]
    no_register: bool,
    /// How long to wait for the daemon's answer [default: 30s; 1h for `ws
    /// gc`; for `store build`, how long its command may run, 30m].
    #[arg(long, global = true, value_name = "DURATION", value_parser = humantime::parse_duration)]
    timeout: Option<Duration>,
    /// Print nothing on stderr but errors.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Log the CLI's own debug events on stderr (`-vv` for trace).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    /// The agent named on the operations this command causes; overrides `REEF_AGENT`.
    // `ws run` passes it on to its command as `REEF_AGENT` (R2.05).
    #[arg(long, global = true, value_name = "NAME", env = AGENT_ENV, hide_env = true)]
    agent: Option<String>,
}

impl Global {
    fn overrides(&self) -> PathOverrides {
        PathOverrides {
            state_dir: self.state_dir.clone(),
            socket: self.socket.clone(),
            config_file: self.config.clone(),
        }
    }

    fn load_config(&self) -> Result<ClientConfig, CliError> {
        ClientConfig::load(&self.overrides()).map_err(CliError::Config)
    }

    /// The agent name, `--agent` else `REEF_AGENT`, checked and trimmed;
    /// `None` when neither names one.
    pub(crate) fn agent(&self) -> Result<Option<&str>, CliError> {
        let Some(agent) = &self.agent else {
            return Ok(None);
        };
        reef_client::check_agent(agent).map_err(CliError::Daemon)
    }

    /// `reef daemon start` with the flags that choose which daemon this
    /// command line talks to, so the advice starts that one.
    pub(crate) fn daemon_start(&self) -> Vec<String> {
        let mut argv = vec!["reef".to_owned()];
        for (flag, value) in [
            ("--state-dir", &self.state_dir),
            ("--socket", &self.socket),
            ("--config", &self.config),
        ] {
            if let Some(value) = value {
                argv.push(flag.to_owned());
                argv.push(value.display().to_string());
            }
        }
        argv.extend(["daemon".to_owned(), "start".to_owned()]);
        argv
    }

    /// `reef daemon stop` with the flags that choose which daemon this
    /// command line talks to.
    pub(crate) fn daemon_stop(&self) -> Vec<String> {
        let mut argv = self.daemon_start();
        argv.pop();
        argv.push("stop".to_owned());
        argv
    }
}

/// The default for `-o`, so a harness that gives its agent a terminal can
/// still have JSON everywhere.
const OUTPUT_ENV: &str = "REEF_OUTPUT";
/// The default for `--agent`, so a harness names each agent once (R2.05).
pub(crate) const AGENT_ENV: &str = "REEF_AGENT";

/// In the order `--help` lists them: the agent's path first.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Create, list, inspect, and release workspaces.
    #[command(visible_alias = "workspace", infer_subcommands = true)]
    Ws(ws::Args),
    /// Show the workspace you are in, its lease, and what to do next.
    #[command(after_help = "Example: reef here -o json | jq .next")]
    Here(here::Args),
    /// Print the agent guide: what Reef is and the commands that matter.
    #[command(visible_alias = "agent-guide")]
    Guide(guide::Args),
    /// Set up a repository for coding agents: register it and write AGENTS.md and the like.
    #[command(after_help = "Example: reef init --check   # in CI: exit 3 if a file is stale")]
    Init(init::Args),
    /// Register and forget repositories.
    #[command(infer_subcommands = true)]
    Repo(repo::Args),
    /// Build and list shared dependency stores.
    #[command(infer_subcommands = true)]
    Store(store::Args),
    /// Run, start, stop, or query the daemon.
    #[command(infer_subcommands = true)]
    Daemon(daemon::Args),
    /// Inspect the daemon's configuration.
    #[command(infer_subcommands = true)]
    Config(config::Args),
    /// Back up the daemon's state to a directory.
    Backup(backup::Args),
    /// Restore the daemon's state from a backup (offline).
    Restore(restore::Args),
    /// Print a shell completion script.
    Completions {
        /// The shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// What a command that talks to the daemon gets.
pub(crate) struct Ctx {
    pub(crate) client: Client,
    pub(crate) out: Out,
    pub(crate) timeout: Duration,
    /// `--timeout` as given, for a command whose default differs (`ws gc`).
    pub(crate) timeout_flag: Option<Duration>,
    /// Whether a command given a path in no registered repository registers
    /// it and retries (R1.25): `[repo] auto_register` and no
    /// `--no-register`.
    pub(crate) auto_register: bool,
    /// `--agent` or `REEF_AGENT`, which the client sends on every call and
    /// `ws run` passes on to its command.
    pub(crate) agent: Option<String>,
}

impl Ctx {
    /// Awaits `call` for at most the RPC deadline.
    pub(crate) async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, CliError> {
        self.call_within(self.timeout, call).await
    }

    /// Awaits `call` for at most `deadline`, for a call known to take
    /// longer than most.
    pub(crate) async fn call_within<T>(
        &self,
        deadline: Duration,
        call: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, CliError> {
        match tokio::time::timeout(deadline, call).await {
            Ok(result) => result.map_err(CliError::Daemon),
            Err(_) => Err(CliError::Timeout { after: deadline }),
        }
    }

    /// [`Ctx::call_with_repo_within`] with the RPC deadline.
    pub(crate) async fn call_with_repo<T, F, Fut>(
        &self,
        repo: &RepoArg,
        call: F,
    ) -> Result<T, CliError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        self.call_with_repo_within(self.timeout, repo, call).await
    }

    /// Awaits `call(repo)`, `repo` as the request's repository field, for at
    /// most `deadline`. If `repo` is a path in no registered repository,
    /// registers the repository containing it, initializing jj alongside a
    /// plain git checkout (R1.25), says so on stderr, and calls once more;
    /// a second failure is returned as it is. Not with `--no-register` or
    /// `[repo] auto_register = false`, and never for a repo id or prefix.
    pub(crate) async fn call_with_repo_within<T, F, Fut>(
        &self,
        deadline: Duration,
        repo: &RepoArg,
        call: F,
    ) -> Result<T, CliError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let result = self.call_within(deadline, call(repo.text())).await;
        let RepoArg::Path(path) = repo else {
            return result;
        };
        match result {
            Err(error) if self.auto_register && is_unregistered_path(&error) => {
                self.register_implicitly(path).await?;
                self.call_within(deadline, call(repo.text())).await
            }
            result => result,
        }
    }

    /// `Repositories.Register` with `INIT_ALONGSIDE_GIT`, and the stderr
    /// line that says the checkout was changed.
    async fn register_implicitly(&self, path: &Path) -> Result<(), CliError> {
        let response = self
            .call_within(
                self.timeout_flag.unwrap_or(REGISTER_TIMEOUT),
                self.client
                    .register_repository(path, IfNotJj::InitAlongsideGit),
            )
            .await?;
        let root = response
            .repository
            .as_ref()
            .map(|repo| repo.root.clone())
            .ok_or_else(|| repo::missing("repository"))?;
        if response.initialized_jj {
            self.out.note(&format!(
                "reef: registered {root} and initialized jj alongside git"
            ));
        } else if !response.already_registered {
            // Already registered: another command did it meanwhile.
            self.out.note(&format!("reef: registered {root}"));
        }
        Ok(())
    }
}

/// Whether the daemon refused a request because no registered repository
/// contains the path it named: reason `REPOSITORY_NOT_REGISTERED` (R2.05).
/// Every v1 daemon sends it; a 0.1 daemon is restarted before this matters.
fn is_unregistered_path(error: &CliError) -> bool {
    matches!(
        error,
        CliError::Daemon(ClientError::Rpc { reason, .. })
            if reason == reef_api::error::REPOSITORY_NOT_REGISTERED
    )
}

/// Parses the command line, runs the command, and maps its failure to an
/// exit code. Never panics on a daemon error.
pub(crate) fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return usage_error(&error, &args),
    };
    let Cli { global, command } = cli;
    let Some(command) = command else {
        // `reef` alone: the guide, exit 0, never clap's usage error.
        let out = Out::detect(global.output.or(Some(Output::Text)), global.quiet);
        return match guide::print(&out, &guide::Args::default()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                out.error(&error);
                ExitCode::from(error.exit_code())
            }
        };
    };
    // Before logging and the runtime: the daemon binary replaces this
    // process, and must start before it has any other thread.
    if let Command::Daemon(daemon::Args {
        command: daemon::DaemonCommand::Run,
    }) = &command
    {
        let out = Out::detect(global.output, global.quiet);
        return daemon::run_foreground(&global, &out);
    }
    // `config show` prints TOML unless asked for JSON; see its module. `ws
    // run`'s stdout is its command's, so it prints JSON only when asked. The
    // guide is prose for a model to read; JSON escapes would cost it tokens.
    let flag = match &command {
        Command::Config(_) | Command::Guide(_) => global.output.or(Some(Output::Text)),
        Command::Ws(args) if args.is_run() => global.output.or(Some(Output::Text)),
        Command::Init(args) if args.prints_text() => global.output.or(Some(Output::Text)),
        _ => global.output,
    };
    let out = Out::detect(flag, global.quiet);
    if let Err(error) = init_logging(global.verbose) {
        out.error(&error);
        return ExitCode::from(error.exit_code());
    }
    tracing::debug!(target: "reef", version = env!("CARGO_PKG_VERSION"), "starting");
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(source) => {
            let error = CliError::Runtime(source);
            out.error(&error);
            return ExitCode::from(error.exit_code());
        }
    };
    let rerunnable = !matches!(command, Command::Daemon(_));
    let result = match runtime.block_on(run(global.clone(), command, &out)) {
        // Only while no call has succeeded: one that did may have changed
        // something, which the rerun would repeat.
        Err(CliError::Daemon(error @ ClientError::Incompatible { .. }))
            if rerunnable && !reef_client::any_call_answered() =>
        {
            runtime.block_on(rerun(&global, &out, error, &args))
        }
        Err(CliError::Daemon(error @ ClientError::Incompatible { .. })) => {
            Err(old_daemon(&global, error))
        }
        result => result,
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            out.error(&error);
            ExitCode::from(error.exit_code())
        }
    }
}

/// The running daemon could not serve the command (`incompatible`, a
/// [`ClientError::Incompatible`]): restart it if it is idle and run the
/// command once more, from the same arguments (R2.01). Called only when no
/// call of the command had succeeded, so the failed one, which changed
/// nothing, was its first and the rerun repeats nothing.
async fn rerun(
    global: &Global,
    out: &Out,
    incompatible: ClientError,
    args: &[OsString],
) -> Result<ExitCode, CliError> {
    daemon::restart(global, out, incompatible).await?;
    let Cli { global, command } =
        Cli::try_parse_from(args).map_err(|error| CliError::Usage(error.to_string()))?;
    let Some(command) = command else {
        return Ok(ExitCode::SUCCESS);
    };
    match run(global.clone(), command, out).await {
        Err(CliError::Daemon(error @ ClientError::Incompatible { .. })) => {
            Err(old_daemon(&global, error))
        }
        result => result,
    }
}

/// A daemon that cannot serve this reef and is not restarted, with the
/// `reef daemon stop` that lets the next command start the matching one.
fn old_daemon(global: &Global, error: ClientError) -> CliError {
    let active_workspaces = match &error {
        ClientError::Incompatible {
            active_workspaces, ..
        } => *active_workspaces,
        _ => 0,
    };
    CliError::OldDaemon {
        stop: global.daemon_stop(),
        active_workspaces,
        error: Box::new(error),
    }
}

/// The CLI's filter when neither `REEF_LOG` nor `RUST_LOG` is set and no
/// `-v` is given: the CLI's own `info` events are not for the person at the
/// terminal. `reef-daemon` sets up its own logging.
const CLI_DEFAULT_FILTER: &str = "warn";

fn init_logging(verbose: u8) -> Result<(), CliError> {
    let env = EnvLog::from_env().map_err(CliError::Logging)?;
    let filter = match verbose {
        0 => env.filter.unwrap_or_else(|| CLI_DEFAULT_FILTER.to_owned()),
        1 => "debug".to_owned(),
        _ => "trace".to_owned(),
    };
    reef_client::logging::init(&filter, env.format).map_err(CliError::Logging)
}

async fn run(global: Global, command: Command, out: &Out) -> Result<ExitCode, CliError> {
    let done = match command {
        Command::Guide(args) => guide::print(out, &args),
        Command::Here(args) => here::run(&connect(&global, out).await?, args).await,
        Command::Init(args) => init::run(&global, out, args).await,
        Command::Daemon(args) => daemon::run(&global, out, args).await,
        Command::Config(args) => args.run(&global, out),
        Command::Completions { shell } => {
            let mut command = Cli::command();
            let mut buffer = Vec::new();
            clap_complete::generate(shell, &mut command, "reef", &mut buffer);
            out.raw(&buffer)
        }
        Command::Repo(args) => repo::run(&connect(&global, out).await?, args).await,
        Command::Ws(args) => return ws::run(&connect(&global, out).await?, args).await,
        Command::Store(args) => return store::run(&connect(&global, out).await?, args).await,
        Command::Backup(args) => return backup::run(&connect(&global, out).await?, args).await,
        Command::Restore(args) => restore::run(&global, out, &args),
    };
    done.map(|()| ExitCode::SUCCESS)
}

/// Connects to the daemon, starting it first when nothing answers on its
/// socket, unless `--no-autostart`.
pub(crate) async fn connect(global: &Global, out: &Out) -> Result<Ctx, CliError> {
    // Before the socket is tried, so a bad name starts no daemon.
    let agent = global.agent()?.map(str::to_owned);
    let config = global.load_config()?;
    let socket = &config.socket;
    let client = match Client::connect(socket).await {
        Ok(client) => client,
        Err(ClientError::Connect { .. }) if !global.no_autostart => {
            let (client, pid) = autostart::start(global, &config).await?;
            out.note(&format!("reef: started daemon (pid {pid})"));
            client
        }
        Err(ClientError::Connect { .. }) => {
            return Err(CliError::NotRunning {
                socket: socket.clone(),
                start: global.daemon_start(),
                autostart_off: true,
            });
        }
        Err(error) => return Err(CliError::Daemon(error)),
    };
    let client = match &agent {
        Some(agent) => client.with_agent(agent).map_err(CliError::Daemon)?,
        None => client,
    };
    Ok(Ctx {
        client,
        out: out.clone(),
        timeout: global.timeout.unwrap_or(DEFAULT_TIMEOUT),
        timeout_flag: global.timeout,
        auto_register: config.auto_register && !global.no_register,
        agent,
    })
}

/// The output mode a command line asks for, read from the raw arguments
/// for when clap could not parse them, else from `REEF_OUTPUT`.
fn output_mode(args: &[OsString]) -> Option<Output> {
    output_flag(args).or_else(|| {
        let value = std::env::var(OUTPUT_ENV).ok()?;
        <Output as clap::ValueEnum>::from_str(&value, false).ok()
    })
}

/// The output mode the raw arguments ask for with `-o`.
fn output_flag(args: &[OsString]) -> Option<Output> {
    let parse = |value: &str| match value {
        "json" => Some(Output::Json),
        "text" | "toml" => Some(Output::Text),
        _ => None,
    };
    let mut found = None;
    let mut iter = args.iter().skip(1).filter_map(|arg| arg.to_str());
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == "-o" || arg == "--output" {
            iter.next()
        } else if let Some(value) = arg.strip_prefix("--output=") {
            Some(value)
        } else if let Some(value) = arg.strip_prefix("-o") {
            Some(value.strip_prefix('=').unwrap_or(value))
        } else {
            None
        };
        if let Some(mode) = value.and_then(parse) {
            found = Some(mode);
        }
    }
    found
}

/// Commands an agent guesses when it wants to know where it is, which
/// Reef does not have: every agent in RX.07's scenario D tried `reef
/// status` before `reef here`. A hint, never an alias: R2.02 owns `reef
/// status` (ADR 0025). Unique prefixes of real commands (`st` for `store`)
/// never get here.
const ORIENTATION_GUESSES: &[&str] = &["status", "info", "where", "whereami", "whoami"];

/// clap could not parse the command line: help and version print as clap
/// prints them; anything else is a usage error, exit 2, with the JSON error
/// object on stdout in JSON mode, its hint the usage line, or `reef here`
/// for a top-level orientation guess.
fn usage_error(error: &clap::Error, args: &[OsString]) -> ExitCode {
    use clap::error::ErrorKind as Clap;
    if matches!(error.kind(), Clap::DisplayHelp | Clap::DisplayVersion) {
        // A closed stdout is not worth reporting.
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    let out = Out::detect(output_mode(args), false);
    let rendered = error.render().to_string();
    let usage = usage_line(&rendered);
    let message = match error.kind() {
        // clap renders the command's help, whose first line is its `about`.
        Clap::DisplayHelpOnMissingArgumentOrSubcommand | Clap::MissingSubcommand => {
            "a subcommand is required; see --help".to_owned()
        }
        _ => rendered
            .lines()
            .next()
            .unwrap_or_default()
            .trim_start_matches("error: ")
            .to_owned(),
    };
    let fix = orientation_guess(error, usage).map(|guess| {
        Advice::run(
            format!(
                "Reef has no `{guess}`; `reef here` shows the workspace you are in and its change."
            ),
            here_argv(args),
        )
    });
    out.usage_error(&rendered, &message, usage, fix.as_ref());
    ExitCode::from(ErrorKind::InvalidArgument.exit_code())
}

/// The usage line of clap's rendering of an error.
fn usage_line(rendered: &str) -> Option<&str> {
    rendered
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Usage: "))
}

/// `reef here` with the raw arguments' `--state-dir`, `--socket`, and
/// `--config`, for when clap could not parse them: it asks the daemon the
/// command line addressed, as [`Global::daemon_start`] does.
fn here_argv(args: &[OsString]) -> Vec<String> {
    const FLAGS: [&str; 3] = ["--state-dir", "--socket", "--config"];
    let mut next = vec!["reef".to_owned()];
    let mut iter = args.iter().skip(1).filter_map(|arg| arg.to_str());
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        for flag in FLAGS {
            let value = if arg == flag {
                iter.next()
            } else {
                arg.strip_prefix(flag)
                    .and_then(|rest| rest.strip_prefix('='))
            };
            if let Some(value) = value {
                next.extend([flag.to_owned(), value.to_owned()]);
            }
        }
    }
    next.push("here".to_owned());
    next
}

/// The unknown command, if clap rejected one of [`ORIENTATION_GUESSES`]
/// in `reef`'s own position (`reef status`, not `reef ws status`): the
/// usage line clap rendered is the root command's.
fn orientation_guess<'a>(error: &'a clap::Error, usage: Option<&str>) -> Option<&'a str> {
    use clap::error::{ContextKind, ContextValue, ErrorKind as Clap};
    if error.kind() != Clap::InvalidSubcommand {
        return None;
    }
    let Some(ContextValue::String(name)) = error.get(ContextKind::InvalidSubcommand) else {
        return None;
    };
    let root = Cli::command().render_usage().to_string();
    (ORIENTATION_GUESSES.contains(&name.as_str()) && usage == Some(root.trim()))
        .then_some(name.as_str())
}

/// A repository as a command line names it (`--repo`, `repo forget`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepoArg {
    /// A file or directory that exists, made absolute: what implicit
    /// registration may register (R1.25).
    Path(PathBuf),
    /// Anything else, as the request's repository field: an id or id prefix
    /// (4 to 16 hex characters) as given, or a path that does not exist made
    /// absolute. Never registered.
    Named(String),
}

impl RepoArg {
    /// `text` as a path if it exists on disk, else as an id or prefix if it
    /// looks like one, else as a path that does not exist. An existing
    /// directory whose name is hex is a path.
    pub(crate) fn parse(text: &str) -> Result<RepoArg, CliError> {
        let path = Path::new(text);
        if !text.is_empty() && path.exists() {
            return absolute(path).map(RepoArg::Path);
        }
        let is_prefix =
            (4..=16).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit());
        if is_prefix {
            return Ok(RepoArg::Named(text.to_owned()));
        }
        absolute(path).map(|path| RepoArg::Named(path.display().to_string()))
    }

    /// The current directory: the default repository of commands with
    /// `--repo`.
    pub(crate) fn current() -> Result<RepoArg, CliError> {
        current_dir().map(RepoArg::Path)
    }

    /// The text of the request's repository field.
    pub(crate) fn text(&self) -> String {
        match self {
            RepoArg::Path(path) => path.display().to_string(),
            RepoArg::Named(text) => text.clone(),
        }
    }
}

/// The current directory.
fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir().map_err(|source| CliError::Io {
        op: "read the current directory",
        path: PathBuf::from("."),
        source,
    })
}

/// `path` made absolute against the current directory.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf, CliError> {
    std::path::absolute(path).map_err(|source| CliError::Io {
        op: "resolve",
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_command_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// Help written for a model (ADR 0025): one line of `about`, at most
    /// five of `long_about` at 100 columns (clap joins a paragraph's lines).
    #[test]
    fn help_text_is_short() {
        fn check(command: &clap::Command, path: &str, long: &mut Vec<String>) {
            if let Some(about) = command.get_about() {
                let about = about.to_string();
                if about.contains('\n') || about.len() > 100 {
                    long.push(format!("{path}: about: {about}"));
                }
            }
            if let Some(text) = command.get_long_about() {
                let lines: usize = text
                    .to_string()
                    .lines()
                    .map(|line| line.len().div_ceil(100).max(1))
                    .sum();
                if lines > 5 {
                    long.push(format!("{path}: long_about is {lines} lines"));
                }
            }
            for child in command.get_subcommands() {
                check(child, &format!("{path} {}", child.get_name()), long);
            }
        }
        let mut long = Vec::new();
        check(&Cli::command(), "reef", &mut long);
        assert!(long.is_empty(), "{long:#?}");
    }

    #[test]
    fn output_flag_reads_every_spelling() {
        for line in [
            &["reef", "-o", "json", "ws"][..],
            &["reef", "ws", "--output", "json"],
            &["reef", "--output=json"],
            &["reef", "-ojson"],
            &["reef", "-o=json"],
        ] {
            assert_eq!(output_flag(&args(line)), Some(Output::Json), "{line:?}");
        }
        assert_eq!(
            output_flag(&args(&["reef", "-o", "text"])),
            Some(Output::Text)
        );
        assert_eq!(output_flag(&args(&["reef", "ws", "list"])), None);
        assert_eq!(output_flag(&args(&["reef", "--", "-o", "json"])), None);
    }

    #[test]
    fn orientation_guesses_at_the_root_only() {
        let guess = |list: &[&str]| {
            let error = Cli::try_parse_from(args(list)).unwrap_err();
            let rendered = error.render().to_string();
            orientation_guess(&error, usage_line(&rendered)).map(str::to_owned)
        };
        for word in ORIENTATION_GUESSES {
            assert_eq!(guess(&["reef", word]).as_deref(), Some(*word));
        }
        assert_eq!(
            guess(&["reef", "-o", "json", "--state-dir", "/x", "status"]).as_deref(),
            Some("status")
        );
        assert_eq!(guess(&["reef", "ws", "status"]), None);
        assert_eq!(guess(&["reef", "frobnicate"]), None);
        assert_eq!(guess(&["reef", "ws", "create", "--change"]), None);
        // A unique prefix of a real command parses: `st` is `store`.
        assert!(Cli::try_parse_from(args(&["reef", "st", "ls"])).is_ok());
    }

    #[test]
    fn here_keeps_the_daemon_the_command_line_named() {
        assert_eq!(
            here_argv(&args(&["reef", "-o", "json", "status"])),
            ["reef", "here"]
        );
        assert_eq!(
            here_argv(&args(&[
                "reef",
                "--socket",
                "/s.sock",
                "--state-dir=/st",
                "-q",
                "--config",
                "/c.toml",
                "status",
            ])),
            [
                "reef",
                "--socket",
                "/s.sock",
                "--state-dir",
                "/st",
                "--config",
                "/c.toml",
                "here"
            ]
        );
        assert_eq!(
            here_argv(&args(&["reef", "--", "status", "--socket", "/s"])),
            ["reef", "here"]
        );
    }

    #[test]
    fn repo_args_are_prefixes_or_absolute_paths() {
        let named = |text: &str| RepoArg::Named(text.to_owned());
        assert_eq!(RepoArg::parse("CAFE").unwrap(), named("CAFE"));
        assert_eq!(
            RepoArg::parse("0123456789abcdef").unwrap(),
            named("0123456789abcdef")
        );
        for path in ["./cafe", "abc", "cafe0123456789abc"] {
            let RepoArg::Named(text) = RepoArg::parse(path).unwrap() else {
                panic!("{path} does not exist, so it is not a path to register");
            };
            assert!(Path::new(&text).is_absolute(), "{path}");
        }
        assert_eq!(
            RepoArg::parse("/nonexistent/x").unwrap(),
            named("/nonexistent/x")
        );
        assert_eq!(
            RepoArg::parse(".").unwrap(),
            RepoArg::Path(std::env::current_dir().unwrap())
        );
        // An existing directory is a path even when its name is hex.
        let dir = tempfile::tempdir().unwrap();
        let hex = dir.path().join("cafe");
        std::fs::create_dir(&hex).unwrap();
        assert_eq!(
            RepoArg::parse(hex.to_str().unwrap()).unwrap(),
            RepoArg::Path(hex.clone())
        );
    }
}
