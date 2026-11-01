//! Entry point for the `reef` CLI; the command line is [`cli`], the agent
//! files `reef init` writes are [`agents`], `reef mcp` is [`mcp`], and
//! `reef schema`'s document is [`schema`].

mod agents;
mod cli;
mod mcp;
mod schema;

fn main() -> std::process::ExitCode {
    cli::main()
}
