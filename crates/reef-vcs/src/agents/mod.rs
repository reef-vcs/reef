//! The files `reef init` writes for coding agents (R1.27): a managed
//! section in `AGENTS.md` and `CLAUDE.md`, and whole files Reef owns (the
//! Claude Code skill, repository and user level, and the Cursor rule), all
//! from the agent guide (`docs/agent-guide.md`, R1.26), so there is one
//! guide and it cannot drift; and Reef's entry in `.mcp.json` (R2.13),
//! which starts `reef mcp`. Since R2.25 also Codex's and opencode's: the
//! shared Agent Skills file and their MCP entries, in the repository and
//! in the home directory; and `packages` (compiled for its tests), the plugins built from the
//! same renderers.
//!
//! Everything here but [`apply`] is a pure function of the guide and what
//! is on disk. Reef never overwrites text it did not write: a section lives
//! between [`BEGIN`] and [`END`] marker lines and the bytes around it are
//! kept as they are; a whole file is replaced only when its frontmatter has
//! [`GENERATED_BY`]; in `.mcp.json`, Reef owns only `mcpServers.reef`, and
//! likewise `mcp.reef` in `opencode.json` and `[mcp_servers.reef]` in
//! Codex's `config.toml`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

// Rendered and checked by its tests, and written by `just packages`; the
// binary never writes them.
#[cfg(test)]
mod packages;

/// A plugin format a harness installs, and the tree Reef ships in it
/// (R2.25, rendered by the `packages` tests).
#[cfg_attr(not(test), allow(dead_code, reason = "read by the packages' tests"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Package {
    /// Claude Code's own format: the skill, the MCP entry, and the hooks
    /// that report declared edits.
    ClaudeCode,
    /// The vendor-neutral Agent Plugins 1.0.0 format, which Codex installs:
    /// the skill and the MCP entry.
    AgentPlugins,
}

/// The section format version. A section of an older version is replaced;
/// one of a newer version is left to the newer `reef` that wrote it.
const SECTION_VERSION: u32 = 1;

/// The start of a begin marker line; the version and ` -->` follow.
const BEGIN: &str = "<!-- reef:begin v";

/// The end marker line.
const END: &str = "<!-- reef:end -->";

/// The frontmatter line that marks a whole file as Reef's to overwrite.
const GENERATED_BY: &str = "generated-by: reef init";

/// The skill's and the rule's `description`: when the model should use
/// them.
const DESCRIPTION: &str = "Isolate work in this repository in a Reef workspace (reef ws run), \
     find out where you are (reef here), and read errors' next field for the fix.";

/// The guide's two forms (`reef guide --short` and `reef guide`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Guide<'a> {
    /// Into the `AGENTS.md` and `CLAUDE.md` sections.
    pub(crate) short: &'a str,
    /// Into the skill and the Cursor rule.
    pub(crate) long: &'a str,
}

/// A file `reef init` can write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Harness {
    /// A section in `AGENTS.md`, which most harnesses read.
    AgentsMd,
    /// A section in `CLAUDE.md`, unless it imports `AGENTS.md`.
    ClaudeMd,
    /// The Claude Code skill: `.claude/skills/reef/SKILL.md` in the
    /// repository, or in the home directory when `user`.
    ClaudeSkill {
        /// `~/.claude/skills/reef/SKILL.md` rather than the repository's.
        user: bool,
    },
    /// The Cursor rule, `.cursor/rules/reef.mdc`.
    CursorRule,
    /// Reef's entry in `.mcp.json`, the project's MCP servers (Claude Code
    /// reads it; R2.13).
    McpJson,
    /// The skill in the Agent Skills standard's shared directory, which
    /// Codex and opencode read: `.agents/skills/reef/SKILL.md`, or in the
    /// home directory when `user` (R2.25).
    AgentSkill {
        /// `~/.agents/skills/reef/SKILL.md` rather than the repository's.
        user: bool,
    },
    /// Reef's `[mcp_servers.reef]` in Codex's `config.toml`: the project's
    /// `.codex/config.toml`, or `$CODEX_HOME/config.toml` when `user`.
    CodexConfig {
        /// The user's file rather than the project's.
        user: bool,
    },
    /// Reef's `mcp.reef` in opencode's `opencode.json`: the project's, or
    /// `$XDG_CONFIG_HOME/opencode/opencode.json` when `user`.
    OpencodeJson {
        /// The user's file rather than the project's.
        user: bool,
    },
}

impl Harness {
    /// Where the file is: under `root`, or `home` for a user-level file.
    /// None when that needs a home directory and there is none.
    pub(crate) fn path(self, root: &Path, home: Option<&Home>) -> Option<PathBuf> {
        let claude_skill = Path::new(".claude/skills/reef/SKILL.md");
        let agent_skill = Path::new(".agents/skills/reef/SKILL.md");
        Some(match self {
            Harness::AgentsMd => root.join("AGENTS.md"),
            Harness::ClaudeMd => root.join("CLAUDE.md"),
            Harness::ClaudeSkill { user: false } => root.join(claude_skill),
            Harness::ClaudeSkill { user: true } => home?.dir.join(claude_skill),
            Harness::CursorRule => root.join(".cursor/rules/reef.mdc"),
            Harness::McpJson => root.join(".mcp.json"),
            Harness::AgentSkill { user: false } => root.join(agent_skill),
            Harness::AgentSkill { user: true } => home?.dir.join(agent_skill),
            Harness::CodexConfig { user: false } => root.join(".codex/config.toml"),
            Harness::CodexConfig { user: true } => home?.codex.join("config.toml"),
            Harness::OpencodeJson { user: false } => root.join("opencode.json"),
            Harness::OpencodeJson { user: true } => home?.config.join("opencode/opencode.json"),
        })
    }

    /// The path to report when there is no home directory to put it in.
    fn home_path(self) -> &'static str {
        match self {
            Harness::AgentSkill { .. } => "~/.agents/skills/reef/SKILL.md",
            Harness::CodexConfig { .. } => "~/.codex/config.toml",
            Harness::OpencodeJson { .. } => "~/.config/opencode/opencode.json",
            _ => "~/.claude/skills/reef/SKILL.md",
        }
    }

    /// Whether the file is outside the repository, in the home directory.
    fn is_user(self) -> bool {
        matches!(
            self,
            Harness::ClaudeSkill { user: true }
                | Harness::AgentSkill { user: true }
                | Harness::CodexConfig { user: true }
                | Harness::OpencodeJson { user: true }
        )
    }

    /// Whether Reef owns the whole file, rather than a section of it.
    fn is_whole_file(self) -> bool {
        matches!(
            self,
            Harness::ClaudeSkill { .. } | Harness::CursorRule | Harness::AgentSkill { .. }
        )
    }
}

/// The home directory and the agents' directories in it, which their
/// variables can move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Home {
    /// `$HOME`.
    dir: PathBuf,
    /// `$CODEX_HOME`, else `~/.codex`.
    codex: PathBuf,
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    config: PathBuf,
}

