//! Claude Code's `--output-format stream-json --verbose` events, and the
//! metrics the benchmark reads from them.
//!
//! One JSON object per line. The parser keeps only what the metrics need:
//! the `system`/`init` event (model and Claude Code version), every
//! `tool_use` block in an `assistant` message, the `tool_result` blocks in
//! `user` messages that answer them, and the final `result` event. Any
//! other event, and any field it does not name, is ignored, so a newer
//! Claude Code that adds events still parses. Token counts come from the
//! `result` event alone: Claude Code repeats a message's `usage` on every
//! content block it streams, so summing `assistant` events would count
//! them several times.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool call: the `tool_use` block and, once it arrives, its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The block's id, which its `tool_result` names.
    pub id: String,
    /// The tool, such as `Bash` or `Read`.
    pub name: String,
    /// For `Bash`, the command line.
    pub command: Option<String>,
    /// `is_error` from the result: for `Bash`, a non-zero exit. `None`
    /// when no result arrived (the run ended first).
    pub is_error: Option<bool>,
}

/// The `result` event's fields the benchmark reports.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ResultEvent {
    /// `success`, or why the run stopped: `error_max_budget_usd`,
    /// `error_max_turns`, `error_during_execution`.
    #[serde(default)]
    pub subtype: String,
    /// Whether Claude Code counts the run as failed.
    #[serde(default)]
    pub is_error: bool,
    /// The final answer, on success.
    #[serde(default)]
    pub result: Option<String>,
    /// Model round trips.
    #[serde(default)]
    pub num_turns: Option<u64>,
    /// Spend for the whole run, in US dollars.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    /// Token usage for the whole run.
    #[serde(default)]
    pub usage: Usage,
    /// Tool calls the permission policy refused.
    #[serde(default)]
    pub permission_denials: Vec<Value>,
}

/// Token counts from the `result` event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens neither written to nor read from the prompt cache.
    #[serde(default)]
    pub input_tokens: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Input tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Output tokens, thinking included.
    #[serde(default)]
    pub output_tokens: u64,
}

impl Usage {
    /// Every input token, cached or not: what the model read.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

/// A parsed stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    /// `model` from the `init` event.
    pub model: Option<String>,
    /// `claude_code_version` from the `init` event.
    pub claude_code_version: Option<String>,
    /// `slash_commands` from the `init` event: built-in commands and every
    /// skill that loaded. `None` if the event or the field is missing.
    pub slash_commands: Option<Vec<String>>,
    /// Tool calls in the order the model made them, subagents' included.
    pub tool_calls: Vec<ToolCall>,
    /// The text of the last `text` block an assistant message carried: the
    /// final answer when the run ends without a `result` event.
    pub last_text: Option<String>,
    /// The final `result` event, if the run got that far.
    pub result: Option<ResultEvent>,
    /// Lines that were not JSON objects (a crash's output, a truncated
    /// last line).
    pub unparsed_lines: usize,
}

impl Transcript {
    /// Parses a whole stream. Never fails: what it cannot read it counts in
    /// [`Transcript::unparsed_lines`].
    pub fn parse(stream: &str) -> Self {
        let mut transcript = Transcript::default();
        for line in stream.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Value>(line) {
                Ok(event @ Value::Object(_)) => transcript.add(&event),
                _ => transcript.unparsed_lines += 1,
            }
        }
        transcript
    }

    fn add(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                self.model = event["model"].as_str().map(str::to_owned);
                self.claude_code_version = event["claude_code_version"].as_str().map(str::to_owned);
                self.slash_commands = event["slash_commands"].as_array().map(|commands| {
                    commands
                        .iter()
                        .filter_map(|command| command.as_str().map(str::to_owned))
                        .collect()
                });
            }
            Some("assistant") => {
                for block in content(event) {
                    match block["type"].as_str() {
                        Some("tool_use") => self.tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name: block["name"].as_str().unwrap_or_default().to_owned(),
                            command: block["input"]["command"].as_str().map(str::to_owned),
                            is_error: None,
                        }),
                        Some("text") => {
                            if let Some(text) = block["text"].as_str() {
                                self.last_text = Some(text.to_owned());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for block in content(event).filter(|block| block["type"] == "tool_result") {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    if let Some(call) = self.tool_calls.iter_mut().rev().find(|call| call.id == id)
                    {
                        call.is_error = Some(block["is_error"].as_bool().unwrap_or(false));
                    }
                }
            }
            Some("result") => {
                self.result = serde_json::from_value(event.clone()).ok();
            }
            _ => {}
        }
    }

    /// The agent's final answer: the `result` event's text, else the last
    /// text it wrote.
    pub fn final_text(&self) -> &str {
        self.result
            .as_ref()
            .and_then(|result| result.result.as_deref())
            .or(self.last_text.as_deref())
            .unwrap_or_default()
    }

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
            let invocations = reef_invocations(command);
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

