//! What the agent benchmarks share: the onboarding benchmark (RX.07) and
//! the token-efficiency benchmark (R2.17).
//!
//! A trial runs a real agent harness headless in a cleared environment
//! ([`mod@env`]), in its own process group with a timeout ([`process`]), and
//! reads what it did from the harness's event stream ([`transcript`]).
//! Everything that differs between harnesses (arguments, MCP
//! configuration, the stream's format) lives in one adapter per harness
//! under [`harness`]; nothing else branches on the harness. Saved streams
//! and logs go through [`scrub`]; results become Markdown tables through
//! [`table`].

pub mod env;
pub mod harness;
pub mod process;
pub mod scrub;
pub mod shell;
pub mod table;
pub mod transcript;
