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
#[derive(Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.GuideJson")]
pub(crate) struct GuideJson {
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
        Output::Json | Output::Brief => out.json_only(&GuideJson {
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

    /// Every daemon error with a `next` (ADR 0025), through the status and
    /// the client as the CLI receives it: each `next` is `reef` and a real
    /// command's words. The daemon cannot see the CLI's command tree, so
    /// the check lives here.
    #[test]
    fn every_daemon_next_is_a_real_command() {
        use reef_client::ClientError;
        use reef_daemon::db::DbError;
        use reef_daemon::error::status;
        use reef_daemon::lease::LeaseError;
        use reef_daemon::model::{RepoId, Timestamp, WorkspaceId, WorkspaceState};
        use reef_daemon::store::StoreError;
        use reef_daemon::workspace::ManagerError;

        let ws: WorkspaceId = "bbbb000000000001".parse().unwrap();
        let repo: RepoId = "aaaa000000000001".parse().unwrap();
        let not_found = |what| DbError::NotFound {
            what,
            id: "abcd".into(),
        };
        let statuses = [
            status(ManagerError::Store(StoreError::Missing {
                name: "deps".into(),
                hash: "ff".into(),
                repo,
                commit: "c0ffee".into(),
            })),
            status(ManagerError::NotRegistered(std::env::temp_dir())),
            status(ManagerError::NotRegistered("/nonexistent/repo".into())),
            status(ManagerError::NotActive {
                id: ws,
                repo,
                state: WorkspaceState::Reclaimed,
            }),
            status(ManagerError::FinalSnapshotFailed {
                id: ws,
                source: Box::new(ManagerError::NotLoaded(ws)),
            }),
            status(LeaseError::Reclaimed {
                workspace: ws,
                repo,
                state: WorkspaceState::Released,
                at: None,
            }),
            status(LeaseError::Expired {
                workspace: ws,
                repo,
                lease: "cccc000000000001".parse().unwrap(),
                grace_ended_at: Timestamp(0),
            }),
            status(not_found("workspace")),
            status(not_found("repository")),
        ];
        let commands = leaf_commands();
        for status in statuses {
            let message = status.message().to_owned();
            let advice = ClientError::from(status)
                .advice()
                .unwrap_or_else(|| panic!("no advice: {message}"));
            let (reef, rest) = advice.next.split_first().unwrap();
            assert_eq!(reef, "reef", "{message}");
            assert!(
                commands
                    .iter()
                    .any(|command| rest.len() >= command.len()
                        && rest[..command.len()] == command[..]),
                "{message}: {:?} is not a command",
                advice.next
            );
        }
    }
}
