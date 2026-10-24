//! Setting up and running one trial: a fresh home, state directory, and
//! repository; Claude Code with the scenario's flags and a timeout; the
//! judge; the saved stream; and cleanup.
//!
//! Each trial's environment is cleared and rebuilt, so nothing from the
//! machine running the benchmark (a Claude Code session's variables, a
//! user's `~/.claude`, a `reef` on `PATH`) reaches the agent. The variables
//! kept are in [`PASSED_THROUGH`].

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::judge::{WorkspaceRecord, judge};
use super::stream::Transcript;
use super::{Outcome, Pins, SCHEMA, Scenario, TrialResult};
use crate::synth::{self, Preset, Spec};

/// The repository the benchmark uses by default: small, public, and with a
/// `README.md` spelled exactly so (a `Readme.md` would make Linux and
/// macOS trials differ). Pinned so every trial starts from the same tree.
pub const DEFAULT_REPOSITORY: &str = "https://github.com/BurntSushi/same-file.git";
/// The commit [`DEFAULT_REPOSITORY`] is pinned to.
pub const DEFAULT_REVISION: &str = "e7d851bc8e888200d6d08ab612d4cb9b5e53bdf7";

/// Variables a trial keeps from the harness's environment. Everything else
/// is cleared.
pub const PASSED_THROUGH: [&str; 11] = [
    "ANTHROPIC_API_KEY",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
];

/// Where the agent's repository comes from.
#[derive(Debug, Clone)]
pub enum RepoSource {
    /// `git clone <url>`, then the checked-out branch reset to `rev`.
    Clone {
        /// The clone URL.
        url: String,
        /// The commit to reset to.
        rev: String,
    },
    /// The `small` synthetic tree, seed 1, committed once on `main`: for
    /// running without a network.
    Synthetic,
}

impl RepoSource {
    fn describe(&self) -> String {
        match self {
            RepoSource::Clone { url, rev } => format!("{url}@{}", short(rev)),
            RepoSource::Synthetic => "reef-synth small, seed 1".to_owned(),
        }
    }
}

/// Everything a run needs, fixed for all its trials.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// `claude`, as an absolute path.
    pub claude: PathBuf,
    /// `--model`.
    pub model: String,
    /// The directory holding the `reef` and `reef-daemon` builds to test.
    pub reef_bin_dir: PathBuf,
    /// The Reef checkout, for `scripts/package.sh` (scenario C's mirror).
    pub source_root: PathBuf,
    /// The agent's repository.
    pub repository: RepoSource,
    /// How long the agent may run.
    pub timeout: Duration,
    /// `--max-budget-usd`.
    pub budget_usd: f64,
    /// Where results go.
    pub out_dir: PathBuf,
    /// Where trial directories go (a short path: the daemon's socket lives
    /// in one, and Unix sockets have a 103-byte limit on macOS).
    pub work_root: PathBuf,
    /// Keep trial directories instead of deleting them.
    pub keep: bool,
}

/// What a run measures once, before its trials.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// `reef --version`.
    pub reef_version: String,
    /// The commit in it.
    pub reef_commit: Option<String>,
    /// `claude --version`, the fallback when a stream has no `init` event.
    pub claude_version: Option<String>,
    /// Scenario C's installer mirror (`REEF_INSTALL_URL`) and target
    /// (`REEF_TARGET`), or why it could not be built.
    pub mirror: Option<Result<(PathBuf, String), String>>,
}

/// Measures the versions and, if `scenarios` has C, builds the installer
/// mirror under `config.work_root`.
pub fn prepare(config: &RunConfig, scenarios: &[Scenario]) -> Result<Prepared, String> {
    let reef = config.reef_bin_dir.join("reef");
    let reef_version = output(Command::new(&reef).arg("--version"))
        .map_err(|e| format!("cannot run {}: {e}", reef.display()))?;
    let claude_version = output(Command::new(&config.claude).arg("--version"))
        .ok()
        .and_then(|v| v.split_whitespace().next().map(str::to_owned));
    let mirror = scenarios
        .contains(&Scenario::C)
        .then(|| build_mirror(config, &config.work_root.join("mirror")));
    Ok(Prepared {
        reef_commit: reef_commit(&reef_version),
        reef_version,
        claude_version,
        mirror,
    })
}