impl Home {
    /// From the environment (`var` is `std::env::var_os`); None without a
    /// `HOME`. An empty or relative variable counts as unset.
    pub(crate) fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Home> {
        let path = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let dir = var("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)?;
        Some(Home {
            codex: path("CODEX_HOME").unwrap_or_else(|| dir.join(".codex")),
            config: path("XDG_CONFIG_HOME").unwrap_or_else(|| dir.join(".config")),
            dir,
        })
    }

    /// Whether `place` exists as a directory.
    fn has(&self, place: Place) -> bool {
        match place {
            Place::Dot(name) => self.dir.join(name).is_dir(),
            Place::CodexHome => self.codex.is_dir(),
            Place::Config(name) => self.config.join(name).is_dir(),
        }
    }
}

/// A directory in the home directory that an agent creates when it runs,
/// so its presence means the agent is used on this machine.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// A directory directly under `$HOME`, such as `.claude`.
    Dot(&'static str),
    /// `$CODEX_HOME`, else `~/.codex`.
    CodexHome,
    /// A directory under `$XDG_CONFIG_HOME`, else `~/.config`.
    Config(&'static str),
}

/// A coding agent that reads files of its own besides `AGENTS.md`, which
/// every agent gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agent {
    /// `CLAUDE.md`, the Claude Code skill, and `.mcp.json`.
    ClaudeCode,
    /// The Cursor rule.
    Cursor,
    /// The shared skill and `.codex/config.toml`.
    Codex,
    /// The shared skill and `opencode.json`.
    Opencode,
}

/// How `auto` recognizes an agent, and what it writes for it.
pub(crate) struct Sign {
    /// The agent this row is for.
    pub(crate) agent: Agent,
    /// Names at the root whose presence means the repository uses it; one
    /// ending in `/` counts only as a directory.
    names: &'static [&'static str],
    /// A variable the agent sets, to this value, in the processes it
    /// starts, so `reef init` run by the agent writes its files though the
    /// repository has none yet. Only a variable the agent documents or
    /// its pinned release's source sets.
    env: Option<(&'static str, &'static str)>,
    /// What Reef writes for it in the repository.
    harnesses: &'static [Harness],
    /// Directories in the home directory that show the agent is used on
    /// this machine, so `auto --user` writes its user-level files.
    home: &'static [Place],
    /// What `--user` adds outside the repository, which also turns the
    /// agent's files on.
    user: &'static [Harness],
    /// The plugin a harness installs Reef with, if it has one (R2.25).
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the packages' tests"))]
    package: Option<Package>,
    /// The one line that sets the harness up for Reef, as the quickstart
    /// documents it.
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the packages' tests"))]
    install: &'static str,
}

/// Every agent Reef writes files for besides `AGENTS.md`. Recognizing
/// another agent is a row here; writing for a new one also needs its
/// [`Harness`] variants and an `--agents` value.
pub(crate) const SIGNS: &[Sign] = &[
    Sign {
        agent: Agent::ClaudeCode,
        names: &["CLAUDE.md", ".claude/"],
        // Its shells, hooks and MCP servers, and its IDE extensions'
        // terminals.
        env: Some(("CLAUDECODE", "1")),
        harnesses: &[
            Harness::ClaudeMd,
            Harness::ClaudeSkill { user: false },
            Harness::McpJson,
        ],
        home: &[Place::Dot(".claude")],
        // Its user MCP configuration is `~/.claude.json`, a state file
        // Reef does not edit; the plugin is the user-level route.
        user: &[Harness::ClaudeSkill { user: true }],
        package: Some(Package::ClaudeCode),
        install: "claude plugin install reef --marketplace reef-vcs/reef",
    },
    Sign {
        agent: Agent::Cursor,
        names: &[".cursor/"],
        env: None,
        harnesses: &[Harness::CursorRule],
        home: &[],
        user: &[],
        package: None,
        install: "reef init --agents cursor",
    },
    Sign {
        agent: Agent::Codex,
        names: &[".codex/"],
        // None documented.
        env: None,
        harnesses: &[
            Harness::AgentSkill { user: false },
            Harness::CodexConfig { user: false },
        ],
        home: &[Place::CodexHome],
        user: &[
            Harness::AgentSkill { user: true },
            Harness::CodexConfig { user: true },
        ],
        package: Some(Package::AgentPlugins),
        install: "codex plugin marketplace add reef-vcs/reef && codex plugin add reef@reef",
    },
    Sign {
        agent: Agent::Opencode,
        names: &["opencode.json", "opencode.jsonc", ".opencode/"],
        // Set on its own process at startup, so its shells inherit it
        // (opencode-ai 1.19.5); undocumented.
        env: Some(("OPENCODE", "1")),
        harnesses: &[
            Harness::AgentSkill { user: false },
            Harness::OpencodeJson { user: false },
        ],
        home: &[Place::Config("opencode")],
        user: &[
            Harness::AgentSkill { user: true },
            Harness::OpencodeJson { user: true },
        ],
        // Its plugins are npm modules that cannot be shown to declare an
        // MCP server; `reef init` writes both files instead.
        package: None,
        install: "reef init --agents opencode --user",
    },
];

/// The harnesses `auto` writes: `AGENTS.md` always, then each agent's when
/// the repository has its files at `root` or the agent is `running` `reef
/// init`; with `user`, also each agent's user-level files when `home` shows
/// it is used on this machine, which turns its repository files on too.
pub(crate) fn detect(root: &Path, user: Option<&Home>, running: &[Agent]) -> Vec<Harness> {
    let mut found = vec![Harness::AgentsMd];
    for sign in SIGNS {
        let mut present = false;
        let mut blocked = false;
        for name in sign.names {
            // `symlink_metadata`: a `CLAUDE.md` linked to a missing
            // `AGENTS.md` still counts.
            let exists = fs::symlink_metadata(root.join(name.trim_end_matches('/'))).is_ok();
            match name.strip_suffix('/') {
                Some(dir) if root.join(dir).is_dir() => present = true,
                // A file, or a link to nothing, where the agent's directory
                // goes: writing under it would fail.
                Some(_) => blocked |= exists,
                None => present |= exists,
            }
        }
        let running = !blocked && running.contains(&sign.agent);
        let user = user.is_some_and(|home| {
            !sign.user.is_empty()
                && (present || running || sign.home.iter().any(|&place| home.has(place)))
        });
        if present || user || running {
            push_unique(&mut found, sign.harnesses);
        }
        if user {
            push_unique(&mut found, sign.user);
        }
    }
    found
}

/// Appends each of `harnesses` that `list` does not have yet: the shared
/// skill is two agents'.
fn push_unique(list: &mut Vec<Harness>, harnesses: &[Harness]) {
    for &harness in harnesses {
        if !list.contains(&harness) {
            list.push(harness);
        }
    }
}

/// What Reef writes for `agent` in the repository. Every [`Agent`] has a
/// row in [`SIGNS`] (a test checks), so none is ever empty.
pub(crate) fn harnesses(agent: Agent) -> &'static [Harness] {
    sign(agent).map_or(&[], |sign| sign.harnesses)
}

/// What `--user` adds for `agent`.
pub(crate) fn user_harnesses(agent: Agent) -> &'static [Harness] {
    sign(agent).map_or(&[], |sign| sign.user)
}

fn sign(agent: Agent) -> Option<&'static Sign> {
    SIGNS.iter().find(|sign| sign.agent == agent)
}

/// Every harness in the repository: `AGENTS.md`, then each agent's.
pub(crate) fn all() -> Vec<Harness> {
    let mut all = vec![Harness::AgentsMd];
    for sign in SIGNS {
        push_unique(&mut all, sign.harnesses);
    }
    all
}

/// The agents that say they started this process, by the variables in
/// [`SIGNS`]; `var` looks one up (`std::env::var_os`).
pub(crate) fn running(var: impl Fn(&str) -> Option<OsString>) -> Vec<Agent> {
    SIGNS
        .iter()
        .filter(|sign| {
            sign.env
                .is_some_and(|(name, value)| var(name).is_some_and(|set| set == value))
        })
        .map(|sign| sign.agent)
        .collect()
}

/// The text Reef writes for `harness`: for a section, the marker lines and
/// what is between them, with no newline after the end marker; for a whole
/// file, the file. `version` is the `reef` that writes it.
pub(crate) fn render(harness: Harness, guide: &Guide<'_>, version: &str) -> String {
    let preface =
        format!("Written by `reef init` (reef {version}); `reef guide` prints the current guide.");
    render_with(harness, guide, &preface)
}

/// [`render`] with another first line, such as a plugin's.
pub(crate) fn render_with(harness: Harness, guide: &Guide<'_>, preface: &str) -> String {
    let frontmatter = match harness {
        Harness::AgentsMd | Harness::ClaudeMd => {
            return format!(
                "{BEGIN}{SECTION_VERSION} -->\n{preface}\n\n{}\n{END}",
                guide.short.trim_end()
            );
        }
        Harness::ClaudeSkill { .. } => {
            format!("name: reef\ndescription: {DESCRIPTION}\n{GENERATED_BY}")
        }
        Harness::CursorRule => {
            format!("description: {DESCRIPTION}\nalwaysApply: false\n{GENERATED_BY}")
        }
        // The standard's keys only; others go under `metadata`.
        Harness::AgentSkill { .. } => {
            format!("name: reef\ndescription: {DESCRIPTION}\nmetadata:\n  {GENERATED_BY}")
        }
        Harness::McpJson => return JsonTarget::MCP_JSON.render(None),
        Harness::OpencodeJson { .. } => return JsonTarget::OPENCODE.render(None),
        Harness::CodexConfig { .. } => return codex_toml(toml_edit::DocumentMut::new()),
    };
    format!(
        "---\n{frontmatter}\n---\n\n{preface}\n\n{}\n",
        guide.long.trim_end()
    )
}

