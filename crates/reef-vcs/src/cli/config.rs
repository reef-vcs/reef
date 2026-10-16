//! `reef config show`: the configuration the daemon would run with, resolved
//! the same way ([`Config::load`]) without contacting it.

use std::io::{self, Write as _};

use anyhow::Context as _;
use reef_daemon::config::{Config, PathOverrides};

/// `reef config`.
#[derive(Debug, clap::Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ConfigCommand {
    /// Print the effective configuration, with the layer each value came
    /// from (default, file, env, or flag).
    Show {
        /// `text` prints TOML with a `# source:` comment per key; `json` one
        /// object with a `sources` map.
        #[arg(short = 'o', long, value_enum, default_value_t = Output::Text)]
        output: Output,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Output {
    /// TOML, loadable as `config.toml`.
    #[value(alias = "toml")]
    Text,
    /// JSON.
    Json,
}

impl ConfigArgs {
    pub(crate) fn run(self, overrides: &PathOverrides) -> anyhow::Result<()> {
        match self.command {
            ConfigCommand::Show { output } => show(overrides, output),
        }
    }
}

fn show(overrides: &PathOverrides, output: Output) -> anyhow::Result<()> {
    let config = Config::load(overrides).context("load the daemon's configuration")?;
    let text = match output {
        Output::Text => config.to_effective_toml(),
        Output::Json => {
            let mut value = serde_json::to_value(config.effective_file())
                .context("serialize the configuration")?;
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "sources".to_owned(),
                    serde_json::to_value(&config.sources).context("serialize the sources")?,
                );
            }
            format!("{value:#}\n")
        }
    };
    match io::stdout().lock().write_all(text.as_bytes()) {
        // `reef config show | head` is not a failure.
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            Err(error).context("write to stdout")
        }
        _ => Ok(()),
    }
}