/// The commit in `reef 0.0.0 (0123456789ab, aarch64-apple-darwin)`.
fn reef_commit(version: &str) -> Option<String> {
    let inner = version.split_once('(')?.1;
    let commit = inner.split([',', ')']).next()?.trim();
    (!commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit())).then(|| commit.to_owned())
}

/// Deletes what [`prepare`] built (the installer mirror), unless the run
/// keeps its directories.
pub fn finish(config: &RunConfig) {
    if !config.keep {
        let _ = fs::remove_dir_all(config.work_root.join("mirror"));
    }
}

/// A release-shaped directory for `REEF_INSTALL_URL`: this build's tarball
/// and `SHA256SUMS`, as `just installer` makes. Until 0.1.0 is released
/// there is nothing on GitHub for the installer to fetch.
fn build_mirror(config: &RunConfig, dist: &Path) -> Result<(PathBuf, String), String> {
    // A previous run's tarball would end up in SHA256SUMS too.
    let _ = fs::remove_dir_all(dist);
    let scripts = config.source_root.join("scripts");
    let version = output(Command::new("bash").arg(scripts.join("version.sh")))?;
    let target = output(Command::new("rustc").arg("-vV"))?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or("`rustc -vV` names no host")?;
    output(
        Command::new("bash")
            .arg(scripts.join("package.sh"))
            .arg(&config.reef_bin_dir)
            .arg(&version)
            .arg(&target)
            .arg(dist),
    )?;
    output(
        Command::new("sh")
            .arg("-c")
            .arg("(sha256sum ./*.tar.gz 2>/dev/null || shasum -a 256 ./*.tar.gz) | sed 's| \\./| |' > SHA256SUMS")
            .current_dir(dist),
    )?;
    Ok((dist.to_path_buf(), target))
}

/// A trial's directories, under one temporary root.
struct TrialDir {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    bin: PathBuf,
    repo: PathBuf,
}

