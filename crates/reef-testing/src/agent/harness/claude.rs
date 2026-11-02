//! Claude Code: `claude -p` with `--output-format stream-json --verbose`.
//!
//! One JSON object per line. The parser keeps the `system`/`init` event
//! (model, version, skills, tools), every `tool_use` block in an
//! `assistant` message, the `tool_result` blocks in `user` messages that
//! answer them, and the final `result` event. Any other event, and any
//! field it does not name, is ignored, so a newer Claude Code that adds
//! events still parses. Token counts come from the `result` event alone:
//! Claude Code repeats a message's `usage` on every content block it
//! streams, so summing `assistant` events would count them several times.
//! The first `assistant` event's usage is kept on its own, as the run's
//! first request.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Harness, Invocation, McpServer, Tools};
use crate::agent::transcript::{ToolCall, Transcript, Usage};

/// Claude Code at `program`.
#[derive(Debug, Clone)]
pub struct Claude {
    /// `claude`, as an absolute path.
    pub program: PathBuf,
}

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn args(&self, invocation: &Invocation<'_>) -> Vec<String> {
        let budget = invocation.budget_usd.to_string();
        let mut args: Vec<String> = [
            "-p",
            invocation.prompt,
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            invocation.model,
        ]
        .map(str::to_owned)
        .to_vec();
        let tools: &[&str] = match invocation.tools {
            Tools::ShellAndEdit => &[
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash,Read,Edit",
            ],
            Tools::ShellEditAndServer { server } => {
                args.extend(
                    [
                        "--permission-mode",
                        "acceptEdits",
                        "--allowedTools",
                        &format!("Bash,Read,Edit,mcp__{server}"),
                    ]
                    .map(str::to_owned),
                );
                &[]
            }
            Tools::ShellAndRead => &["--tools", "Bash,Read", "--allowedTools", "Bash,Read"],
            Tools::Mcp { config, server } => {
                // Claude Code names an MCP server's tools `mcp__<server>__<tool>`
                // and allows them all by the prefix.
                args.extend(
                    [
                        "--tools",
                        "",
                        "--mcp-config",
                        &config.to_string_lossy(),
                        "--strict-mcp-config",
                        "--allowedTools",
                        &format!("mcp__{server}"),
                    ]
                    .map(str::to_owned),
                );
                &[]
            }
        };
        args.extend(tools.iter().map(|arg| (*arg).to_owned()));
        args.extend(
            [
                "--permission-prompts",
                "none",
                "--max-budget-usd",
                &budget,
                "--no-session-persistence",
            ]
            .map(str::to_owned),
        );
        if invocation.isolated {
            args.push("--bare".to_owned());
        }
        args
    }

    fn mcp_config(&self, server: &McpServer) -> String {
        let env: serde_json::Map<String, Value> = server
            .env
            .iter()
            .map(|(name, value)| (name.clone(), Value::from(value.to_string_lossy())))
            .collect();
        let config = json!({
            "mcpServers": {
                &server.name: {
                    "command": server.command.to_string_lossy(),
                    "args": server.args,
                    "env": env,
                }
            }
        });
        format!("{config:#}\n")
    }

    fn parse(&self, stream: &str) -> Transcript {
        Transcript::parse_lines(stream, add)
    }

    fn is_shell(&self, call_name: &str) -> bool {
        call_name == "Bash"
    }

    fn credentials(&self) -> &'static [&'static str] {
        &["ANTHROPIC_API_KEY"]
    }

    fn vendor_counter(&self) -> &'static str {
        "anthropic"
    }
}