/// What splicing a section into a file comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Splice {
    /// No file: these are its contents.
    Create(String),
    /// A file without markers: these are its contents, the section after a
    /// blank line.
    Append(String),
    /// A file with a different section: these are its contents, identical
    /// outside the markers.
    Replace(String),
    /// The file already has this section.
    Unchanged,
    /// The file imports `AGENTS.md` on a line of its own (`@AGENTS.md`) and
    /// has no section, so it already has the guide.
    SkipImports,
    /// Markers out of order, unpaired, or repeated: nothing is safe to
    /// replace, and appending would add a second section.
    Damaged,
    /// A section of a newer format than this `reef` writes.
    Newer(u32),
}

/// Splices `section` (from [`render`]) between the markers in `existing`
/// (None when the file does not exist), keeping the file's line ending
/// (CRLF if it has any) and whether it ends in one.
pub(crate) fn splice(existing: Option<&str>, section: &str) -> Splice {
    let Some(existing) = existing else {
        return Splice::Create(format!("{section}\n"));
    };
    let eol = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let section = section.replace('\n', eol);
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    let mut imports = false;
    // A byte order mark is not part of the first line's text.
    let mut offset = if existing.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    for line in existing[offset..].split_inclusive('\n') {
        let text = line.trim_end();
        imports |= matches!(text, "@AGENTS.md" | "@./AGENTS.md");
        if let Some(rest) = text.strip_prefix(BEGIN) {
            begins.push((
                offset,
                rest.strip_suffix(" -->").and_then(|v| v.parse().ok()),
            ));
        } else if text == END {
            // The section ends before the end line's line ending.
            ends.push(offset + text.len());
        }
        offset += line.len();
    }
    match (begins.as_slice(), ends.as_slice()) {
        // An import makes a section redundant, but a section already there
        // is kept current rather than left stale.
        ([], []) if imports => Splice::SkipImports,
        ([], []) if existing.is_empty() => Splice::Append(format!("{section}{eol}")),
        ([], []) if existing.ends_with('\n') => {
            Splice::Append(format!("{existing}{eol}{section}{eol}"))
        }
        ([], []) => Splice::Append(format!("{existing}{eol}{eol}{section}")),
        (&[(begin, Some(version))], &[end]) if begin < end => {
            if version > SECTION_VERSION {
                return Splice::Newer(version);
            }
            if existing[begin..end] == section {
                return Splice::Unchanged;
            }
            Splice::Replace(format!(
                "{}{section}{}",
                &existing[..begin],
                &existing[end..]
            ))
        }
        _ => Splice::Damaged,
    }
}

/// What `reef init` does, or would do, to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// A new file.
    Write,
    /// A changed file.
    Update,
    /// Already current.
    Unchanged,
    /// Left alone, and why.
    Skip(String),
}

impl Action {
    /// The word `reef init` reports: `wrote`, `updated`, `unchanged`, or
    /// `skipped`.
    pub(crate) fn done(&self) -> &'static str {
        match self {
            Action::Write => "wrote",
            Action::Update => "updated",
            Action::Unchanged => "unchanged",
            Action::Skip(_) => "skipped",
        }
    }

    /// Why the file was skipped.
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Action::Skip(reason) => Some(reason),
            _ => None,
        }
    }
}

/// One file of a [`plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileAction {
    /// The path the harness reads.
    pub(crate) path: PathBuf,
    /// The file written: `path`, or its target if `path` is a symbolic
    /// link, so that a link (`CLAUDE.md` to `AGENTS.md`) stays one.
    pub(crate) target: PathBuf,
    /// What happens to it.
    pub(crate) action: Action,
    /// The new contents, for [`Action::Write`] and [`Action::Update`].
    pub(crate) contents: Option<String>,
}

/// A file operation that failed.
#[derive(Debug)]
pub(crate) struct FileError {
    /// What was being done, such as `read`.
    pub(crate) op: &'static str,
    /// The file.
    pub(crate) path: PathBuf,
    /// The underlying error.
    pub(crate) source: io::Error,
}

/// What writing `harnesses` under `root` (canonical) does to each file,
/// in the order given. Reads files and writes nothing. `home` is where the
/// user-level files go; a harness that needs it is skipped without one.
pub(crate) fn plan(
    root: &Path,
    home: Option<&Home>,
    harnesses: &[Harness],
    guide: &Guide<'_>,
    version: &str,
) -> Result<Vec<FileAction>, FileError> {
    let mut plan: Vec<FileAction> = Vec::new();
    for &harness in harnesses {
        let Some(path) = harness.path(root, home) else {
            plan.push(FileAction {
                path: PathBuf::from(harness.home_path()),
                target: PathBuf::new(),
                action: Action::Skip("HOME is not set".to_owned()),
                contents: None,
            });
            continue;
        };
        let skip = |path: PathBuf, why: String| FileAction {
            target: path.clone(),
            path,
            action: Action::Skip(why),
            contents: None,
        };
        let target = match resolve(&path)? {
            // A link (the file's or a directory's) may point anywhere; a
            // repository's files stay in it. The home directory's links are
            // the user's own.
            Ok(target) if !harness.is_user() && !target.starts_with(root) => {
                plan.push(skip(path, "symbolic link out of the repository".to_owned()));
                continue;
            }
            Ok(target) => target,
            Err(why) => {
                plan.push(skip(path, why));
                continue;
            }
        };
        if let Some(same) = plan.iter().find(|done| done.target == target) {
            let why = format!("same file as {}", display(&same.path, root));
            plan.push(skip(path, why));
            continue;
        }
        if let Harness::OpencodeJson { .. } = harness {
            // opencode reads `opencode.jsonc` first, and Reef does not
            // rewrite a file with comments in it.
            let jsonc = path.with_extension("jsonc");
            if fs::symlink_metadata(&jsonc).is_ok() {
                let entry = Json::Object(IndexMap::from([(
                    "mcp".to_owned(),
                    Json::Object(IndexMap::from([(
                        MCP_SERVER.to_owned(),
                        mcp_entry(McpShape::Opencode),
                    )])),
                )]));
                let entry = serde_json::to_string(&entry).unwrap_or_default();
                let why = format!(
                    "{}: comments are not rewritten; add {entry} by hand",
                    display(&jsonc, root)
                );
                plan.push(skip(path, why));
                continue;
            }
        }
        let existing = match fs::read(&target) {
            Ok(bytes) => {
                let Ok(text) = String::from_utf8(bytes) else {
                    plan.push(skip(path, "not UTF-8".to_owned()));
                    continue;
                };
                Some(text)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(FileError {
                    op: "read",
                    path,
                    source,
                });
            }
        };
        let rendered = render(harness, guide, version);
        let (action, contents) = if harness == Harness::McpJson {
            merge_mcp_json(existing.as_deref())
        } else if let Harness::OpencodeJson { .. } = harness {
            JsonTarget::OPENCODE.merge(existing.as_deref())
        } else if let Harness::CodexConfig { .. } = harness {
            merge_codex_toml(existing.as_deref())
        } else if harness.is_whole_file() {
            whole_file(existing.as_deref(), &rendered)
        } else {
            match splice(existing.as_deref(), &rendered) {
                Splice::Create(text) => (Action::Write, Some(text)),
                Splice::Append(text) | Splice::Replace(text) => (Action::Update, Some(text)),
                Splice::Unchanged => (Action::Unchanged, None),
                Splice::SkipImports => (Action::Skip("imports AGENTS.md".to_owned()), None),
                Splice::Damaged => (Action::Skip("markers damaged".to_owned()), None),
                Splice::Newer(v) => (
                    Action::Skip(format!("section v{v} is from a newer reef")),
                    None,
                ),
            }
        };
        plan.push(FileAction {
            path,
            target,
            action,
            contents,
        });
    }
    Ok(plan)
}

