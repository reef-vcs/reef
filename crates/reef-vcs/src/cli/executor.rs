//! `reef executor` (R3.06, ADR 0060): Reef's reference executor. `run`
//! claims jobs and runs their checks in a sandbox; `check` reports the
//! configuration, the sandbox's escape probes, and what a revision would
//! run. The work is [`reef_executor`]'s; this is its command line.
//! `docs/executor.md` is the operator's guide.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reef_api::v1;
use reef_client::remote::Token;
use reef_error::ErrorKind;
use reef_executor::config::{Config, RepoChecks, RepositoryChecks};
use reef_executor::declare::{self, TreeChecks};
use reef_executor::job::{Context, Hidden};
use reef_executor::materialize::BlobCache;
use reef_executor::report::{
    CheckReport, JobReport, PlanReport, PlannedReport, ProbeReport, SandboxReport,
};
use reef_executor::sandbox::Backend;
use reef_executor::serve::{self, Options};
use tokio::sync::watch;

use super::out::{TextRender, block};
use super::scope::{self, ScopeArgs};
use super::{CliError, Global, Out, connect, connect_with};

/// `reef executor`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ExecutorCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ExecutorCommand {
    /// Claim jobs, run their checks in a sandbox, and return signed evidence, until stopped.
    #[command(after_help = "Example: reef executor run --exit-when-idle 2m   # in CI")]
    Run(RunArgs),
    /// Check the configuration and the sandbox (its escape probes), and show what a revision would run.
    #[command(
        after_help = "Example: reef executor check -r @ --kind test --target crates/reef-lib"
    )]
    Check(CheckArgs),
    /// Run one escape probe inside the sandbox (used by `check`).
    #[command(name = "__probe", hide = true)]
    Probe {
        /// The probe.
        name: String,
        /// Its argument.
        #[arg(default_value = "")]
        arg: String,
    },
}

/// `reef executor run`.
#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The configuration file [default: `REEF_EXECUTOR_CONFIG`, else executor.toml in the state directory].
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Only jobs of this repository, by id (repeatable) [default: `[executor] repos`, else all].
    #[arg(long = "repo", value_name = "REPO")]
    repos: Vec<String>,
    /// Jobs to run at once, 1 to 16.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
    jobs: u8,
    /// Claim one job (one per --jobs), run it, and exit.
    #[arg(long)]
    once: bool,
    /// With --once, how long to wait for a job.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration, default_value = "60s", requires = "once")]
    wait: Duration,
    /// With --once, exit 3 when no job came.
    #[arg(long, requires = "once")]
    require_job: bool,
    /// Exit after this long with no job (a CI step that drains the queue).
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    exit_when_idle: Option<Duration>,
    /// Keep each job's directory under the work directory, for debugging.
    #[arg(long)]
    keep_work: bool,
    /// Read the remote token from this file instead of `REEF_REMOTE_TOKEN`.
    #[arg(long, value_name = "FILE")]
    token_file: Option<PathBuf>,
}

/// `reef executor check`.
#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// The configuration file [default: `REEF_EXECUTOR_CONFIG`, else executor.toml in the state directory].
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Also show what this revision's tree would run, from its reef-checks.toml.
    #[arg(short = 'r', long, value_name = "REV")]
    revision: Option<String>,
    /// With -r, a kind to plan (repeatable) [default: the kinds this executor claims].
    #[arg(long = "kind", value_name = "KIND", requires = "revision")]
    kinds: Vec<String>,
    /// With -r, a target to plan (repeatable) [default: the whole check].
    #[arg(long = "target", value_name = "TARGET", requires = "revision")]
    targets: Vec<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

impl Args {
    /// `__probe`: runs inside a sandbox, with no daemon and no output
    /// rules but its own.
    pub(crate) fn probe(&self) -> Option<i32> {
        match &self.command {
            ExecutorCommand::Probe { name, arg } => {
                Some(reef_executor::probe::probe_main(name, arg))
            }
            _ => None,
        }
    }
}

/// Runs `reef executor`.
pub(crate) async fn run(global: &Global, out: &Out, args: Args) -> Result<(), CliError> {
    match args.command {
        ExecutorCommand::Run(run) => serve_jobs(global, out, run).await,
        ExecutorCommand::Check(check) => check_config(global, out, check).await,
        ExecutorCommand::Probe { .. } => Ok(()),
    }
}

/// The executor's error as the CLI's.
fn refused(error: reef_executor::Error) -> CliError {
    match error {
        reef_executor::Error::Daemon(error) => CliError::Daemon(*error),
        error => CliError::Refused {
            kind: error.kind(),
            advice: error.advice(),
            message: error.to_string(),
        },
    }
}

