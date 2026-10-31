//! The metrics the onboarding benchmark reads from a trial's
//! [`Transcript`]: calls to the first working `reef ws run`, failed `reef`
//! calls, and escapes to git.

use serde::{Deserialize, Serialize};

use crate::agent::shell::{invocations, positional, program, program_is, simple_commands};
pub use crate::agent::transcript::{ResultEvent, ToolCall, Transcript, Usage};

impl Transcript {
    /// Counts what the brief measures. `wall_ms` is the harness's own
    /// clock.
    pub fn metrics(&self, wall_ms: u64) -> Metrics {
        let mut metrics = Metrics {
            tool_calls: count(self.tool_calls.len()),
            wall_ms,
            ..Metrics::default()
        };
        for (index, call) in self.tool_calls.iter().enumerate() {
            let Some(command) = call.command.as_deref().filter(|_| call.name == "Bash") else {
                continue;
            };
            let invocations = invocations(command, "reef");
            if !invocations.is_empty() {
                metrics.reef_calls += 1;
                if call.is_error == Some(true) {
                    metrics.failed_reef_calls += 1;
                }
                let ran = invocations.iter().any(|words| is_ws_run(words));
                if ran && call.is_error == Some(false) && metrics.calls_to_success.is_none() {
                    metrics.calls_to_success = Some(count(index + 1));
                }
            }
            if git_escape(command).is_some() {
                metrics.git_escapes.push(command.to_owned());
            }
        }
        if let Some(result) = &self.result {
            metrics.num_turns = result.num_turns;
            metrics.total_cost_usd = result.total_cost_usd;
            metrics.input_tokens = result.usage.total_input();
            metrics.output_tokens = result.usage.output_tokens;
            metrics.permission_denials = count(result.permission_denials.len());
        }
        metrics
    }
}

/// What one trial measured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Tool calls up to and including the first `Bash` call that ran
    /// `reef ws run` and exited 0. `None` if none did.
    pub calls_to_success: Option<u32>,
    /// Every tool call, any tool.
    pub tool_calls: u32,
    /// `Bash` calls that ran `reef`.
    pub reef_calls: u32,
    /// Of those, the ones that exited non-zero.
    pub failed_reef_calls: u32,
    /// `Bash` commands that ran `git worktree`, `git stash`, or
    /// `git checkout`: the agent went around Reef.
    pub git_escapes: Vec<String>,
    /// Model round trips, from the `result` event.
    pub num_turns: Option<u64>,
    /// Input tokens, cached and uncached.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Spend in US dollars, from the `result` event.
    pub total_cost_usd: Option<f64>,
    /// Tool calls the permission policy refused.
    pub permission_denials: u32,
    /// From starting the agent to its exit.
    pub wall_ms: u64,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `reef`'s global options that take a value, so `--state-dir x ws run`
/// still reads as `ws run`.
const REEF_VALUE_OPTIONS: [&str; 6] = [
    "--state-dir",
    "--socket",
    "--config",
    "--timeout",
    "--output",
    "-o",
];

/// Whether the words after `reef` start with `ws run` (or `workspace run`).
fn is_ws_run(words: &[&str]) -> bool {
    let subcommands = positional(words, &REEF_VALUE_OPTIONS);
    matches!(subcommands.as_slice(), ["ws" | "workspace", "run", ..])
}

