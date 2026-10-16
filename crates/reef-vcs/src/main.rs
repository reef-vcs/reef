//! Entry point for the `reef` CLI. R1.22 builds the full command set on this
//! skeleton; later tickets add `reef mcp`.

mod cli;

use std::process::ExitCode;

use clap::Parser as _;
use reef_daemon::logging::{self, LogOptions};
use reef_lib::error::{Classify as _, ErrorChain};

#[expect(clippy::print_stderr, reason = "the binary reports its own failures")]
fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let _logging = match LogOptions::from_env().and_then(|opts| logging::init(&opts)) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("error: initialize logging: {}", ErrorChain(&error));
            return ExitCode::from(error.kind().exit_code());
        }
    };
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "starting");
    match cli.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(reef_daemon::exit_code(&error))
        }
    }
}