fn add(transcript: &mut Transcript, event: &Value) {
    match event["type"].as_str() {
        Some("system") if event["subtype"] == "init" => {
            transcript.model = event["model"].as_str().map(str::to_owned);
            transcript.harness_version = event["claude_code_version"].as_str().map(str::to_owned);
            transcript.slash_commands = strings(&event["slash_commands"]);
            transcript.tools = strings(&event["tools"]);
        }
        Some("assistant") => {
            if transcript.first_usage.is_none() {
                transcript.first_usage =
                    serde_json::from_value::<Usage>(event["message"]["usage"].clone()).ok();
            }
            for block in content(event) {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let command = block["input"]["command"].as_str().map(str::to_owned);
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        let input = match (&command, name.as_str()) {
                            (Some(command), "Bash") => command.clone(),
                            _ => block["input"].to_string(),
                        };
                        transcript.tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name,
                            command,
                            input,
                            result: None,
                            is_error: None,
                        });
                    }
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            transcript.last_text = Some(text.to_owned());
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            for block in content(event).filter(|block| block["type"] == "tool_result") {
                let id = block["tool_use_id"].as_str().unwrap_or_default();
                let is_error = block["is_error"].as_bool().unwrap_or(false);
                transcript.answer(id, result_text(&block["content"]), is_error);
            }
        }
        Some("result") => {
            transcript.result = serde_json::from_value(event.clone()).ok();
        }
        _ => {}
    }
}

/// A `tool_result`'s content as text: a string, or the text blocks of an
/// array joined by newlines.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    })
}

fn content(event: &Value) -> impl Iterator<Item = &Value> {
    event["message"]["content"].as_array().into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Claude {
        Claude {
            program: PathBuf::from("claude"),
        }
    }

    /// Test 1: a real Claude Code 2.1.314 stream with a `Bash` call and an MCP
    /// call, local paths replaced: each call's input and result text, the
    /// first request's usage, and the result event's totals.
    #[test]
    fn claude_stream_texts() {
        let transcript = claude().parse(include_str!(
            "../../../tokens/fixtures/claude-2.1.314-bash-and-mcp.jsonl"
        ));
        assert_eq!(transcript.unparsed_lines, 0);
        assert_eq!(transcript.harness_version.as_deref(), Some("2.1.314"));
        let tools = transcript.tools.as_deref().unwrap();
        assert!(tools.iter().any(|t| t == "Bash"), "{tools:?}");
        assert!(
            tools.iter().any(|t| t == "mcp__reef__get_status"),
            "{tools:?}"
        );
        let [bash, mcp] = transcript.tool_calls.as_slice() else {
            panic!("{:?}", transcript.tool_calls);
        };
        assert_eq!(bash.name, "Bash");
        assert_eq!(bash.input, "git status --porcelain=v1");
        assert_eq!(bash.command.as_deref(), Some("git status --porcelain=v1"));
        assert_eq!(bash.is_error, Some(false));
        assert_eq!(bash.result.as_deref(), Some(" M README.md"));
        assert_eq!(mcp.name, "mcp__reef__get_status");
        assert_eq!(mcp.input, "{}");
        assert_eq!(mcp.command, None);
        assert_eq!(mcp.is_error, Some(false));
        assert!(mcp.result.as_deref().unwrap().contains("\"repository\""));
        let first = transcript.first_usage.unwrap();
        assert!(first.total_input() > 1_000, "{first:?}");
        let result = transcript.result.as_ref().unwrap();
        assert!(result.usage.total_input() > first.total_input());
        assert!(result.total_cost_usd.unwrap() > 0.0);
    }

    #[test]
    fn result_text_takes_strings_and_blocks() {
        assert_eq!(result_text(&json!("a")), "a");
        assert_eq!(
            result_text(
                &json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}])
            ),
            "a\nb"
        );
        assert_eq!(result_text(&Value::Null), "");
    }

    #[test]
    fn arguments_per_tool_set() {
        let invocation = Invocation {
            prompt: "p",
            model: "m",
            budget_usd: 1.0,
            isolated: true,
            tools: Tools::ShellAndRead,
        };
        let args = claude().args(&invocation);
        assert!(args.windows(2).any(|w| w == ["--tools", "Bash,Read"]));
        assert_eq!(args.last().map(String::as_str), Some("--bare"));

        let config = Path::new("/c/mcp.json");
        let args = claude().args(&Invocation {
            tools: Tools::Mcp {
                config,
                server: "reef",
            },
            isolated: false,
            ..invocation
        });
        assert!(args.windows(2).any(|w| w == ["--tools", ""]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--mcp-config", "/c/mcp.json"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--allowedTools", "mcp__reef"])
        );
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        assert!(!args.contains(&"--bare".to_owned()));
    }
}