/// The action for a whole file Reef owns: written if missing, replaced if
/// its frontmatter says Reef wrote it, else skipped. A CRLF file is
/// compared and written with CRLF.
fn whole_file(existing: Option<&str>, rendered: &str) -> (Action, Option<String>) {
    let Some(existing) = existing else {
        return (Action::Write, Some(rendered.to_owned()));
    };
    if !generated_by_reef(existing) {
        return (Action::Skip("not written by reef init".to_owned()), None);
    }
    let rendered = if existing.contains("\r\n") {
        rendered.replace('\n', "\r\n")
    } else {
        rendered.to_owned()
    };
    if existing == rendered {
        (Action::Unchanged, None)
    } else {
        (Action::Update, Some(rendered))
    }
}

/// The key of Reef's server among a harness's servers.
const MCP_SERVER: &str = "reef";

/// The forms a harness's configuration gives an MCP server in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpShape {
    /// `.mcp.json`'s `mcpServers` entry, which the Claude Code plugin and
    /// the Agent Plugins `mcp.json` use too.
    McpJson,
    /// opencode's `mcp` entry: the command and its arguments in one array.
    Opencode,
    /// Codex's `[mcp_servers.reef]` table.
    CodexToml,
    /// An MCPB bundle manifest's `mcp_config`: the `reef` beside it.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "rendered by the packages' tests")
    )]
    Mcpb,
}

/// Reef's server entry in `shape`: `reef mcp` from `PATH`, as every
/// installer puts it there (a bundle's own `reef` for MCPB). No absolute
/// path, which would bake one machine's layout into a committed file; no
/// `env`, since `REEF_AGENT` is the harness's to set; and no version, so a
/// new `reef` does not make the file stale.
pub(crate) fn render_mcp_entry(shape: McpShape) -> serde_json::Value {
    serde_json::to_value(mcp_entry(shape)).unwrap_or_default()
}

/// [`render_mcp_entry`] in the order a person writes it.
fn mcp_entry(shape: McpShape) -> Json {
    let text = |text: &str| Json::String(text.to_owned());
    let object = |pairs: Vec<(&str, Json)>| {
        Json::Object(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    };
    let args = || Json::Array(vec![text("mcp")]);
    match shape {
        McpShape::McpJson => object(vec![
            ("type", text("stdio")),
            ("command", text("reef")),
            ("args", args()),
        ]),
        McpShape::Opencode => object(vec![
            ("type", text("local")),
            ("command", Json::Array(vec![text("reef"), text("mcp")])),
            ("enabled", Json::Bool(true)),
        ]),
        McpShape::CodexToml => object(vec![("command", text("reef")), ("args", args())]),
        McpShape::Mcpb => object(vec![
            ("command", text("${__dirname}/reef")),
            ("args", args()),
        ]),
    }
}

/// A JSON value whose objects keep their keys' order, so a merge changes
/// nothing but Reef's entry and whitespace (and a number's spelling:
/// `1e5` is written `100000.0`). Not `serde_json`'s `preserve_order`:
/// features unify across the build, and it would reorder `config show
/// --json`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(IndexMap<String, Json>),
}

/// A JSON file whose one key Reef owns: an MCP server among the file's
/// servers.
#[derive(Clone, Copy, Debug)]
struct JsonTarget {
    /// The key that holds the servers.
    servers: &'static str,
    /// The shape of Reef's entry there.
    shape: McpShape,
    /// The `$schema` a new file starts with.
    schema: Option<&'static str>,
}

impl JsonTarget {
    /// `.mcp.json`'s `mcpServers.reef`.
    const MCP_JSON: JsonTarget = JsonTarget {
        servers: "mcpServers",
        shape: McpShape::McpJson,
        schema: None,
    };

    /// `opencode.json`'s `mcp.reef`.
    const OPENCODE: JsonTarget = JsonTarget {
        servers: "mcp",
        shape: McpShape::Opencode,
        schema: Some("https://opencode.ai/config.json"),
    };

    /// The file as Reef writes it: `existing` with Reef's entry added, two-
    /// space JSON with a final newline; a new file with only the entry
    /// (after `$schema`) when `existing` is `None`.
    fn render(self, existing: Option<IndexMap<String, Json>>) -> String {
        let mut file = existing.unwrap_or_else(|| {
            self.schema
                .map(|schema| ("$schema".to_owned(), Json::String(schema.to_owned())))
                .into_iter()
                .collect()
        });
        let entry = mcp_entry(self.shape);
        match file
            .entry(self.servers.to_owned())
            .or_insert_with(|| Json::Object(IndexMap::new()))
        {
            Json::Object(servers) => {
                servers.insert(MCP_SERVER.to_owned(), entry);
            }
            other => *other = Json::Object(IndexMap::from([(MCP_SERVER.to_owned(), entry)])),
        }
        let mut text = serde_json::to_string_pretty(&file).unwrap_or_default();
        text.push('\n');
        text
    }

    /// The action for the file: Reef owns `<servers>.reef` and nothing
    /// else. A missing (or blank) file is written with only Reef's entry; a
    /// file without it gets it added after the other servers, everything
    /// else kept in order; an entry that differs from Reef's was changed by
    /// someone (an `--agent`, an `env`) and is kept, as is a file that is
    /// not a JSON object. A CRLF file stays CRLF.
    fn merge(self, existing: Option<&str>) -> (Action, Option<String>) {
        let Some(text) = existing.filter(|text| !text.trim().is_empty()) else {
            return (Action::Write, Some(self.render(None)));
        };
        let not_object = || (Action::Skip("not a JSON object".to_owned()), None);
        let Ok(Json::Object(file)) = serde_json::from_str::<Json>(text) else {
            return not_object();
        };
        match file.get(self.servers) {
            None => {}
            Some(Json::Object(servers)) => {
                if let Some(entry) = servers.get(MCP_SERVER) {
                    let entry = serde_json::to_value(entry).unwrap_or_default();
                    return if entry == render_mcp_entry(self.shape) {
                        (Action::Unchanged, None)
                    } else {
                        let why = format!("{}.{MCP_SERVER} is not reef init's", self.servers);
                        (Action::Skip(why), None)
                    };
                }
            }
            Some(_) => return not_object(),
        }
        (
            Action::Update,
            Some(keep_crlf(text, self.render(Some(file)))),
        )
    }
}

/// `written` with CRLF line endings if `existing` has any.
fn keep_crlf(existing: &str, written: String) -> String {
    if existing.contains("\r\n") {
        written.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        written
    }
}

/// `.mcp.json`'s action ([`JsonTarget::merge`]).
fn merge_mcp_json(existing: Option<&str>) -> (Action, Option<String>) {
    JsonTarget::MCP_JSON.merge(existing)
}

/// Codex's `config.toml` with Reef's `[mcp_servers.reef]` added to `doc`.
fn codex_toml(mut doc: toml_edit::DocumentMut) -> String {
    let Json::Object(entry) = mcp_entry(McpShape::CodexToml) else {
        return doc.to_string();
    };
    let mut table = toml_edit::Table::new();
    for (key, value) in entry {
        let value = match value {
            Json::String(text) => toml_edit::value(text),
            Json::Array(items) => {
                let array: toml_edit::Array = items
                    .into_iter()
                    .filter_map(|item| match item {
                        Json::String(text) => Some(text),
                        _ => None,
                    })
                    .collect();
                toml_edit::value(array)
            }
            _ => continue,
        };
        table.insert(&key, value);
    }
    if let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_mut)
    {
        // Under `[mcp_servers.x]` headers the parent is implicit, and
        // stays so.
        servers.insert(MCP_SERVER, toml_edit::Item::Table(table));
    } else if let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_inline_table_mut)
    {
        // `mcp_servers = { ... }`: an inline table cannot be extended by a
        // header, so the entry goes inside it, inline too.
        servers.insert(
            MCP_SERVER,
            toml_edit::Value::InlineTable(table.into_inline_table()),
        );
    } else {
        let mut servers = toml_edit::Table::new();
        servers.set_implicit(true);
        servers.insert(MCP_SERVER, toml_edit::Item::Table(table));
        doc.insert("mcp_servers", toml_edit::Item::Table(servers));
    }
    doc.to_string()
}

