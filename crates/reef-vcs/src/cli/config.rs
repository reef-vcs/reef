//! `reef config show`: the configuration the daemon would run with, resolved
//! the same way ([`Config::load`]) without contacting it.
//!
//! Unlike other commands, its output defaults to text (TOML) even when
//! stdout is not a terminal, because the text form is itself machine
//! readable and loads back as a `config.toml` (`reef config show >
//! config.toml`). `-o json` selects JSON.

use reef_daemon::config::{Config, PathOverrides};

use super::{CliError, Out, Output};

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
    pub(crate) fn run(self, overrides: &PathOverrides, out: &Out) -> Result<(), CliError> {
        match self.command {
            ConfigCommand::Show => show(overrides, out),
        }
    }
}

fn show(overrides: &PathOverrides, out: &Out) -> Result<(), CliError> {
    let config = Config::load(overrides).map_err(CliError::Config)?;
    let text = match out.mode() {
        Output::Text => config.to_effective_toml(),
        Output::Json => {
            let mut value =
                serde_json::to_value(config.effective_file()).map_err(|e| json_error(&e))?;
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "sources".to_owned(),
                    serde_json::to_value(&config.sources).map_err(|e| json_error(&e))?,
                );
            }
            format!("{value:#}\n")
        }
    };
    out.raw(text.as_bytes())
}

fn json_error(error: &serde_json::Error) -> CliError {
    CliError::Refused {
        kind: reef_lib::error::ErrorKind::Internal,
        message: format!("serialize the configuration: {error}"),
        advice: None,
    }
}