async fn serve_jobs(global: &Global, out: &Out, args: RunArgs) -> Result<(), CliError> {
    let state_dir = global.state_dir()?;
    let config = Config::locate(args.config.as_deref(), &state_dir).map_err(refused)?;
    let backend = Backend::resolve(&config.sandbox).map_err(refused)?;
    // A check can read this process's arguments and environment on macOS
    // (KERN_PROCARGS2 has no Seatbelt rule): run again with only the
    // variables the executor needs, the remote token on a pipe.
    #[cfg(target_os = "macos")]
    if matches!(backend, Backend::Seatbelt { .. }) && needs_scrub(&config, &args) {
        return Err(reexec_scrubbed(&config, &args));
    }
    if backend != Backend::None && reef_executor::config::home_dirs().is_empty() {
        return Err(CliError::Refused {
            kind: ErrorKind::Unavailable,
            message: "no home directory to hide from checks: HOME is unset and the password database names none".into(),
            advice: Some(reef_error::Advice::hint("Set HOME to this user's home directory.")),
        });
    }
    if backend == Backend::None {
        out.note(
            "reef executor: [sandbox] kind = \"none\": checks run with this user's privileges, unsandboxed; every object records sandbox none",
        );
    } else {
        backend.self_test().map_err(refused)?;
    }
    let token = match &args.token_file {
        Some(path) => Some(read_token(path)?),
        None => None,
    };
    let ctx = connect_with(global, out, token).await?;
    serve::harden();
    let lock = serve::take_work_dir(&config.work_dir).map_err(refused)?;
    let cache = BlobCache::new(config.work_dir.join("blobs"), config.materialize.cache_size)
        .map_err(|source| CliError::Refused {
            kind: ErrorKind::Io,
            message: format!("the blob cache in {}: {source}", config.work_dir.display()),
            advice: None,
        })?;
    let hidden = hidden(global, &config, args.token_file.as_deref());
    let (shutdown_tx, shutdown) = watch::channel(false);
    let stop = Arc::new(shutdown_tx);
    serve::handle_signals(Arc::clone(&stop));
    let name = config.name();
    let context = Arc::new(Context {
        client: ctx.client.clone(),
        config: Arc::new(config),
        backend,
        cache,
        hidden,
        name: name.clone(),
        keep_work: args.keep_work,
        shutdown,
        stop,
    });
    out.note(&format!(
        "reef executor: {name} claiming {} in sandbox {}",
        context.config.kinds().join(", "),
        context.backend.kind()
    ));
    let printer = out.clone();
    let on_report: Arc<dyn Fn(&JobReport) + Send + Sync> = Arc::new(move |report: &JobReport| {
        if printer.is_json() {
            let _ = printer.stream_item(&Line(report.clone()));
        } else {
            printer.note(&job_line(report));
        }
    });
    let options = Options {
        repos: args.repos,
        jobs: usize::from(args.jobs),
        once: args.once,
        wait: args.wait,
        exit_when_idle: args.exit_when_idle,
    };
    let served = serve::serve(Arc::clone(&context), options, on_report)
        .await
        .map_err(refused)?;
    drop(lock);
    if args.once && args.require_job && served.jobs == 0 {
        return Err(refused(reef_executor::Error::NoJob(
            humantime::format_duration(args.wait).to_string(),
        )));
    }
    Ok(())
}

/// The paths a check never sees, beyond the home: Reef's state and
/// configuration, the socket, the runtime directory, the default workspace
/// root `~/.reef` (#260), the executor's own configuration and token file.
fn hidden(global: &Global, config: &Config, token_file: Option<&Path>) -> Hidden {
    let mut denied = Vec::new();
    if let Ok(client) = global.load_config() {
        denied.push(client.state_dir);
        denied.push(client.config_file);
        denied.push(client.socket);
    } else if let Ok(state) = global.state_dir() {
        denied.push(state);
    }
    if let Ok(defaults) = reef_client::paths::platform_defaults() {
        denied.extend(defaults.runtime_dir);
        // Every agent's workspace, outside the state directory since #260.
        denied.extend(defaults.home.map(|home| home.join(".reef")));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|r| !r.is_empty()) {
        denied.push(PathBuf::from(runtime));
    }
    denied.extend(config.path.iter().cloned());
    denied.extend(token_file.map(Path::to_owned));
    denied.sort();
    denied.dedup();
    Hidden {
        homes: reef_executor::config::home_dirs(),
        denied,
    }
}

