Reef gives each coding agent its own workspace: a copy-on-write directory on one change, snapshotted as it is written.

Tools default to the repository or workspace the server was started in; pass `repo` or `workspace` to name another. `@` is a workspace's change. read_file, list_files and edit_files work on any revision, without a directory.

A write returns the workspace: `workspace.divergent` true means its change has several commits; list_changes with revset `change_id(<id>)` lists them, and abandon_change drops one.

A failed call returns `{"error": {kind, reason, message, hint, next}}`: `hint` says how to fix it, and `next` is a `reef` command that does.

The server renews the leases of workspaces it creates. get_workspace's `step` names your change's next step: describe_change, propose_change, land_change or publish_change. wait_for_operations waits for other agents' work, and with kind comment on your change delivers a reviewer's instruction: do it, then propose again.
