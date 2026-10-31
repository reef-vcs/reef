//! `reef config show`: the configuration the daemon would run with,
//! resolved the same way without contacting it. The daemon's binary
//! resolves it (`reef-daemon config show`, R2.05); this prints what it
//! wrote.
//!
//! Unlike other commands, its output defaults to text (TOML) even when
//! stdout is not a terminal, because the text form is itself machine
//! readable and loads back as a `config.toml` (`reef config show >
//! config.toml`). `-o json` selects JSON.

use super::{CliError, Global, Out, Output, helper};

/// `reef config`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ConfigCommand {
    /// Print the effective configuration and where each value came from.
    ///
    /// TOML with a `# source: default | file | env | flag` comment per key,
    /// or with `-o json` one object with a `sources` map.
    Show,
}

impl Args {
    pub(crate) fn run(self, global: &Global, out: &Out) -> Result<(), CliError> {
        match self.command {
            ConfigCommand::Show => show(global, out),
        }
    }
}

fn show(global: &Global, out: &Out) -> Result<(), CliError> {
    let mut args = vec!["config".into(), "show".into()];
    if out.is_json() {
        args.push("--json".into());
    }
    let text = helper::run(global, &args)?;
    if out.mode() == Output::Brief {
        // The helper prints its JSON pretty; brief is compact.
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&text) {
            return out.json_only(&value);
        }
    }
    out.raw(&text)
}