impl TrialDir {
    fn create(work_root: &Path) -> Result<Self, String> {
        fs::create_dir_all(work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
        let root = tempfile::Builder::new()
            .prefix("t")
            .tempdir_in(work_root)
            .map_err(|e| format!("trial directory: {e}"))?
            .keep();
        let dir = TrialDir {
            home: root.join("h"),
            state: root.join("s"),
            bin: root.join("bin"),
            repo: root.join("repo"),
            root,
        };
        for path in [&dir.home, &dir.bin] {
            fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(dir)
    }
}

/// Runs one trial and writes its result, stream, and stderr to
/// `config.out_dir`.
pub fn run_trial(
    config: &RunConfig,
    prepared: &Prepared,
    scenario: Scenario,
    trial: u32,
) -> Result<TrialResult, String> {
    let now = OffsetDateTime::now_utc();
    let date = format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let stem = format!("{date}-{scenario}-{trial}");
    let mut result = TrialResult {
        schema: SCHEMA,
        scenario,
        trial,
        date,
        started_at: now.format(&Rfc3339).unwrap_or_default(),
        pins: Pins {
            model: config.model.clone(),
            claude_code_version: prepared.claude_version.clone(),
            reef_version: prepared.reef_version.clone(),
            reef_commit: prepared.reef_commit.clone(),
            os: std::env::consts::OS.to_owned(),
            repository: config.repository.describe(),
            prompt: scenario.prompt().to_owned(),
        },
        outcome: Outcome::SetupFailed {
            error: String::new(),
        },
        result_subtype: None,
        budget_exceeded: false,
        metrics: None,
        verdict: None,
        success: false,
        stream_file: None,
    };
    fs::create_dir_all(&config.out_dir)
        .map_err(|e| format!("{}: {e}", config.out_dir.display()))?;

    let dir = TrialDir::create(&config.work_root)?;
    let saving = match setup(config, prepared, scenario, &dir) {
        Err(error) => {
            result.outcome = Outcome::SetupFailed { error };
            Ok(())
        }
        Ok(setup) => run_and_judge(config, &dir, &setup, &stem, &mut result),
    };

    // Before the error, so a failed save leaves no daemon running.
    cleanup(config, &dir);
    saving?;
    let json = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    fs::write(
        config.out_dir.join(format!("{stem}.json")),
        scrub(&json) + "\n",
    )
    .map_err(|e| format!("saving the result: {e}"))?;
    Ok(result)
}

/// Runs the agent, judges what it did, and saves its stream and stderr as
/// `<stem>.stream.jsonl` and `<stem>.stderr.log`; the error is a failed
/// save.
fn run_and_judge(
    config: &RunConfig,
    dir: &TrialDir,
    setup: &Setup,
    stem: &str,
    result: &mut TrialResult,
) -> Result<(), String> {
    let scenario = result.scenario;
    let stream_path = dir.root.join("stream.jsonl");
    let stderr_path = dir.root.join("stderr.log");
    let started = Instant::now();
    let outcome = run_agent(
        config,
        scenario,
        dir,
        &setup.env,
        &stream_path,
        &stderr_path,
    );
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result.outcome = outcome.unwrap_or_else(|error| Outcome::SetupFailed { error });

    let stream = fs::read_to_string(&stream_path).unwrap_or_default();
    let transcript = Transcript::parse(&stream);
    if let Some(version) = &transcript.claude_code_version {
        result.pins.claude_code_version = Some(version.clone());
    }
    result.result_subtype = transcript.result.as_ref().map(|r| r.subtype.clone());
    result.budget_exceeded = result.result_subtype.as_deref() == Some("error_max_budget_usd");
    if let Some(error) = never_reached_the_model(&result.outcome, &transcript)
        .or_else(|| skill_missing(scenario, &transcript))
    {
        result.outcome = Outcome::SetupFailed { error };
    }
    if result.agent_ran() {
        let (listed, workspaces) = list_workspaces(config, dir);
        // The judge's input, for reading a verdict later.
        let _ = fs::write(
            config.out_dir.join(format!("{stem}.workspaces.json")),
            listed,
        );
        let mut verdict = judge(
            scenario,
            transcript.final_text(),
            &workspaces,
            &setup.base_readme,
            |commit| readme_at(&dir.repo, commit),
        );
        // The reason may quote the agent's answer.
        verdict.reason = scrub(&verdict.reason);
        result.success = matches!(result.outcome, Outcome::Exited { .. })
            && !result.budget_exceeded
            && verdict.success;
        result.verdict = Some(verdict);
        result.metrics = Some(transcript.metrics(wall_ms));
    }

    let saved = format!("{stem}.stream.jsonl");
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    fs::write(config.out_dir.join(&saved), scrub(&stream))
        .and_then(|()| {
            fs::write(
                config.out_dir.join(format!("{stem}.stderr.log")),
                scrub(&stderr),
            )
        })
        .map_err(|e| format!("saving the stream: {e}"))?;
    result.stream_file = Some(saved);
    Ok(())
}

/// Why the agent never got going, if it exited non-zero without a tool
/// call or any spend: no credential, an API error, Claude Code failing to
/// start. Not the agent's failure, so it is reported as a setup failure.
fn never_reached_the_model(outcome: &Outcome, transcript: &Transcript) -> Option<String> {
    let Outcome::Exited { code } = outcome else {
        return None;
    };
    let spent = transcript
        .result
        .as_ref()
        .and_then(|result| result.total_cost_usd)
        .is_some_and(|cost| cost > 0.0);
    if *code == Some(0) || spent || !transcript.tool_calls.is_empty() {
        return None;
    }
    let said = transcript
        .final_text()
        .lines()
        .next()
        .unwrap_or("no output");
    Some(format!("the agent never reached the model: {said}"))
}

/// For B, why the trial did not measure what it should: the stream's `init`
/// event does not list the `reef` skill that `reef init` wrote, so Claude
/// Code did not load the project's files and B would measure A.
fn skill_missing(scenario: Scenario, transcript: &Transcript) -> Option<String> {
    let commands = transcript.slash_commands.as_ref()?;
    (scenario == Scenario::B && !commands.iter().any(|command| command == "reef"))
        .then(|| "the `reef` skill from `reef init` did not load".to_owned())
}

/// What setup hands the agent's run.
struct Setup {
    env: Vec<(String, OsString)>,
    base_readme: String,
}

fn setup(
    config: &RunConfig,
    prepared: &Prepared,
    scenario: Scenario,
    dir: &TrialDir,
) -> Result<Setup, String> {
    // A git identity, as any developer machine has; the synthetic
    // repository's commit and anything the agent commits need one.
    fs::write(
        dir.home.join(".gitconfig"),
        "[user]\n\tname = Onboarding Agent\n\temail = onboarding@example.com\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n",
    )
    .map_err(|e| format!("~/.gitconfig: {e}"))?;
    if scenario != Scenario::C {
        for binary in ["reef", "reef-daemon"] {
            fs::copy(config.reef_bin_dir.join(binary), dir.bin.join(binary))
                .map_err(|e| format!("copying {binary}: {e}"))?;
        }
    }

    let mut env = base_env(&dir.home, &dir.state);
    let inherited = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let path = agent_path(
        &inherited,
        scenario != Scenario::C,
        &dir.bin,
        &config.reef_bin_dir,
        &dir.root.join("path"),
    )?;
    env.push(("PATH".to_owned(), path));
    if scenario == Scenario::C {
        match &prepared.mirror {
            Some(Ok((dist, target))) => {
                env.push((
                    "REEF_INSTALL_URL".to_owned(),
                    format!("file://{}", dist.display()).into(),
                ));
                env.push(("REEF_TARGET".to_owned(), target.into()));
            }
            Some(Err(error)) => return Err(format!("installer mirror: {error}")),
            None => return Err("no installer mirror was built".to_owned()),
        }
    }

    let git = |args: &[&str], cwd: &Path| {
        output(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(env.iter().cloned()),
        )
    };
    match &config.repository {
        RepoSource::Clone { url, rev } => {
            git(&["clone", "--quiet", url, "repo"], &dir.root)?;
            git(&["reset", "--quiet", "--hard", rev], &dir.repo)?;
        }
        RepoSource::Synthetic => {
            synth::generate(&dir.repo, &Spec::preset(Preset::Small), 1, false)
                .map_err(|e| format!("synthetic repository: {e}"))?;
            fs::write(
                dir.repo.join("README.md"),
                "# Synthetic repository\n\nGenerated by reef-synth for the onboarding benchmark.\n",
            )
            .map_err(|e| format!("README.md: {e}"))?;
            git(&["init", "--quiet"], &dir.repo)?;
            git(&["add", "-A"], &dir.repo)?;
            git(&["commit", "--quiet", "-m", "Initial commit"], &dir.repo)?;
        }
    }
    let base_readme = fs::read_to_string(dir.repo.join("README.md"))
        .map_err(|e| format!("the repository has no README.md: {e}"))?;

    if scenario == Scenario::B {
        output(
            Command::new(dir.bin.join("reef"))
                .args(["init", "--agents", "claude"])
                .current_dir(&dir.repo)
                .env_clear()
                .envs(env.iter().cloned()),
        )
        .map_err(|e| format!("reef init: {e}"))?;
    }
    Ok(Setup { env, base_readme })
}

/// The cleared environment's base: home, state directory, a shell, and
/// [`PASSED_THROUGH`].
fn base_env(home: &Path, state: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = PASSED_THROUGH
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var_os(name)?)))
        .collect();
    env.push(("HOME".to_owned(), home.into()));
    env.push(("REEF_STATE_DIR".to_owned(), state.into()));
    let shell = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    env.push(("SHELL".to_owned(), shell.into()));
    env
}

