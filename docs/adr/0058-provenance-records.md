# 0058. Provenance records

- Status: accepted
- Date: 2026-11-06
- Ticket: R3.04
- Amends: [0055](0055-evidence-object-format.md) (a third predicate type in the format crate; `evidence_keys` also holds the keys that sign provenance), [0057](0057-operator-identities-and-session-keys.md) (daemon-held keys issued internally, without a proof of possession, for `max_lifetime`; records pin their keys; revocation drops held keys)

## Context

The [design](../design.md) wants every snapshot to carry "a provenance record: agent identity, model and version, session ID, originating task reference, a hash of the instruction that produced the change, and a signature over all of it", signed by a short-lived session key that chains to a person or pipeline, so a thousand agent commits are "auditable after the fact". Until now, "who wrote it" was whatever an operation's `reef-agent` said ([0044](0044-history-index-and-query-layer.md)), unsigned and rewritten by GC.

[0055](0055-evidence-object-format.md) left open whether provenance reuses the evidence envelope. [0057](0057-operator-identities-and-session-keys.md) reserved `custody: daemon` for keys the daemon holds in memory, one per workspace session, and ruled out a signing RPC with them. Reef never starts an agent and never holds the prompt that started one.

Snapshots are hot: the watcher commits one every few seconds while an agent writes, and the load test gates their cost. Operation ids do not survive GC; commit ids do, and change ids survive every rewrite.

## Decision

1. **One record per commit the daemon records for a workspace.** Each snapshot that commits (`watcher`, `explicit`, `final`, `move`, `shutdown`, and the carried follow-ups of [0039](0039-carrying-edits-through-a-write.md)) and each `EditFiles` naming the workspace that writes the workspace's own change (`edit`; through the remote listener, only the creating token's) gets one record. Nothing is written into the commit: no trailer or description, so a git push or export carries no provenance and commit ids do not change. Commits the daemon did not record (the jj CLI, describes, rebases, `EditFiles` naming no workspace) have none. Readers find records by change id, across rewrites; `(repo, commit)` is unique.
2. **A third predicate type in [0055](0055-evidence-object-format.md)'s envelope.** An in-toto Statement v1 whose one subject is `{"name": "commit", "digest": {"gitCommit": <commit id>}}`, predicate type `urn:reef-vcs:provenance:v1`, predicate `change`, `tree` (`{"gitTree": …}`, absent when conflicted), `workspace`, `kind`, `session`, `sequence`, `agent`, `recordedOn` and `context` (`model`, `harness`, `task`, `instruction` as `{"sha256": …}`, `clientSession`, `declaredOn`), in a DSSE envelope signed once, with the envelope's PAE, canonical form, size, depth, duplicate-key and key rules. `reef-evidence`'s `provenance` module signs and checks it; `sign` takes a `Record`, never bytes. The record's id is the SHA-256 of the canonical envelope. Identity is not in the predicate: it is the signing key's certificate. [0055](0055-evidence-object-format.md)'s stability rules apply.
3. **Observed, certified and declared.** Commit, change, tree, workspace, kind, session, sequence and the times are what the daemon observed. Identity, kind, actor, custody and the key's window are what the authority certified. Agent label, model, harness, task, instruction hash and client session are what a client declared; Reef verifies none of them, and every reader is told so.
4. **One provenance session per workspace, signed by a daemon-held key.** A session (`ses_<16 hex>`, `provenance_sessions`) starts with its workspace and ends at release or reclaim, after the final snapshot recorded. Its identity and actor are fixed at creation by R3.03's issuance rules: over the socket the default `person` identity or one named, actor `agent` unless declared; through the remote listener the token's linked identity, never `human`. A named identity or actor the rules refuse fails the creation; naming nothing never does. At the first record the daemon generates a key in memory and certifies it with `custody: daemon`, `presence: none`, for `[session_keys] max_lifetime`, without a proof of possession (it generated the key), counted toward the identity's key limit. A key within five minutes of its end, gone after a restart, or dropped by a revocation is replaced in the same session. A key is never written anywhere. Where no identity can be bound (no default, an unlinked token, a revoked identity or token), records are signed by an uncertified held key and read `UNBOUND`, with the session's `problem` saying why.
5. **Declarations at creation and through `DeclareProvenance`, which snapshots first.** `CreateWorkspaceRequest.provenance` sets the initial context, identity and actor; `DeclareProvenance` replaces context fields (set replaces, empty clears, unset keeps) after an explicit snapshot, so edits already on disk record under the context they were made under; `no_snapshot` skips it. Identity and actor cannot change. A declaration records no operation. Through the remote listener only the creating token may declare (`Rule::OwnWorkspace`).
6. **The instruction is a SHA-256 of its exact bytes, computed by the client.** The text is never sent or stored; `reef provenance declare --instruction-file` hashes a file in the CLI.
7. **Recording never fails a snapshot or a write.** Renewal, signing and the insert run on the snapshot's blocking half under a per-workspace lock, after the snapshot's index row; any failure is logged at `warn` and counted in the session's `missing_records`. Nothing is backfilled. `recordedOn` is when the daemon signed the record, at or just after the operation ended, so it lies inside the window of a key issued for it.
8. **Every read checks every record.** `GetProvenance` and `ListProvenance` check the envelope, the key, the signature and the statement, then every index column against the payload (`SIGNATURE_CHECK_VALID` or `INVALID`), then R3.03's certificate and status at `recordedOn`, the daemon being its own witness. A certificate whose custody is not `daemon`, or that certifies another session than the record's, makes the identity `INVALID`.
9. **GC compacts and expires records.** Step 8, after evidence: records older than `[provenance] compact_after` (7 days) keep only the first and last of each run of one session's records of one change under one context digest, and any of a commit visible in the head's view; records of changes with no visible commit are deleted after `[provenance] retention` (90 days). Records of visible changes are kept for good once compacted. Nothing at or after `keep_newer` is touched. Records pin their keys in R3.03's sweep.

## Consequences

- R3.37 answers "task, agent, instruction" for a change by `ListProvenance(change_ids)`; R3.25 reports each conflict side's records; R3.07 can require bound provenance and exclude a change's own sessions from signing its tests; R4.07 publishes records with their certificates.
- On one uid a record shows what the daemon saw and what it was told, not who typed what: any same-uid process can write into a workspace, declare any context, read `authority.pem`, or read a held key from the daemon's memory (S-67, S-68). Remote callers are bound to their token's identity (S-69).
- Every committing snapshot pays a signature and an insert; the overhead and the load test are gates.
- After compaction, "which commit within an instruction's run introduced this line" is answered at the run's level.
- `restore` leaves the root help's first screen for `provenance`.

## Alternatives considered

- **The tree as subject.** Two sessions writing the same tree would share one record.
- **The change as subject.** Not content-addressed, and says nothing about which commit.
- **One record per session or context, listing its commits.** A crash loses the unsigned tail, and a reader of one commit needs a list it cannot check alone.
- **Client-held keys signing provenance.** The watcher records commits with no client present.
- **One daemon key for every workspace.** No session to revoke or join on.
- **Operation attributes as provenance.** Unsigned, and rewritten by GC.
- **A trailer in the commit.** Rewrites commits and leaks into git.
- **Storing the instruction text.** The design's non-goal; a salted hash was the other option, and loses "which commits did this instruction produce".
- **Backfilling missing records.** A signature would claim a later time.
- **No retention.** One row per watcher snapshot, kept forever.
