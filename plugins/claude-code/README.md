# Reef

Reef gives each coding agent its own workspace: a copy-on-write copy of a git or jj repository, leased to one process and snapshotted after every write. This plugin adds Reef's skill, which starts work in a workspace, and Reef's MCP server (`reef mcp`), which answers status, diff and history questions and edits files without a directory. Its hooks declare the files the agent is about to edit, so `reef presence watch` and the web UI show them.

It runs the `reef` on your `PATH`; install Reef first (https://github.com/reef-vcs/reef#install). Then:

```sh
claude plugin install reef --marketplace reef-vcs/reef
```

Generated from Reef's agent guide by `just packages`; `reef guide` prints the guide.
