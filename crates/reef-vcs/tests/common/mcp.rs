//! A small MCP client for `reef mcp` (R2.13): JSON-RPC lines over the
//! child's stdin and stdout, written by hand so the tests see the wire as
//! a harness does, every stdout line included.

use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// The protocol version the client offers.
pub(crate) const PROTOCOL: &str = "2025-11-25";

/// How long a response may take before the test fails.
const ANSWER_BOUND: Duration = Duration::from_secs(60);

/// A running `reef mcp`, initialized.
pub(crate) struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: mpsc::UnboundedReceiver<Value>,
    /// Every stdout line, as received.
    pub(crate) lines: Arc<Mutex<Vec<String>>>,
    /// Stderr so far.
    pub(crate) stderr: Arc<Mutex<String>>,
    /// Notifications received while waiting for responses.
    pub(crate) notifications: Vec<Value>,
    /// Responses that came before their turn: the server answers
    /// concurrent calls in any order.
    early: HashMap<u64, Value>,
    /// The `initialize` result.
    pub(crate) init: Value,
    next_id: u64,
}

impl Mcp {
    /// Spawns `command` (which ends in `mcp`) and initializes it.
    pub(crate) async fn start(command: Command) -> Mcp {
        let mut mcp = Mcp::spawn(command);
        let init = mcp
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": {"name": "reef-tests", "version": "0"}
                }),
            )
            .await;
        mcp.init = init["result"].clone();
        assert!(mcp.init.is_object(), "initialize failed: {init}");
        mcp.notify("notifications/initialized", json!({})).await;
        mcp
    }

    /// Spawns `command` without initializing.
    pub(crate) fn spawn(mut command: Command) -> Mcp {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        let (sender, messages) = mpsc::unbounded_channel();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                seen.lock().unwrap().push(line.clone());
                let value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| Value::String(format!("NOT JSON: {line}")));
                let _ = sender.send(value);
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut buffer = [0; 4096];
            while let Ok(n) = stderr_pipe.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });
        Mcp {
            stdin: child.stdin.take(),
            child,
            messages,
            lines,
            stderr,
            notifications: Vec::new(),
            early: HashMap::new(),
            init: Value::Null,
            next_id: 1,
        }
    }

    async fn write(&mut self, message: &Value) {
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        let stdin = self.stdin.as_mut().expect("stdin is closed");
        stdin.write_all(&line).await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// Sends a request and returns its id, without waiting.
    pub(crate) async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// A notification.
    pub(crate) async fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The response to request `id`; notifications meanwhile are kept.
    pub(crate) async fn response(&mut self, id: u64) -> Value {
        if let Some(response) = self.early.remove(&id) {
            return response;
        }
        let deadline = Instant::now() + ANSWER_BOUND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match tokio::time::timeout(left, self.messages.recv()).await {
                Ok(Some(message)) => message,
                Ok(None) => panic!("reef mcp closed stdout waiting for {id}: {}", self.stderr()),
                Err(elapsed) => panic!("no response to {id} ({elapsed}): {}", self.stderr()),
            };
            assert!(message.is_object(), "{message}");
            match message.get("id").and_then(Value::as_u64) {
                Some(answered) if answered == id => return message,
                Some(answered) => {
                    self.early.insert(answered, message);
                }
                None => self.notifications.push(message),
            }
        }
    }

    /// A request and its response.
    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    /// `tools/call`: whether it is an error result, and its structured
    /// content, after checking that the text block holds the same JSON.
    pub(crate) async fn tool(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let response = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        result_of(&response)
    }

    /// A tool that must succeed.
    pub(crate) async fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let (is_error, value) = self.tool(name, arguments.clone()).await;
        assert!(!is_error, "{name} {arguments}: {value}");
        value
    }

    /// A tool that must fail: its error object.
    pub(crate) async fn err(&mut self, name: &str, arguments: Value) -> Value {
        let (is_error, value) = self.tool(name, arguments.clone()).await;
        assert!(is_error, "{name} {arguments} succeeded: {value}");
        value["error"].clone()
    }

    pub(crate) fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Closes stdin and waits for the exit: its status and how long it
    /// took.
    pub(crate) async fn close(mut self) -> (ExitStatus, Duration) {
        drop(self.stdin.take());
        let started = Instant::now();
        let status = tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("reef mcp did not exit after stdin closed")
            .unwrap();
        (status, started.elapsed())
    }

    /// Sends SIGTERM and waits for the exit, as [`Mcp::close`].
    pub(crate) async fn terminate(mut self) -> (ExitStatus, Duration) {
        let pid = i32::try_from(self.child.id().unwrap()).unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let started = Instant::now();
        let status = tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("reef mcp did not exit on SIGTERM")
            .unwrap();
        (status, started.elapsed())
    }
}

/// A `tools/call` response's error flag and structured content.
pub(crate) fn result_of(response: &Value) -> (bool, Value) {
    let result = &response["result"];
    assert!(result.is_object(), "not a tool result: {response}");
    let structured = result["structuredContent"].clone();
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let copy: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {response}"));
    assert_eq!(copy, structured, "the text block is the structured content");
    (result["isError"] == json!(true), structured)
}
