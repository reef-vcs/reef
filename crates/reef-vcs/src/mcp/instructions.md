Reef gives each coding agent its own workspace: a copy-on-write directory on one change, snapshotted as it is written.

Tools default to the repository or workspace the server was started in; pass `repo` or `workspace` to name another. `@` is a workspace's change and needs one. read_file, list_files and edit_files work on any revision, without a directory.

A write in a workspace returns the workspace afterwards: `workspace.divergent` true means its change has more than one commit; list_changes with revset `change_id(<id>)` lists them, and abandon_change drops one.

A failed call returns `{"error": {kind, reason, message, hint, next}}`: `hint` says how to fix it, and `next` is a `reef` command line that does.

The server keeps the leases of workspaces it creates alive while it runs. wait_for_operations waits for other agents' work.
