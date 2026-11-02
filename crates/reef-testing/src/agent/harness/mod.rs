//! One adapter per agent harness. An adapter turns a harness-neutral
//! [`Invocation`] into the harness's arguments, writes its MCP
//! configuration, and parses its event stream into a [`Transcript`].
//! Nothing outside this module names a harness or branches on one; a new
//! harness is a new adapter and a recorded stream to test it against.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::transcript::Transcript;

pub mod claude;

/// What the agent may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tools<'a> {
    /// A shell, a file reader, and a file editor, with edits accepted.
    ShellAndEdit,
    /// [`Tools::ShellAndEdit`], and the tools of an MCP server the harness
    /// already has (from a plugin), by the harness's name for the server.
    ShellEditAndServer {
        /// The server's name, as the harness prefixes its tools.
        server: &'a str,
    },
    /// A shell and a file reader, nothing else.
    ShellAndRead,
    /// Only the tools of one MCP server, configured by
    /// [`Harness::mcp_config`] in `config`.
    Mcp {
        /// The configuration file.
        config: &'a Path,
        /// The server's name in it.
        server: &'a str,
    },
}

/// One run of an agent, as every harness understands it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Invocation<'a> {
    /// The prompt.
    pub prompt: &'a str,
    /// The model to ask for.
    pub model: &'a str,
    /// The most the run may spend, in US dollars, where the harness can cap
    /// it.
    pub budget_usd: f64,
    /// Start without user or project configuration: no instruction files,
    /// skills, hooks, or memory.
    pub isolated: bool,
    /// The tools.
    pub tools: Tools<'a>,
}

/// An MCP server over stdio.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    /// Its name.
    pub name: String,
    /// The program.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables set for it.
    pub env: Vec<(String, OsString)>,
}

/// An agent harness.
pub trait Harness {
    /// The harness's name, for row labels and file names.
    fn name(&self) -> &'static str;
    /// Its executable.
    fn program(&self) -> &Path;
    /// The arguments for `invocation`.
    fn args(&self, invocation: &Invocation<'_>) -> Vec<String>;
    /// An MCP configuration file's contents naming `server`, for
    /// [`Tools::Mcp`].
    fn mcp_config(&self, server: &McpServer) -> String;
    /// Parses the run's stdout.
    fn parse(&self, stream: &str) -> Transcript;
    /// Whether a tool call is a shell command.
    fn is_shell(&self, call_name: &str) -> bool;
    /// The variables holding the harness's credentials: passed to its
    /// process and nothing else.
    fn credentials(&self) -> &'static [&'static str];
    /// The counter of the vendor whose models the harness runs
    /// (`anthropic`, `o200k`), which its live rows report answer tokens in.
    fn vendor_counter(&self) -> &'static str;
}