/// `inherited` (the harness's `PATH`) with no `reef` on it: the build
/// under test's directory is left out, and any other directory holding a
/// `reef` or `reef-daemon` is replaced by a directory under `shadow_root`
/// of symlinks to everything else in it, so the agent keeps the rest of
/// what is there (`~/.cargo/bin`, `/opt/homebrew/bin`). The trial's `bin`
/// comes first when the scenario has Reef installed.
fn agent_path(
    inherited: &OsStr,
    with_reef: bool,
    bin: &Path,
    reef_bin_dir: &Path,
    shadow_root: &Path,
) -> Result<OsString, String> {
    let mut dirs = Vec::new();
    if with_reef {
        dirs.push(bin.to_path_buf());
    }
    for (n, dir) in std::env::split_paths(inherited).enumerate() {
        if dir == reef_bin_dir {
            continue;
        }
        if !dir.join("reef").exists() && !dir.join("reef-daemon").exists() {
            dirs.push(dir);
            continue;
        }
        let shadow = shadow_root.join(n.to_string());
        fs::create_dir_all(&shadow).map_err(|e| format!("{}: {e}", shadow.display()))?;
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name != "reef" && name != "reef-daemon" {
                std::os::unix::fs::symlink(entry.path(), shadow.join(&name))
                    .map_err(|e| format!("{}: {e}", shadow.display()))?;
            }
        }
        dirs.push(shadow);
    }
    std::env::join_paths(dirs).map_err(|e| format!("PATH: {e}"))
}

