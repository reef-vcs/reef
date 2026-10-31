//! The live part: a real agent per trial, through a harness adapter, in the
//! trial agent's worktree or workspace, asked one question; graded against
//! the truth, and counted from what it actually read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::count::Counter;
use super::fixture::Fixture;
use super::grade::Grade;
use super::scenario::{Question, Scenario};
use super::{Arm, SCHEMA};
use crate::agent::env::short_tempdir;
use crate::agent::harness::{Harness, Invocation, McpServer, Tools};
use crate::agent::process::{AgentCommand, Outcome, reef_commit, run_agent, today_and_now};
use crate::agent::scrub::scrub;
use crate::agent::shell::invocations;
use crate::agent::transcript::{Transcript, Usage};

/// What a live run needs, fixed for all its trials.
pub struct LiveConfig<'a> {
    /// The harness.
    pub harness: &'a dyn Harness,
    /// The model to ask for.
    pub model: String,
    /// The most a trial may spend.
    pub budget_usd: f64,
    /// How long a trial may run.
    pub timeout: Duration,
    /// Where results go.
    pub out_dir: PathBuf,
    /// `reef --version`.
    pub reef_version: String,
    /// The harness's `--version`, the fallback when a stream has none.
    pub harness_version: Option<String>,
    /// Keep each trial's directory.
    pub keep: bool,
}

/// What a trial ran against, so rows compare like with like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pins {
    /// The harness.
    pub harness: String,
    /// Its version.
    pub harness_version: Option<String>,
    /// The model asked for.
    pub model: String,
    /// `reef --version`.
    pub reef_version: String,
    /// The commit in it.
    pub reef_commit: Option<String>,
    /// `linux` or `macos`.
    pub os: String,
    /// [`Scenario::version`].
    pub fixture_version: String,
    /// The prompt, verbatim.
    pub prompt: String,
}

/// The tokens an answer cost: every tool call's text and the text of
/// every result the agent read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnswerCost {
    /// Tool calls.
    pub calls: u32,
    /// UTF-8 bytes.
    pub bytes: u64,
    /// By the harness vendor's counter, if it ran.
    pub tokens: Option<u64>,
    /// The counter `tokens` is from.
    pub counter: String,
}

/// One trial, as written to `results/<date>-live-<question>-<arm>-<harness>-<trial>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialResult {
    /// The file format's version.
    pub schema: u32,
    /// The question.
    pub question: String,
    /// The arm.
    pub arm: Arm,
    /// 1-based.
    pub trial: u32,
    /// UTC, `YYYY-MM-DD`.
    pub date: String,
    /// UTC, RFC 3339.
    pub started_at: String,
    /// What the trial ran against.
    pub pins: Pins,
    /// How it ended.
    pub outcome: Outcome,
    /// The final event's subtype, such as `success`.
    pub result_subtype: Option<String>,
    /// Whether the run hit its budget. Such a trial is a failure.
    pub budget_exceeded: bool,
    /// The grade; `None` if the agent never ran.
    pub grade: Option<Grade>,
    /// The agent exited within budget and answered exactly right.
    pub correct: bool,
    /// What answering cost in context.
    pub answer: Option<AnswerCost>,
    /// The first model request's input tokens: the session's cost before
    /// it did anything.
    pub session_overhead: Option<u64>,
    /// What the harness billed.
    pub billed: Option<Usage>,
    /// Spend in US dollars.
    pub cost_usd: Option<f64>,
    /// Model round trips.
    pub turns: Option<u64>,
    /// From starting the agent to its exit.
    pub wall_ms: u64,
    /// Tool calls that left the arm's tool: `git` run in the `reef` arm,
    /// any shell call in the `mcp` arm.
    pub escapes: Vec<String>,
    /// Whether the fixture changed during the trial; such a trial is kept
    /// out of the medians and the fixture is rebuilt.
    pub mutated: bool,
    /// The saved stream, next to this file.
    pub stream_file: Option<String>,
}

impl TrialResult {
    /// Whether the trial counts: the agent ran and left the fixture as it
    /// was.
    pub fn counts(&self) -> bool {
        !matches!(self.outcome, Outcome::SetupFailed { .. }) && !self.mutated
    }
}

