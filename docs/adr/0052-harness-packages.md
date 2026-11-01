# 0052. Harness packages

- Status: accepted
- Date: 2026-11-01
- Ticket: R2.25

## Context

An agent got Reef in one of two ways: a person ran `reef init` in a repository, which wrote `AGENTS.md` and the files of the agents it recognized (Claude Code and Cursor), or an agent with a shell read `reef` and `reef --help`. Both worked one repository at a time. No harness could install Reef the way it installs its other tools, and no MCP client outside a repository could find `reef mcp`: it was listed nowhere.

The user's standing direction is that Reef stays agent-agnostic: the default surfaces (`AGENTS.md`, the CLI with `hint` and `next`, MCP) are harness-neutral, each harness's files are thin adapters rendered from the same guide, facts about each harness live in one table, a harness is added only with facts verified against it, and text a person copies names a harness only where that harness behaves differently.

What the harnesses accept was checked on 2026-11-01 by installing each one's pinned release with no account signed in: Claude Code 2.1.316, Codex 0.166.0 and opencode 1.19.5, and the MCP registry's schema 2025-12-11 and publisher 1.8.7. Claude Code and Codex install plugins from a git marketplace, each pinning a ref, and Codex reads the vendor-neutral Agent Plugins 1.0.0 format. opencode's plugins are npm modules that cannot be shown to declare an MCP server. The registry lists only servers with a package it checks; Reef publishes none of npm, PyPI, NuGet, crates.io or OCI, but an MCPB bundle on a GitHub release counts.

## Decision

1. **A package adds only what a harness cannot get from the neutral surfaces:** its skill, its MCP entry, and where its hooks can name the file being edited, declared edits as presence. Every package runs the `reef` on `PATH`; none downloads or installs anything, which would be a second, unreviewed install path.
2. **Per-harness facts live in `reef init`'s agent table** (`crates/reef-vcs/src/agents/mod.rs`, `SIGNS`) alone: how an agent is recognized (root names, an environment signal, a home directory), what Reef writes for it in the repository and in the home directory, its package format, and its install line. Codex and opencode join Claude Code and Cursor, recognized by their files (`.codex/`; `opencode.json`, `opencode.jsonc`, `.opencode/`), opencode also by the `OPENCODE=1` its release sets on its own process. They share one skill in the Agent Skills standard's `.agents/skills`, whose frontmatter keeps to the standard's keys, and get their MCP entries in `.codex/config.toml` (merged with `toml_edit`, keeping the file's comments and layout) and `opencode.json` (as `.mcp.json` is merged; an `opencode.jsonc` is not rewritten, and the reason gives the entry to paste). Reef owns one key in each file and keeps any entry someone changed. One function renders every MCP entry, and a test checks that each form starts the same `reef mcp`.
3. **Plugin trees are rendered into the repository and checked**, as `AGENTS.md` is: `plugins/claude-code` in Claude Code's format (skill, `.mcp.json`, hooks) and `plugins/agent-plugins` in the Agent Plugins format, which Codex installs. They are named by format, not by harness: another harness that adopts a format installs the same tree. This repository is the marketplace for both (`.claude-plugin/marketplace.json`, `.agents/plugins/marketplace.json`), each entry pinned to the `v<version>` tag, so a harness installs what the released `reef` supports. `just packages` writes the trees; a test fails until they are current, and the release workflow's version check fails a release pull request that did not run it.
4. **opencode, and Codex without its plugin, go through `reef init --agents <agent> --user`**, which writes the skill and the MCP entry in the home directory. No npm package, which would need an account and could not be shown to carry an MCP server.
5. **`--user` selects by home directory.** With `auto`, an agent's user-level files are written when its directory exists in the home directory (`~/.claude`, `$CODEX_HOME` or `~/.codex`, `$XDG_CONFIG_HOME/opencode` or `~/.config/opencode`), the repository shows it, or it runs `reef init`; that also turns on its repository files, as `--user` did for Claude Code. An agent named with `--agents` is written regardless. `--user` used to write Claude Code's skill always, and now does so on a machine where Claude Code has run.
6. **Hooks report declared edits, with a TTL, never fail, and print nothing.** `reef presence hook edit` reads the harness's hook event on stdin and takes JSON Pointers to its paths and session id, so the command knows no harness; the pointers are facts in the Claude Code plugin's hooks. It reports one editing entry per session, in the one workspace holding every path, replaced by the session's next edit and lasting 5 minutes (at most 10); `reef presence hook end` clears it at the session's end. This departs from [R2.11](../dev-tickets/phase-2/R2.11-agent-presence-reporting.md)'s report-before, clear-after: an entry that lasts one tool call is never seen. It always exits 0 once its arguments parse, writes nothing to stdout, logs a failure only when `REEF_LOG` asks (the plugin's commands discard both streams anyway: Claude Code reads JSON on stdout as a decision and shows stderr on a failure), never starts a daemon, and gives up after 2 seconds. A session whose next edit is in another workspace ends its old entry and starts one there. Only Claude Code's plugin has hooks: Codex runs plugin hooks in its desktop app alone, and its edits name no path a pointer could select.
7. **The registry listing uses one MCPB bundle per target**, `reef-<version>-<target>.mcpb` holding `reef`, `reef-daemon`, `LICENSE` and `README.md`, whose manifest lists the tools from the server's own table. `reef mcp` finds `reef-daemon` beside it, so a bundle runs its own daemon. The listing is `io.github.reef-vcs/reef`, with all four targets. Publishing the crates to crates.io is deferred.
8. **The release publishes the listing by GitHub OIDC on every tag**, with the publisher pinned and checked with cosign, and a failure warns rather than fails the release, as the Homebrew tap does. A dry run builds and runs every bundle, installs every package into every harness from the checkout, and validates `server.json`; it publishes nothing. The release also attaches `reef schema`'s document and descriptor set ([0050](0050-published-schemas.md)).

