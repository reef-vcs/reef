//! Entry point for the Reef daemon; the body is [`reef_daemon::run_main`],
//! or with a subcommand [`reef_daemon::offline::run`].

use std::process::ExitCode;

use clap::Parser as _;
use reef_daemon::DaemonArgs;

fn main() -> ExitCode {
    let mut args = DaemonArgs::parse();
    if let Some(command) = args.command.take() {
        return reef_daemon::offline::run(&args, command);
    }
    match reef_daemon::run_main(args) {
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
