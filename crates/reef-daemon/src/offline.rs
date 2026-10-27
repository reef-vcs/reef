//! The `reef-daemon` subcommands that need the daemon's library but no
//! running daemon: `config show` and `restore` (R2.05). `reef config show`
//! and `reef restore` run them, so the `reef` binary never links this
//! crate (ADR 0033).
//!
//! Each writes exactly one document on stdout and exits: on success, its
//! result (`config show`'s TOML, or JSON with `--json`; `restore`'s
//! [`Restored`] as JSON) and exit 0; on failure, the [`ErrorDocument`]
//! `reef` prints for any error, whatever the format, and the kind's exit
//! code. Stderr is inherited; they log at `warn` unless `REEF_LOG` says
//! otherwise.

use std::error::Error;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use reef_client::error::kind_text;
use reef_client::json::{ErrorDocument, ErrorObject, Restored, RestoredRepo};
use reef_client::logging::EnvLog;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};

use crate::DaemonArgs;
use crate::config::{Config, ConfigError, PathOverrides};
use crate::error::{Advice, Advise};
use crate::maintenance::backup::{Manifest, ManifestError};
use crate::maintenance::restore::{RestoreError, RestoreOptions, restore};
use crate::model::RepoId;

/// The filter when neither `REEF_LOG` nor `RUST_LOG` is set: the library's
/// `info` events are for the daemon's log, not the terminal.
const DEFAULT_FILTER: &str = "warn";

/// A subcommand of the `reef-daemon` binary; none runs the daemon.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum Offline {
    /// The daemon's configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Restore the daemon's state from a backup (offline); `reef restore`.
    Restore(RestoreArgs),
}

/// `reef-daemon config`.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum ConfigCommand {
    /// Print the effective configuration and where each value came from:
    /// TOML with a `# source:` comment per key, or one JSON object with a
    /// `sources` map.
    Show {
        /// JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
}

/// `reef-daemon restore`, `reef restore`'s arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct RestoreArgs {
    /// The backup directory to restore from.
    pub dir: PathBuf,
    /// The state directory to restore into [default: the daemon's].
    #[arg(long, value_name = "DIR")]
    pub into: Option<PathBuf>,
    /// Only this repository's jj state (id or id prefix, from the backup).
    #[arg(long, value_name = "REPO")]
    pub repo: Option<String>,
    /// Restore the repository at this directory instead of its registered
    /// root.
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,
    /// Replace the daemon's current database, and repositories with work
    /// the backup lacks.
    #[arg(long)]
    pub force: bool,
}