/// The git subcommand if `command` runs `git worktree`, `git stash`, or
/// `git checkout`.
fn git_escape(command: &str) -> Option<&'static str> {
    simple_commands(command).iter().find_map(|words| {
        let [first, rest @ ..] = program(words) else {
            return None;
        };
        if !program_is(first, "git") {
            return None;
        }
        let subcommands = positional(rest, &["-C", "-c", "--git-dir", "--work-tree"]);
        match subcommands.first() {
            Some(&"worktree") => Some("worktree"),
            Some(&"stash") => Some("stash"),
            Some(&"checkout") => Some("checkout"),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::harness::Harness as _;
    use crate::agent::harness::claude::Claude;

    fn parse(stream: &str) -> Transcript {
        Claude {
            program: PathBuf::from("claude"),
        }
        .parse(stream)
    }

    #[test]
    fn sees_ws_run_in_compound_commands() {
        for command in [
            "reef ws run -- sh -c 'echo hi >> README.md'",
            "cd /x && reef ws run -- true",
            "~/.local/bin/reef --state-dir /s ws run --purpose y -- true",
            "REEF_OUTPUT=json reef workspace run -- true",
            "out=$(reef ws run -- true); echo $out",
            "bash -lc 'reef ws run -- true'",
            "timeout 120 reef ws run -- true",
        ] {
            let invocations = invocations(command, "reef");
            assert!(
                invocations.iter().any(|words| is_ws_run(words)),
                "{command}"
            );
        }
        for command in [
            "reef ws list",
            "reef guide",
            "echo reef ws run",
            "reef ws create -- run",
            "cat reef/ws/run",
        ] {
            let invocations = invocations(command, "reef");
            assert!(
                !invocations.iter().any(|words| is_ws_run(words)),
                "{command}"
            );
        }
    }

    #[test]
    fn sees_git_escapes() {
        assert_eq!(git_escape("git worktree add ../x"), Some("worktree"));
        assert_eq!(git_escape("cd a && git -C b stash"), Some("stash"));
        assert_eq!(git_escape("git checkout -b feature"), Some("checkout"));
        assert_eq!(git_escape("git status && git log"), None);
        assert_eq!(git_escape("reef ws run -- git status"), None);
    }

    /// A real Claude Code 2.1.309 stream: one Bash call that fails, one
    /// that succeeds, local paths replaced.
    #[test]
    fn parses_a_recorded_stream() {
        let transcript = parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.309-failed-bash.jsonl"
        ));
        assert_eq!(transcript.unparsed_lines, 0);
        assert_eq!(transcript.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(transcript.harness_version.as_deref(), Some("2.1.309"));
        let commands = transcript.slash_commands.as_deref().unwrap();
        assert!(commands.iter().any(|c| c == "dataviz"), "{commands:?}");
        let calls: Vec<_> = transcript
            .tool_calls
            .iter()
            .map(|call| (call.name.as_str(), call.command.as_deref(), call.is_error))
            .collect();
        assert_eq!(
            calls,
            [
                ("Bash", Some("ls nonexistent_dir"), Some(true)),
                ("Bash", Some("cat README.md"), Some(false)),
            ]
        );
        assert_eq!(transcript.final_text(), "DONE");
        let metrics = transcript.metrics(1_000);
        assert_eq!(metrics.tool_calls, 2);
        assert_eq!(metrics.reef_calls, 0);
        assert_eq!(metrics.calls_to_success, None);
        assert_eq!(metrics.num_turns, Some(3));
        // From the result event: 6 + 14,250 cache writes + 58,038 cache reads.
        assert_eq!(metrics.input_tokens, 72_294);
        assert_eq!(metrics.output_tokens, 394);
        assert_eq!(metrics.total_cost_usd, Some(0.003_627_98));
        assert_eq!(metrics.permission_denials, 0);
    }

    /// A real scenario B trial on Claude Code 2.1.310 (the first live
    /// baseline): the agent loads the `reef` skill, then one `reef ws run`
    /// does the work. Local paths and ids replaced.
    #[test]
    fn parses_a_recorded_scenario_b_stream() {
        let transcript = parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.310-scenario-b.jsonl"
        ));
        assert_eq!(transcript.unparsed_lines, 0);
        assert_eq!(transcript.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(transcript.harness_version.as_deref(), Some("2.1.310"));
        let commands = transcript.slash_commands.as_deref().unwrap();
        assert!(commands.iter().any(|c| c == "reef"), "{commands:?}");
        let calls: Vec<_> = transcript
            .tool_calls
            .iter()
            .map(|call| (call.name.as_str(), call.is_error))
            .collect();
        assert_eq!(calls, [("Skill", Some(false)), ("Bash", Some(false))]);
        assert!(
            transcript.tool_calls[1]
                .command
                .as_deref()
                .unwrap()
                .starts_with("reef ws run -- sh -c ")
        );
        assert!(transcript.final_text().ends_with("\n7cd07ef68318a348"));
        let metrics = transcript.metrics(1_000);
        assert_eq!(metrics.tool_calls, 2);
        assert_eq!(metrics.reef_calls, 1);
        assert_eq!(metrics.failed_reef_calls, 0);
        assert_eq!(metrics.calls_to_success, Some(2));
        assert_eq!(metrics.git_escapes, Vec::<String>::new());
        assert_eq!(metrics.num_turns, Some(4));
        // From the result event: 6 + 6,933 cache writes + 47,046 cache reads.
        assert_eq!(metrics.input_tokens, 53_985);
        assert_eq!(metrics.output_tokens, 252);
        assert_eq!(metrics.total_cost_usd, Some(0.024_569_1));
        assert_eq!(metrics.permission_denials, 0);
    }

    #[test]
    fn bad_lines_are_counted_not_fatal() {
        let transcript = parse("not json\n[1]\n{\"type\":\"other\"}\n");
        assert_eq!(transcript.unparsed_lines, 2);
        assert_eq!(transcript.tool_calls, []);
        assert_eq!(transcript.final_text(), "");
    }
}
