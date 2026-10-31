//! What an agent did, read from its harness's event stream: the harness's
//! own format is parsed by its adapter ([`super::harness`]); the result is
//! this module's [`Transcript`], which the benchmarks read.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool call: the call and, once it arrives, its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The call's id, which its result names.
    pub id: String,
    /// The tool, as the harness names it: `Bash`, `Read`, or an MCP
    /// server's tool.
    pub name: String,
    /// For a shell tool, the command line.
    pub command: Option<String>,
    /// The text the call put in the agent's context: a shell tool's command
    /// line, else the arguments as compact JSON.
    pub input: String,
    /// The result's text as the agent read it (after any truncation the
    /// harness applied). `None` when no result arrived (the run ended
    /// first).
    pub result: Option<String>,
    /// Whether the result was an error: for a shell, a non-zero exit.
    /// `None` when no result arrived.
    pub is_error: Option<bool>,
}

/// The final event's fields the benchmarks report.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ResultEvent {
    /// `success`, or why the run stopped, such as `error_max_budget_usd`,
    /// `error_max_turns` or `error_during_execution`.
    #[serde(default)]
    pub subtype: String,
    /// Whether the harness counts the run as failed.
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

/// Token counts the harness reports.
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
    /// The model, as the harness reports it.
    pub model: Option<String>,
    /// The harness's version, as its stream reports it.
    pub harness_version: Option<String>,
    /// The commands and skills the harness offered: built-ins and every
    /// skill that loaded. `None` if the stream does not say.
    pub slash_commands: Option<Vec<String>>,
    /// The tools the harness offered the model. `None` if the stream does
    /// not say.
    pub tools: Option<Vec<String>>,
    /// Tool calls in the order the model made them, subagents' included.
    pub tool_calls: Vec<ToolCall>,
    /// The text of the last text block the agent wrote: the final answer
    /// when the run ends without a final event.
    pub last_text: Option<String>,
    /// The usage of the run's first model request, before any tool call:
    /// what a session costs before it does anything.
    pub first_usage: Option<Usage>,
    /// The final event, if the run got that far.
    pub result: Option<ResultEvent>,
    /// Lines that were not JSON objects (a crash's output, a truncated
    /// last line).
    pub unparsed_lines: usize,
}

impl Transcript {
    /// Parses a whole stream of one JSON object per line with `add`. Never
    /// fails: what it cannot read it counts in
    /// [`Transcript::unparsed_lines`].
    pub fn parse_lines(stream: &str, mut add: impl FnMut(&mut Transcript, &Value)) -> Self {
        let mut transcript = Transcript::default();
        for line in stream.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Value>(line) {
                Ok(event @ Value::Object(_)) => add(&mut transcript, &event),
                _ => transcript.unparsed_lines += 1,
            }
        }
        transcript
    }

    /// Attaches a result to the call `id` names.
    pub fn answer(&mut self, id: &str, text: String, is_error: bool) {
        if let Some(call) = self.tool_calls.iter_mut().rev().find(|call| call.id == id) {
            call.result = Some(text);
            call.is_error = Some(is_error);
        }
    }

    /// The agent's final answer: the final event's text, else the last
    /// text it wrote.
    pub fn final_text(&self) -> &str {
        self.result
            .as_ref()
            .and_then(|result| result.result.as_deref())
            .or(self.last_text.as_deref())
            .unwrap_or_default()
    }
}
