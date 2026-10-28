# 0041. GC on a timer

- Status: accepted
- Date: 2026-10-27
- Ticket: issue #95

## Context

R1.17's GC runs only when someone types `reef ws gc`, so nothing enforces `base_cache.max_entries` (default 8) between runs. This repository's daemon built up 93 bases (446 MB by `du`) in ten days before its first run. That run also showed the report overstating what it freed: `base_bytes_freed` said 388 MiB while the volume gained about 110 MiB, because incremental bases (R1.08) are `clonefile` copies of a nearby base that share its blocks, and `base_bytes_freed` sums each base's allocated blocks in full.

The issue held the jj step off a timer until #94 was fixed, since each run then broke the registered root's `jj`. [0040](0040-gc-and-the-users-jj-workspaces.md) fixed it: GC moves the user's jj workspaces under their lock, abandons nothing while a `jj` command holds one, and re-checks before collecting.

## Decision

1. **`[gc] interval`**, a duration, default `"1d"`. `"0s"` turns the timer off; any other value under `1h` is refused at load, since a run holds a repository's creates, moves and writes off and runs `git gc`. `"off"` is not accepted: `0s` is how other keys say "none". The key is read at start, like every other.
2. **When a run is due:** at the latest of `meta.last_gc_at + interval`, 10 minutes after the timer starts, and an in-memory floor (an interval after the last timed run, or 10 minutes after the maintenance lock was found held). After the clock goes back, a `last_gc_at` in the future is ignored and the floor is pulled back to at most an interval from now, so a clock that was fast never postpones GC by its error. `meta.last_gc_at` is decimal unix milliseconds, written at the end of every timed run that went through every repository and every manual run over every repository that is not a dry run. So a restart does not reset the clock, a daemon that has never collected does so 10 minutes after it starts, and a manual full run postpones the timer by an interval. A manual run of one repository or a dry run writes nothing.
3. **Wall clock, checked at least every 10 minutes.** The loop sleeps on tokio's clock for at most 10 minutes and re-reads the system clock each time: `Instant` on macOS does not advance while the machine sleeps, so one day-long sleep could take days on a laptop. No jitter: one daemon per user, and the run is local.
4. **One run at a time.** A timed run takes the maintenance lock, as a manual GC, backup or quiesce does. Finding it held, the timer tries again 10 minutes later. A manual task refused while a timed run holds the lock gets `UNAVAILABLE`, as before, and the message now names the holder: "maintenance in progress (a timed GC); try again when it finishes".
5. **All four steps, every repository,** in registration order, through the same per-repository code and `GcReport` as the RPC, logged rather than returned. Unlike a requested run, a timed run goes on past a repository that fails (logged with the repository), so one broken repository does not stop every timed run.
6. **The jj step runs unattended.** A busy checkout or a workspace that will not settle means nothing is abandoned that run, logged at `warn` instead of `error`, and the next run retries; the undo window is a week, so a day's delay costs nothing.
7. **Shutdown.** The loop's sleep is raced against the daemon's shutdown token; a run never is. A run checks for shutdown before each repository and stops there without recording itself, so the next start runs it again. Its shutdown hook waits for it and runs right after the quiesce hook, before the snapshot scheduler stops, so the repository in progress finishes with snapshots working.
8. **`GcReport.base_bytes_unshared`** (`optional uint64`, API revision 6, additive): for each base removed, the bytes no other file or filesystem snapshot shared when it was removed, which is what the volume gains. Each base is measured with APFS's private size (`getattrlist`, `ATTR_CMNEXT_PRIVATESIZE`) just before it is deleted, so a block cloned bases share counts once, when the last of them goes. A dry run reports each entry's private size now, a lower bound. Unset where the filesystem cannot tell (everything but APFS for now); 0 when nothing was removed. `base_bytes_freed` keeps its meaning and is documented as an upper bound.
9. **No cap on create.** Evicting when a create makes a base past `max_entries` would need the create lock exclusively per new base, stalling the creates behind it, and the maintenance lock against a quiesce, refusing a `reef ws gc` or `reef backup` that follows a create. The timer bounds the cache to `max_entries` plus a day's new bases; a shorter `interval` is the lever.

## Consequences

- A repository's creates, moves and writes wait out a timed run once a day: seconds on the bench, longer for a large `git gc`.
- A daemon stopping during a timed run exits once the repository in progress is done, which for a large `git gc` can outlast `reef daemon stop`'s 15-second wait; the command then fails though the daemon exits later.
- Whatever steps 2 to 4 come to, the snapshot index is reconciled after the jj step rewrote the operation ids, so a repository whose later step fails every day keeps a current index. The store leftovers no repository owns go with the first repository whose run gets through, not only the first registered.
- ADR 0040's residual race (a `jj` command that committed before GC loaded the head and checks out after its re-check) can now happen at a moment nobody chose, daily instead of rarely. Its repair is automatic too: the next run, or a manual `reef ws gc`.
- `git gc --prune` runs daily in the user's colocated `.git` (S-13), pruning unreachable objects older than the undo window.
- Subscriptions see GC's rewrite daily; R2.09's GC callback already moves caught-up ones.
- Upgraded 0.1 daemons collect 10 minutes after the new daemon starts, which also repairs a checkout a 0.1 GC broke.
- `base_bytes_unshared` is unset on Linux, where reflink bases share blocks too. `FIEMAP_EXTENT_SHARED` could count them, but btrfs updates sharing through delayed references; that needs its own measurement on CI's loop-mounted images, a follow-up.
- Test daemons (`TestDaemon`, the spawned binary) start with the timer off, so no timed run lands partway through a long test.

## Alternatives considered

- **`statvfs` before and after.** The volume is shared with every workspace and agents keep writing during GC; the figure would be noise, sometimes negative.
- **Renaming `base_bytes_freed`.** v1 only grows ([0029](0029-daemon-api-v1.md)).
- **A cap on create.** Decision 9.
- **Only steps 2 to 4 on the timer.** The issue's interim plan, for until #94 was fixed; it is.
- **An in-memory clock.** A daemon restarted more often than the interval would never collect.
- **`"off"` as a spelling.** One duration grammar ([0013](0013-daemon-state-directory-and-configuration.md)).
- **Jitter.** Nothing to spread.
