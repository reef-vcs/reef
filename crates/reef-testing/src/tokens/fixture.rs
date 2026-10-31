//! The fixture: one history built twice from commits in a source checkout
//! (Reef's own), with work in progress in git worktrees in one copy and in
//! Reef workspaces in the other.
//!
//! The history is an "Initial import" of the base commit's tree, with every
//! agent and harness configuration file removed, then one commit per
//! landed session holding its source commit's diff, authored by its agent.
//! It is built once and cloned into both copies, so their commits are
//! identical. Commits are dated at build time: the history index keeps
//! imported changes within its retention window, so dates in the past
//! would change the truth. Ids therefore differ between builds, and the
//! truth never names one.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Arm;
use super::scenario::{SELF_PATCH, Scenario, Session};
use crate::agent::env::{
    base_env, copy_reef, short_tempdir, trial_path, without_credentials, write_gitconfig,
};
use crate::agent::process::output;

/// Where the fixture comes from and goes.
#[derive(Debug, Clone)]
pub struct FixtureConfig {
    /// A git repository holding the scenario's commits (a full clone of
    /// Reef; a shallow one fails setup).
    pub source: PathBuf,
    /// The directory holding the `reef` and `reef-daemon` builds to test.
    pub reef_bin_dir: PathBuf,
    /// Where the fixture's directory goes (a short path: the daemon's
    /// socket lives in it).
    pub work_root: PathBuf,
    /// How long to wait for the daemon's indexes.
    pub settle_timeout: Duration,
}

/// One open session's Reef workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Its id.
    pub id: String,
    /// Its directory.
    pub path: PathBuf,
    /// Its change, as jj prints it.
    pub change: String,
}

/// A built fixture.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// Everything lives under here.
    pub root: PathBuf,
    /// The home the fixture's commands (and its daemon) run with.
    pub home: PathBuf,
    /// The daemon's state directory.
    pub state: PathBuf,
    /// The copies of `reef` and `reef-daemon` the daemon and the agents run.
    pub bin: PathBuf,
    /// The build under test's directory, kept off agents' `PATH`.
    pub reef_bin_dir: PathBuf,
    /// The git copy's main checkout.
    pub git_root: PathBuf,
    /// Each open session's git worktree, by agent.
    pub worktrees: BTreeMap<String, PathBuf>,
    /// The Reef copy's root checkout.
    pub reef_root: PathBuf,
    /// The Reef copy's repository id.
    pub repo_id: String,
    /// Each open session's workspace, by agent.
    pub workspaces: BTreeMap<String, Workspace>,
    /// The agent trials run as.
    pub trial_agent: String,
}

/// The `:(exclude)` pathspecs for the strip list.
fn excludes(strip: &[String]) -> Vec<String> {
    strip
        .iter()
        .map(|path| format!(":(exclude){path}"))
        .collect()
}