/// The action for Codex's `config.toml`: as [`JsonTarget::merge`], for
/// `[mcp_servers.reef]`, with the file's comments, order and formatting
/// outside that table kept by `toml_edit`.
fn merge_codex_toml(existing: Option<&str>) -> (Action, Option<String>) {
    let Some(text) = existing.filter(|text| !text.trim().is_empty()) else {
        return (
            Action::Write,
            Some(codex_toml(toml_edit::DocumentMut::new())),
        );
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return (Action::Skip("not TOML".to_owned()), None);
    };
    match doc.get("mcp_servers") {
        None => {}
        Some(servers) if servers.is_table_like() => {
            if let Some(entry) = servers.get(MCP_SERVER) {
                return if codex_entry(entry) == Some(render_mcp_entry(McpShape::CodexToml)) {
                    (Action::Unchanged, None)
                } else {
                    let why = format!("mcp_servers.{MCP_SERVER} is not reef init's");
                    (Action::Skip(why), None)
                };
            }
        }
        Some(_) => return (Action::Skip("mcp_servers is not a table".to_owned()), None),
    }
    // Appended at the end, so the file's text before it is kept byte for
    // byte; a file that defines `mcp_servers` inline or by dotted keys
    // cannot take a `[mcp_servers.reef]` header, and `toml_edit` places
    // the entry instead.
    let mut appended = text.to_owned();
    if !appended.ends_with('\n') {
        appended.push('\n');
    }
    appended.push('\n');
    appended.push_str(&codex_toml(toml_edit::DocumentMut::new()));
    let added = |text: &str| {
        text.parse::<toml_edit::DocumentMut>().is_ok_and(|doc| {
            doc.get("mcp_servers")
                .and_then(|servers| servers.get(MCP_SERVER))
                .and_then(codex_entry)
                == Some(render_mcp_entry(McpShape::CodexToml))
        })
    };
    let written = if added(&appended) {
        appended
    } else {
        codex_toml(doc)
    };
    (Action::Update, Some(keep_crlf(text, written)))
}

/// A `config.toml` server entry as JSON, to compare with Reef's; None when
/// it holds something JSON cannot say the same way.
fn codex_entry(item: &toml_edit::Item) -> Option<serde_json::Value> {
    let table = item.as_table_like()?;
    let mut object = serde_json::Map::new();
    for (key, value) in table.iter() {
        let value = match value.as_value()? {
            toml_edit::Value::String(text) => serde_json::Value::from(text.value().as_str()),
            toml_edit::Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(serde_json::Value::from))
                .collect::<Option<Vec<_>>>()?
                .into(),
            toml_edit::Value::Boolean(flag) => serde_json::Value::from(*flag.value()),
            _ => return None,
        };
        object.insert(key.to_owned(), value);
    }
    Some(object.into())
}

/// Whether `text` opens with YAML frontmatter that has the
/// [`GENERATED_BY`] line, at the top level or (in an Agent Skills file)
/// under `metadata`.
fn generated_by_reef(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_end);
    lines.next() == Some("---")
        && lines
            .take_while(|line| *line != "---")
            .any(|line| line.trim_start() == GENERATED_BY)
}

/// The file to write for `path`, canonical: what it links to if it is a
/// link, else the canonical form of its deepest existing directory with
/// the rest of the path after it. The inner error is why the file is
/// skipped.
fn resolve(path: &Path) -> Result<Result<PathBuf, String>, FileError> {
    let fail = |source| FileError {
        op: "resolve",
        path: path.to_owned(),
        source,
    };
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => Ok(Ok(target)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Err("symbolic link to a missing file".to_owned()))
            }
            Err(source) => Err(fail(source)),
        },
        Ok(meta) if !meta.is_file() => Ok(Err("not a regular file".to_owned())),
        _ => {
            let Some(dir) = path.ancestors().skip(1).find(|dir| dir.exists()) else {
                return Ok(Ok(path.to_owned()));
            };
            let rest = path.strip_prefix(dir).unwrap_or(path);
            Ok(Ok(fs::canonicalize(dir).map_err(fail)?.join(rest)))
        }
    }
}

