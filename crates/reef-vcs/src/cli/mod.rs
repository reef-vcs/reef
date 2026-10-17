//! The `reef` command line (R1.22): every command is a call to the daemon
//! through [`reef_daemon::client::Client`], except `config show`,
//! `completions`, and `daemon run`. Output follows [`out`]'s rules and
//! failures exit with their kind's code ([`error`]).
//!
//! Later tickets fill in the stubs: `ws move` (R1.06), `ws gc` (R1.17),
//! `backup` and `restore` (R1.18), and `store` (R1.20).

mod autostart;
mod backup;
mod config;
mod daemon;
mod env;
mod error;
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
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::{Config, PathOverrides};
use reef_daemon::logging::{self, LogOptions};

pub(crate) use error::CliError;
pub(crate) use out::{Out, Output};

/// The RPC deadline when `--timeout` is not given.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// `reef`.
#[derive(Debug, clap::Parser)]
#[command(
    name = "reef",
    version,
    propagate_version = true,
    about = "Reef: version control built for a world where AI agents write most of the code"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

/// Flags every command takes.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct Global {
    /// Output format: `json` when stdout is not a terminal, `text` when it is.
    #[arg(short, long, global = true, value_enum, value_name = "FORMAT")]
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
    /// How long to wait for the daemon's answer [default: 30s].
    #[arg(long, global = true, value_name = "DURATION", value_parser = humantime::parse_duration)]
    timeout: Option<Duration>,
    /// Print nothing on stderr but errors.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Log the CLI's own debug events on stderr (`-vv` for trace).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
}

impl Global {
    fn overrides(&self) -> PathOverrides {
        PathOverrides {
            state_dir: self.state_dir.clone(),
            socket: self.socket.clone(),
            config_file: self.config.clone(),
        }
    }

    fn load_config(&self) -> Result<Config, CliError> {
        Config::load(&self.overrides()).map_err(CliError::Config)
    }
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Run, start, stop, or query the daemon.
    Daemon(daemon::Args),
    /// Inspect the daemon's configuration.
    Config(config::Args),
    /// Register and forget repositories.
    Repo(repo::Args),
    /// Create, list, inspect, and release workspaces.
    Ws(ws::Args),
    /// Build and list shared dependency stores.
    Store(store::Args),
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
}

impl Ctx {
    /// Awaits `call` for at most the RPC deadline.
    pub(crate) async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, CliError> {
        match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result.map_err(CliError::Daemon),
            Err(_) => Err(CliError::Timeout {
                after: self.timeout,
            }),
        }
    }
}

/// Parses the command line, runs the command, and maps its failure to an
/// exit code. Never panics on a daemon error.
pub(crate) fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return usage_error(&error, &args),
    };
    // Before logging and the runtime: the daemon installs its own and must
    // start before the process has any other thread.
    if let Command::Daemon(daemon::Args {
        command: daemon::DaemonCommand::Run,
    }) = cli.command
    {
        return daemon::run_foreground(&cli.global);
    }
    // `config show` prints TOML unless asked for JSON; see its module. `ws
    // run`'s stdout is its command's, so it prints JSON only when asked.
    let flag = match &cli.command {
        Command::Config(_) => cli.global.output.or(Some(Output::Text)),
        Command::Ws(args) if args.is_run() => cli.global.output.or(Some(Output::Text)),
        _ => cli.global.output,
    };
    let out = Out::detect(flag, cli.global.quiet);
    let _logging = match init_logging(cli.global.verbose) {
        Ok(guard) => guard,
        Err(error) => {
            out.error(&error);
            return ExitCode::from(error.exit_code());
        }
    };
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
    match runtime.block_on(run(cli, &out)) {
        Ok(code) => code,
        Err(error) => {
            out.error(&error);
            ExitCode::from(error.exit_code())
        }
    }
}

fn init_logging(verbose: u8) -> Result<logging::LoggingGuard, CliError> {
    let mut options = LogOptions::from_env().map_err(CliError::Logging)?;
    match verbose {
        0 => {}
        1 => options.filter = Some("debug".to_owned()),
        _ => options.filter = Some("trace".to_owned()),
    }
    logging::init(&options).map_err(CliError::Logging)
}

async fn run(cli: Cli, out: &Out) -> Result<ExitCode, CliError> {
    let Cli { global, command } = cli;
    let done = match command {
        Command::Daemon(args) => daemon::run(&global, out, args).await,
        Command::Config(args) => args.run(&global.overrides(), out),
        Command::Completions { shell } => {
            let mut command = Cli::command();
            let mut buffer = Vec::new();
            clap_complete::generate(shell, &mut command, "reef", &mut buffer);
            out.raw(&buffer)
        }
        Command::Repo(args) => repo::run(&connect(&global, out).await?, args).await,
        Command::Ws(args) => {
            // Stubs answer without a daemon, so they never start one.
            if let Some(ticket) = args.stub() {
                return Err(CliError::not_implemented(ticket));
            }
            return ws::run(&connect(&global, out).await?, args).await;
        }
        Command::Store(args) => store::run(args),
        Command::Backup(args) => backup::run(args),
        Command::Restore(args) => restore::run(args),
    };
    done.map(|()| ExitCode::SUCCESS)
}

/// Connects to the daemon, starting it first when nothing answers on its
/// socket, unless `--no-autostart`.
pub(crate) async fn connect(global: &Global, out: &Out) -> Result<Ctx, CliError> {
    let config = global.load_config()?;
    let socket = &config.paths.socket;
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
            });
        }
        Err(error) => return Err(CliError::Daemon(error)),
    };
    Ok(Ctx {
        client,
        out: out.clone(),
        timeout: global.timeout.unwrap_or(DEFAULT_TIMEOUT),
    })
}

/// The output mode a command line asks for, read from the raw arguments
/// for when clap could not parse them.
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

/// clap could not parse the command line: help and version print as clap
/// prints them; anything else is a usage error, exit 2, with the JSON error
/// object on stdout in JSON mode.
fn usage_error(error: &clap::Error, args: &[OsString]) -> ExitCode {
    use clap::error::ErrorKind as Clap;
    if matches!(error.kind(), Clap::DisplayHelp | Clap::DisplayVersion) {
        // A closed stdout is not worth reporting.
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    let out = Out::detect(output_flag(args), false);
    let rendered = error.render().to_string();
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
    out.usage_error(&rendered, &message);
    ExitCode::from(reef_lib::error::ErrorKind::InvalidArgument.exit_code())
}

/// `text` as the repository field of a request: an id or id prefix (4 to 16
/// hex characters) as given, anything else as a path made absolute. A
/// directory whose name is hex can be given as `./name`.
pub(crate) fn repo_arg(text: &str) -> Result<String, CliError> {
    let is_prefix = (4..=16).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit());
    if is_prefix {
        return Ok(text.to_owned());
    }
    absolute(Path::new(text)).map(|path| path.display().to_string())
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
    fn repo_args_are_prefixes_or_absolute_paths() {
        assert_eq!(repo_arg("CAFE").unwrap(), "CAFE");
        assert_eq!(repo_arg("0123456789abcdef").unwrap(), "0123456789abcdef");
        for path in [".", "./cafe", "abc", "cafe0123456789abc"] {
            assert!(Path::new(&repo_arg(path).unwrap()).is_absolute(), "{path}");
        }
        assert_eq!(repo_arg("/src/x").unwrap(), "/src/x");
    }
}
