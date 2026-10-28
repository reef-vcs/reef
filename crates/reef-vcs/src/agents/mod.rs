//! The files `reef init` writes for coding agents (R1.27): a managed
//! section in `AGENTS.md` and `CLAUDE.md`, and whole files Reef owns (the
//! Claude Code skill, repository and user level, and the Cursor rule), all
//! from the agent guide (`docs/agent-guide.md`, R1.26), so there is one
//! guide and it cannot drift; and Reef's entry in `.mcp.json` (R2.13),
//! which starts `reef mcp`.
//!
//! Everything here but [`apply`] is a pure function of the guide and what
//! is on disk. Reef never overwrites text it did not write: a section lives
//! between [`BEGIN`] and [`END`] marker lines and the bytes around it are
//! kept as they are; a whole file is replaced only when its frontmatter has
//! [`GENERATED_BY`]; in `.mcp.json`, Reef owns only `mcpServers.reef`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

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
}

impl Harness {
    /// Where the file is: under `root`, or `home` for the user-level skill.
    /// None when that needs a home directory and there is none.
    pub(crate) fn path(self, root: &Path, home: Option<&Path>) -> Option<PathBuf> {
        let skill = Path::new(".claude/skills/reef/SKILL.md");
        Some(match self {
            Harness::AgentsMd => root.join("AGENTS.md"),
            Harness::ClaudeMd => root.join("CLAUDE.md"),
            Harness::ClaudeSkill { user: false } => root.join(skill),
            Harness::ClaudeSkill { user: true } => home?.join(skill),
            Harness::CursorRule => root.join(".cursor/rules/reef.mdc"),
            Harness::McpJson => root.join(".mcp.json"),
        })
    }

    /// Whether Reef owns the whole file, rather than a section of it.
    fn is_whole_file(self) -> bool {
        matches!(self, Harness::ClaudeSkill { .. } | Harness::CursorRule)
    }
}

/// The one harness whose file is outside the repository.
const USER_SKILL: Harness = Harness::ClaudeSkill { user: true };

/// A coding agent that reads files of its own besides `AGENTS.md`, which
/// every agent gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agent {
    /// `CLAUDE.md`, the Claude Code skill, and `.mcp.json`.
    ClaudeCode,
    /// The Cursor rule.
    Cursor,
}

/// How `auto` recognizes an agent, and what it writes for it.
struct Sign {
    /// The agent this row is for.
    agent: Agent,
    /// Names at the root whose presence means the repository uses it; one
    /// ending in `/` counts only as a directory.
    names: &'static [&'static str],
    /// A variable the agent sets, to this value, in the processes it
    /// starts, so `reef init` run by the agent writes its files though the
    /// repository has none yet. Only a variable the agent documents.
    env: Option<(&'static str, &'static str)>,
    /// What Reef writes for it in the repository.
    harnesses: &'static [Harness],
}

/// Every agent Reef writes files for besides `AGENTS.md`. Recognizing
/// another agent is a row here; writing for a new one also needs its
/// [`Harness`] variants and an `--agents` value.
const SIGNS: &[Sign] = &[
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
    },
    Sign {
        agent: Agent::Cursor,
        names: &[".cursor/"],
        env: None,
        harnesses: &[Harness::CursorRule],
    },
];

/// The harnesses `auto` writes: `AGENTS.md` always, then each agent's when
/// the repository has its files at `root` or the agent is `running`
/// `reef init`. `user` (the user-level skill) turns Claude Code's on.
pub(crate) fn detect(root: &Path, user: bool, running: &[Agent]) -> Vec<Harness> {
    let mut found = vec![Harness::AgentsMd];
    for sign in SIGNS {
        let present = sign.names.iter().any(|name| match name.strip_suffix('/') {
            Some(dir) => root.join(dir).is_dir(),
            // `symlink_metadata`: a `CLAUDE.md` linked to a missing
            // `AGENTS.md` still counts.
            None => fs::symlink_metadata(root.join(name)).is_ok(),
        });
        let user = user && sign.agent == Agent::ClaudeCode;
        if present || user || running.contains(&sign.agent) {
            found.extend_from_slice(sign.harnesses);
        }
        if user {
            found.push(USER_SKILL);
        }
    }
    found
}

/// What Reef writes for `agent` in the repository.
pub(crate) fn harnesses(agent: Agent) -> &'static [Harness] {
    SIGNS
        .iter()
        .find(|sign| sign.agent == agent)
        .map_or(&[], |sign| sign.harnesses)
}