/// Closes the pipe a scrubbing re-exec passed as `/dev/fd/N` once it is
/// read, so no check inherits it.
#[allow(unsafe_code)]
fn close_passed_fd(path: &Path) {
    use std::os::fd::FromRawFd as _;
    if let Some(fd) = path
        .to_str()
        .and_then(|p| p.strip_prefix("/dev/fd/"))
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|fd| *fd > 2)
    {
        // SAFETY: the fd was opened for this process by the re-exec that
        // named it, and nothing else holds it; dropping it closes it.
        drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
    }
}

/// The token in `path`: its first line, trimmed.
fn read_token(path: &Path) -> Result<Token, CliError> {
    let text = zeroize::Zeroizing::new(std::fs::read_to_string(path).map_err(|source| {
        CliError::Refused {
            kind: reef_error::classify_by_source(&source),
            message: format!("reading the token file {}: {source}", path.display()),
            advice: None,
        }
    })?);
    close_passed_fd(path);
    let line = text.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return Err(CliError::Refused {
            kind: ErrorKind::InvalidArgument,
            message: format!("the token file {} is empty", path.display()),
            advice: None,
        });
    }
    Ok(Token::new(line))
}

/// The variables `reef executor run` keeps when it runs itself again on
/// macOS: what it and the checks' environment need, never a credential.
#[cfg(target_os = "macos")]
fn kept_variable(name: &str, config: &Config) -> bool {
    const KEPT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "LANG",
        "TZ",
        "TERM",
        "RUST_LOG",
        "NO_COLOR",
        // Set by the system in every process: a text-encoding hint.
        "__CF_USER_TEXT_ENCODING",
    ];
    name != reef_client::remote::TOKEN_ENV
        && (KEPT.contains(&name)
            || name.starts_with("LC_")
            || name.starts_with("REEF_")
            || config.sandbox.pass_env.iter().any(|kept| kept == name))
}

/// Set in the environment of the process a scrubbing re-exec starts.
#[cfg(target_os = "macos")]
const SCRUBBED: &str = "REEF_EXECUTOR_SCRUBBED";

/// Whether this process's environment holds anything [`kept_variable`]
/// drops.
#[cfg(target_os = "macos")]
fn needs_scrub(config: &Config, args: &RunArgs) -> bool {
    // Once only, whatever the system adds to a new process.
    if std::env::var_os(SCRUBBED).is_some() {
        return false;
    }
    let token = std::env::var_os(reef_client::remote::TOKEN_ENV).is_some();
    (token && args.token_file.is_none())
        || std::env::vars_os().any(|(name, _)| {
            name.to_str()
                .is_none_or(|name| !kept_variable(name, config))
        })
}

/// Replaces this process with itself, its environment cut to
/// [`kept_variable`]'s and `REEF_REMOTE_TOKEN`, if set, passed through a
/// pipe as `--token-file /dev/fd/N`. Returns only on failure.
#[cfg(target_os = "macos")]
fn reexec_scrubbed(config: &Config, run: &RunArgs) -> CliError {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::CommandExt as _;
    let fail = |message: String| CliError::Refused {
        kind: ErrorKind::Internal,
        message,
        advice: Some(reef_error::Advice::hint(
            "Run the executor from an environment holding only what it needs, with the token in --token-file.",
        )),
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail(format!("finding this executable: {error}")),
    };
    let mut args = std::env::args_os();
    let first = args.next().unwrap_or_else(|| exe.clone().into_os_string());
    let mut command = std::process::Command::new(&exe);
    command.arg0(first).args(args).env_clear();
    command.envs(std::env::vars_os().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|name| kept_variable(name, config))
    }));
    command.env(SCRUBBED, "1");
    // Held until the exec, which keeps its read end.
    let mut _reader = None;
    if run.token_file.is_none()
        && let Some(token) =
            std::env::var_os(reef_client::remote::TOKEN_ENV).filter(|t| !t.is_empty())
    {
        let (reader, mut writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => return fail(format!("a pipe for the token: {error}")),
        };
        if let Err(error) = writer
            .write_all(token.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
        {
            return fail(format!("passing the token: {error}"));
        }
        drop(writer);
        if let Err(error) = nix::fcntl::fcntl(
            &reader,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
        ) {
            return fail(format!("passing the token: {error}"));
        }
        command
            .arg("--token-file")
            .arg(format!("/dev/fd/{}", reader.as_raw_fd()));
        _reader = Some(reader);
    }
    let dropped: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| !kept_variable(name, config))
        .collect();
    tracing::debug!(
        ?dropped,
        "running again with only the variables the executor needs"
    );
    let error = command.exec();
    fail(format!("running again with a reduced environment: {error}"))
}

