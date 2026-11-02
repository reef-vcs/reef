//! Setting up and running one trial: a fresh home, state directory, and
//! repository; Claude Code with the scenario's flags and a timeout; the
//! judge; the saved stream; and cleanup.
//!
//! Each trial's environment is cleared and rebuilt ([`crate::agent::env`]),
//! so nothing from the machine running the benchmark reaches the agent.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub use crate::agent::env::PASSED_THROUGH;
use crate::agent::env::{base_env, short_tempdir, trial_path, write_gitconfig};
use crate::agent::harness::claude::Claude;
use crate::agent::harness::{Harness as _, Invocation, Tools};
use crate::agent::process::{AgentCommand, output, reef_commit, run_agent, today_and_now};
pub use crate::agent::scrub::scrub;
use crate::agent::transcript::Transcript;

use super::judge::{WorkspaceRecord, judge};
use super::{Outcome, Pins, SCHEMA, Scenario, TrialResult};
use crate::synth::{self, Preset, Spec};

/// The repository the benchmark uses by default: small, public, and with a
/// `README.md` spelled exactly so (a `Readme.md` would make Linux and
/// macOS trials differ). Pinned so every trial starts from the same tree.
pub const DEFAULT_REPOSITORY: &str = "https://github.com/BurntSushi/same-file.git";
/// The commit [`DEFAULT_REPOSITORY`] is pinned to.
pub const DEFAULT_REVISION: &str = "e7d851bc8e888200d6d08ab612d4cb9b5e53bdf7";

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
        let root = short_tempdir(work_root, "t")?;
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
    let (date, started_at) = today_and_now();
    let stem = format!("{date}-{scenario}-{trial}");
    let mut result = TrialResult {
        schema: SCHEMA,
        scenario,
        trial,
        date,
        started_at,
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
    let harness = claude(config);
    let args = harness.args(&Invocation {
        prompt: scenario.prompt(),
        model: &config.model,
        budget_usd: config.budget_usd,
        isolated: scenario.bare(),
        tools: scenario_tools(scenario),
    });
    let mut command = if scenario == Scenario::D {
        // `reef ws run` starts the agent in a new workspace, with
        // `REEF_WORKSPACE` and the rest set.
        let mut command = Command::new(dir.bin.join("reef"));
        command.args(["ws", "run", "--"]).arg(harness.program());
        command
    } else {
        Command::new(harness.program())
    };
    command.args(args);
    let started = Instant::now();
    let outcome = run_agent(
        AgentCommand {
            command,
            cwd: &dir.repo,
            env: &setup.env,
        },
        config.timeout,
        &stream_path,
        &stderr_path,
    );
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result.outcome = outcome.unwrap_or_else(|error| Outcome::SetupFailed { error });

    let stream = fs::read_to_string(&stream_path).unwrap_or_default();
    let transcript = harness.parse(&stream);
    if let Some(version) = &transcript.harness_version {
        result.pins.claude_code_version = Some(version.clone());
    }
    result.result_subtype = transcript.result.as_ref().map(|r| r.subtype.clone());
    result.budget_exceeded = result.result_subtype.as_deref() == Some("error_max_budget_usd");
    if let Some(error) = never_reached_the_model(&result.outcome, &transcript)
        .or_else(|| skill_missing(scenario, &transcript))
        .or_else(|| plugin_missing(scenario, &transcript))
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

/// The tools a trial's agent may use: a shell and an editor, and for E
/// the plugin's MCP server too, which Claude Code names
/// `plugin_<plugin>_<server>`.
fn scenario_tools(scenario: Scenario) -> Tools<'static> {
    match scenario {
        Scenario::E => Tools::ShellEditAndServer {
            server: "plugin_reef_reef",
        },
        _ => Tools::ShellAndEdit,
    }
}

/// For E, why the trial did not measure what it should: the stream's `init`
/// event lists neither the plugin's skill nor its MCP server's tools, so
/// the plugin did not load and E would measure A.
fn plugin_missing(scenario: Scenario, transcript: &Transcript) -> Option<String> {
    if scenario != Scenario::E {
        return None;
    }
    let skill = transcript
        .slash_commands
        .iter()
        .flatten()
        .any(|command| command == "reef" || command.ends_with(":reef"));
    let server = transcript.tools.iter().flatten().any(|tool| {
        tool.starts_with("mcp__") && tool.contains("reef") && tool.ends_with("__get_status")
    });
    match (skill, server) {
        (true, true) => None,
        (false, _) => Some("the Reef plugin's skill did not load".to_owned()),
        (true, false) => Some("the Reef plugin's MCP server did not load".to_owned()),
    }
}

