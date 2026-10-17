//! Entry point for the Reef daemon; the body is [`reef_daemon::run_main`].

use std::process::ExitCode;

use clap::Parser as _;
use reef_daemon::DaemonArgs;

fn main() -> ExitCode {
    match reef_daemon::run_main(DaemonArgs::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            #[expect(clippy::print_stderr, reason = "the binary reports its own failure")]
            {
                eprintln!("error: {error:#}");
            }
            ExitCode::from(reef_daemon::exit_code(&error))
        }
    }
}