/// One finished job, on stderr in text mode.
fn job_line(report: &JobReport) -> String {
    let kinds: Vec<String> = {
        let mut kinds: Vec<String> = report.pairs.iter().map(|p| p.kind.clone()).collect();
        kinds.dedup();
        kinds
    };
    let short = report.job_id.get(..12).unwrap_or(&report.job_id);
    let mut line = format!(
        "job {short} {}: {}  pass {} fail {} error {}  {}",
        kinds.join(" "),
        report.outcome.replace('_', " "),
        report.pass,
        report.fail,
        report.error,
        humantime::format_duration(Duration::from_secs(report.elapsed_ms / 1000)),
    );
    if let Some(message) = &report.message {
        use std::fmt::Write as _;
        let _ = write!(line, "  ({message})");
    }
    line
}

/// A job report as a JSON line.
#[derive(serde::Serialize)]
#[serde(transparent)]
struct Line(JobReport);

impl TextRender for Line {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", job_line(&self.0))
    }
}

#[expect(clippy::too_many_lines, reason = "the report, then each way it fails")]
async fn check_config(global: &Global, out: &Out, args: CheckArgs) -> Result<(), CliError> {
    let state_dir = global.state_dir()?;
    let config = Config::locate(args.config.as_deref(), &state_dir).map_err(refused)?;
    let (backend, problem) = match Backend::resolve(&config.sandbox) {
        Ok(backend) => match backend.self_test() {
            Ok(()) => (Some(backend), None),
            Err(error) => (Some(backend), Some(error)),
        },
        Err(error) => (None, Some(error)),
    };
    let mut probes = Vec::new();
    if let (Some(backend), None) = (&backend, &problem) {
        let exe = std::env::current_exe().map_err(|source| CliError::Refused {
            kind: ErrorKind::Io,
            message: format!("finding this executable: {source}"),
            advice: None,
        })?;
        let hidden = hidden(global, &config, None);
        let mut hidden_dirs: Vec<PathBuf> = hidden.homes.clone();
        hidden_dirs.push(config.work_dir.clone());
        let surround = reef_executor::probe::Surround {
            readable: config.read_paths(),
            hidden: hidden_dirs,
            denied: hidden
                .denied
                .into_iter()
                .chain(config.deny_paths())
                .collect(),
            env: vec![
                (
                    "PATH".into(),
                    std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
                ),
                ("TERM".into(), "dumb".into()),
            ],
        };
        probes = reef_executor::probe::run_all(backend, &exe, &surround).map_err(refused)?;
    }
    let plan = match &args.revision {
        Some(revision) => Some(plan_at(global, out, &config, &args, revision).await?),
        None => None,
    };
    let kind = backend.as_ref().map_or("none", Backend::kind);
    let held = problem.is_none() && reef_executor::probe::holds(&probes);
    let report = CheckReport {
        config: config.path.as_ref().map(|p| p.display().to_string()),
        name: config.name(),
        kinds: config.kinds(),
        labels: {
            let mut labels = vec![
                format!("os={}", std::env::consts::OS),
                format!("arch={}", std::env::consts::ARCH),
                format!("sandbox={kind}"),
            ];
            labels.extend(config.executor.labels.iter().cloned());
            labels
        },
        repository_checks: match config.repository.checks {
            RepositoryChecks::Allow => "allow".into(),
            RepositoryChecks::Deny => "deny".into(),
        },
        max_repository_network: config.sandbox.max_repository_network.as_str().into(),
        work_dir: config.work_dir.display().to_string(),
        sandbox: SandboxReport {
            kind: kind.to_owned(),
            holds: held,
            problem: problem.as_ref().map(ToString::to_string),
            probes,
        },
        checks: config.checks.iter().map(|c| c.name.clone()).collect(),
        plan,
    };
    // A failure prints its error document alone in JSON; text shows the
    // report first.
    let failing = problem.is_some() || (kind != "none" && !held);
    if !failing || !out.is_json() {
        out.object(&Report(report.clone()))?;
    }
    if let Some(error) = problem {
        return Err(refused(error));
    }
    if kind == "none" {
        out.note("reef executor: [sandbox] kind = \"none\": nothing is refused, by configuration");
        return Ok(());
    }
    if !held {
        let failed: Vec<String> = report
            .sandbox
            .probes
            .iter()
            .filter(|p| p.boundary && !p.refused)
            .map(|p| format!("{} ({})", p.name, p.detail))
            .collect();
        return Err(CliError::Refused {
            kind: ErrorKind::Unavailable,
            message: format!("the sandbox did not refuse: {}", failed.join("; ")),
            advice: Some(reef_error::Advice::hint(
                "Each failing probe's detail says what it reached; docs/executor.md's troubleshooting section covers each.",
            )),
        });
    }
    Ok(())
}

