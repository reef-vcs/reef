//! `reef init [PATH]` (R1.27): registers the repository, initializing jj
//! alongside a plain git checkout (R1.25), then writes the agent files
//! ([`crate::agents`]) for the harnesses it finds or is told. It never
//! commits, snapshots, or stages what it writes, and never prompts.
//!
//! `--check` and `--print` write nothing, and so register nothing either;
//! neither needs the daemon. Without registration the repository's root is
//! the nearest directory from PATH up that has `.jj` or `.git`, else PATH
//! itself (a Reef workspace has neither).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use reef_api::v1::IfNotJj;
use reef_daemon::client::json::Repository;
use reef_daemon::error::Advice;
use reef_lib::error::ErrorKind;
use serde::Serialize;

use super::out::TextRender;
use super::{CliError, Global, Out, Output, REGISTER_TIMEOUT, absolute, connect, guide, repo};
use crate::agents::{self, Action, FileAction, FileError, Guide, Harness};

/// `reef init`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// A path inside the repository.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Which harnesses to write for, besides `AGENTS.md`: `auto` (those the
    /// repository has files for), `all`, `none`, or a list.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "auto",
        value_name = "AGENTS"
    )]
    agents: Vec<Agents>,
    /// Also write the user-level Claude Code skill, ~/.claude/skills/reef.
    #[arg(long)]
    user: bool,
    /// Write nothing; exit 3 if a file is missing or stale.
    #[arg(long, conflicts_with = "print")]
    check: bool,
    /// Write nothing; print the `AGENTS.md` section to paste elsewhere.
    #[arg(long)]
    print: bool,
}