/// `path` relative to `root` when it is under it, else as it is.
pub(crate) fn display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Writes every file the plan writes or updates, each atomically: a
/// temporary file in the same directory, renamed over the old one, so a
/// reader never sees half a file. A replaced file keeps its permissions.
pub(crate) fn apply(plan: &[FileAction]) -> Result<(), FileError> {
    for file in plan {
        let Some(contents) = &file.contents else {
            continue;
        };
        write_atomically(&file.target, contents.as_bytes())?;
    }
    Ok(())
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), FileError> {
    let fail = |op, path: &Path| {
        let path = path.to_owned();
        move |source| FileError { op, path, source }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(fail("create", dir))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.reef-{}.tmp", std::process::id()));
    let written = fs::write(&tmp, contents)
        .and_then(|()| match fs::metadata(path) {
            Ok(meta) => fs::set_permissions(&tmp, meta.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        })
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        // Best effort: the error being reported is the write's.
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(fail("write", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUIDE: Guide<'static> = Guide {
        short: "# Reef\n\nShort form.\n",
        long: "# Reef\n\nShort form.\n\n## More\n\nLong form.\n",
    };

    fn section() -> String {
        render(Harness::AgentsMd, &GUIDE, "1.2.3")
    }

    #[test]
    fn sections_are_marked_and_name_the_version() {
        let section = section();
        assert!(section.starts_with("<!-- reef:begin v1 -->\n"), "{section}");
        assert!(
            section.ends_with("Short form.\n<!-- reef:end -->"),
            "{section}"
        );
        assert!(section.contains("(reef 1.2.3)"), "{section}");
        assert!(section.contains("`reef guide`"), "{section}");
        assert!(!section.contains("Long form"), "{section}");
        assert_eq!(render(Harness::ClaudeMd, &GUIDE, "1.2.3"), section);
    }

    #[test]
    fn whole_files_have_frontmatter_and_the_long_form() {
        let skill = render(Harness::ClaudeSkill { user: false }, &GUIDE, "1.2.3");
        assert!(skill.starts_with("---\nname: reef\ndescription: Isolate work"));
        assert!(
            skill.contains("\ngenerated-by: reef init\n---\n"),
            "{skill}"
        );
        assert!(skill.ends_with("Long form.\n"), "{skill}");
        assert!(generated_by_reef(&skill));
        let rule = render(Harness::CursorRule, &GUIDE, "1.2.3");
        assert!(rule.contains(&format!("\ndescription: {DESCRIPTION}\n")));
        assert!(rule.contains("\nalwaysApply: false\n"), "{rule}");
        assert!(generated_by_reef(&rule));
        // Plain YAML scalars: nothing that starts a mapping or a comment.
        for text in [&skill, &rule] {
            let frontmatter = text.lines().skip(1).take_while(|line| *line != "---");
            for line in frontmatter {
                let (_, value) = line.split_once(": ").unwrap();
                assert!(!value.contains(": ") && !value.contains(" #"), "{line}");
                assert!(!value.starts_with(['\'', '"', '[', '{', '&', '*', '!']));
            }
        }
    }

    #[test]
    fn splice_creates_a_missing_file() {
        assert_eq!(
            splice(None, &section()),
            Splice::Create(format!("{}\n", section()))
        );
    }

    #[test]
    fn splice_appends_after_a_blank_line() {
        let section = section();
        assert_eq!(
            splice(Some("# Mine\n"), &section),
            Splice::Append(format!("# Mine\n\n{section}\n"))
        );
        // No newline at the end stays that way.
        assert_eq!(
            splice(Some("# Mine"), &section),
            Splice::Append(format!("# Mine\n\n{section}"))
        );
        assert_eq!(
            splice(Some(""), &section),
            Splice::Append(format!("{section}\n"))
        );
    }

    #[test]
    fn splice_replaces_only_between_the_markers() {
        let old = "<!-- reef:begin v1 -->\nold text\n<!-- reef:end -->";
        let file = format!("before\n\n{old}\nafter\n");
        let Splice::Replace(text) = splice(Some(&file), &section()) else {
            panic!("not replaced");
        };
        assert_eq!(text, format!("before\n\n{}\nafter\n", section()));
        assert_eq!(splice(Some(&text), &section()), Splice::Unchanged);
        // An older version is replaced too.
        let older = file.replace("v1", "v0");
        assert!(matches!(
            splice(Some(&older), &section()),
            Splice::Replace(_)
        ));
    }

    #[test]
    fn splice_keeps_crlf() {
        let file = "mine\r\n";
        let Splice::Append(text) = splice(Some(file), &section()) else {
            panic!("not appended");
        };
        assert!(
            text.starts_with("mine\r\n\r\n<!-- reef:begin v1 -->\r\n"),
            "{text:?}"
        );
        assert!(text.ends_with("<!-- reef:end -->\r\n"), "{text:?}");
        assert_eq!(text.matches('\n').count(), text.matches("\r\n").count());
        assert_eq!(splice(Some(&text), &section()), Splice::Unchanged);
        let stale = text.replace("Short form.", "Old form.");
        let Splice::Replace(fixed) = splice(Some(&stale), &section()) else {
            panic!("not replaced");
        };
        assert_eq!(fixed, text);
    }

    #[test]
    fn splice_skips_a_file_that_imports_agents_md() {
        assert_eq!(
            splice(Some("@AGENTS.md\n"), &section()),
            Splice::SkipImports
        );
        assert_eq!(
            splice(Some("# Notes\r\n@./AGENTS.md\r\n"), &section()),
            Splice::SkipImports
        );
        // Not on a line of its own: an ordinary mention.
        assert!(matches!(
            splice(Some("See @AGENTS.md.\n"), &section()),
            Splice::Append(_)
        ));
        // A section from before the import was added is kept current.
        let both = "@AGENTS.md\n\n<!-- reef:begin v1 -->\nold\n<!-- reef:end -->\n";
        let Splice::Replace(text) = splice(Some(both), &section()) else {
            panic!("not replaced");
        };
        assert_eq!(text, format!("@AGENTS.md\n\n{}\n", section()));
    }

    #[test]
    fn splice_sees_markers_after_a_byte_order_mark() {
        let file = "\u{feff}<!-- reef:begin v1 -->\nold\n<!-- reef:end -->\n";
        let Splice::Replace(text) = splice(Some(file), &section()) else {
            panic!("not replaced");
        };
        assert_eq!(text, format!("\u{feff}{}\n", section()));
    }

    #[test]
    fn splice_refuses_damaged_markers() {
        for file in [
            "<!-- reef:end -->\nx\n<!-- reef:begin v1 -->\n",
            "<!-- reef:begin v1 -->\nno end\n",
            "no begin\n<!-- reef:end -->\n",
            "<!-- reef:begin v1 -->\n<!-- reef:end -->\n<!-- reef:begin v1 -->\n<!-- reef:end -->\n",
            "<!-- reef:begin vX -->\n<!-- reef:end -->\n",
        ] {
            assert_eq!(splice(Some(file), &section()), Splice::Damaged, "{file}");
        }
    }

    #[test]
    fn splice_leaves_a_newer_section_alone() {
        let file = "<!-- reef:begin v2 -->\nnewer\n<!-- reef:end -->\n";
        assert_eq!(splice(Some(file), &section()), Splice::Newer(2));
    }

    /// R2.13 test 17's merge cases.
    #[test]
    fn mcp_json_owns_only_reefs_entry() {
        let entry = r#"{"type":"stdio","command":"reef","args":["mcp"]}"#;
        let (action, written) = merge_mcp_json(None);
        assert_eq!(action, Action::Write);
        let written = written.unwrap();
        assert_eq!(
            written,
            "{\n  \"mcpServers\": {\n    \"reef\": {\n      \"type\": \"stdio\",\n      \"command\": \"reef\",\n      \"args\": [\n        \"mcp\"\n      ]\n    }\n  }\n}\n"
        );
        assert_eq!(merge_mcp_json(Some(&written)), (Action::Unchanged, None));
        // Others' keys and servers keep their order and values; Reef's
        // entry goes after the other servers.
        let theirs = r#"{"z": 1.5, "mcpServers": {"other": {"command": "x", "b": true, "a": null}}, "a": [2, 1]}"#;
        let (action, merged) = merge_mcp_json(Some(theirs));
        assert_eq!(action, Action::Update);
        let merged = merged.unwrap();
        let order: Vec<usize> = [
            "\"z\"",
            "\"other\"",
            "\"b\"",
            "\"a\": null",
            "\"reef\"",
            "\"a\": [",
        ]
        .iter()
        .map(|key| {
            merged
                .find(key)
                .unwrap_or_else(|| panic!("{key} in {merged}"))
        })
        .collect();
        assert!(order.is_sorted(), "{merged}");
        assert!(merged.contains("1.5"), "{merged}");
        let (_, crlf) = merge_mcp_json(Some(&theirs.replace(", \"a\"", ",\r\n\"a\"")));
        assert!(crlf.unwrap().contains("\r\n  \"mcpServers\""));
        assert_eq!(
            merge_mcp_json(Some(r#"{"other": 1}"#)).0,
            Action::Update,
            "no mcpServers: added"
        );
        // Changed by someone, or not Reef's to read: kept.
        let changed = r#"{"mcpServers": {"reef": {"command": "reef", "args": ["mcp"], "env": {"REEF_AGENT": "a"}}}}"#;
        assert_eq!(
            merge_mcp_json(Some(changed)).0,
            Action::Skip("mcpServers.reef is not reef init's".to_owned())
        );
        for bad in ["[]", "{", r#"{"mcpServers": []}"#] {
            assert_eq!(
                merge_mcp_json(Some(bad)).0,
                Action::Skip("not a JSON object".to_owned()),
                "{bad}"
            );
        }
        assert_eq!(
            merge_mcp_json(Some(&format!(r#"{{"mcpServers": {{"reef": {entry}}}}}"#))).0,
            Action::Unchanged
        );
    }

    /// A fake home directory: `HOME` only, no agent variables.
    fn home(dir: &Path) -> Home {
        Home::from_env(|name| (name == "HOME").then(|| dir.as_os_str().to_owned())).unwrap()
    }

    #[test]
    fn detection_is_by_files_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let root = root.as_path();
        fs::create_dir(root).unwrap();
        let home = home(&dir.path().join("home"));
        assert_eq!(detect(root, None, &[]), [Harness::AgentsMd]);
        // No agent's directory in the home: `--user` adds nothing.
        assert_eq!(detect(root, Some(&home), &[]), [Harness::AgentsMd]);
        let claude = [&[Harness::AgentsMd], harnesses(Agent::ClaudeCode)].concat();
        fs::write(root.join("CLAUDE.md"), "").unwrap();
        assert_eq!(detect(root, None, &[]), claude);
        // The repository shows Claude Code, so `--user` writes its skill.
        assert_eq!(
            detect(root, Some(&home), &[]),
            [&claude[..], &[Harness::ClaudeSkill { user: true }]].concat()
        );
        fs::remove_file(root.join("CLAUDE.md")).unwrap();
        fs::create_dir(root.join(".claude")).unwrap();
        fs::create_dir(root.join(".cursor")).unwrap();
        assert_eq!(detect(root, None, &[]).last(), Some(&Harness::CursorRule));
        assert_eq!(detect(root, None, &[]).len(), 5);
    }

    /// Test 1: Codex by `.codex/`, opencode by any of its three names, and
    /// the skill they share written once.
    #[test]
    fn codex_and_opencode_are_detected_by_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codex = [&[Harness::AgentsMd], harnesses(Agent::Codex)].concat();
        fs::create_dir(root.join(".codex")).unwrap();
        assert_eq!(detect(root, None, &[]), codex);
        for name in ["opencode.json", "opencode.jsonc", ".opencode"] {
            let other = tempfile::tempdir().unwrap();
            if name.starts_with('.') {
                fs::create_dir(other.path().join(name)).unwrap();
            } else {
                fs::write(other.path().join(name), "{}").unwrap();
            }
            assert_eq!(
                detect(other.path(), None, &[]),
                [&[Harness::AgentsMd], harnesses(Agent::Opencode)].concat(),
                "{name}"
            );
        }
        fs::write(root.join("opencode.json"), "{}").unwrap();
        let both = detect(root, None, &[]);
        assert_eq!(
            both,
            [
                Harness::AgentsMd,
                Harness::AgentSkill { user: false },
                Harness::CodexConfig { user: false },
                Harness::OpencodeJson { user: false },
            ]
        );
        // A file where `.codex/` goes blocks the row, as for the others.
        let blocked = tempfile::tempdir().unwrap();
        fs::write(blocked.path().join(".codex"), "").unwrap();
        assert_eq!(detect(blocked.path(), None, &[]), [Harness::AgentsMd]);
        // `all` lists the new harnesses after the existing ones, once each.
        let all = all();
        let at = |h: Harness| all.iter().position(|&x| x == h).unwrap();
        assert!(at(Harness::McpJson) < at(Harness::AgentSkill { user: false }));
        assert!(at(Harness::CursorRule) < at(Harness::CodexConfig { user: false }));
        assert_eq!(
            all.iter()
                .filter(|&&h| h == Harness::AgentSkill { user: false })
                .count(),
            1
        );
    }

    /// Test 2: `auto --user` writes the user-level files of the agents
    /// whose home directories exist, honoring their variables.
    #[test]
    fn user_level_follows_home() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let home_dir = dir.path().join("home");
        fs::create_dir_all(home_dir.join(".codex")).unwrap();
        let home = home(&home_dir);
        let found = detect(&root, Some(&home), &[]);
        for harness in user_harnesses(Agent::Codex) {
            assert!(found.contains(harness), "{harness:?} in {found:?}");
        }
        assert!(
            !found.contains(&Harness::ClaudeSkill { user: true }),
            "{found:?}"
        );
        assert!(
            !found.contains(&Harness::OpencodeJson { user: true }),
            "{found:?}"
        );
        fs::remove_dir(home_dir.join(".codex")).unwrap();
        fs::create_dir(home_dir.join(".claude")).unwrap();
        let found = detect(&root, Some(&home), &[]);
        assert!(
            found.contains(&Harness::ClaudeSkill { user: true }),
            "{found:?}"
        );
        assert!(
            !found.contains(&Harness::CodexConfig { user: true }),
            "{found:?}"
        );
        // The variables move the agents' directories.
        let codex = dir.path().join("codex-home");
        let config = dir.path().join("xdg");
        fs::create_dir_all(config.join("opencode")).unwrap();
        fs::create_dir(&codex).unwrap();
        let moved = Home::from_env(|name| match name {
            "HOME" => Some(home_dir.clone().into_os_string()),
            "CODEX_HOME" => Some(codex.clone().into_os_string()),
            "XDG_CONFIG_HOME" => Some(config.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let found = detect(&root, Some(&moved), &[]);
        assert!(
            found.contains(&Harness::CodexConfig { user: true }),
            "{found:?}"
        );
        assert!(
            found.contains(&Harness::OpencodeJson { user: true }),
            "{found:?}"
        );
        assert_eq!(
            Harness::CodexConfig { user: true }.path(&root, Some(&moved)),
            Some(codex.join("config.toml"))
        );
        assert_eq!(
            Harness::OpencodeJson { user: true }.path(&root, Some(&moved)),
            Some(config.join("opencode/opencode.json"))
        );
        // A relative variable counts as unset.
        let relative = Home::from_env(|name| match name {
            "HOME" => Some(home_dir.clone().into_os_string()),
            "CODEX_HOME" => Some("rel".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            Harness::CodexConfig { user: true }.path(&root, Some(&relative)),
            Some(home_dir.join(".codex/config.toml"))
        );
        assert_eq!(Home::from_env(|_| None), None);
    }

    /// Test 3.
    #[test]
    fn codex_config_merge() {
        let (action, written) = merge_codex_toml(None);
        assert_eq!(action, Action::Write);
        let written = written.unwrap();
        assert_eq!(
            written,
            "[mcp_servers.reef]\ncommand = \"reef\"\nargs = [\"mcp\"]\n"
        );
        assert_eq!(merge_codex_toml(Some(&written)), (Action::Unchanged, None));
        // Comments, other servers and other tables are kept byte for byte.
        let theirs = "# mine\nmodel = \"o3\" # inline\n\n[mcp_servers.other]\ncommand = \"x\"\n\n[profiles.a]\nmodel = \"b\"\n";
        let (action, merged) = merge_codex_toml(Some(theirs));
        assert_eq!(action, Action::Update);
        let merged = merged.unwrap();
        assert!(merged.starts_with(theirs), "{merged}");
        assert_eq!(merge_codex_toml(Some(&merged)), (Action::Unchanged, None));
        // An explicit `[mcp_servers]`, an inline table, dotted keys and a
        // header each keep the other server.
        for file in [
            "[mcp_servers]\nother = { command = \"x\" }\n",
            "mcp_servers = { other = { command = \"x\" } }\n",
            "mcp_servers.other.command = \"x\"\n",
            "[mcp_servers.other]\ncommand = \"x\"\n",
        ] {
            let (action, merged) = merge_codex_toml(Some(file));
            assert_eq!(action, Action::Update, "{file}");
            let merged = merged.unwrap();
            let doc: toml_edit::DocumentMut = merged.parse().unwrap();
            assert_eq!(
                doc["mcp_servers"]["other"]["command"].as_str(),
                Some("x"),
                "{file} became {merged}"
            );
            assert_eq!(
                merge_codex_toml(Some(&merged)),
                (Action::Unchanged, None),
                "{merged}"
            );
        }
        // Changed by someone: kept, and `--check` does not call it stale.
        let changed = "[mcp_servers.reef]\ncommand = \"reef\"\nargs = [\"mcp\"]\nenv = { REEF_AGENT = \"a\" }\n";
        assert_eq!(
            merge_codex_toml(Some(changed)).0,
            Action::Skip("mcp_servers.reef is not reef init's".to_owned())
        );
        assert_eq!(
            merge_codex_toml(Some("[x")).0,
            Action::Skip("not TOML".to_owned())
        );
        assert_eq!(
            merge_codex_toml(Some("mcp_servers = 1\n")).0,
            Action::Skip("mcp_servers is not a table".to_owned())
        );
        let (_, crlf) = merge_codex_toml(Some("model = \"o3\"\r\n"));
        let crlf = crlf.unwrap();
        assert_eq!(
            crlf.matches('\n').count(),
            crlf.matches("\r\n").count(),
            "{crlf:?}"
        );
        assert_eq!(merge_codex_toml(Some(&crlf)), (Action::Unchanged, None));
    }

    /// Test 4.
    #[test]
    fn opencode_json_merge() {
        let (action, written) = JsonTarget::OPENCODE.merge(None);
        assert_eq!(action, Action::Write);
        let written = written.unwrap();
        assert!(
            written
                .starts_with("{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\""),
            "{written}"
        );
        assert_eq!(
            JsonTarget::OPENCODE.merge(Some(&written)),
            (Action::Unchanged, None)
        );
        let (action, merged) = JsonTarget::OPENCODE.merge(Some(
            r#"{"model": "x", "mcp": {"other": {"type": "remote"}}}"#,
        ));
        assert_eq!(action, Action::Update);
        let merged = merged.unwrap();
        assert!(merged.find("\"other\"").unwrap() < merged.find("\"reef\"").unwrap());
        assert!(
            !merged.contains("$schema"),
            "an existing file gains no $schema"
        );
        assert_eq!(
            JsonTarget::OPENCODE
                .merge(Some(
                    r#"{"mcp": {"reef": {"type": "local", "command": ["reef"]}}}"#
                ))
                .0,
            Action::Skip("mcp.reef is not reef init's".to_owned())
        );
        // `opencode.jsonc` is not rewritten; the reason carries the entry.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("opencode.jsonc"), "// mine\n{}\n").unwrap();
        let plan = plan(
            &root,
            None,
            &[Harness::OpencodeJson { user: false }],
            &GUIDE,
            "1",
        )
        .unwrap();
        let Action::Skip(why) = &plan[0].action else {
            panic!("{plan:?}");
        };
        assert!(
            why.starts_with("opencode.jsonc: comments are not rewritten"),
            "{why}"
        );
        assert!(
            why.contains(
                r#"{"mcp":{"reef":{"type":"local","command":["reef","mcp"],"enabled":true}}}"#
            ),
            "{why}"
        );
    }

    /// Test 5.
    #[test]
    fn agent_skill_follows_the_standard() {
        let skill = render(Harness::AgentSkill { user: false }, &GUIDE, "1.2.3");
        let frontmatter: Vec<&str> = skill
            .lines()
            .skip(1)
            .take_while(|line| *line != "---")
            .collect();
        assert_eq!(frontmatter[0], "name: reef");
        assert_eq!(frontmatter[1], format!("description: {DESCRIPTION}"));
        assert_eq!(frontmatter[2..], ["metadata:", "  generated-by: reef init"]);
        assert!(DESCRIPTION.len() <= 1024);
        assert!(skill.ends_with("Long form.\n"), "{skill}");
        assert!(generated_by_reef(&skill));
        // The older, top-level form is Reef's too.
        assert!(generated_by_reef(&render(
            Harness::ClaudeSkill { user: false },
            &GUIDE,
            "1"
        )));
        assert_eq!(whole_file(Some(&skill), &skill).0, Action::Unchanged);
    }

    /// Test 6.
    #[test]
    fn mcp_shapes_agree() {
        for shape in [
            McpShape::McpJson,
            McpShape::Opencode,
            McpShape::CodexToml,
            McpShape::Mcpb,
        ] {
            let entry = render_mcp_entry(shape);
            let words: Vec<&str> = match &entry["command"] {
                serde_json::Value::Array(words) => {
                    words.iter().map(|word| word.as_str().unwrap()).collect()
                }
                command => std::iter::once(command.as_str().unwrap())
                    .chain(
                        entry["args"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|a| a.as_str().unwrap()),
                    )
                    .collect(),
            };
            let program = if shape == McpShape::Mcpb {
                "${__dirname}/reef"
            } else {
                "reef"
            };
            assert_eq!(words, [program, "mcp"], "{shape:?}");
        }
        // The TOML form parses, and reads back as the same entry.
        let toml = render(Harness::CodexConfig { user: false }, &GUIDE, "1");
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        assert_eq!(
            codex_entry(&doc["mcp_servers"]["reef"]),
            Some(render_mcp_entry(McpShape::CodexToml))
        );
    }

    #[test]
    fn an_agents_directory_counts_only_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".cursor"), "").unwrap();
        fs::write(root.join(".claude"), "").unwrap();
        assert_eq!(detect(root, None, &[]), [Harness::AgentsMd]);
        fs::remove_file(root.join(".cursor")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join(".cursor")).unwrap();
        assert_eq!(detect(root, None, &[]), [Harness::AgentsMd]);
        assert_eq!(all()[0], Harness::AgentsMd);
        let mut unique: Vec<Harness> = Vec::new();
        for sign in SIGNS {
            push_unique(&mut unique, sign.harnesses);
        }
        assert_eq!(all().len(), 1 + unique.len());
        assert_eq!(harnesses(Agent::Cursor), [Harness::CursorRule]);
    }

    /// Every agent, by a match that stops compiling when one is added.
    fn every_agent() -> [Agent; 4] {
        let every = [
            Agent::ClaudeCode,
            Agent::Cursor,
            Agent::Codex,
            Agent::Opencode,
        ];
        for agent in every {
            match agent {
                Agent::ClaudeCode | Agent::Cursor | Agent::Codex | Agent::Opencode => {}
            }
        }
        every
    }

    #[test]
    fn every_agent_has_one_row() {
        for agent in every_agent() {
            let rows = SIGNS.iter().filter(|sign| sign.agent == agent).count();
            assert_eq!(rows, 1, "{agent:?}");
            assert!(!harnesses(agent).is_empty(), "{agent:?}");
        }
    }

    #[test]
    fn a_running_agent_is_blocked_by_a_file_where_its_directory_goes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".claude"), "").unwrap();
        assert_eq!(
            detect(root, None, &[Agent::ClaudeCode]),
            [Harness::AgentsMd]
        );
        fs::remove_file(root.join(".claude")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join(".claude")).unwrap();
        assert_eq!(
            detect(root, None, &[Agent::ClaudeCode]),
            [Harness::AgentsMd]
        );
    }

    #[test]
    fn a_running_agent_counts_as_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let claude = [&[Harness::AgentsMd], harnesses(Agent::ClaudeCode)].concat();
        assert_eq!(detect(dir.path(), None, &[Agent::ClaudeCode]), claude);
        assert_eq!(
            detect(dir.path(), None, &[Agent::Cursor]),
            [Harness::AgentsMd, Harness::CursorRule]
        );
        for sign in SIGNS {
            let Some((name, value)) = sign.env else {
                continue;
            };
            let set =
                |set: &'static str| move |asked: &str| (asked == name).then(|| OsString::from(set));
            assert_eq!(running(set(value)), [sign.agent], "{name}");
            for other in ["", "0", "true"] {
                assert_eq!(running(set(other)), [], "{name}={other:?}");
            }
        }
        assert_eq!(running(|_| None), []);
    }

    #[test]
    fn plan_follows_a_link_and_skips_the_second_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("AGENTS.md"), "mine\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        let plan = plan(
            &root,
            None,
            &[Harness::ClaudeMd, Harness::AgentsMd],
            &GUIDE,
            "1",
        )
        .unwrap();
        assert_eq!(plan[0].path, root.join("CLAUDE.md"));
        assert_eq!(plan[0].target, root.join("AGENTS.md"));
        assert_eq!(plan[0].action, Action::Update);
        assert_eq!(
            plan[1].action,
            Action::Skip("same file as CLAUDE.md".to_owned())
        );
        apply(&plan).unwrap();
        assert!(
            fs::symlink_metadata(root.join("CLAUDE.md"))
                .unwrap()
                .is_symlink()
        );
        let text = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(text.starts_with("mine\n\n<!-- reef:begin v1 -->"), "{text}");
    }

    #[test]
    fn plan_does_not_follow_a_link_out_of_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(base.join("shared.md"), "shared\n").unwrap();
        std::os::unix::fs::symlink("../shared.md", root.join("AGENTS.md")).unwrap();
        let plan = plan(&root, None, &[Harness::AgentsMd], &GUIDE, "1").unwrap();
        assert_eq!(
            plan[0].action,
            Action::Skip("symbolic link out of the repository".to_owned())
        );
        apply(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(base.join("shared.md")).unwrap(),
            "shared\n"
        );
        // A linked directory is a way out too.
        fs::create_dir(base.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", root.join(".cursor")).unwrap();
        let linked = super::plan(&root, None, &[Harness::CursorRule], &GUIDE, "1").unwrap();
        assert_eq!(
            linked[0].action,
            Action::Skip("symbolic link out of the repository".to_owned())
        );
    }

    #[test]
    fn a_dangling_claude_md_link_counts_as_claude() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("AGENTS.md", dir.path().join("CLAUDE.md")).unwrap();
        assert!(detect(dir.path(), None, &[]).contains(&Harness::ClaudeMd));
    }

    #[test]
    fn plan_without_home_skips_the_user_skill() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(
            dir.path(),
            None,
            &[Harness::ClaudeSkill { user: true }],
            &GUIDE,
            "1",
        )
        .unwrap();
        assert_eq!(plan[0].action, Action::Skip("HOME is not set".to_owned()));
    }

    #[test]
    fn whole_files_are_replaced_only_when_reef_wrote_them() {
        let rendered = render(Harness::CursorRule, &GUIDE, "2");
        assert_eq!(whole_file(None, &rendered).0, Action::Write);
        assert_eq!(whole_file(Some(&rendered), &rendered).0, Action::Unchanged);
        let old = render(Harness::CursorRule, &GUIDE, "1");
        assert_eq!(whole_file(Some(&old), &rendered).0, Action::Update);
        let crlf = rendered.replace('\n', "\r\n");
        assert_eq!(whole_file(Some(&crlf), &rendered).0, Action::Unchanged);
        let foreign = "---\ndescription: mine\n---\n\ngenerated-by: reef init\n";
        assert_eq!(
            whole_file(Some(foreign), &rendered).0,
            Action::Skip("not written by reef init".to_owned())
        );
    }

    #[test]
    fn apply_keeps_permissions_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let nested = dir.path().join("a/b/SKILL.md");
        apply(&[
            FileAction {
                path: path.clone(),
                target: path.clone(),
                action: Action::Update,
                contents: Some("new\n".to_owned()),
            },
            FileAction {
                path: nested.clone(),
                target: nested.clone(),
                action: Action::Write,
                contents: Some("skill\n".to_owned()),
            },
        ])
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(&nested).unwrap(), "skill\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