/// Every harness in the repository: `AGENTS.md`, then each agent's.
pub(crate) fn all() -> Vec<Harness> {
    let mut all = vec![Harness::AgentsMd];
    for sign in SIGNS {
        all.extend_from_slice(sign.harnesses);
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
    let frontmatter = match harness {
        Harness::AgentsMd | Harness::ClaudeMd => {
            return format!(
                "{BEGIN}{SECTION_VERSION} -->\n{preface}\n\n{}\n{END}",
                guide.short.trim_end()
            );
        }
        Harness::ClaudeSkill { .. } => format!("name: reef\ndescription: {DESCRIPTION}"),
        Harness::CursorRule => format!("description: {DESCRIPTION}\nalwaysApply: false"),
        Harness::McpJson => return mcp_json(None),
    };
    format!(
        "---\n{frontmatter}\n{GENERATED_BY}\n---\n\n{preface}\n\n{}\n",
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
/// user-level skill goes; a harness that needs it is skipped without one.
pub(crate) fn plan(
    root: &Path,
    home: Option<&Path>,
    harnesses: &[Harness],
    guide: &Guide<'_>,
    version: &str,
) -> Result<Vec<FileAction>, FileError> {
    let mut plan: Vec<FileAction> = Vec::new();
    for &harness in harnesses {
        let Some(path) = harness.path(root, home) else {
            plan.push(FileAction {
                path: PathBuf::from("~/.claude/skills/reef/SKILL.md"),
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
            Ok(target) if harness != USER_SKILL && !target.starts_with(root) => {
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

/// The key of Reef's server under `mcpServers`.
const MCP_SERVER: &str = "reef";

/// Reef's server entry: `reef mcp` from `PATH`, as every installer puts it
/// there. No absolute path, which would bake one machine's layout into a
/// committed file; no `env`, since `REEF_AGENT` is the harness's to set;
/// and no version, so a new `reef` does not make the file stale.
pub(crate) fn render_mcp_entry() -> serde_json::Value {
    serde_json::json!({"type": "stdio", "command": "reef", "args": ["mcp"]})
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

/// `.mcp.json` as Reef writes it: `existing` with Reef's entry added, two-
/// space JSON with a final newline; a new file with only the entry when
/// `existing` is `None`.
fn mcp_json(existing: Option<IndexMap<String, Json>>) -> String {
    let mut file = existing.unwrap_or_default();
    // In the order a person writes it; `render_mcp_entry` is for comparing.
    let text = |text: &str| Json::String(text.to_owned());
    let entry = Json::Object(IndexMap::from([
        ("type".to_owned(), text("stdio")),
        ("command".to_owned(), text("reef")),
        ("args".to_owned(), Json::Array(vec![text("mcp")])),
    ]));
    match file
        .entry("mcpServers".to_owned())
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

/// The action for `.mcp.json`: Reef owns `mcpServers.reef` and nothing
/// else. A missing (or blank) file is written with only Reef's entry; a
/// file without it gets it added after the other servers, everything else
/// kept in order; an entry that differs from Reef's was changed by someone
/// (an `--agent`, an `env`) and is kept, as is a file that is not a JSON
/// object. A CRLF file stays CRLF.
fn merge_mcp_json(existing: Option<&str>) -> (Action, Option<String>) {
    let Some(text) = existing.filter(|text| !text.trim().is_empty()) else {
        return (Action::Write, Some(mcp_json(None)));
    };
    let not_object = || (Action::Skip("not a JSON object".to_owned()), None);
    let Ok(Json::Object(file)) = serde_json::from_str::<Json>(text) else {
        return not_object();
    };
    match file.get("mcpServers") {
        None => {}
        Some(Json::Object(servers)) => {
            if let Some(entry) = servers.get(MCP_SERVER) {
                let entry = serde_json::to_value(entry).unwrap_or_default();
                return if entry == render_mcp_entry() {
                    (Action::Unchanged, None)
                } else {
                    (
                        Action::Skip(format!("mcpServers.{MCP_SERVER} is not reef init's")),
                        None,
                    )
                };
            }
        }
        Some(_) => return not_object(),
    }
    let merged = mcp_json(Some(file));
    let merged = if existing.is_some_and(|text| text.contains("\r\n")) {
        merged.replace('\n', "\r\n")
    } else {
        merged
    };
    (Action::Update, Some(merged))
}

/// Whether `text` opens with YAML frontmatter that has the
/// [`GENERATED_BY`] line.
fn generated_by_reef(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_end);
    lines.next() == Some("---")
        && lines
            .take_while(|line| *line != "---")
            .any(|line| line == GENERATED_BY)
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

    #[test]
    fn detection_is_by_files_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(detect(root, false, &[]), [Harness::AgentsMd]);
        let claude = [
            Harness::AgentsMd,
            Harness::ClaudeMd,
            Harness::ClaudeSkill { user: false },
            Harness::McpJson,
        ];
        assert_eq!(detect(root, true, &[])[..4], claude);
        assert_eq!(detect(root, true, &[]).len(), 5);
        assert_eq!(
            detect(root, true, &[])[4],
            Harness::ClaudeSkill { user: true }
        );
        fs::write(root.join("CLAUDE.md"), "").unwrap();
        assert_eq!(detect(root, false, &[]), claude);
        fs::remove_file(root.join("CLAUDE.md")).unwrap();
        fs::create_dir(root.join(".claude")).unwrap();
        fs::create_dir(root.join(".cursor")).unwrap();
        assert_eq!(detect(root, false, &[]).last(), Some(&Harness::CursorRule));
        assert_eq!(detect(root, false, &[]).len(), 5);
    }

    #[test]
    fn an_agents_directory_counts_only_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".cursor"), "").unwrap();
        fs::write(root.join(".claude"), "").unwrap();
        assert_eq!(detect(root, false, &[]), [Harness::AgentsMd]);
        fs::remove_file(root.join(".cursor")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join(".cursor")).unwrap();
        assert_eq!(detect(root, false, &[]), [Harness::AgentsMd]);
        assert_eq!(all()[0], Harness::AgentsMd);
        assert_eq!(
            all().len(),
            1 + SIGNS.iter().map(|s| s.harnesses.len()).sum::<usize>()
        );
        assert_eq!(harnesses(Agent::Cursor), [Harness::CursorRule]);
    }

    #[test]
    fn a_running_agent_counts_as_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let claude = [
            Harness::AgentsMd,
            Harness::ClaudeMd,
            Harness::ClaudeSkill { user: false },
            Harness::McpJson,
        ];
        assert_eq!(detect(dir.path(), false, &[Agent::ClaudeCode]), claude);
        assert_eq!(
            detect(dir.path(), false, &[Agent::Cursor]),
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
        assert!(detect(dir.path(), false, &[]).contains(&Harness::ClaudeMd));
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