/// Claude Code's arguments for a scenario.
fn claude_args(config: &RunConfig, scenario: Scenario) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        scenario.prompt(),
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &config.model,
        "--permission-mode",
        "acceptEdits",
        "--allowedTools",
        "Bash,Read,Edit",
        "--permission-prompts",
        "none",
        "--max-budget-usd",
        &config.budget_usd.to_string(),
        "--no-session-persistence",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    if scenario.bare() {
        args.push("--bare".to_owned());
    }
    args
}

/// Runs the agent in its own process group, stdout to `stream`, stderr to
/// `stderr`, and stops the group at the timeout: `TERM`, then `KILL` ten
/// seconds later.
fn run_agent(
    config: &RunConfig,
    scenario: Scenario,
    dir: &TrialDir,
    env: &[(String, OsString)],
    stream: &Path,
    stderr: &Path,
) -> Result<Outcome, String> {
    let mut command = if scenario == Scenario::D {
        // `reef ws run` starts the agent in a new workspace, with
        // `REEF_WORKSPACE` and the rest set.
        let mut command = Command::new(dir.bin.join("reef"));
        command.args(["ws", "run", "--"]).arg(&config.claude);
        command
    } else {
        Command::new(&config.claude)
    };
    let stdout = File::create(stream).map_err(|e| format!("{}: {e}", stream.display()))?;
    let stderr = File::create(stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
    let mut child = command
        .args(claude_args(config, scenario))
        .current_dir(&dir.repo)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("starting the agent: {e}"))?;
    let group = child.id();
    let deadline = Instant::now() + config.timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            // Anything the agent left running in the background.
            signal_group(group, "KILL");
            return Ok(Outcome::Exited {
                code: status.code(),
            });
        }
        if Instant::now() >= deadline {
            signal_group(group, "TERM");
            let grace = Instant::now() + Duration::from_secs(10);
            while Instant::now() < grace {
                if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            signal_group(group, "KILL");
            let _ = child.wait();
            return Ok(Outcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn signal_group(group: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), "--", &format!("-{group}")])
        .stderr(Stdio::null())
        .status();
}

