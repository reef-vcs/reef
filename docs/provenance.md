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

Harness hooks, `reef mcp` and the SDKs' workspace blocks declaring on their own are R3.44.

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
