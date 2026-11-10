//! Reef's harness packages (R2.25, ADR 0052): the plugin trees and their
//! marketplaces, the MCPB bundle's manifest and the MCP registry's
//! `server.json`, rendered from the same guide and entries as `reef init`'s
//! files and committed. This repository is the marketplace, each entry
//! pinned to the release's tag, so a harness installs what the released
//! `reef` supports. A test checks the committed files against [`files`];
//! `just packages` writes them.

use std::path::PathBuf;

use indexmap::IndexMap;

use super::{
    Agent, Guide, Harness, HookEvent, HookFacts, Json, McpShape, Package, SIGNS, mcp_entry,
    render_with,
};

impl Package {
    /// The tree's directory, named by format, not by harness: another
    /// harness that adopts the format installs the same tree.
    fn dir(self) -> &'static str {
        match self {
            Package::ClaudeCode => "plugins/claude-code",
            Package::AgentPlugins => "plugins/agent-plugins",
        }
    }
}

/// The repository, as the marketplaces, manifests and listing name it.
const REPO: &str = "reef-vcs/reef";

/// Its URL.
const REPO_URL: &str = "https://github.com/reef-vcs/reef";

/// What both marketplaces clone a plugin's tree from. Claude Code clones the
/// `owner/repo` shorthand over SSH, which fails without a GitHub key (0.3.0
/// and 0.4.0's install line, in a container or on CI); https needs none.
const CLONE_URL: &str = "https://github.com/reef-vcs/reef.git";

/// The plugins' and the bundle's description.
const PLUGIN_DESCRIPTION: &str = "Reef workspaces for coding agents: the skill that starts work \
     in an isolated workspace, and Reef's MCP server.";

/// The plugins' keywords.
const PLUGIN_KEYWORDS: &[&str] = &["version-control", "workspaces", "jj", "mcp"];
const WORKSPACES_KEYWORDS: &[&str] = &["version-control", "workspaces", "worktrees", "jj"];

/// The registry listing's description, at most 100 characters.
const SERVER_DESCRIPTION: &str =
    "Version control for fleets of coding agents: isolated workspaces, status, diff and history.";

/// The registry listing's name, in the namespace GitHub OIDC grants the
/// organization.
const SERVER_NAME: &str = "io.github.reef-vcs/reef";

/// Claude Code's hook facts, from its row in the agent table: the
/// commands below know none of them.
fn claude_code() -> &'static HookFacts {
    SIGNS
        .iter()
        .find(|sign| sign.agent == Agent::ClaudeCode)
        .and_then(|sign| sign.hooks.as_ref())
        .expect("Claude Code's row has its hook facts")
}

/// A hook command: `reef presence hook ...`, run only when `reef` is on
/// `PATH`, never failing the tool call it runs before, and with its output
/// discarded, so a `REEF_LOG` in the session's environment cannot reach
/// Claude Code, which reads a hook's stdout as a decision.
fn hook_command(args: &str) -> String {
    format!("command -v reef >/dev/null 2>&1 && reef presence hook {args} >/dev/null 2>&1 || true")
}

/// The opt-in plugin that makes Claude Code's worktrees Reef workspaces
/// (R2.26, ADR 0053): a tree of its own, since it changes what a session's
/// isolation is.
const WORKSPACES_DIR: &str = "plugins/claude-code-workspaces";
const WORKSPACES_NAME: &str = "reef-workspaces";
const WORKSPACES_DESCRIPTION: &str = "Reef workspaces in place of git worktrees: each worktree \
     session and subagent gets a copy-on-write Reef workspace, held while the session runs.";

