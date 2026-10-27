//! Entry point for the `reef` CLI; the command line is [`cli`], the agent
//! files `reef init` writes are [`agents`], and `reef mcp` is [`mcp`].

mod agents;
mod cli;
mod mcp;

fn main() -> std::process::ExitCode {
    cli::main()
}