/// Why an offline command failed.
#[derive(Debug, thiserror::Error)]
enum OfflineError {
    /// The configuration did not load; `reef`'s message for it.
    #[error("load the daemon's configuration")]
    Config(#[source] ConfigError),
    #[error(transparent)]
    Restore(RestoreError),
    #[error(transparent)]
    Manifest(ManifestError),
    /// `--repo` names no repository of the backup, or several.
    #[error("{0}")]
    Repo(String),
    /// A path could not be made absolute.
    #[error("resolve {}", path.display())]
    Resolve {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The configuration could not be printed as JSON.
    #[error("serialize the configuration")]
    Json(#[source] serde_json::Error),
}

impl Classify for OfflineError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Config(error) => error.kind(),
            Self::Restore(error) => error.kind(),
            Self::Manifest(error) => error.kind(),
            Self::Repo(_) => ErrorKind::InvalidArgument,
            Self::Resolve { source, .. } => reef_lib::error::classify_by_source(source),
            Self::Json(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for OfflineError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Restore(error) => error.advice(),
            Self::Manifest(error) => error.advice(),
            _ => None,
        }
    }
}

/// Runs `command` with `args`' path flags, writes its one document on
/// stdout, and returns the exit code.
pub fn run(args: &DaemonArgs, command: Offline) -> ExitCode {
    // Logging is for the library's warnings; a bad `REEF_LOG` must not keep
    // the command from running.
    let directive = EnvLog::from_env()
        .ok()
        .and_then(|env| env.filter)
        .unwrap_or_else(|| DEFAULT_FILTER.to_owned());
    let format = EnvLog::from_env().map(|env| env.format).unwrap_or_default();
    let _ = reef_client::logging::init(&directive, format);
    let overrides = PathOverrides {
        state_dir: args.state_dir.clone(),
        socket: args.socket.clone(),
        config_file: args.config.clone(),
    };
    let result = match command {
        Offline::Config(ConfigCommand::Show { json }) => config_show(&overrides, json),
        Offline::Restore(restore) => run_restore(&overrides, &restore),
    };
    match result {
        Ok(bytes) => {
            write_stdout(&bytes);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let document = ErrorDocument {
                error: error_object(&error, error.kind(), error.advice()),
            };
            write_stdout(&json_bytes(&document));
            ExitCode::from(error.kind().exit_code())
        }
    }
}

/// The error object for `error`, as `reef` printed it when it ran these
/// commands in its own process: the error's own advice, and no reason.
fn error_object(
    error: &(dyn Error + 'static),
    kind: ErrorKind,
    advice: Option<Advice>,
) -> ErrorObject {
    let advice = advice.unwrap_or_default();
    ErrorObject {
        kind: kind_text(kind).to_owned(),
        reason: String::new(),
        message: ErrorChain(error).to_string(),
        hint: advice.hint,
        next: advice.next,
        metadata: std::collections::BTreeMap::new(),
    }
}

fn write_stdout(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout leaves nothing to report to.
    let _ = stdout.write_all(bytes).and_then(|()| stdout.flush());
}

fn json_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

fn config_show(overrides: &PathOverrides, json: bool) -> Result<Vec<u8>, OfflineError> {
    let config = Config::load(overrides).map_err(OfflineError::Config)?;
    if !json {
        return Ok(config.to_effective_toml().into_bytes());
    }
    let mut value = serde_json::to_value(config.effective_file()).map_err(OfflineError::Json)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "sources".to_owned(),
            serde_json::to_value(&config.sources).map_err(OfflineError::Json)?,
        );
    }
    Ok(format!("{value:#}\n").into_bytes())
}

fn run_restore(overrides: &PathOverrides, args: &RestoreArgs) -> Result<Vec<u8>, OfflineError> {
    let config = Config::load(overrides).map_err(OfflineError::Config)?;
    let src = absolute(&args.dir)?;
    let mut opts = RestoreOptions::from_config(&config).map_err(OfflineError::Restore)?;
    if let Some(into) = &args.into {
        opts.into = absolute(into)?;
    }
    opts.root = args.root.as_deref().map(absolute).transpose()?;
    opts.force = args.force;
    if let Some(repo) = &args.repo {
        opts.repo = Some(resolve_repo(&src, repo)?);
    }
    let report = restore(&src, &opts).map_err(OfflineError::Restore)?;
    let restored = Restored {
        repos: report
            .repos
            .iter()
            .map(|(id, path)| RestoredRepo {
                id: id.as_hex(),
                jj_repo_path: path.display().to_string(),
            })
            .collect(),
        workspaces_kept: report.workspaces_kept,
        leases_extended: report.leases_extended,
        default_workspace_recreated: report.default_workspace_recreated,
        replaced: report
            .replaced
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    };
    Ok(json_bytes(&restored))
}

/// `--repo`: a full id or a prefix of exactly one of the backup's.
fn resolve_repo(src: &Path, text: &str) -> Result<RepoId, OfflineError> {
    let manifest = Manifest::read(src).map_err(OfflineError::Manifest)?;
    let text = text.trim().to_ascii_lowercase();
    let matches: Vec<&str> = manifest
        .repos
        .iter()
        .map(|repo| repo.id.as_str())
        .filter(|id| id.starts_with(&text))
        .collect();
    match matches.as_slice() {
        [id] => id
            .parse()
            .map_err(|_| OfflineError::Repo(format!("{id:?} is not a repository id"))),
        [] => Err(OfflineError::Repo(format!(
            "the backup has no repository {text:?}"
        ))),
        _ => Err(OfflineError::Repo(format!(
            "{text:?} matches {} of the backup's repositories; give more of the id",
            matches.len()
        ))),
    }
}

fn absolute(path: &Path) -> Result<PathBuf, OfflineError> {
    std::path::absolute(path).map_err(|source| OfflineError::Resolve {
        path: path.to_owned(),
        source,
    })
}