## Consequences

- One line sets up Claude Code, Codex or opencode for every repository, and any MCP client can find Reef on the registry.
- Adding a harness is a row in the agent table with its verified facts, a step in `scripts/test-harness-packages.sh`, and a row in the quickstart's table, which a test checks against the agent table.
- Every release pull request runs `just packages` after the version bump. Between that pull request's merge and the tag, a matter of minutes, the marketplaces name a tag that does not exist yet, and an install fails with git's error.
- A Claude Code session with the plugin and a repository's `.mcp.json` gets Reef's server twice. Claude Code asks before it starts the project's, and `reef init` does not skip a committed file because of what one machine has installed.
- Every edit in a Claude Code session with the plugin costs one local RPC, or one failed connect when no daemon runs.
- The bundles hold the release's unsigned binaries (S-14 until R4.17); their provenance is attested as the tarballs' is. A client that downloads one may quarantine its binaries on macOS.
- The pins (`CLAUDE_CODE_VERSION`, `CODEX_VERSION`, `OPENCODE_VERSION`, `MCPB_VERSION`, `MCP_PUBLISHER_VERSION` in `release.yml`) move by pull request, whose dry run tests them.

## Alternatives considered

- **A separate marketplace repository the release pushes to**, as it does the Homebrew tap: a second repository and a deploy key, to save the minutes between a release pull request and its tag.
- **Plugins pointing at `main`:** content ahead of the binary, such as a hook that calls a command the released `reef` lacks.
- **A plugin that installs `reef`:** an install path outside the reviewed installer, checksums and attestations.
- **An npm plugin for opencode:** its plugins cannot be shown to declare an MCP server, and it needs an npm account.
- **crates.io for the listing:** nine permanent crates, an account, the guide copied into `reef-vcs` ([0025](0025-agent-facing-cli-contract.md)), a generated web bundle in `reef-web`'s package, and a Rust toolchain and long build for everyone the listing reaches.
- **OCI for the listing:** an image of a CLI that needs the host's filesystem.
- **A `reef hook` that knows each harness's event:** vendor code in the CLI, where pointers keep it in the table.
- **A `SessionStart` hook that adds context:** every session would pay for text the skill and `AGENTS.md` already give (#122).
