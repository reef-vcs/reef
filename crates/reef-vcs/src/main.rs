//! Entry point for the `reef` CLI; the command line is [`cli`].

mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
