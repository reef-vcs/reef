# Reef

Reef gives each coding agent its own workspace: a copy-on-write copy of a git or jj repository, leased to one process and snapshotted after every write. This plugin adds Reef's skill, which starts work in a workspace, and Reef's MCP server (`reef mcp`), which answers status, diff and history questions and edits files without a directory.

It runs the `reef` on your `PATH`; install Reef first (https://github.com/reef-vcs/reef#install). Then:

```sh
codex plugin marketplace add reef-vcs/reef && codex plugin add reef@reef
```

Generated from Reef's agent guide by `just packages`; `reef guide` prints the guide.