/// `reef` from the build under test, against the trial's daemon, never
/// starting one.
fn harness_reef(config: &RunConfig, dir: &TrialDir) -> Command {
    let mut command = Command::new(config.reef_bin_dir.join("reef"));
    command
        .env_clear()
        .envs(base_env(&dir.home, &dir.state))
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args(["--no-autostart", "-o", "json"])
        .stdin(Stdio::null());
    command
}

/// Every workspace the trial's daemon knows, released ones included, as
/// `reef ws list --all` prints them and parsed; none if no daemon ever
/// started.
fn list_workspaces(config: &RunConfig, dir: &TrialDir) -> (String, Vec<WorkspaceRecord>) {
    let json = output(harness_reef(config, dir).args(["ws", "list", "--all", "--limit", "0"]))
        .unwrap_or_else(|_| "[]".to_owned());
    let records = serde_json::from_str(&json).unwrap_or_default();
    (json, records)
}

/// `README.md` at `commit` in the agent's repository, whose git store holds
/// the daemon's snapshots (jj is colocated with git there).
fn readme_at(repo: &Path, commit: &str) -> Option<String> {
    if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    output(
        Command::new("git")
            .args(["cat-file", "-p", &format!("{commit}:README.md")])
            .current_dir(repo),
    )
    .ok()
}

/// Releases what the agent left active (on Linux an overlay mount would
/// keep the directory busy), stops the daemon, and deletes the trial
/// directory unless asked to keep it.
#[allow(clippy::print_stderr, reason = "a warning from a dev tool")]
fn cleanup(config: &RunConfig, dir: &TrialDir) {
    for workspace in list_workspaces(config, dir)
        .1
        .iter()
        .filter(|ws| ws.state == "active")
    {
        let _ = output(harness_reef(config, dir).args(["ws", "release", &workspace.id]));
    }
    let _ = output(harness_reef(config, dir).args(["daemon", "stop"]));
    if config.keep {
        return;
    }
    if let Err(error) = fs::remove_dir_all(&dir.root) {
        // Not the trial's failure; the next trial uses a new directory.
        eprintln!(
            "onboarding: could not delete {}: {error}",
            dir.root.display()
        );
    }
}

/// Runs `command`, returning its trimmed stdout, or an error naming the
/// command with its stderr.
fn output(command: &mut Command) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        Err(format!(
            "{program} exited with {}: {}",
            out.status,
            last.trim()
        ))
    }
}

fn short(rev: &str) -> &str {
    rev.get(..12).unwrap_or(rev)
}

/// Removes credentials from a saved stream or log: the API key's value
/// wherever it appears, anything that looks like an Anthropic key, and the
/// value of any `Authorization` header.
pub fn scrub(text: &str) -> String {
    let mut text = text.to_owned();
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && key.len() >= 8
    {
        text = text.replace(&key, "[scrubbed]");
    }
    text = scrub_after(&text, "sk-ant-", |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    });
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&scrub_authorization(line));
    }
    out
}

/// Replaces the run of `keep` characters after each `prefix` with
/// `[scrubbed]`.
fn scrub_after(text: &str, prefix: &str, keep: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at]);
        out.push_str("[scrubbed]");
        let after = &rest[at + prefix.len()..];
        let end = after.find(|c: char| !keep(c)).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `Authorization: Bearer x` and `"authorization":"Bearer x"` with the value
