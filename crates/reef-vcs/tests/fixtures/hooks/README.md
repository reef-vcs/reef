# Hook events

Claude Code's hook events for `WorktreeCreate`, `WorktreeRemove` and `UserPromptSubmit`, copied from the examples in its hooks reference (https://code.claude.com/docs/en/hooks, Claude Code 2.1.316, 2026-10-31). `tests/provider.rs`'s `adapter_drives_the_contract` replaces `cwd`, `name` and `worktree_path` with the test's own and runs the `reef-workspaces` plugin's hooks on them as the harness would. Check them against a real session's when the harness's version changes.
