# Provenance

Every commit the daemon records for a workspace gets a **provenance record**: a statement, signed by a key the daemon holds for the workspace's session, that it recorded that commit, in that workspace and session, while a client had declared a model, a task and an instruction. [ADR 0058](adr/0058-provenance-records.md) records why; [docs/identity.md](identity.md) has identities and keys, and [docs/evidence.md](evidence.md) the envelope.

## What a record says, and what it does not

A record is about one commit: each snapshot that commits, of any kind, and each `EditFiles` naming the workspace that writes the workspace's own change (through the remote listener, only the creating token's). A snapshot that finds nothing changed records nothing. A describe, a rebase, the jj CLI, and edits naming no workspace write commits with no record of their own; their change's records still say who produced the change, so records are found by change, across rewrites. Nothing is written into the commit, so a git push carries no provenance.

Each field is one of three kinds:

| Kind | Fields | What you may rely on |
| --- | --- | --- |
| Observed by the daemon | commit, change, tree, workspace, kind, session, sequence, recorded time, declared time | That the daemon recorded them, as far as its key is trusted |
| Certified by the authority | identity, identity kind, actor, custody, the key's window | What the session key's certificate says (docs/identity.md) |
| Declared by a client | agent label, model, harness, task, instruction hash, client session | Only that a client with access to the workspace declared them. Reef verifies none |

**The one-uid limit.** On the daemon's socket every process of your user is trusted the same (ADR 0027). Any of them can write a file into a workspace, which the daemon then records and signs; declare any model, task or instruction; read the authority's key file; or read a held key from the daemon's memory. So against an agent running as you, a record shows what the daemon saw and what it was told, not who typed what (threat model S-67, S-68). Remote callers are bound to their token's identity (S-69).

The daemon signs only records of commits it has just recorded: no caller chooses what it signs, and its held keys sign nothing else.

## Declaring

Declare at creation, or later in the workspace:

```sh
reef ws run --model example/model-x --task T-123 --instruction-file prompt.txt -- my-agent
reef ws create --task https://tracker.example/T-123 --identity alice --actor agent
reef provenance declare --instruction-file next.txt     # inside the workspace, or --workspace <ws>
reef provenance declare --harness example-harness/2.1 --client-session c0ffee
reef provenance declare --no-instruction --task ""       # clear fields
```

- On `ws create` and `ws run`, `--model` and `--task` default to `REEF_MODEL` and `REEF_TASK`, which `reef ws run` passes on to its command, beside `REEF_WORKSPACE`. Any runtime may set them. A value the daemon would refuse is ignored, and so are both when the daemon records no provenance: the environment never fails a creation. `reef provenance declare` reads only its flags, so a later declaration never reverts an earlier one from the environment.
- A field given replaces the old value, an empty one clears it, and one not given keeps it.
- `declare` snapshots the workspace first, so edits already on disk record under the context they were made under; `--no-snapshot` skips that, right after a snapshot.
- `--identity` and `--actor` (`agent` by default, `executor`, or `human`) are fixed when the workspace is created; `ws create --help` and `ws run --help` name these flags in their last line.

### Declared for you

The clients below declare without anyone typing a flag ([R3.44](dev-tickets/phase-3/R3.44-provenance-from-harnesses-and-sdks.md)). What they declare is still a claim, like any other.

| Client | Declares | From |
| --- | --- | --- |
| A harness's prompt hook (Claude Code's `reef` plugin) | each prompt's instruction hash, the harness's session id, the harness | the hook event, by JSON Pointer; the environment |
| Its session-start and model-switch hooks | the model | the hook event (`model`, `to_model`) |
| `reef ws create`, `ws run`, `ws provider create` | the harness, its session, and the session's last instruction and model | the environment, and the session note below |
| `reef mcp` | on every workspace it creates, the same, with the harness from the client's `initialize` (`clientInfo.name/version`); on the workspace it was started in, once and without a snapshot, the harness and session where that workspace has none | as above |
| The SDKs' workspace blocks | what the program passes (`provenance=`, `declare()`), with `REEF_MODEL` and `REEF_TASK` as defaults | the program |

**The harness.** `--harness` or a hook's pointer wins; then, in `reef mcp`, the MCP client's `initialize`; then `AI_AGENT`, a variable several coding agents set to name themselves, read the same way whoever set it (`name_version_…` or `name/version/…` reads as `name/version`, a version of digits and dashes with dots: Claude Code's `claude-code_2-1-316_agent` is `claude-code/2.1.316`; any other value a record can carry is kept as it is); then the harness the agent table recognizes from its environment, by name. **The session** is the variable the agent table names for that harness (Claude Code's `CLAUDE_CODE_SESSION_ID`), taken only when exactly one harness's signal is set and `AI_AGENT`, if set, names the same harness. A stdio MCP server keeps the session it started in across `/clear`, so the workspaces it creates after a `/clear` carry the earlier session, and that session's note (its last instruction before the `/clear`), until it is restarted. **The model** is `REEF_MODEL`, else a variable the harness documents (none does today), else the session note's. A value a record cannot carry is dropped: the environment never fails a command.

**The prompt hook.** A harness runs, on each prompt:

```sh
reef provenance declare --hook --at instruction=/prompt --at client_session=/session_id --at workspace=/cwd
```

with the event on stdin (a field that is not a string is left out, and the rest declared). The prompt's text is hashed in the `reef` process (SHA-256 of the JSON string's UTF-8, as the harness delivered it: Claude Code expands pasted text into it) and dropped; only the hash goes anywhere. `--hook` exits 0 whatever happens, prints nothing (a prompt hook's stdout is context the model sees, and exit 2 blocks the prompt), never starts a daemon, and gives up after two seconds; failures go to the debug log by reason, never with the event. It declares for:

- the workspace `--workspace` names, else the one the event's `cwd` is in, else `REEF_WORKSPACE`'s, else the current directory's, after snapshotting it, so edits made before the prompt keep the instruction they were made under;
- every active workspace whose session declared the same client session, in any repository;
- the **session note**, `<state>/provenance/sessions/<session>.json` (0600, in a 0700 directory): the session's last instruction hash, model and harness, never text, each with when it was declared. `reef ws create`, `ws run`, `ws provider create` and `reef mcp` declare each field from it for 24 hours after it was declared, so the workspace an agent creates in answer to a prompt is created under it. Notes older than that are deleted by the next hook.

Another harness's adapter is the same command with its own pointers: `--stdin` takes the fields as keys of one JSON object (`workspace`, `model`, `harness`, `task`, `instruction`, `instruction_sha256`, `client_session`), `--at FIELD=POINTER` from anywhere in it, as [the workspace provider contract](workspace-provider.md) does. Without `--hook` it fails and prints like any `declare`.

Not covered yet: a subagent's own task message, which reaches the harness as a tool call, not a prompt ([#239](https://github.com/reef-vcs/reef/issues/239)); Codex, whose plugin hooks need a `.codex-plugin` tree (the vendor-neutral tree Reef ships installs `hooks/hooks.json`, but Codex 0.166.0 does not load it; [#237](https://github.com/reef-vcs/reef/issues/237)); Cursor and opencode ([#238](https://github.com/reef-vcs/reef/issues/238)).

### The instruction hash

The instruction is the text the agent was given for this work, as UTF-8, exactly as delivered: a person's prompt, a non-interactive run's task message, or a review comment addressed to the agent. A harness's own system prompt and context files are not the instruction. Reef receives only its SHA-256, never the text:

```sh
printf %s "$instruction" | shasum -a 256      # what --instruction-file computes, of the file's exact bytes
```

Nothing is trimmed: a trailing newline changes the hash. Anyone who can read provenance can guess a short instruction from its hash (S-70).

## Reading

```sh
reef provenance show            # @'s record; @- for its parent, or any revision
reef provenance show --envelope # with the signed envelope and statement
reef provenance list --change @ # every record of a change, newest first
reef provenance list --mine     # the workspace's records
reef ws inspect <ws>            # the session: identity, actor, context, record count
```

`show` of a commit with no record of its own prints its change's newest record and says so. Every read checks every record again:

- `signature: valid` when the envelope, its signature and the stored index check out; `invalid` when something was changed behind the daemon.
- `identity` is R3.03's status at the record's time: `bound`, `expired`, `revoked`, `unbound` (no certificate: the session could bind no identity, or the key was never certified), or `invalid`. A record whose key's certificate says it is held by a client, or certifies another session, reads `invalid`. An uncertified key reads `unbound` whoever holds it, so only a `bound` record says the daemon's authority vouched for the key.

Whom to trust is the repository's [policy](policy.md)'s: a rule can require, for example, that every record of a change is signed and bound.

## Sessions and keys

Each workspace has one provenance session from creation to release. Which identity it binds:

| Created through | Identity | When the rules refuse |
| --- | --- | --- |
| The socket, none named | the default `person` identity | no usable default: records read `unbound` (`no_default_identity`) |
| The socket, one named | that `person` identity | the creation fails |
| A remote token linked to X | X | naming another, or `human`, fails the creation |
| A remote token linked to none | none | records read `unbound` (`token_not_linked`) |

The daemon generates the session's key in memory at its first record, has its authority certify it with `custody: daemon` for `[session_keys] max_lifetime`, and replaces it in the same session near its end, after a restart, or after a restore (from the new authority; old records still verify). Revoking the session (`reef identity session revoke --session <id>`) starts a new session at the next record; revoking the identity leaves later records `unbound` (`identity_revoked`). A revocation drops the held keys it covers at once; one written into the database behind the daemon takes effect at the next renewal, within a day. A failure to record never fails the snapshot; `ws inspect` counts the missing records, and the daemon's log names each.

## Retention

GC compacts and expires records (`[provenance]` in the daemon's `config.toml`):

- `compact_after` (7 days, `0s` never): of each run of a session's records of one change under one context, keep the first and the last, and any of a visible commit.
- `retention` (90 days, `0s` never): delete records of changes with no visible commit. A visible change's records are kept. A change squashed into another (`jj squash`) has no visible commit, so its records expire too, until landing pins the changes it consumed (R3.13).
- `enabled = false` records nothing and refuses declarations; stored records stay readable.

Nothing younger than GC's undo window is touched. Records are in `reef.sqlite3` and in backups; `reef repo forget` deletes them.