/// `--at` for each pair.
fn at(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(field, pointer)| format!("--at {field}={pointer}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The provider hooks' commands: create, release, re-hold. Create and
/// release fail loudly, so a harness without `reef` never falls back to
/// git unnoticed. Re-hold runs on every prompt: it never fails (exit 2
/// would block the prompt), prints nothing (its stdout becomes context),
/// starts no daemon, and is a lookup that finds nothing outside a
/// workspace and keeps the follower of a workspace already held.
fn provider_commands() -> [String; 3] {
    let [create, release, rehold] = claude_code().provider;
    [
        format!(
            "reef ws provider create {} --hold-while parent -o text",
            at(create.at)
        ),
        // For this session: kept while another session holds it.
        format!(
            "reef ws provider release {} --hold-while parent",
            at(release.at)
        ),
        format!(
            "reef --no-autostart ws provider attach {} --hold-while parent >/dev/null 2>&1 || true",
            at(rehold.at)
        ),
    ]
}

/// A declaring hook's command (R3.44): `reef provenance declare --hook`,
/// which never fails, prints nothing and starts no daemon, guarded as the
/// presence hooks are.
fn declare_command(event: &HookEvent) -> String {
    format!(
        "command -v reef >/dev/null 2>&1 && reef provenance declare --hook {} >/dev/null 2>&1 || true",
        at(event.at)
    )
}

/// An instruction hook's command (R3.12): `reef comment next --hook`, which
/// never fails, starts no daemon, and prints only an instruction it took,
/// in `emit`'s form, so its stdout is kept.
fn instruct_command(event: &HookEvent, emit: crate::cli::comment::Emit) -> String {
    let form = match emit {
        crate::cli::comment::Emit::Context => "context",
        crate::cli::comment::Emit::Block => "block",
    };
    format!(
        "command -v reef >/dev/null 2>&1 && reef comment next --hook {} --emit {form} 2>/dev/null || true",
        at(event.at)
    )
}

/// The hook commands in `hooks.json`, as a harness runs them, for tests:
/// the presence hooks', then each declaring event's, then each
/// instruction event's.
fn hook_commands() -> Vec<String> {
    let facts = claude_code();
    let paths: Vec<String> = facts
        .edit
        .2
        .iter()
        .map(|pointer| format!("--path-at {pointer}"))
        .collect();
    let mut commands = vec![
        hook_command(&format!(
            "edit {} --session-at {}",
            paths.join(" "),
            facts.session
        )),
        hook_command(&format!("end --session-at {}", facts.session)),
    ];
    commands.extend(facts.declare.iter().map(declare_command));
    commands.extend(
        facts
            .instruct
            .iter()
            .map(|(event, emit)| instruct_command(event, *emit)),
    );
    commands
}

/// Every file Reef's packages are, relative to the repository's root, with
/// its contents, for `version`.
fn files(guide: &Guide<'_>, version: &str) -> Vec<(PathBuf, String)> {
    let preface =
        format!("Part of the Reef plugin (reef {version}); `reef guide` prints the current guide.");
    let mut files = Vec::new();
    let mut add = |path: &str, contents: String| files.push((PathBuf::from(path), contents));
    for sign in SIGNS {
        let Some(package) = sign.package else {
            continue;
        };
        let dir = package.dir();
        add(&format!("{dir}/README.md"), readme(package, sign.install));
        match package {
            Package::ClaudeCode => {
                add(
                    &format!("{dir}/.claude-plugin/plugin.json"),
                    pretty(&plugin_json(
                        "reef",
                        PLUGIN_DESCRIPTION,
                        PLUGIN_KEYWORDS,
                        version,
                        None,
                    )),
                );
                add(
                    &format!("{dir}/skills/reef/SKILL.md"),
                    render_with(Harness::ClaudeSkill { user: false }, guide, &preface),
                );
                add(
                    &format!("{dir}/.mcp.json"),
                    pretty(&servers(None, McpShape::McpJson)),
                );
                add(&format!("{dir}/hooks/hooks.json"), pretty(&hooks_json()));
                add(&format!("{WORKSPACES_DIR}/README.md"), workspaces_readme());
                add(
                    &format!("{WORKSPACES_DIR}/.claude-plugin/plugin.json"),
                    pretty(&plugin_json(
                        WORKSPACES_NAME,
                        WORKSPACES_DESCRIPTION,
                        WORKSPACES_KEYWORDS,
                        version,
                        None,
                    )),
                );
                add(
                    &format!("{WORKSPACES_DIR}/hooks/hooks.json"),
                    pretty(&provider_hooks_json()),
                );
            }
            Package::AgentPlugins => {
                add(
                    &format!("{dir}/plugin.json"),
                    pretty(&plugin_json(
                        "reef",
                        PLUGIN_DESCRIPTION,
                        PLUGIN_KEYWORDS,
                        version,
                        Some("https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"),
                    )),
                );
                add(
                    &format!("{dir}/skills/reef/SKILL.md"),
                    render_with(Harness::AgentSkill { user: false }, guide, &preface),
                );
                // Codex loads the servers only when the file names its
                // schema, which the standard requires.
                add(
                    &format!("{dir}/mcp.json"),
                    pretty(&servers(
                        Some("https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"),
                        McpShape::McpJson,
                    )),
                );
            }
        }
    }
    add(
        ".claude-plugin/marketplace.json",
        pretty(&claude_marketplace(version)),
    );
    add(
        ".agents/plugins/marketplace.json",
        pretty(&agents_marketplace(version)),
    );
    add(
        "packaging/mcpb/manifest.json",
        pretty(&mcpb_manifest(version)),
    );
    add("packaging/mcp/server.json", pretty(&server_json(version)));
    files
}

/// A plugin's README, which its listing and the people who install it
/// read.
fn readme(package: Package, install: &str) -> String {
    let hooks = match package {
        Package::ClaudeCode => {
            " Its hooks declare the files the agent is about to edit, so `reef presence watch` \
             and the web UI show them; and each prompt's SHA-256 (never its text), the session and \
             the model, as the provenance of the work done in Reef workspaces (`reef provenance \
             show`)."
        }
        Package::AgentPlugins => "",
    };
    format!(
        "# Reef\n\n\
         Reef gives each coding agent its own workspace: a copy-on-write copy of a git or jj \
         repository, leased to one process and snapshotted after every write. This plugin adds \
         Reef's skill, which starts work in a workspace, and Reef's MCP server (`reef mcp`), \
         which answers status, diff and history questions and edits files without a \
         directory.{hooks}\n\n\
         It runs the `reef` on your `PATH`; install Reef first ({REPO_URL}#install). Then:\n\n\
         ```sh\n{install}\n```\n\n\
         Generated from Reef's agent guide by `just packages`; `reef guide` prints the guide.\n"
    )
}

/// The workspaces plugin's README: what changes for a session, said
/// before how to install it.
fn workspaces_readme() -> String {
    format!(
        "# Reef workspaces\n\n\
         This plugin makes every worktree Claude Code creates (`claude --worktree`, the \
         `EnterWorktree` tool, subagents with `isolation: worktree`) a Reef workspace instead: a \
         copy-on-write copy of the repository on its own jj change, snapshotted as it is \
         written, held while the session runs and reclaimed with a final snapshot after it ends. \
         It calls `reef ws provider` ({REPO_URL}/blob/main/docs/workspace-provider.md) from \
         Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks, and holds a resumed session's \
         workspace again from its first prompt (`UserPromptSubmit`).\n\n\
         What changes:\n\n\
         - A workspace has no `.git`: `git status` in it fails, and Claude Code's check for \
         uncommitted changes finds none, so it removes an unnamed session's worktree without \
         asking. Nothing is lost: the release snapshots first.\n\
         - Work lands as a jj change (`reef log`), not a branch.\n\
         - `.worktreeinclude` is not applied; untracked files stay in your checkout.\n\
         - A workspace is held while a session that uses it runs, and reclaimed with a final \
         snapshot when its lease runs out after that (35 minutes by default). Subagents' \
         workspaces last until the session ends. A background session the supervisor stops \
         loses its hold the same way; a name used again later starts on the change its last \
         workspace left.\n\
         - The first worktree in a repository registers it with Reef, which initializes jj \
         beside git there (`reef init` does the same, and says so).\n\
         - The repository is the one the session is in when it asks for the worktree, which \
         follows `cd`.\n\n\
         Requires Claude Code 2.1.300 or later. A plugin's `WorktreeCreate` hook does not run \
         for `claude --worktree --tmux`, which then makes a git worktree; to use Reef there too, \
         put this plugin's `hooks/hooks.json` entries in a settings file \
         (`~/.claude/settings.json`). Claude Code refuses a worktree inside a git repository, \
         so if your home directory is one, set `REEF_STATE_DIR` to a directory outside it.\n\n\
         It runs the `reef` on your `PATH`; install Reef first ({REPO_URL}#install). Then:\n\n\
         ```sh\nclaude plugin install {WORKSPACES_NAME} --marketplace {REPO}\n```\n\n\
         Generated by `just packages` from Reef's agent table.\n"
    )
}

/// Claude Code's worktree hooks over `reef ws provider`. No timeout on
/// create or remove: a first workspace of a large repository builds its
/// base, inside the harness's default.
fn provider_hooks_json() -> Json {
    let [create, release, rehold] = provider_commands();
    let [create_event, release_event, rehold_event] = claude_code().provider;
    let hook = |command: &str, timeout: Option<u64>, status: Option<&str>| {
        let mut pairs = vec![("type", text("command")), ("command", text(command))];
        if let Some(timeout) = timeout {
            pairs.push(("timeout", Json::Number(timeout.into())));
        }
        if let Some(status) = status {
            pairs.push(("statusMessage", text(status)));
        }
        Json::Array(vec![object(pairs)])
    };
    object(vec![(
        "hooks",
        object(vec![
            (
                create_event.name,
                Json::Array(vec![object(vec![(
                    "hooks",
                    hook(&create, None, Some("Creating a Reef workspace")),
                )])]),
            ),
            (
                release_event.name,
                Json::Array(vec![object(vec![("hooks", hook(&release, None, None))])]),
            ),
            (
                rehold_event.name,
                Json::Array(vec![object(vec![("hooks", hook(&rehold, Some(10), None))])]),
            ),
        ]),
    )])
}

/// The tag a release is published at, which the marketplaces pin.
fn tag(version: &str) -> String {
    format!("v{version}")
}

fn text(text: &str) -> Json {
    Json::String(text.to_owned())
}

fn object(pairs: Vec<(&str, Json)>) -> Json {
    Json::Object(
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<IndexMap<_, _>>(),
    )
}

/// Two-space JSON with a final newline, keys in the order given.
fn pretty(json: &Json) -> String {
    let mut text = serde_json::to_string_pretty(json).unwrap_or_default();
    text.push('\n');
    text
}

fn author() -> Json {
    object(vec![
        ("name", text("Reef contributors")),
        ("url", text(REPO_URL)),
    ])
}

/// A plugin's manifest; `schema` is the Agent Plugins one, whose
/// `repository` is a string and which allows no other keys.
fn plugin_json(
    name: &str,
    description: &str,
    keywords: &[&str],
    version: &str,
    schema: Option<&str>,
) -> Json {
    let mut pairs = Vec::new();
    if let Some(schema) = schema {
        pairs.push(("$schema", text(schema)));
    }
    pairs.extend([
        ("name", text(name)),
        ("version", text(version)),
        ("description", text(description)),
        ("author", author()),
        ("homepage", text(REPO_URL)),
        ("repository", text(REPO_URL)),
        ("license", text("Apache-2.0")),
        (
            "keywords",
            Json::Array(keywords.iter().copied().map(text).collect()),
        ),
    ]);
    object(pairs)
}

/// An MCP configuration file with Reef's server alone.
fn servers(schema: Option<&str>, shape: McpShape) -> Json {
    let mut pairs = Vec::new();
    if let Some(schema) = schema {
        pairs.push(("$schema", text(schema)));
    }
    pairs.push(("mcpServers", object(vec![("reef", mcp_entry(shape))])));
    object(pairs)
}

/// Claude Code's hooks: before an edit, declare it; at the session's end,
/// clear the declaration; and on each declaring event (a prompt, a
/// session's start, a model switch), declare its provenance (R3.44).
fn hooks_json() -> Json {
    let facts = claude_code();
    let commands = hook_commands();
    let hook = |command: &str| {
        Json::Array(vec![object(vec![
            ("type", text("command")),
            ("command", text(command)),
            ("timeout", Json::Number(5.into())),
        ])])
    };
    let mut events = vec![
        (
            facts.edit.0,
            Json::Array(vec![object(vec![
                ("matcher", text(facts.edit.1)),
                ("hooks", hook(&commands[0])),
            ])]),
        ),
        (
            facts.end,
            Json::Array(vec![object(vec![("hooks", hook(&commands[1]))])]),
        ),
    ];
    let names = facts
        .declare
        .iter()
        .map(|event| event.name)
        .chain(facts.instruct.iter().map(|(event, _)| event.name));
    for (name, command) in names.zip(&commands[2..]) {
        let group = object(vec![("hooks", hook(command))]);
        // An event with several hooks keeps one key, its groups in order.
        match events.iter_mut().find(|(event, _)| *event == name) {
            Some((_, Json::Array(groups))) => groups.push(group),
            _ => events.push((name, Json::Array(vec![group]))),
        }
    }
    object(vec![("hooks", object(events))])
}

/// Claude Code's marketplace: this repository, the plugin at the tag.
fn claude_marketplace(version: &str) -> Json {
    object(vec![
        ("name", text("reef")),
        (
            "description",
            text("Reef, version control for fleets of coding agents."),
        ),
        ("owner", author()),
        (
            "plugins",
            Json::Array(vec![
                object(vec![
                    ("name", text("reef")),
                    (
                        "source",
                        object(vec![
                            ("source", text("git-subdir")),
                            ("url", text(CLONE_URL)),
                            ("path", text(Package::ClaudeCode.dir())),
                            ("ref", text(&tag(version))),
                        ]),
                    ),
                    ("description", text(PLUGIN_DESCRIPTION)),
                    ("category", text("development")),
                ]),
                object(vec![
                    ("name", text(WORKSPACES_NAME)),
                    (
                        "source",
                        object(vec![
                            ("source", text("git-subdir")),
                            ("url", text(CLONE_URL)),
                            ("path", text(WORKSPACES_DIR)),
                            ("ref", text(&tag(version))),
                        ]),
                    ),
                    ("description", text(WORKSPACES_DESCRIPTION)),
                    ("category", text("development")),
                ]),
            ]),
        ),
    ])
}

/// Codex's marketplace: the Agent Plugins tree at the tag. Reef needs no
/// sign-in, so authentication is asked for on install, which is nothing.
fn agents_marketplace(version: &str) -> Json {
    object(vec![
        ("name", text("reef")),
        ("interface", object(vec![("displayName", text("Reef"))])),
        (
            "plugins",
            Json::Array(vec![object(vec![
                ("name", text("reef")),
                (
                    "source",
                    object(vec![
                        ("source", text("git-subdir")),
                        ("url", text(CLONE_URL)),
                        ("path", text(Package::AgentPlugins.dir())),
                        ("ref", text(&tag(version))),
                    ]),
                ),
                (
                    "policy",
                    object(vec![
                        ("installation", text("AVAILABLE")),
                        ("authentication", text("ON_INSTALL")),
                    ]),
                ),
                ("category", text("Coding")),
            ])]),
        ),
    ])
}

/// The MCPB bundle's manifest, for every platform: `scripts/package-mcpb.sh`
/// narrows `compatibility.platforms` to the target's.
fn mcpb_manifest(version: &str) -> Json {
    let tools = crate::mcp::tool_summaries()
        .into_iter()
        .map(|(name, description)| {
            object(vec![
                ("name", text(name)),
                ("description", text(description)),
            ])
        })
        .collect();
    object(vec![
        ("manifest_version", text("0.3")),
        ("name", text("reef")),
        ("display_name", text("Reef")),
        ("version", text(version)),
        ("description", text(PLUGIN_DESCRIPTION)),
        ("author", author()),
        (
            "repository",
            object(vec![("type", text("git")), ("url", text(REPO_URL))]),
        ),
        ("homepage", text(REPO_URL)),
        ("license", text("Apache-2.0")),
        (
            "server",
            object(vec![
                ("type", text("binary")),
                ("entry_point", text("reef")),
                ("mcp_config", mcp_entry(McpShape::Mcpb)),
            ]),
        ),
        ("tools", Json::Array(tools)),
        (
            "compatibility",
            object(vec![(
                "platforms",
                Json::Array(vec![text("darwin"), text("linux")]),
            )]),
        ),
    ])
}

/// The registry listing without its packages, which `scripts/mcp-server-
/// json.sh` adds from the release's checksums.
fn server_json(version: &str) -> Json {
    object(vec![
        (
            "$schema",
            text("https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json"),
        ),
        ("name", text(SERVER_NAME)),
        ("title", text("Reef")),
        ("description", text(SERVER_DESCRIPTION)),
        ("version", text(version)),
        (
            "repository",
            object(vec![("url", text(REPO_URL)), ("source", text("github"))]),
        ),
        ("websiteUrl", text(REPO_URL)),
        ("packages", Json::Array(Vec::new())),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use clap::Parser as _;

    use super::*;
    use crate::cli::guide;

    fn guide_text() -> (String, &'static str) {
        (guide::long(), guide::short())
    }

    fn rendered() -> Vec<(PathBuf, String)> {
        let (long, short) = guide_text();
        files(&Guide { short, long: &long }, env!("CARGO_PKG_VERSION"))
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Test 7: the committed packages are what the renderers produce; with
    /// `REEF_BLESS=1` (`just packages`) the test writes them instead.
    #[test]
    fn packages_are_current() {
        let root = root();
        let bless = std::env::var_os("REEF_BLESS").is_some_and(|value| value == "1");
        let mut stale = Vec::new();
        for (path, contents) in rendered() {
            let full = root.join(&path);
            if fs::read_to_string(&full).ok().as_deref() == Some(contents.as_str()) {
                continue;
            }
            if bless {
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(&full, contents).unwrap();
            } else {
                stale.push(path.display().to_string());
            }
        }
        // Nothing under `plugins/` that the renderers do not produce.
        let produced: Vec<PathBuf> = rendered().into_iter().map(|(path, _)| path).collect();
        let mut extra = Vec::new();
        let mut dirs = vec![PathBuf::from("plugins")];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = fs::read_dir(root.join(&dir)) else {
                continue;
            };
            for entry in entries {
                let path = dir.join(entry.unwrap().file_name());
                if root.join(&path).is_dir() {
                    dirs.push(path);
                } else if !produced.contains(&path) {
                    extra.push(path.display().to_string());
                }
            }
        }
        assert!(
            stale.is_empty() && extra.is_empty(),
            "the harness packages are not current; run `just packages`.\nstale: {stale:?}\nnot rendered: {extra:?}"
        );
    }

    fn json(path: &str) -> serde_json::Value {
        let (_, contents) = rendered()
            .into_iter()
            .find(|(file, _)| file == Path::new(path))
            .unwrap_or_else(|| panic!("{path} not rendered"));
        serde_json::from_str(&contents).unwrap()
    }

    /// Test 8.
    #[test]
    fn packages_agree() {
        let version = env!("CARGO_PKG_VERSION");
        for path in [
            "plugins/claude-code/.claude-plugin/plugin.json",
            "plugins/claude-code-workspaces/.claude-plugin/plugin.json",
            "plugins/agent-plugins/plugin.json",
            "packaging/mcpb/manifest.json",
            "packaging/mcp/server.json",
        ] {
            assert_eq!(json(path)["version"], version, "{path}");
        }
        for path in [
            ".claude-plugin/marketplace.json",
            ".agents/plugins/marketplace.json",
        ] {
            for plugin in json(path)["plugins"].as_array().unwrap() {
                assert_eq!(plugin["source"]["ref"], format!("v{version}"), "{path}");
                // A clone that needs no SSH key.
                assert_eq!(plugin["source"]["url"], CLONE_URL, "{path}");
                assert!(CLONE_URL.starts_with("https://"), "{CLONE_URL}");
            }
        }
        let (long, _) = guide_text();
        for (path, contents) in rendered() {
            if path.ends_with("SKILL.md") {
                assert!(
                    contents.ends_with(&format!("\n\n{}\n", long.trim_end())),
                    "{path:?}"
                );
                assert!(contents.contains("Part of the Reef plugin"), "{path:?}");
            }
        }
        let entry = super::super::render_mcp_entry(McpShape::McpJson);
        assert_eq!(
            json("plugins/claude-code/.mcp.json")["mcpServers"]["reef"],
            entry
        );
        assert_eq!(
            json("plugins/agent-plugins/mcp.json")["mcpServers"]["reef"],
            entry
        );
        let tools: Vec<_> = json("packaging/mcpb/manifest.json")["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        let served: Vec<_> = crate::mcp::tool_summaries()
            .into_iter()
            .map(|(name, _)| name.to_owned())
            .collect();
        assert_eq!(tools, served);
        assert!(SERVER_DESCRIPTION.chars().count() <= 100);
        let name_ok = SERVER_NAME.split_once('/').is_some_and(|(space, name)| {
            !space.is_empty()
                && !name.is_empty()
                && space
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        });
        assert!(name_ok, "{SERVER_NAME}");
        // Every hook runs a `reef presence hook` or a `reef provenance
        // declare --hook` this binary parses, with the row's pointers.
        let hooks = json("plugins/claude-code/hooks/hooks.json");
        let mut commands = Vec::new();
        for (_, groups) in hooks["hooks"].as_object().unwrap() {
            for group in groups.as_array().unwrap() {
                for hook in group["hooks"].as_array().unwrap() {
                    commands.push(hook["command"].as_str().unwrap().to_owned());
                }
            }
        }
        // The test's JSON sorts keys; the file keeps the renderer's order.
        let mut expected = hook_commands();
        expected.sort();
        commands.sort();
        assert_eq!(commands, expected);
        for command in commands {
            let line = command
                .strip_prefix("command -v reef >/dev/null 2>&1 && ")
                .and_then(|line| {
                    line.strip_suffix(" >/dev/null 2>&1 || true")
                        .or_else(|| line.strip_suffix(" 2>/dev/null || true"))
                })
                .unwrap_or_else(|| panic!("{command}"));
            let words: Vec<&str> = line.split(' ').collect();
            assert!(
                words[..3] == ["reef", "presence", "hook"]
                    || words[..4] == ["reef", "provenance", "declare", "--hook"]
                    || words[..4] == ["reef", "comment", "next", "--hook"],
                "{line}"
            );
            // Only an instruction hook prints: its stdout is the
            // instruction the harness hands the model (R3.12).
            assert_eq!(
                command.contains(" >/dev/null 2>&1 || true"),
                words[..3] != ["reef", "comment", "next"],
                "{command}"
            );
            crate::cli::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
    }

    /// R3.44 test 10: each declaring event's hook names only fields
    /// `declare` takes, with the row's pointers, and the prompt's carries
    /// the instruction.
    #[test]
    fn declaring_hooks_name_declare_fields() {
        let declare = &claude_code().declare;
        assert!(!declare.is_empty());
        for event in *declare {
            for (field, pointer) in event.at {
                assert!(
                    crate::cli::provenance::DECLARE_FIELDS.contains(field),
                    "{field}"
                );
                assert!(pointer.starts_with('/'), "{pointer}");
            }
        }
        assert!(
            declare
                .iter()
                .any(|event| event.at.iter().any(|(field, _)| *field == "instruction")),
            "no hook declares the instruction"
        );
    }

    /// R2.26 test 15: every provider hook runs a `reef ws provider` this
    /// binary parses, create prints the path alone, and re-hold can never
    /// fail a session's start.
    #[test]
    fn provider_hooks_parse() {
        let hooks = json("plugins/claude-code-workspaces/hooks/hooks.json");
        let mut commands = Vec::new();
        for (_, groups) in hooks["hooks"].as_object().unwrap() {
            for group in groups.as_array().unwrap() {
                for hook in group["hooks"].as_array().unwrap() {
                    commands.push(hook["command"].as_str().unwrap().to_owned());
                }
            }
        }
        // The test's JSON sorts keys; the file keeps the renderer's order.
        let mut expected = provider_commands().to_vec();
        expected.sort();
        commands.sort();
        assert_eq!(commands, expected);
        let [create, _, rehold] = provider_commands();
        for command in &commands {
            let line = command
                .strip_suffix(" >/dev/null 2>&1 || true")
                .unwrap_or(command);
            let words: Vec<&str> = line.split(' ').collect();
            let leaf: Vec<&str> = words
                .iter()
                .copied()
                .filter(|word| *word != "--no-autostart")
                .collect();
            assert_eq!(leaf[..3], ["reef", "ws", "provider"], "{line}");
            crate::cli::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
        assert!(create.ends_with(" -o text"), "{create}");
        assert!(rehold.ends_with("|| true"), "{rehold}");
        assert!(rehold.contains("--no-autostart"), "{rehold}");
        // Worktree events and UserPromptSubmit take no matcher.
        for (_, groups) in hooks["hooks"].as_object().unwrap() {
            assert!(groups[0].get("matcher").is_none(), "{groups}");
        }
        let marketplace = json(".claude-plugin/marketplace.json");
        let names: Vec<&str> = marketplace["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|plugin| plugin["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["reef", WORKSPACES_NAME]);
    }

    /// Test 17: the quickstart documents each install line as the agent
    /// table has it.
    #[test]
    fn quickstart_lists_the_install_lines() {
        let quickstart = fs::read_to_string(root().join("docs/quickstart.md")).unwrap();
        for sign in SIGNS {
            if sign.package.is_some() || sign.install.starts_with("reef init") {
                assert!(
                    quickstart.contains(&format!("`{}`", sign.install)),
                    "docs/quickstart.md lacks `{}` ({:?})",
                    sign.install,
                    sign.agent
                );
            }
        }
    }
}
