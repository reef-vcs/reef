# The triage inbox

The inbox is where a person running a fleet of agents sees what needs them. It is the web UI's home page, across every registered repository, and each repository's **Inbox** tab. `reef inbox` prints the same buckets in a terminal. Its design is [ADR 0088](adr/0088-triage-inbox.md).

## The buckets

**Needs you now** holds what only a person can do:

| Item | Why it is here | What to do |
| --- | --- | --- |
| A help request | An agent asked a person ([comments](comments.md#help-requests)) | **Answer** it in place: the answer goes to the session that asked, as its next instruction |
| A proposal that needs a sign-off | Its destination's policy asks for a person's sign-off (a `[[signoff]]` rule, the owners' on a change to the policy, a resolution's approval), and nothing else blocks it: no failed or missing check, and no other rule | Open it and **Accept** ([proposals](proposals.md#reviewing-in-the-web-ui)) |
| A proposal a landing returned | Its landing came back (a failed check, a bisect culprit, a conflict) and it has not changed since; one the automatic service is still queueing again by itself (a busy destination, up to three times) is watched until it stops | Open it, or tell its agent |
| A proposal with no executor | Its checks were asked for and no executor claims them | `reef executor run` ([executor](executor.md)) |
| A conflict nobody owns | Its owner's session ended and nobody queued it ([conflicts](conflicts.md)) | `reef conflict assign <id> --queue`, or take it |
| A blocked conflict | Its tier asks a person first, or its attempts are spent | `reef conflict allow <id>` |
| A workspace that needs attention | Its watch failed, its final snapshot failed, a store is missing | Open the workspace |
| Your instruction | It waits for your Confirm after its estimate | Open the proposal and Confirm or Cancel |

**Watching** holds what is in flight: every other open proposal, with where the automatic service has it (a failed check or a rule its agent fixes is here, since the daemon tells the agent), and every instruction pending, delivered or running.

**Landed since you last looked** is a digest per repository: how many changes landed, who proposed them, and the newest ten. It counts from your mark, or with none, from a day ago.

Each item has a stable key (`proposal:<repo>:<change>`, `help:<repo>:<comment>`, and so on), the same whatever bucket it is in.

## Live, and held in place

The page reads the inbox again when an operation that can change it is recorded (a comment, a transition, a landing, a conflict assignment, a workspace created or released) and when a proposal's automatic status or a workspace's attention changes, which the daemon reports as presence (kinds `automatic` and `attention`). Nothing reorders under your cursor: new items wait behind a count in their bucket's header until you press **Show**, and an item that leaves (answered, signed off, landed) keeps its row, dimmed, until then. The browser tab's title carries the needs-you-now count.

## Since you last looked

The daemon keeps a mark per person and repository: the newest lifecycle transition you have seen. On the local web UI and the socket, you are the daemon's default person; through the remote listener, the identity your token links. When a repository's digest has been on screen for 3 seconds in a visible tab, the page marks it seen through the newest landing it shows. The digest stays as it is for the rest of the visit; the next visit starts from the mark. A mark only moves forward. `reef inbox` reads the digest and never moves the mark.

## `reef inbox`

```
reef inbox              # every repository
reef inbox --repo R     # one or more, repeatable
```

It prints the three buckets as text, or JSON when piped: `needs_you_now`, `watching` and `landed`, each item with its `key`, `reason`, `headline`, the agent's own words in `text`, and in `next` a command that acts on it. An agent supervising others can read it; it is not an MCP tool.

## Limits

- Each bucket lists at most 200 items per repository, newest first; the total counts every one.
- A repository that fails to read shows what it showed last, with a notice, until it reads again.
- A proposal under `[landing] checks = "proposal"` without `automatic` has no status, so a sign-off it waits for shows under watching.
- A daemon restart judges every proposal again: for a moment, sign-offs show as "not judged yet".
