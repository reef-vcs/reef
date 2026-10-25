<!-- reef:begin v1 -->
Written by `reef init` (reef 0.1.0); `reef guide` prints the current guide.

# Reef

Reef gives each agent its own workspace: a copy-on-write copy of a git or
jj repository, leased to one process and snapshotted after every write.
Work lands as a jj change; nothing is lost when a workspace is released.

From inside the repository:

    reef ws run -- <command>   # new workspace, run there, release it
    reef here                  # where am I, and what next

The first command in a repository registers it (adding jj beside git).
Output is JSON when piped; errors carry `hint` and `next`, a command to run.
<!-- reef:end -->
