//! Entry point for the `reef` CLI; the command line is [`cli`], and the
//! agent files `reef init` writes are [`agents`].

mod agents;
mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