/// What each pair would run at `revision`, from the executor's checks and
/// the tree's reef-checks.toml.
async fn plan_at(
    global: &Global,
    out: &Out,
    config: &Config,
    args: &CheckArgs,
    revision: &str,
) -> Result<Vec<PlanReport>, CliError> {
    let ctx = connect(global, out).await?;
    let scope = scope::resolve(&ctx, &args.scope).await?;
    let tree = if config.repository.checks == RepositoryChecks::Deny {
        TreeChecks::Denied
    } else {
        let read = ctx
            .call(ctx.client.read_file_to_end(v1::ReadFileRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision: revision.to_owned(),
                path: reef_executor::CHECKS_FILE.to_owned(),
                ..v1::ReadFileRequest::default()
            }))
            .await;
        match read {
            Ok(read) => match RepoChecks::parse(&read.content) {
                Ok(checks) => TreeChecks::Parsed(checks),
                Err(message) => TreeChecks::Invalid(message),
            },
            Err(CliError::Daemon(error)) if error.reason() == reef_api::error::PATH_NOT_FOUND => {
                TreeChecks::Absent
            }
            Err(error) => return Err(error),
        }
    };
    let kinds = if args.kinds.is_empty() {
        config.kinds()
    } else {
        args.kinds.clone()
    };
    let pairs = declare::pairs(&kinds, &args.targets);
    Ok(declare::plan(config, &tree, &pairs)
        .into_iter()
        .map(|plan| PlanReport {
            kind: plan.pair.kind,
            target: plan.pair.target,
            checks: plan
                .checks
                .iter()
                .map(|p| PlannedReport {
                    name: p.check.name.clone(),
                    source: p.check.source.as_str().to_owned(),
                    argv: p.argv.clone(),
                    network: p.network.as_str().to_owned(),
                })
                .collect(),
            note: plan.note,
        })
        .collect())
}

#[derive(Clone, serde::Serialize)]
#[serde(transparent)]
struct Report(CheckReport);

impl TextRender for Report {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        block(
            w,
            &[
                (
                    "config",
                    r.config.clone().unwrap_or_else(|| "(defaults)".into()),
                ),
                ("name", r.name.clone()),
                ("kinds", r.kinds.join(", ")),
                ("labels", r.labels.join(", ")),
                ("repository checks", r.repository_checks.clone()),
                ("repository network", r.max_repository_network.clone()),
                ("work", r.work_dir.clone()),
                (
                    "checks",
                    if r.checks.is_empty() {
                        "(none of its own)".into()
                    } else {
                        r.checks.join(", ")
                    },
                ),
                (
                    "sandbox",
                    format!(
                        "{} ({})",
                        r.sandbox.kind,
                        match (&r.sandbox.problem, r.sandbox.holds) {
                            (Some(problem), _) => format!("cannot be applied: {problem}"),
                            (None, true) => "holds".into(),
                            (None, false) => "a probe escaped".into(),
                        }
                    ),
                ),
            ],
        )?;
        for probe in &r.sandbox.probes {
            writeln!(w, "  {}", probe_line(probe))?;
        }
        if let Some(plan) = &r.plan {
            writeln!(w, "plan:")?;
            for pair in plan {
                let label = match &pair.target {
                    Some(target) => format!("{} {target}", pair.kind),
                    None => pair.kind.clone(),
                };
                if pair.checks.is_empty() {
                    writeln!(
                        w,
                        "  {label}: error ({})",
                        pair.note.clone().unwrap_or_default()
                    )?;
                }
                for check in &pair.checks {
                    writeln!(
                        w,
                        "  {label}: {} ({}, network {}): {}",
                        check.name,
                        check.source,
                        check.network,
                        super::out::shell_line(&check.argv)
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn probe_line(probe: &ProbeReport) -> String {
    let verdict = match (probe.refused, probe.boundary) {
        (true, _) => "refused",
        (false, true) => "ESCAPED",
        (false, false) => "allowed (a documented limit)",
    };
    format!("{:<22} {verdict}: {}", probe.name, probe.detail)
}
