//! Reef's harness packages (R2.25, ADR 0052): the plugin trees and their
//! marketplaces, the MCPB bundle's manifest and the MCP registry's
//! `server.json`, rendered from the same guide and entries as `reef init`'s
//! files and committed. This repository is the marketplace, each entry
//! pinned to the release's tag, so a harness installs what the released
//! `reef` supports. A test checks the committed files against [`files`];
//! `just packages` writes them.

use std::path::PathBuf;

use indexmap::IndexMap;

use super::{Guide, Harness, Json, McpShape, Package, SIGNS, mcp_entry, render_with};

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

/// The plugins' and the bundle's description.
const PLUGIN_DESCRIPTION: &str = "Reef workspaces for coding agents: the skill that starts work \
     in an isolated workspace, and Reef's MCP server.";

/// The registry listing's description, at most 100 characters.
const SERVER_DESCRIPTION: &str =
    "Version control for fleets of coding agents: isolated workspaces, status, diff and history.";

/// The registry listing's name, in the namespace GitHub OIDC grants the
/// organization.
const SERVER_NAME: &str = "io.github.reef-vcs/reef";

/// The Claude Code tools whose input names the file being edited, and
/// where (JSON Pointers into the hook event), as `reef presence hook edit`
/// reads them. Facts about one harness; the command knows none of them.
const CLAUDE_CODE_EDIT_TOOLS: &str = "Edit|Write|MultiEdit|NotebookEdit";
const CLAUDE_CODE_PATHS: &[&str] = &["/tool_input/file_path", "/tool_input/notebook_path"];
const CLAUDE_CODE_SESSION: &str = "/session_id";

/// A hook command: `reef presence hook ...`, run only when `reef` is on
/// `PATH`, never failing the tool call it runs before, and with its output
/// discarded, so a `REEF_LOG` in the session's environment cannot reach
/// Claude Code, which reads a hook's stdout as a decision.
fn hook_command(args: &str) -> String {
    format!("command -v reef >/dev/null 2>&1 && reef presence hook {args} >/dev/null 2>&1 || true")
}

/// The hook commands in `hooks.json`, as a harness runs them, for tests.
fn hook_commands() -> Vec<String> {
    let paths: Vec<String> = CLAUDE_CODE_PATHS
        .iter()
        .map(|pointer| format!("--path-at {pointer}"))
        .collect();
    vec![
        hook_command(&format!(
            "edit {} --session-at {CLAUDE_CODE_SESSION}",
            paths.join(" ")
        )),
        hook_command(&format!("end --session-at {CLAUDE_CODE_SESSION}")),
    ]
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
                    pretty(&plugin_json(version, None)),
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
            }
            Package::AgentPlugins => {
                add(
                    &format!("{dir}/plugin.json"),
                    pretty(&plugin_json(
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
             and the web UI show them."
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
fn plugin_json(version: &str, schema: Option<&str>) -> Json {
    let mut pairs = Vec::new();
    if let Some(schema) = schema {
        pairs.push(("$schema", text(schema)));
    }
    pairs.extend([
        ("name", text("reef")),
        ("version", text(version)),
        ("description", text(PLUGIN_DESCRIPTION)),
        ("author", author()),
        ("homepage", text(REPO_URL)),
        ("repository", text(REPO_URL)),
        ("license", text("Apache-2.0")),
        (
            "keywords",
            Json::Array(
                ["version-control", "workspaces", "jj", "mcp"]
                    .into_iter()
                    .map(text)
                    .collect(),
            ),
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
/// clear the declaration.
fn hooks_json() -> Json {
    let commands = hook_commands();
    let hook = |command: &str| {
        Json::Array(vec![object(vec![
            ("type", text("command")),
            ("command", text(command)),
            ("timeout", Json::Number(5.into())),
        ])])
    };
    object(vec![(
        "hooks",
        object(vec![
            (
                "PreToolUse",
                Json::Array(vec![object(vec![
                    ("matcher", text(CLAUDE_CODE_EDIT_TOOLS)),
                    ("hooks", hook(&commands[0])),
                ])]),
            ),
            (
                "SessionEnd",
                Json::Array(vec![object(vec![("hooks", hook(&commands[1]))])]),
            ),
        ]),
    )])
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
            Json::Array(vec![object(vec![
                ("name", text("reef")),
                (
                    "source",
                    object(vec![
                        ("source", text("git-subdir")),
                        ("url", text(REPO)),
                        ("path", text(Package::ClaudeCode.dir())),
                        ("ref", text(&tag(version))),
                    ]),
                ),
                ("description", text(PLUGIN_DESCRIPTION)),
                ("category", text("development")),
            ])]),
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
                        ("url", text(&format!("{REPO_URL}.git"))),
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
            assert_eq!(
                json(path)["plugins"][0]["source"]["ref"],
                format!("v{version}"),
                "{path}"
            );
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
        // Every hook runs a `reef presence hook` this binary parses.
        let hooks = json("plugins/claude-code/hooks/hooks.json");
        let mut commands = Vec::new();
        for (_, groups) in hooks["hooks"].as_object().unwrap() {
            for group in groups.as_array().unwrap() {
                for hook in group["hooks"].as_array().unwrap() {
                    commands.push(hook["command"].as_str().unwrap().to_owned());
                }
            }
        }
        assert_eq!(commands, hook_commands());
        for command in commands {
            let line = command
                .strip_prefix("command -v reef >/dev/null 2>&1 && ")
                .and_then(|line| line.strip_suffix(" >/dev/null 2>&1 || true"))
                .unwrap_or_else(|| panic!("{command}"));
            let words: Vec<&str> = line.split(' ').collect();
            assert_eq!(words[..3], ["reef", "presence", "hook"], "{line}");
            crate::cli::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
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