fn content(event: &Value) -> impl Iterator<Item = &Value> {
    event["message"]["content"].as_array().into_iter().flatten()
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Shell words of `command`, split into simple commands at `;`, `&`, `|`,
/// parentheses, backquotes, `$`, and newlines, with quotes stripped from
/// each word's ends. Not a shell parser: enough to see `reef ws run` in
/// `cd x && reef ws run -- sh -c '...'` or `"$(reef here)"`, which is all
/// the metrics ask of it.
fn simple_commands(command: &str) -> Vec<Vec<&str>> {
    command
        .split([';', '&', '|', '(', ')', '`', '$', '\n'])
        .map(|segment| {
            segment
                .split_whitespace()
                .map(|word| word.trim_matches(['\'', '"', '\\']))
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|words| !words.is_empty())
        .collect()
}

/// For each simple command that runs `reef` (by name or path), the words
/// after it.
fn reef_invocations(command: &str) -> Vec<Vec<&str>> {
    simple_commands(command)
        .iter()
        .filter_map(|words| match program(words) {
            [first, rest @ ..] if program_is(first, "reef") => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

/// A simple command's words from the program it runs: past variable
/// assignments, wrappers such as `sudo` and `timeout 60`, and `sh -c`.
fn program<'s, 'a>(mut words: &'s [&'a str]) -> &'s [&'a str] {
    loop {
        words = match words {
            [word, rest @ ..] if word.contains('=') && !word.starts_with('-') => rest,
            [
                "sudo" | "env" | "time" | "exec" | "command" | "nohup",
                rest @ ..,
            ]
            | ["timeout", _, rest @ ..] => rest,
            ["sh" | "bash" | "zsh", flag, rest @ ..]
                if flag.starts_with('-') && flag.ends_with('c') =>
            {
                rest
            }
            _ => return words,
        };
    }
}

fn program_is(word: &str, name: &str) -> bool {
    word.rsplit('/').next() == Some(name)
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

/// The words that are not options or option values, up to a `--`.
fn positional<'a>(words: &[&'a str], value_options: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            break;
        }
        if word.starts_with('-') {
            if !word.contains('=') && value_options.contains(word) {
                words.next();
            }
            continue;
        }
        out.push(*word);
    }
    out
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
    use super::*;

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
            let invocations = reef_invocations(command);
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
            let invocations = reef_invocations(command);
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
        let transcript = Transcript::parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.309-failed-bash.jsonl"
        ));
        assert_eq!(transcript.unparsed_lines, 0);
        assert_eq!(transcript.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(transcript.claude_code_version.as_deref(), Some("2.1.309"));
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
        let transcript = Transcript::parse(include_str!(
            "../../onboarding/fixtures/claude-2.1.310-scenario-b.jsonl"
        ));
        assert_eq!(transcript.unparsed_lines, 0);
        assert_eq!(transcript.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(transcript.claude_code_version.as_deref(), Some("2.1.310"));
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
        let transcript = Transcript::parse("not json\n[1]\n{\"type\":\"other\"}\n");
        assert_eq!(transcript.unparsed_lines, 2);
        assert_eq!(transcript.tool_calls, []);
        assert_eq!(transcript.final_text(), "");
    }
}