impl Args {
    /// `--print` prints Markdown as text even on a pipe, like the guide.
    pub(crate) fn prints_text(&self) -> bool {
        self.print
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Agents {
    // Not doc comments, which `--help` would list.
    Auto,
    All,
    None,
    Claude,
    Cursor,
}

/// The JSON result.
#[derive(Serialize)]
struct InitJson {
    /// The repository registration found or made; null when it was not
    /// asked for.
    registered: Option<Repository>,
    files: Vec<FileJson>,
}

#[derive(Serialize)]
struct FileJson {
    path: String,
    action: &'static str,
    reason: Option<String>,
    /// For text output: relative to the root.
    #[serde(skip)]
    shown: String,
}

impl TextRender for InitJson {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for file in &self.files {
            match &file.reason {
                Some(reason) => writeln!(w, "{} {} ({reason})", file.action, file.shown)?,
                None => writeln!(w, "{} {}", file.action, file.shown)?,
            }
        }
        Ok(())
    }
}

/// `--print`'s JSON form.
#[derive(Serialize)]
struct PrintJson {
    section: String,
}

pub(crate) async fn run(global: &Global, out: &Out, args: Args) -> Result<(), CliError> {
    let version = env!("CARGO_PKG_VERSION");
    let long = guide::long();
    let guide = Guide {
        short: guide::short(),
        long: &long,
    };
    if args.print {
        let section = agents::render(Harness::AgentsMd, &guide, version);
        return match out.mode() {
            Output::Text => out.raw(format!("{section}\n").as_bytes()),
            Output::Json => out.json_only(&PrintJson { section }),
        };
    }
    let harnesses = harnesses(&args)?;
    let path = absolute(&args.path)?;
    let (registered, root) = if args.check || global.no_register {
        (None, find_root(&path)?)
    } else {
        let repo = register(global, out, &path).await?;
        let root = PathBuf::from(&repo.root);
        (Some(repo), root)
    };
    let root = root.canonicalize().map_err(|source| CliError::Io {
        op: "resolve",
        path: root.clone(),
        source,
    })?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    if args.user && home.is_none() {
        return Err(CliError::Usage(
            "--user writes under the home directory, and HOME is not set".to_owned(),
        ));
    }
    let harnesses = harnesses.unwrap_or_else(|| agents::detect(&root, args.user));
    let plan = agents::plan(&root, home.as_deref(), &harnesses, &guide, version)?;
    if args.check {
        check(&plan, &root, &args, global)?;
    } else {
        agents::apply(&plan)?;
    }
    out.object(&InitJson {
        registered,
        files: plan.iter().map(|file| file_json(file, &root)).collect(),
    })?;
    if !args.check && plan.iter().any(|file| file.contents.is_some()) {
        out.note("reef: files written, not committed");
    }
    Ok(())
}

/// The harnesses `--agents` and `--user` name, or None for `auto`, which
/// needs the root to look at.
fn harnesses(args: &Args) -> Result<Option<Vec<Harness>>, CliError> {
    let only = |name: &str| {
        CliError::Usage(format!(
            "--agents {name} cannot be combined with other values"
        ))
    };
    let mut list = Vec::new();
    for &agents in &args.agents {
        let single = matches!(agents, Agents::Auto | Agents::All | Agents::None);
        if single && args.agents.len() > 1 {
            return Err(only(match agents {
                Agents::Auto => "auto",
                Agents::All => "all",
                _ => "none",
            }));
        }
        match agents {
            Agents::Auto => return Ok(None),
            Agents::None => {}
            Agents::All => list.extend([
                Harness::AgentsMd,
                Harness::ClaudeMd,
                Harness::ClaudeSkill { user: false },
                Harness::CursorRule,
            ]),
            Agents::Claude => {
                list.extend([Harness::ClaudeMd, Harness::ClaudeSkill { user: false }]);
            }
            Agents::Cursor => list.push(Harness::CursorRule),
        }
    }
    if !list.is_empty() && list[0] != Harness::AgentsMd {
        list.insert(0, Harness::AgentsMd);
    }
    if args.user {
        list.push(Harness::ClaudeSkill { user: true });
    }
    let mut unique = Vec::new();
    for harness in list {
        if !unique.contains(&harness) {
            unique.push(harness);
        }
    }
    Ok(Some(unique))
}

/// Registers the repository containing `path` as `repo register` does,
/// with the stderr line implicit registration prints. Already registered
/// is not a failure here.
async fn register(global: &Global, out: &Out, path: &Path) -> Result<Repository, CliError> {
    let ctx = connect(global, out).await?;
    let response = ctx
        .call_within(
            ctx.timeout_flag.unwrap_or(REGISTER_TIMEOUT),
            ctx.client
                .register_repository(path, IfNotJj::InitAlongsideGit),
        )
        .await?;
    let repo = response
        .repository
        .as_ref()
        .map(Repository::from)
        .ok_or_else(|| repo::missing("repository"))?;
    if response.initialized_jj {
        out.note(&format!(
            "reef: registered {} and initialized jj alongside git",
            repo.root
        ));
    } else if !response.already_registered {
        out.note(&format!("reef: registered {}", repo.root));
    }
    Ok(repo)
}

/// The nearest directory from `path` up with `.jj` or `.git`, else `path`.
fn find_root(path: &Path) -> Result<PathBuf, CliError> {
    if !path.exists() {
        return Err(CliError::Io {
            op: "find",
            path: path.to_owned(),
            source: io::ErrorKind::NotFound.into(),
        });
    }
    Ok(path
        .ancestors()
        .find(|dir| dir.join(".jj").exists() || dir.join(".git").exists())
        .unwrap_or(path)
        .to_owned())
}

/// `--check`: fails with the files `reef init` would write, and the `reef
/// init` that writes them.
fn check(plan: &[FileAction], root: &Path, args: &Args, global: &Global) -> Result<(), CliError> {
    let stale: Vec<String> = plan
        .iter()
        .filter_map(|file| {
            let why = match file.action {
                Action::Write => "missing",
                Action::Update => "stale",
                Action::Unchanged | Action::Skip(_) => return None,
            };
            Some(format!("{} ({why})", agents::display(&file.path, root)))
        })
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    let mut next = vec!["reef".to_owned(), "init".to_owned()];
    if global.no_register {
        next.push("--no-register".to_owned());
    }
    if args.path != Path::new(".") {
        next.push(args.path.display().to_string());
    }
    if args.agents != [Agents::Auto] {
        let names: Vec<_> = args
            .agents
            .iter()
            .filter_map(clap::ValueEnum::to_possible_value)
            .map(|value| value.get_name().to_owned())
            .collect();
        next.extend(["--agents".to_owned(), names.join(",")]);
    }
    if args.user {
        next.push("--user".to_owned());
    }
    Err(CliError::Refused {
        kind: ErrorKind::NotFound,
        message: format!("agent files are not current: {}", stale.join(", ")),
        advice: Some(Advice::run(
            "Run reef init to write them, then commit them.",
            next,
        )),
    })
}

fn file_json(file: &FileAction, root: &Path) -> FileJson {
    FileJson {
        path: file.path.display().to_string(),
        action: file.action.done(),
        reason: file.action.reason().map(str::to_owned),
        shown: agents::display(&file.path, root),
    }
}

impl From<FileError> for CliError {
    fn from(error: FileError) -> Self {
        CliError::Io {
            op: error.op,
            path: error.path,
            source: error.source,
        }
    }
}
