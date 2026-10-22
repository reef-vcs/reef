//! `reef guide`, and `reef` with no arguments (R1.26, ADR 0025): the agent
//! guide, `docs/agent-guide.md`, embedded so the published document and the
//! binary cannot drift. The text before the `<!-- reef:long -->` line is
//! the short form (`--short`, and what R1.27 writes into `AGENTS.md`).
//!
//! The guide prints as text even on a pipe, since it is prose for a model
//! to read; `-o json` (or `REEF_OUTPUT=json`) asks for the JSON object.

use clap::CommandFactory as _;
use serde::Serialize;

use super::{Cli, CliError, Out, Output};

/// The guide as written.
pub(crate) const GUIDE: &str = include_str!("../../../../docs/agent-guide.md");

/// The line that ends the short form.
const LONG_MARKER: &str = "<!-- reef:long -->";

/// `reef guide`.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct Args {
    /// Only the short form: what a workspace is and the two commands.
    #[arg(long)]
    short: bool,
}

/// The short form, up to the marker line.
pub(crate) fn short() -> &'static str {
    GUIDE
        .split_once(LONG_MARKER)
        .map_or(GUIDE, |(short, _)| short)
}

/// The whole guide without the marker line.
pub(crate) fn long() -> String {
    GUIDE.replacen(&format!("{LONG_MARKER}\n"), "", 1)
}

/// The JSON form: the text, and every leaf command as its words, so a
/// harness can generate a tool per command.
#[derive(Serialize)]
struct GuideJson {
    guide: String,
    commands: Vec<Vec<String>>,
}

/// Every runnable command as its words, from clap's tree: `["ws", "run"]`,
/// `["here"]`. clap's own `help` is not one.
pub(crate) fn leaf_commands() -> Vec<Vec<String>> {
    fn walk(command: &clap::Command, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let children: Vec<_> = command
            .get_subcommands()
            .filter(|child| child.get_name() != "help")
            .collect();
        if children.is_empty() {
            out.push(path.clone());
        }
        for child in children {
            path.push(child.get_name().to_owned());
            walk(child, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(&Cli::command(), &mut Vec::new(), &mut out);
    out
}

/// Prints the guide: text as written, or the JSON object.
pub(crate) fn print(out: &Out, args: &Args) -> Result<(), CliError> {
    let text = if args.short {
        short().to_owned()
    } else {
        long()
    };
    match out.mode() {
        Output::Text => out.raw(text.as_bytes()),
        Output::Json => out.json_only(&GuideJson {
            guide: text,
            commands: leaf_commands(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_fits_its_budget() {
        // About 400 tokens at four bytes a token (ADR 0025).
        assert!(GUIDE.len() <= 1600, "{} bytes", GUIDE.len());
        assert!(long().len() <= 1600);
        assert!(GUIDE.contains(LONG_MARKER));
        assert!(short().lines().count() <= 40, "{}", short());
        for line in GUIDE.lines() {
            assert!(line.chars().count() <= 100, "too wide: {line}");
            assert!(!line.trim_start().starts_with('|'), "a table row: {line}");
            assert!(!line.starts_with("###"), "a heading below ##: {line}");
        }
    }

    #[test]
    fn the_short_form_names_the_two_commands() {
        assert!(short().contains("reef ws run -- <command>"));
        assert!(short().contains("reef here"));
        assert!(!long().contains(LONG_MARKER));
    }

    #[test]
    fn leaf_commands_are_runnable_paths() {
        let commands = leaf_commands();
        for expected in [
            &["ws", "run"][..],
            &["ws", "create"],
            &["here"],
            &["guide"],
            &["completions"],
        ] {
            assert!(
                commands.iter().any(|command| command == expected),
                "{expected:?} in {commands:?}"
            );
        }
        assert!(
            !commands
                .iter()
                .any(|command| command.contains(&"help".to_owned()))
        );
        assert!(!commands.iter().any(|command| command == &["ws"]));
    }
}