/// replaced, case-insensitively.
fn scrub_authorization(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let Some(at) = lower.find("authorization") else {
        return line.to_owned();
    };
    let after = at + "authorization".len();
    let rest = &line[after..];
    let Some(colon) = rest.find(':') else {
        return line.to_owned();
    };
    if !rest[..colon]
        .chars()
        .all(|c| c == '"' || c == '\\' || c.is_whitespace())
    {
        return line.to_owned();
    }
    let value = &rest[colon + 1..];
    let start = value.len() - value.trim_start_matches([' ', '"', '\\']).len();
    let end = value[start..]
        .find(['"', '\\', '\n', ','])
        .map_or(value.len(), |e| start + e);
    format!(
        "{}{}[scrubbed]{}",
        &line[..=after + colon],
        &value[..start],
        scrub_authorization(&value[end..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_keys_and_authorization() {
        assert_eq!(
            scrub("key sk-ant-api03-AbC_d-9 end\n"),
            "key [scrubbed] end\n"
        );
        assert_eq!(
            scrub("Authorization: Bearer abc.def\n"),
            "Authorization: [scrubbed]\n"
        );
        assert_eq!(
            scrub("{\"headers\":{\"authorization\":\"Bearer x\",\"a\":1}}"),
            "{\"headers\":{\"authorization\":\"[scrubbed]\",\"a\":1}}"
        );
        assert_eq!(
            scrub(r#"{"content":"curl -H \"Authorization: Bearer x\" u"}"#),
            r#"{"content":"curl -H \"Authorization: [scrubbed]\" u"}"#
        );
        let clean = "{\"type\":\"result\",\"result\":\"authorization flow\"}\n";
        assert_eq!(scrub(clean), clean);
    }

    #[test]
    fn b_without_the_reef_skill_is_a_setup_failure() {
        let init = |commands: &str| {
            Transcript::parse(&format!(
                r#"{{"type":"system","subtype":"init","slash_commands":{commands}}}"#
            ))
        };
        assert!(skill_missing(Scenario::B, &init(r#"["review"]"#)).is_some());
        assert_eq!(
            skill_missing(Scenario::B, &init(r#"["review","reef"]"#)),
            None
        );
        assert_eq!(skill_missing(Scenario::A, &init(r#"["review"]"#)), None);
        // No `slash_commands` to go by: not a failure.
        assert_eq!(
            skill_missing(
                Scenario::B,
                &Transcript::parse("{\"type\":\"system\",\"subtype\":\"init\"}")
            ),
            None
        );
    }

    /// The check holds against a real B trial: Claude Code 2.1.310 lists
    /// the skill `reef init` wrote in its `init` event.
    #[test]
    fn a_recorded_b_trial_loaded_the_reef_skill() {
        let transcript = Transcript::parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.310-scenario-b.jsonl"
        ));
        assert_eq!(skill_missing(Scenario::B, &transcript), None);
    }

    #[test]
    fn reads_the_commit_from_reef_version() {
        assert_eq!(
            reef_commit("reef 0.0.0 (0123456789ab, aarch64-apple-darwin)").as_deref(),
            Some("0123456789ab")
        );
        assert_eq!(
            reef_commit("reef 0.0.0 (unknown, x86_64-unknown-linux-gnu)"),
            None
        );
        assert_eq!(reef_commit("reef 0.0.0"), None);
    }

    #[test]
    fn the_agent_path_has_no_other_reef() {
        let tmp = tempfile::tempdir().unwrap();
        let [bin, build, tools, plain, shadows] =
            ["bin", "build", "tools", "plain", "shadows"].map(|name| tmp.path().join(name));
        for dir in [&bin, &build, &tools, &plain] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(build.join("reef"), "").unwrap();
        for name in ["reef", "reef-daemon", "git"] {
            fs::write(tools.join(name), "").unwrap();
        }
        let inherited = std::env::join_paths([&build, &tools, &plain]).unwrap();

        let path = agent_path(&inherited, true, &bin, &build, &shadows).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        // `tools` keeps its `git` through a shadow without `reef`.
        let shadow = shadows.join("1");
        assert_eq!(dirs, [bin.clone(), shadow.clone(), plain.clone()]);
        assert!(shadow.join("git").exists());
        assert!(!shadow.join("reef").exists() && !shadow.join("reef-daemon").exists());

        let path = agent_path(&inherited, false, &bin, &build, &tmp.path().join("s2")).unwrap();
        assert!(!std::env::split_paths(&path).any(|dir| dir == bin));
    }
}