/// Runs `command` with `input` on stdin; its trimmed stdout, or an error.
fn with_stdin(command: &mut Command, input: &[u8]) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(input)
        .map_err(|e| format!("{program}: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!(
            "{program} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Raw stdout of a command, which must succeed.
fn raw(command: &mut Command) -> Result<Vec<u8>, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "{program} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The commits the scenario pins that `source` lacks.
pub fn missing_commits(source: &Path, scenario: &Scenario) -> Vec<String> {
    let fixture = &scenario.fixture;
    std::iter::once(&fixture.base)
        .chain(
            fixture
                .landed
                .iter()
                .chain(&fixture.open)
                .filter_map(|s| s.commit.as_ref()),
        )
        .filter(|commit| {
            Command::new("git")
                .arg("-C")
                .arg(source)
                .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
                .stderr(Stdio::null())
                .status()
                .map_or(true, |status| !status.success())
        })
        .cloned()
        .collect()
}

impl Fixture {
    /// Builds both copies and waits for the daemon's indexes to settle.
    pub fn build(config: &FixtureConfig, scenario: &Scenario) -> Result<Fixture, String> {
        let missing = missing_commits(&config.source, scenario);
        if !missing.is_empty() {
            return Err(format!(
                "{} lacks the scenario's commits {} (a shallow clone, or a rewritten `main`)",
                config.source.display(),
                missing.join(", ")
            ));
        }
        let root = short_tempdir(&config.work_root, "f")?;
        let mut fixture = Fixture {
            home: root.join("h"),
            state: root.join("s"),
            bin: root.join("bin"),
            reef_bin_dir: config.reef_bin_dir.clone(),
            git_root: root.join("git"),
            worktrees: BTreeMap::new(),
            reef_root: root.join("reef"),
            repo_id: String::new(),
            workspaces: BTreeMap::new(),
            trial_agent: scenario.fixture.trial_agent.clone(),
            root,
        };
        let built = fixture.build_inside(config, scenario);
        if let Err(error) = built {
            fixture.teardown(false);
            return Err(error);
        }
        Ok(fixture)
    }

    #[expect(clippy::too_many_lines, reason = "the build, step by step, in order")]
    fn build_inside(&mut self, config: &FixtureConfig, scenario: &Scenario) -> Result<(), String> {
        write_gitconfig(&self.home, "fixture", "fixture@fixture.invalid")?;
        copy_reef(&config.reef_bin_dir, &self.bin)?;
        let spec = &scenario.fixture;
        let excludes = excludes(&spec.strip);

        // The history, once.
        let base = self.root.join("base");
        std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        self.git(&base, &["init", "--quiet", "--initial-branch", "main"])?;
        let tree = raw(Command::new("git").arg("-C").arg(&config.source).args([
            "archive",
            "--format=tar",
            &spec.base,
        ]))?;
        with_stdin(Command::new("tar").arg("-x").arg("-C").arg(&base), &tree)?;
        for path in &spec.strip {
            let path = base.join(path);
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::remove_file(&path);
        }
        self.git(&base, &["add", "--all"])?;
        self.commit_as(&base, "import", "import@fixture.invalid", "Initial import")?;
        for session in &spec.landed {
            let commit = session
                .commit
                .as_deref()
                .ok_or("a landed session needs a commit")?;
            let patch = source_diff(&config.source, commit, &excludes)?;
            apply(self.git_command(&base).args(["apply", "--index"]), &patch)?;
            let subject = output(Command::new("git").arg("-C").arg(&config.source).args([
                "log",
                "-1",
                "--format=%s",
                commit,
            ]))?;
            let email = format!("{}@agents.invalid", session.agent);
            self.commit_as(&base, &session.agent, &email, &subject)?;
        }

        // Two identical copies.
        for copy in [&self.git_root, &self.reef_root] {
            self.git(
                &self.root,
                &[
                    "clone",
                    "--quiet",
                    &base.to_string_lossy(),
                    &copy.to_string_lossy(),
                ],
            )?;
            self.git(copy, &["remote", "remove", "origin"])?;
        }
        std::fs::remove_dir_all(&base).map_err(|e| e.to_string())?;

        // Work in progress: git worktrees.
        let patches: Vec<(String, Vec<u8>)> = spec
            .open
            .iter()
            .map(|session| {
                Ok((
                    session.agent.clone(),
                    session_patch(config, session, &excludes)?,
                ))
            })
            .collect::<Result<_, String>>()?;
        for (agent, patch) in &patches {
            let worktree = self.root.join("wt").join(agent);
            self.git(
                &self.git_root,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    &format!("agent/{agent}"),
                    &worktree.to_string_lossy(),
                    "main",
                ],
            )?;
            apply(self.git_command(&worktree).arg("apply"), patch)?;
            self.worktrees.insert(agent.clone(), worktree);
        }

        // And Reef workspaces.
        let registered: Value = json(&self.reef(&self.reef_root, &["repo", "register"])?)?;
        registered["id"]
            .as_str()
            .ok_or("`repo register` gave no id")?
            .clone_into(&mut self.repo_id);
        for (agent, patch) in &patches {
            let created: Value = json(&self.reef(
                &self.reef_root,
                &[
                    "--agent",
                    agent,
                    "ws",
                    "create",
                    "--repo",
                    &self.repo_id,
                    "--owner",
                    agent,
                    "--new-on",
                    "main",
                    "--purpose",
                    &format!("{agent}'s work"),
                    "--lease",
                    "24h",
                ],
            )?)?;
            let field = |name: &str| {
                created[name]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`ws create` gave no {name}"))
            };
            let workspace = Workspace {
                id: field("id")?,
                path: PathBuf::from(field("path")?),
                change: field("change_id_reverse_hex")?,
            };
            apply(self.git_command(&workspace.path).arg("apply"), patch)?;
            self.reef(
                &self.reef_root,
                &["--agent", agent, "ws", "snapshot", &workspace.id],
            )?;
            self.workspaces.insert(agent.clone(), workspace);
        }
        self.settle(config.settle_timeout)
    }

    /// Waits until the history index has caught up and every open
    /// session's Rust files have symbols.
    fn settle(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        loop {
            match self.settled() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => last = error,
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the daemon's indexes did not settle in {} s{}",
                    timeout.as_secs(),
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!(": {last}")
                    }
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn settled(&self) -> Result<bool, String> {
        let history = json(&self.reef(
            &self.reef_root,
            &["history", "--repo", &self.repo_id, "--limit", "1"],
        )?)?;
        let coverage = &history["coverage"];
        if coverage["backfilling"] != Value::Bool(false)
            || coverage["pending"].as_u64() != Some(0)
            || coverage["behind"].as_u64() != Some(0)
        {
            return Ok(false);
        }
        for workspace in self.workspaces.values() {
            let diff = json(&self.reef(
                &self.reef_root,
                &[
                    "diff",
                    "--workspace",
                    &workspace.id,
                    "--no-snapshot",
                    "--symbols",
                    "--call-sites",
                    "0",
                ],
            )?)?;
            let pending = diff["files"].as_array().into_iter().flatten().any(|file| {
                file["path"]
                    .as_str()
                    .is_some_and(|p| Path::new(p).extension().is_some_and(|e| e == "rs"))
                    && file["symbols"]["coverage"] != "covered"
            });
            if pending {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// A string that changes if any copy's work, history or workspaces
    /// changed: each worktree's porcelain status and `main`, and each
    /// workspace's state and commit with the Reef copy's `main`.
    pub fn fingerprint(&self) -> Result<String, String> {
        let mut out = String::new();
        out.push_str(&self.git(&self.git_root, &["rev-parse", "main"])?);
        out.push_str(&self.git(&self.git_root, &["worktree", "list", "--porcelain"])?);
        for (agent, worktree) in &self.worktrees {
            out.push('\n');
            out.push_str(agent);
            out.push_str(":\n");
            out.push_str(&self.git(worktree, &["status", "--porcelain=v1", "-uall"])?);
        }
        out.push_str(&self.git(&self.reef_root, &["rev-parse", "main"])?);
        let listed = json(&self.reef(
            &self.reef_root,
            &[
                "ws",
                "list",
                "--repo",
                &self.repo_id,
                "--all",
                "--limit",
                "0",
            ],
        )?)?;
        let mut workspaces: Vec<String> = listed
            .as_array()
            .into_iter()
            .flatten()
            .map(|ws| format!("{} {} {}", ws["id"], ws["state"], ws["commit_id"]))
            .collect();
        workspaces.sort();
        out.push_str(&workspaces.join("\n"));
        Ok(out)
    }

    /// The open sessions other than the trial agent's, in agent order:
    /// what `{wt:*}` and the like repeat over.
    pub fn others(&self) -> Vec<&str> {
        self.worktrees
            .keys()
            .map(String::as_str)
            .filter(|agent| *agent != self.trial_agent)
            .collect()
    }

    /// A placeholder's value: `wt:<agent>`, `ws:<agent>` or
    /// `change:<agent>`.
    pub fn value(&self, kind: &str, agent: &str) -> Option<String> {
        match kind {
            "wt" => self
                .worktrees
                .get(agent)
                .map(|p| p.to_string_lossy().into_owned()),
            "ws" => self.workspaces.get(agent).map(|w| w.id.clone()),
            "change" => self.workspaces.get(agent).map(|w| w.change.clone()),
            _ => None,
        }
    }

    /// Where an arm's agent works: the trial agent's worktree or workspace.
    pub fn cwd(&self, arm: Arm) -> &Path {
        match arm {
            Arm::Git => &self.worktrees[&self.trial_agent],
            Arm::Reef | Arm::Mcp => &self.workspaces[&self.trial_agent].path,
        }
    }

    /// The environment an arm's commands run with, given a home: no `reef`
    /// and no Reef variables for git; `reef` first on `PATH` and the
    /// fixture's state directory for the others.
    pub fn env(&self, arm: Arm, home: &Path) -> Result<Vec<(String, OsString)>, String> {
        let mut env = base_env(home, &self.state);
        without_credentials(&mut env);
        let with_reef = arm != Arm::Git;
        if !with_reef {
            env.retain(|(name, _)| name != "REEF_STATE_DIR");
        }
        let shadow = self.root.join("path").join(arm.name());
        let _ = std::fs::remove_dir_all(&shadow);
        env.push((
            "PATH".to_owned(),
            trial_path(with_reef, &self.bin, &self.reef_bin_dir, &shadow)?,
        ));
        Ok(env)
    }

    /// The fixture's own environment, for its setup commands.
    fn own_env(&self) -> Vec<(String, OsString)> {
        let mut env = base_env(&self.home, &self.state);
        without_credentials(&mut env);
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(
                [
                    "/usr/bin",
                    "/bin",
                    "/usr/sbin",
                    "/sbin",
                    "/usr/local/bin",
                    "/opt/homebrew/bin",
                ]
                .map(PathBuf::from),
            ),
        )
        .unwrap_or_default();
        env.push(("PATH".to_owned(), path));
        env
    }

    fn git_command(&self, cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command.current_dir(cwd).env_clear().envs(self.own_env());
        command
    }

    /// `git args` in `cwd`, as the fixture.
    pub fn git(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        output(self.git_command(cwd).args(args))
    }

    /// `reef -o json args` in `cwd`, as the fixture, against its daemon.
    pub fn reef(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        output(
            Command::new(self.bin.join("reef"))
                .current_dir(cwd)
                .env_clear()
                .envs(self.own_env())
                .args(["-o", "json"])
                .args(args),
        )
    }

    fn commit_as(&self, cwd: &Path, name: &str, email: &str, subject: &str) -> Result<(), String> {
        output(
            self.git_command(cwd)
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email)
                .args(["commit", "--quiet", "--allow-empty", "--message", subject]),
        )
        .map(drop)
    }

    /// Unless `keep` (which leaves everything, the daemon included, for
    /// reading): releases the workspaces (an overlay mount would keep the
    /// directory busy on Linux), stops the daemon, and deletes everything.
    #[allow(clippy::print_stderr, reason = "a warning from a dev tool")]
    pub fn teardown(&self, keep: bool) {
        if keep {
            return;
        }
        for workspace in self.workspaces.values() {
            let _ = self.reef(&self.reef_root, &["ws", "release", &workspace.id]);
        }
        let _ = self.reef(&self.reef_root, &["--no-autostart", "daemon", "stop"]);
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            eprintln!("tokens: could not delete {}: {error}", self.root.display());
        }
    }
}

/// `commit`'s diff against its parent in `source`, without the stripped
/// paths.
fn source_diff(source: &Path, commit: &str, excludes: &[String]) -> Result<Vec<u8>, String> {
    raw(Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["diff", "--binary", &format!("{commit}^"), commit, "--", "."])
        .args(excludes))
}

fn session_patch(
    config: &FixtureConfig,
    session: &Session,
    excludes: &[String],
) -> Result<Vec<u8>, String> {
    match (&session.commit, &session.patch) {
        (Some(commit), _) => source_diff(&config.source, commit, excludes),
        (None, Some(_)) => Ok(SELF_PATCH.as_bytes().to_vec()),
        (None, None) => Err(format!("{} has no work", session.agent)),
    }
}

fn apply(command: &mut Command, patch: &[u8]) -> Result<(), String> {
    if patch.is_empty() {
        return Ok(());
    }
    with_stdin(command, patch).map(drop)
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| {
        format!(
            "not JSON ({e}): {}",
            text.chars().take(200).collect::<String>()
        )
    })
}
