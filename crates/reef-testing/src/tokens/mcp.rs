//! A minimal MCP client over a server's stdio: newline-delimited JSON-RPC,
//! with `initialize`, `notifications/initialized`, `tools/list` and
//! `tools/call`. Enough to measure what a session loads and what each call
//! returns, as a harness would put them in context.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// A running server.
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// The server's `instructions` from `initialize`.
    pub instructions: String,
}

/// One `tools/call` result.
#[derive(Debug, Clone, PartialEq)]
pub struct CallResult {
    /// The `content` text blocks joined by newlines: what clients put in
    /// context.
    pub text: String,
    /// `structuredContent` as compact JSON, if the server sent it.
    pub structured: Option<String>,
    /// `isError`.
    pub is_error: bool,
}

impl McpClient {
    /// Starts `program args` in `cwd` with exactly `env`, and initializes
    /// the session.
    pub fn start(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        env: &[(String, OsString)],
    ) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("starting {}: {e}", program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut client = McpClient {
            child,
            stdin,
            stdout,
            next_id: 0,
            instructions: String::new(),
        };
        let init = client.request(
            "initialize",
            &json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "reef-tokens", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        init["instructions"]
            .as_str()
            .unwrap_or_default()
            .clone_into(&mut client.instructions);
        client.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(client)
    }

    /// `tools/list`'s whole result, and its tools.
    pub fn list_tools(&mut self) -> Result<(Value, Vec<Value>), String> {
        let result = self.request("tools/list", &json!({}))?;
        let tools = result["tools"].as_array().cloned().unwrap_or_default();
        Ok((result, tools))
    }

    /// Calls `tool` with `arguments`.
    pub fn call(&mut self, tool: &str, arguments: &Value) -> Result<CallResult, String> {
        let result = self.request("tools/call", &json!({"name": tool, "arguments": arguments}))?;
        let text = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CallResult {
            text,
            structured: result
                .get("structuredContent")
                .filter(|v| !v.is_null())
                .map(Value::to_string),
            is_error: result["isError"].as_bool().unwrap_or(false),
        })
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{message}").map_err(|e| format!("writing to the server: {e}"))?;
        self.stdin.flush().map_err(|e| e.to_string())
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| format!("reading from the server: {e}"))?;
            if read == 0 {
                return Err(format!("the server closed its stdout during `{method}`"));
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] != json!(id) || message.get("method").is_some() {
                // A notification, or a request from the server: not ours.
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("`{method}` failed: {error}"));
            }
            return Ok(message["result"].clone());
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