/// The repository whose Claude Code plugin scenario E installs: the
/// checkout this binary was built from.
const CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// A marketplace at `into` holding the checkout's Claude Code plugin: its
/// marketplace file, with the plugin's source (the release tag, which a
/// change under test does not have) rewritten to the copied tree.
pub(crate) fn plugin_marketplace(checkout: &Path, into: &Path) -> Result<(), String> {
    let file = checkout.join(".claude-plugin/marketplace.json");
    let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut marketplace: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let plugin = marketplace["plugins"]
        .get_mut(0)
        .ok_or_else(|| format!("{} lists no plugin", file.display()))?;
    plugin["source"] = "./plugins/claude-code".into();
    fs::create_dir_all(into.join(".claude-plugin")).map_err(|e| e.to_string())?;
    fs::write(
        into.join(".claude-plugin/marketplace.json"),
        serde_json::to_string_pretty(&marketplace).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("the marketplace: {e}"))?;
    copy_tree(
        &checkout.join("plugins/claude-code"),
        &into.join("plugins/claude-code"),
    )
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| format!("{}: {e}", target.display()))?;
        }
    }
    Ok(())
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
    // The synthetic repository's commit and anything the agent commits need
    // a git identity.
    write_gitconfig(&dir.home, "Onboarding Agent", "onboarding@example.com")?;
    if scenario != Scenario::C {
        for binary in ["reef", "reef-daemon"] {
            fs::copy(config.reef_bin_dir.join(binary), dir.bin.join(binary))
                .map_err(|e| format!("copying {binary}: {e}"))?;
        }
    }

    let mut env = base_env(&dir.home, &dir.state);
    let path = trial_path(
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
    if scenario == Scenario::E {
        // Into the trial's home, as the documented install line does, from
        // the checkout under test; no credential is needed.
        let marketplace = dir.root.join("marketplace");
        plugin_marketplace(Path::new(CHECKOUT), &marketplace)?;
        let mut plain = env.clone();
        crate::agent::env::without_credentials(&mut plain);
        for args in [
            &["plugin", "marketplace", "add"][..],
            &["plugin", "install", "reef@reef"],
        ] {
            let mut command = Command::new(&config.claude);
            command.args(args);
            if args.len() == 3 {
                command.arg(&marketplace);
            }
            output(
                command
                    .current_dir(&dir.root)
                    .env_clear()
                    .envs(plain.iter().cloned()),
            )
            .map_err(|e| format!("claude {}: {e}", args.join(" ")))?;
        }
    }
    Ok(Setup { env, base_readme })
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

/// Claude Code at the run's executable.
fn claude(config: &RunConfig) -> Claude {
    Claude {
        program: config.claude.clone(),
    }
}

fn short(rev: &str) -> &str {
    rev.get(..12).unwrap_or(rev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(stream: &str) -> Transcript {
        Claude {
            program: PathBuf::from("claude"),
        }
        .parse(stream)
    }

    /// R2.25 16: E's marketplace is the checkout's, its plugin's source the
    /// copied tree, and a trial whose `init` event lacks the plugin's skill
    /// or server is a setup failure.
    #[test]
    fn e_installs_the_checkouts_plugin() {
        let into = tempfile::tempdir().unwrap();
        plugin_marketplace(Path::new(CHECKOUT), into.path()).unwrap();
        let text = fs::read_to_string(into.path().join(".claude-plugin/marketplace.json")).unwrap();
        let marketplace: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(marketplace["name"], "reef");
        assert_eq!(marketplace["plugins"][0]["name"], "reef");
        assert_eq!(marketplace["plugins"][0]["source"], "./plugins/claude-code");
        for file in [
            ".claude-plugin/plugin.json",
            ".mcp.json",
            "hooks/hooks.json",
            "skills/reef/SKILL.md",
        ] {
            assert!(
                into.path().join("plugins/claude-code").join(file).is_file(),
                "{file}"
            );
        }

        let init = |commands: &str, tools: &str| {
            parse(&format!(
                r#"{{"type":"system","subtype":"init","slash_commands":{commands},"tools":{tools}}}"#
            ))
        };
        let loaded = init(
            r#"["review","reef:reef"]"#,
            r#"["Bash","mcp__plugin_reef_reef__get_status"]"#,
        );
        assert_eq!(plugin_missing(Scenario::E, &loaded), None);
        assert!(
            plugin_missing(
                Scenario::E,
                &init(r#"["review"]"#, r#"["mcp__plugin_reef_reef__get_status"]"#)
            )
            .unwrap()
            .contains("skill")
        );
        assert!(
            plugin_missing(Scenario::E, &init(r#"["reef:reef"]"#, r#"["Bash"]"#))
                .unwrap()
                .contains("MCP server")
        );
        assert_eq!(plugin_missing(Scenario::A, &init("[]", "[]")), None);
        assert!(!Scenario::E.bare());
        assert_eq!(Scenario::E.prompt(), Scenario::A.prompt());
    }

    #[test]
    fn b_without_the_reef_skill_is_a_setup_failure() {
        let init = |commands: &str| {
            parse(&format!(
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
                &parse("{\"type\":\"system\",\"subtype\":\"init\"}")
            ),
            None
        );
    }

    /// The check holds against a real B trial: Claude Code 2.1.310 lists
    /// the skill `reef init` wrote in its `init` event.
    #[test]
    fn a_recorded_b_trial_loaded_the_reef_skill() {
        let transcript = parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.310-scenario-b.jsonl"
        ));
        assert_eq!(skill_missing(Scenario::B, &transcript), None);
    }

    /// The arguments RX.07's baseline ran with, unchanged by the move onto
    /// the adapter.
    #[test]
    fn claude_code_gets_the_baseline_arguments() {
        let config = RunConfig {
            claude: PathBuf::from("claude"),
            model: "m".to_owned(),
            reef_bin_dir: PathBuf::new(),
            source_root: PathBuf::new(),
            repository: RepoSource::Synthetic,
            timeout: Duration::from_secs(1),
            budget_usd: 2.0,
            out_dir: PathBuf::new(),
            work_root: PathBuf::new(),
            keep: false,
        };
        let args = |scenario: Scenario| {
            claude(&config).args(&Invocation {
                prompt: "p",
                model: &config.model,
                budget_usd: config.budget_usd,
                isolated: scenario.bare(),
                tools: Tools::ShellAndEdit,
            })
        };
        let baseline = [
            "-p",
            "p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "m",
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            "Bash,Read,Edit",
            "--permission-prompts",
            "none",
            "--max-budget-usd",
            "2",
            "--no-session-persistence",
        ];
        assert_eq!(args(Scenario::B), baseline);
        assert_eq!(args(Scenario::A)[..baseline.len()], baseline);
        assert_eq!(args(Scenario::A)[baseline.len()..], ["--bare"]);
    }
}
