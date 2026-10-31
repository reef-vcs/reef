//! Running an agent: in its own process group, stdout and stderr to files,
//! stopped at a timeout; and running the short helper commands a benchmark
//! needs around it.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How a trial ended, before judging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Outcome {
    /// Setup failed before the agent started (a clone, `reef init`, a
    /// fixture): not the agent's failure, reported apart.
    SetupFailed {
        /// What failed.
        error: String,
    },
    /// The agent exited by itself.
    Exited {
        /// Its exit status; `None` if it died by a signal.
        code: Option<i32>,
    },
    /// The agent ran past the timeout and was stopped.
    TimedOut,
}

/// What to run: `command` with `env` (and nothing else) in `cwd`.
pub struct AgentCommand<'a> {
    /// The command, its program and arguments set.
    pub command: Command,
    /// The working directory.
    pub cwd: &'a Path,
    /// The whole environment.
    pub env: &'a [(String, OsString)],
}

/// Runs the agent in its own process group, stdout to `stream`, stderr to
/// `stderr`, and stops the group at `timeout`: `TERM`, then `KILL` ten
/// seconds later.
pub fn run_agent(
    agent: AgentCommand<'_>,
    timeout: Duration,
    stream: &Path,
    stderr: &Path,
) -> Result<Outcome, String> {
    let AgentCommand {
        mut command,
        cwd,
        env,
    } = agent;
    let stdout = File::create(stream).map_err(|e| format!("{}: {e}", stream.display()))?;
    let stderr = File::create(stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
    let mut child = command
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("starting the agent: {e}"))?;
    let group = child.id();
    let deadline = Instant::now() + timeout;
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

/// Runs `command`, returning its trimmed stdout, or an error naming the
/// command with its stderr.
pub fn output(command: &mut Command) -> Result<String, String> {
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

/// The commit in `reef 0.0.0 (0123456789ab, aarch64-apple-darwin)`.
pub fn reef_commit(version: &str) -> Option<String> {
    let inner = version.split_once('(')?.1;
    let commit = inner.split([',', ')']).next()?.trim();
    (!commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit())).then(|| commit.to_owned())
}

/// Today in UTC, `YYYY-MM-DD`, and now as RFC 3339.
pub fn today_and_now() -> (String, String) {
    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let stamp = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    (date, stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