/// The text a call put in context: its input and its result.
pub fn call_text(input: &str, result: Option<&str>) -> String {
    format!("{input}\n{}", result.unwrap_or_default())
}

/// The calls that left the arm's tool.
pub fn escapes(harness: &dyn Harness, arm: Arm, transcript: &Transcript) -> Vec<String> {
    transcript
        .tool_calls
        .iter()
        .filter(|call| harness.is_shell(&call.name))
        .filter(|call| match arm {
            Arm::Git => false,
            Arm::Reef => {
                !invocations(call.command.as_deref().unwrap_or_default(), "git").is_empty()
            }
            Arm::Mcp => true,
        })
        .map(|call| call.input.clone())
        .collect()
}

/// Why the agent never got going, if it exited non-zero without a tool
/// call or any spend: no credential, an API error, the harness failing to
/// start. Not the agent's failure.
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

/// Runs one trial of `question` in `arm` and writes its result, stream, and
/// stderr. `vendor_counter` counts answer tokens if given.
pub fn run_trial(
    config: &LiveConfig<'_>,
    scenario: &Scenario,
    fixture: &Fixture,
    question: &Question,
    arm: Arm,
    trial: u32,
    vendor_counter: Option<&mut dyn Counter>,
) -> Result<TrialResult, String> {
    let harness = config.harness;
    let (date, started_at) = today_and_now();
    let stem = format!(
        "{date}-live-{}-{arm}-{}-{trial}",
        question.id,
        harness.name()
    );
    let prompt = scenario.prompt(question, arm);
    let mut result = TrialResult {
        schema: SCHEMA,
        question: question.id.clone(),
        arm,
        trial,
        date,
        started_at,
        pins: Pins {
            harness: harness.name().to_owned(),
            harness_version: config.harness_version.clone(),
            model: config.model.clone(),
            reef_version: config.reef_version.clone(),
            reef_commit: reef_commit(&config.reef_version),
            os: std::env::consts::OS.to_owned(),
            fixture_version: Scenario::version(),
            prompt: prompt.clone(),
        },
        outcome: Outcome::SetupFailed {
            error: String::new(),
        },
        result_subtype: None,
        budget_exceeded: false,
        grade: None,
        correct: false,
        answer: None,
        session_overhead: None,
        billed: None,
        cost_usd: None,
        turns: None,
        wall_ms: 0,
        escapes: Vec::new(),
        mutated: false,
        stream_file: None,
    };
    fs::create_dir_all(&config.out_dir)
        .map_err(|e| format!("{}: {e}", config.out_dir.display()))?;

    let before = fixture.fingerprint();
    let dir = short_tempdir(&fixture.root.join("t"), "t")?;
    let ran = match before {
        Err(error) => {
            result.outcome = Outcome::SetupFailed {
                error: format!("the fixture before the trial: {error}"),
            };
            Ok(())
        }
        Ok(before) => {
            let ran = run(
                config,
                fixture,
                question,
                arm,
                &prompt,
                &dir,
                &stem,
                &mut result,
                vendor_counter,
            );
            match fixture.fingerprint() {
                Ok(after) => result.mutated = after != before,
                Err(_) => result.mutated = true,
            }
            ran
        }
    };
    if !config.keep {
        let _ = fs::remove_dir_all(&dir);
    }
    ran?;
    let json = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    fs::write(
        config.out_dir.join(format!("{stem}.json")),
        scrub(&json) + "\n",
    )
    .map_err(|e| format!("saving the result: {e}"))?;
    Ok(result)
}

#[allow(clippy::too_many_arguments, reason = "one trial's whole context")]
#[expect(clippy::too_many_lines, reason = "one trial, start to saved stream")]
fn run(
    config: &LiveConfig<'_>,
    fixture: &Fixture,
    question: &Question,
    arm: Arm,
    prompt: &str,
    dir: &Path,
    stem: &str,
    result: &mut TrialResult,
    vendor_counter: Option<&mut dyn Counter>,
) -> Result<(), String> {
    let harness = config.harness;
    let home = dir.join("h");
    fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let env = fixture.env(arm, &home)?;
    let mcp_config = dir.join("mcp.json");
    let tools = match arm {
        Arm::Git | Arm::Reef => Tools::ShellAndRead,
        Arm::Mcp => {
            let server = McpServer {
                name: "reef".to_owned(),
                command: fixture.bin.join("reef"),
                args: vec!["mcp".to_owned()],
                env: env
                    .iter()
                    .filter(|(name, _)| ["REEF_STATE_DIR", "HOME", "PATH"].contains(&name.as_str()))
                    .cloned()
                    .collect(),
            };
            fs::write(&mcp_config, harness.mcp_config(&server)).map_err(|e| e.to_string())?;
            Tools::Mcp {
                config: &mcp_config,
                server: "reef",
            }
        }
    };
    // The agent alone gets the harness's credentials; the MCP server above,
    // the daemon and the fixture never see them.
    let mut env = env;
    env.extend(
        harness
            .credentials()
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), std::env::var_os(name)?))),
    );
    let args = harness.args(&Invocation {
        prompt,
        model: &config.model,
        budget_usd: config.budget_usd,
        isolated: true,
        tools,
    });
    let mut command = Command::new(harness.program());
    command.args(args);
    let stream_path = dir.join("stream.jsonl");
    let stderr_path = dir.join("stderr.log");
    let started = Instant::now();
    let outcome = run_agent(
        AgentCommand {
            command,
            cwd: fixture.cwd(arm),
            env: &env,
        },
        config.timeout,
        &stream_path,
        &stderr_path,
    );
    result.wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result.outcome = outcome.unwrap_or_else(|error| Outcome::SetupFailed { error });

    let stream = fs::read_to_string(&stream_path).unwrap_or_default();
    let transcript = harness.parse(&stream);
    if let Some(version) = &transcript.harness_version {
        result.pins.harness_version = Some(version.clone());
    }
    result.result_subtype = transcript.result.as_ref().map(|r| r.subtype.clone());
    result.budget_exceeded = result
        .result_subtype
        .as_deref()
        .is_some_and(|s| s.contains("budget"));
    if let Some(error) = never_reached_the_model(&result.outcome, &transcript) {
        result.outcome = Outcome::SetupFailed { error };
    }
    if !matches!(result.outcome, Outcome::SetupFailed { .. }) {
        let grade = question
            .kind
            .grade(transcript.final_text(), &question.truth);
        result.correct = matches!(result.outcome, Outcome::Exited { .. })
            && !result.budget_exceeded
            && grade.correct;
        let mut grade = grade;
        grade.answer = scrub(&grade.answer);
        result.grade = Some(grade);

        let text: String = transcript
            .tool_calls
            .iter()
            .map(|call| call_text(&call.input, call.result.as_deref()))
            .collect::<Vec<_>>()
            .join("\n");
        let (tokens, counter) = match vendor_counter {
            Some(counter) => (counter.count(&text).ok(), counter.name().to_owned()),
            None => (None, String::new()),
        };
        result.answer = Some(AnswerCost {
            calls: u32::try_from(transcript.tool_calls.len()).unwrap_or(u32::MAX),
            bytes: text.len() as u64,
            tokens,
            counter,
        });
        result.session_overhead = transcript.first_usage.map(|u| u.total_input());
        if let Some(final_event) = &transcript.result {
            result.billed = Some(final_event.usage);
            result.cost_usd = final_event.total_cost_usd;
            result.turns = final_event.num_turns;
        }
        result.escapes = escapes(harness, arm, &transcript);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::claude::Claude;

    #[test]
    fn escapes_are_per_arm() {
        let harness = Claude {
            program: PathBuf::from("claude"),
        };
        let transcript = harness.parse(include_str!(
            "../../tokens/fixtures/claude-2.1.314-bash-and-mcp.jsonl"
        ));
        // The recorded stream runs `git status` in a shell and one MCP call.
        assert_eq!(
            escapes(&harness, Arm::Git, &transcript),
            Vec::<String>::new()
        );
        assert_eq!(
            escapes(&harness, Arm::Reef, &transcript),
            ["git status --porcelain=v1"]
        );
        assert_eq!(
            escapes(&harness, Arm::Mcp, &transcript),
            ["git status --porcelain=v1"]
        );
    }
}
