# Issue #95 GC runs on a timer

Phase 2 · Size M (no row; see "Size") · Fixes: issue #95 · Depends on: R1.17 (`maintenance::gc::run`, its four steps and `GcReport`), #94 (GC moves the user's jj workspaces, so the jj step can run unattended) · Unblocks: nothing listed

## Context

GC runs only when someone types `reef ws gc`. Nothing enforces `base_cache.max_entries` (default 8) in between, so this repository's daemon built up 93 bases (446 MB by `du`) in ten days. The first manual run also showed that the report overstates what it frees: `base_bytes_freed` said 388 MiB while the volume gained about 110 MiB, because incremental bases (R1.08) are `clonefile` copies of a nearby base and share its blocks, and `base_bytes_freed` counts every base at full size.

This ticket makes the daemon run GC by itself:

- **A timer.** `[gc] interval` (default `"1d"`, `"0s"` turns it off) runs the same four steps as `reef ws gc`, over every repository, through the same code and report, logged instead of returned. The first run waits 10 minutes after the daemon starts. The time of the last full run is kept in the database, so a restart does not reset the clock and a manual `reef ws gc` postpones the next timed run.
- **The jj step runs too.** The issue held it back until #94 was fixed; it is (PR #96, ADR 0040). A run that meets a `jj` command holding the user's checkout abandons nothing, logs it, and the next run tries again.
- **An honest freed figure.** `GcReport` gains `base_bytes_unshared`: for each base removed, the bytes no other file or snapshot shared at the moment it was removed, which is what the volume gains. It is measured on APFS. Elsewhere it is unset. `base_bytes_freed` keeps its meaning and is documented as an upper bound. This is API revision 6 and is additive.
- **Who holds the maintenance lock.** A manual GC, backup or quiesce refused because a timed GC is running now says so.

The issue's "cheap cap on create" is not built; see "No cap on create".

### What was verified while writing this ticket

Reef at `3a14316` (#96 merged). Paths are under `crates/` unless they start with `docs/`.

- **GC today.**
  - `maintenance/gc.rs:1-2` says "explicit only (`Maintenance.Gc`, `reef ws gc`); nothing runs it on a timer in Phase 1", as R1.17's brief does (`docs/dev-tickets/phase-1/R1.17-garbage-collection.md:23`; out of scope at `:134`).
  - `gc::run` (`maintenance/gc.rs:183-199`) walks the repositories in registration order and stops at the first failure (`?` at `:196`). `run_repo` (`:201-305`) holds the repository's create lock exclusively throughout (`:219-220`) and runs the four steps. It logs a stopped jj step at `error` (`:242-244`). Its report sums `Evicted.bytes` into `base_bytes_freed` (`:281`) and logs "garbage collection finished" with fields named as the report's (`:289-303`).
  - The RPC (`service/maintenance.rs:89-111`) takes the maintenance lock with `try_acquire` (`:93`) inside a spawned task, so a client that stops waiting cannot cut GC in half.
- **The maintenance lock** (`maintenance/mod.rs`) refuses rather than queues (`:5-9`). `try_acquire` returns `Option<Guard>` (`:38-40`), and `Busy` is a unit struct, "maintenance in progress; try again when it finishes" (`:43-46`). Callers: the GC RPC (`service/maintenance.rs:93`), backup (`:127`), quiesce (`maintenance/quiesce.rs:161`), and test 7 in `reef-daemon/tests/gc.rs:415-430`.
- **The create lock.** Creates (`workspace/create.rs:131-132`), moves (`workspace/move_.rs:121-122`) and R2.03's writes (`workspace/rewrite.rs:218-219`) take `repo_lock` shared, and GC takes it exclusively (`workspace/manager.rs:205-207`). So a GC run holds that repository's creates, moves and writes off for its whole duration. R1.17's bench measured 3.8 to 4.9 s for `medium` after 1,000 snapshots (`docs/benchmarks.md:105-125`).
- **The jj step and a busy checkout.** `gc_jj` (`workspace/gc.rs:61`) refuses to abandon when a Reef workspace cannot be settled (`:87-104`). Since #96 it does the same when the user's checkout is locked: `JjStep.error` = "no operations were abandoned: …; run reef ws gc again when it finishes" (`:122-128`). The other three steps still run.
  - ADR 0040 decision 2 says a busy lock means nothing is abandoned that run. Its consequences say "A busy lock defers the whole abandon to the next run, which matters for a GC timer (#95)". There is a residual race: a `jj` command that committed before GC loaded the head and reaches its check-out after GC's re-check writes an old id, and "the next GC repairs it".
  - So a timed jj step is safe in the sense that matters: it never strands the root silently, and a stranded root is repaired by the next run.
- **Eviction.** `BaseCache::evict` (`backend/base_cache.rs:448-494`) removes unused entries least recently used first down to the cap. Its contract says "The caller keeps creates of this repository out meanwhile, so `keep` stays true" (`:455-456`). That is the reason for GC's exclusive lock: a create that has its base but no row yet would otherwise lose the base it is about to clone. `evict_one` (`:497-522`) measures with `allocated_bytes` (`:499`) and logs `freed_bytes`. A dry run measures the same way (`:478`).
- **Why the figure overcounts.** `fs_util::allocated_bytes` (`backend/fs_util.rs:33-55`) sums `st_blocks * 512` per file. A clone's blocks count in full in every copy. `ensure_from` (`base_cache.rs:272-310`) builds a new base by cloning the most recently used one (`nearest_base`, `workspace/create.rs:533-545`). Only backends with a `DirCloner` do this: APFS (`backend/apfs.rs:201`) and reflink (`backend/reflink.rs:125`). Copy and overlay bases share nothing (the default `dir_cloner` is `None`, `backend/mod.rs:395`).
- **APFS can say what a delete frees.** `getattrlist(2)` with `FSOPT_ATTR_CMN_EXTENDED` and `ATTR_CMNEXT_PRIVATESIZE` returns "the number of bytes that are not trapped inside a clone or snapshot, and which would be freed immediately if the file were deleted" (man page; `sys/attr.h:560`; `libc` exports the constants and `getattrlist`).
  - Checked by hand on this Mac (APFS data volume): an 8 MiB random file, cloned with `cp -c`, one byte changed in the clone. `du` gives 8 MiB each, and the private size is 16 KiB each. After the original was removed, the clone's private size was 8 MiB at once.
  - A directory returns 0. A path on devfs does not return the attribute: its bit is clear in `ATTR_CMN_RETURNED_ATTRS`.
  - So measuring each base just before it is removed counts every block exactly once, after the bases that shared it are gone.
- **Why not `statvfs`.** `fs_util::free_bytes` (`fs_util.rs:63-70`) is there, but the volume is shared with every workspace, and agents keep writing during a GC. The APFS clone test already takes "the least of three tries" because "other tests write to the same volume meanwhile" (`backend/apfs.rs:356-357`). A daemon GC with agents running builds would report noise, sometimes negative.
- **Config.**
  - `GcSection` (`daemon/src/config.rs:425-446`) has `undo_window`, `git_executable` and `reindex_limit`. `KEYS` lists them (`:107-108`), `config show` prints them (`:1139-1149`), and `validate` holds the rules (`:1358-1424`; `base_cache.max_entries >= 1` at `:1399-1403`).
  - Durations are humantime strings (`:580-598`, ADR 0013). Other keys use `0s` to mean "none" (`:334`, `:339`).
  - Test-only limits live on `Config`, not in the file (`subscribe`, `presence`, `:671-677`). The defaults test lists every key (`config/tests.rs:135-190`).
  - ADR 0013 says adding a key is "reviewed like an API change" (`docs/adr/0013-daemon-state-directory-and-configuration.md:33`). Its section list does not change here. The keys and defaults table is the Phase 1 README's (`docs/dev-tickets/phase-1/README.md:111`).
- **Daemon loops and shutdown.**
  - The lease expiry loop (`lease/mod.rs:433-450`) is the pattern: a spawned task, `select!` between a stop token and the next tick, and "a reclaim in progress always finishes".
  - Shutdown hooks run in registration order after the server stops (`daemon.rs:143-180`). `bring_up` adds them as quiesce (`:529`), snapshot scheduler (`:538`), lease expiry loop (`:544-557`) and shutdown snapshots (`:558`), with the bind at `:542`.
  - The start sequence in the module doc ends at step 8, the lease loop (`:40-41`).
- **Persisted times.** `meta.last_backup_at` (`db/mod.rs:77-79`) is decimal unix milliseconds. GC reads it with a warning on a value that does not parse (`maintenance/gc.rs:166-178`). `Db::meta` and `set_meta` are at `db/mod.rs:392` and `:436`. A new key needs no migration.
- **Tests and clocks.**
  - `Daemon::start` needs a multi-threaded runtime (`daemon.rs:3-5`), and `start_paused` needs the current-thread one. So paused-clock tests are unit tests, as in `presence/session.rs:367` and `reef-client/src/presence.rs:547`.
  - `TestDaemon::start_with` builds `Config::with_defaults` and skips validation (`testing.rs:49-57`), so tests can set any interval.
  - Spawned daemons write a `FileConfig` to `config.toml` (`tests/common/spawned.rs:135-136`). The load tests run long enough to reach a 10-minute first run, so both harnesses must turn the timer off.
- **Field names.** `reef_lib::telemetry` allows the names in `fields` or a `_count`, `_ms` or `_bytes` suffix (`telemetry.rs:13-18`). The test covers only reef-lib (ADR 0009 at `:42`: "Binaries may log other fields"). The daemon's GC line already mirrors the report's names (`base_bytes_freed`, `keep_newer`).
- **API.**
  - `GcReport` (`reef-api/proto/reef/api/v1/maintenance.proto:65-99`) uses fields 1 to 14. Its comment says "Byte counts are allocated blocks" (`:65-66`), and `base_bytes_freed` is "What they had allocated" (`:85-86`).
  - `API_REVISION` is 5 (`reef-api/src/lib.rs:49`), with one digest per revision (`:51-65`) and `api_digest_matches_revision` (`:313`). ADR 0029 item 17: v1 only grows, nothing is renamed, and every schema change bumps the revision.
  - `optional` scalars are already used (`diff.proto:57`, `files.proto:235`).
  - The client's JSON `GcReport` is `reef-client/src/json.rs:750-800`. The CLI's text line and totals are `reef-vcs/src/cli/render.rs:330-391`, with its test at `:409-436`.
- **Security.** Threat-model S-13 (`docs/threat-model.md:40`, `:85`): `git gc` reads the repository's `.git/config`. jj-lib runs `git gc --prune=@<secs>` (jj-lib 0.46.0 `git_backend.rs:971-978`). A timer runs that without a command, in repositories the user registered, which S-02 already limits to ones the user owns.
- **ADRs.** R1.17 has no ADR. The index ends at 0040 and no PRs are open, so the next number is 0041.

## Decisions

### Size

M, with no `dev-phases.md` row. It changes how R1.17's GC is triggered, as #94 changed what it writes. The work:

- in the daemon: a timer module, one config key, one `meta` key, the maintenance lock naming its holder, the timed run's per-repository loop, and a block-sharing-aware measurement in eviction;
- one optional proto field (API revision 6), the client's JSON and the CLI's text;
- about 20 tests, ADR 0041, and doc edits.

It stays M because the cap on create is left out. With the cap it would be L (a second eviction path under the create lock, and its interplay with quiesce).

### Config

`[gc] interval`, a humantime duration:

- **Default `"1d"`.** At the issue's numbers (93 bases in 10 days, about 1.3 MiB each once sharing is counted), a day adds about nine bases. That is small next to the cap.
- **`"0s"` turns the timer off.** No loop is started, and the daemon logs "GC timer off" at start.
- **Otherwise at least `1h`.** `validate` gains `gc.interval`: "must be 0s (off) or at least 1h". A run holds the repository's creates, moves and writes off, snapshots every active workspace and runs `git gc`. Minutes apart, that would be a steady load for nothing. Tests set shorter intervals directly on `Config`.
- **`"off"` is not accepted.** ADR 0013 has one duration grammar, and `0s` is how other keys say "none". See open question 1.

The key goes in `GcSection` with the doc comment "How often the daemon runs GC by itself, over every repository. Default 1 day; `0s` turns it off; otherwise at least 1 hour." It also goes in `KEYS`, `config show` (`("interval", d(f.gc.interval))`, after `undo_window`), the defaults test, and the Phase 1 README's config table.

Test-only knobs live beside `subscribe` and `presence` on `Config`, not in the file: `gc_timer: maintenance::timer::Limits { start_delay: 10 min, busy_retry: 10 min, check_every: 10 min }`.

### When it runs

- **Due time.** A run is due at `max(last + interval, started + start_delay, busy_until)`:
  - `last` is `meta.last_gc_at`, clamped to now if it is in the future (the clock went back);
  - `started` is the wall-clock time the timer started;
  - `busy_until` is in memory.
- **No last run.** A fresh state directory, or a 0.1 one, has no `last_gc_at`. Its first run is `start_delay` (10 minutes) after start. For an upgraded 0.1 daemon with a bloated cache, that is the fix arriving 10 minutes after the upgrade. It also repairs a checkout a 0.1 GC broke (#94's upgrade note).
- **The start delay** keeps GC off the start path (reindex, loading workspaces, watcher attachment). It also keeps a daemon that restarts often from collecting at every start.
- **Across restarts** the clock is `meta.last_gc_at` (decimal unix milliseconds, as `last_backup_at`; a value that does not parse is ignored with a warning). It is written at the end of:
  - every **timed run** that went through every repository, failures included (they are logged, and the next attempt is a full interval later);
  - every **manual run over every repository** (`reef ws gc` with no `--repo`) that is not a dry run and returned `Ok`.

  A manual run of one repository or a dry run does not write it. A manual full run therefore postpones the timer by a full interval.
- **Wall clock, checked at least every 10 minutes.** The loop sleeps `min(time until due, check_every)` on tokio's clock and re-reads the wall clock each time. Rust's `Instant` on macOS does not advance while the machine sleeps, so a single 24-hour sleep could take days on a laptop. With the check, a run that came due during sleep starts within 10 minutes of waking.
- **No jitter.** There is one daemon per user and the run is local; there is nothing to spread.
- **After a run** the in-memory floor is set to `now + interval`, so a failed `last_gc_at` write cannot cause a hot loop. Only the next start then runs early.

### Concurrency

- **One run at a time.** A timed run takes the maintenance lock with `try_acquire(Task::TimedGc)`.
- **Lock busy.** A manual GC, backup or quiesce holds it. The timer sets `busy_until = now + busy_retry` (10 minutes), logs at `info`, and checks again then. A manual full run that just finished has written `last_gc_at`, so the retry finds nothing due.
- **Manual during timed.** `reef ws gc` while a timed run holds the lock gets `UNAVAILABLE`, as today, now saying which task holds it:
  - `Lock::try_acquire(&self, task: Task) -> Result<Guard, Busy>`, with `Task { Gc, TimedGc, Backup, Quiesce }`;
  - `Busy { holder: Option<Task> }`, displayed as "maintenance in progress (a timed GC); try again when it finishes";
  - the holder is set after the lock is taken and cleared in `Guard`'s drop. A `None` read in that gap prints today's message.

  No reason, no schema change. The CLI's exit code for `Unavailable` is unchanged.
- **Never cancelled mid-run.** The loop awaits the run outside any `select!`, so nothing cuts it between abandoning operations and moving working copies (the RPC's reason for its spawned task).
- **Shutdown.** The timed run checks `state.shutdown` before each repository. Once it is cancelled, the run stops, logs how many repositories it skipped, and writes no `last_gc_at`, so the next start runs again after the start delay. The shutdown hook "gc timer" awaits the loop's handle. It is registered right after the "quiesce" hook and before "snapshot scheduler", so a run in progress finishes its repository while snapshots still work. A quiesce holds the maintenance lock, so no timed run overlaps one.

### What a timed run does

- **All four steps, every repository**, in registration order, through `run_repo` with `dry_run = false`. It is `gc::run_timed(state) -> TimedRun { reports, failures, stopped }`, beside `gc::run`, and it shares `run_repo` and `GcReport`.
- **It goes on past a failed repository.** It logs the error with the repository id and moves on, where `run` stops at the first. One repository with an undeletable state directory would otherwise stop every timed run forever. The manual RPC keeps stopping at the first failure; its caller sees the error.
- **Zero repositories:** the run does nothing, writes `last_gc_at`, and logs at `debug`.
- **Many repositories:** one at a time, each holding only its own create lock, as now.
- **`run_repo` gains `trigger: Trigger { Request, Timer }`.** It is recorded as `trigger` in its span (`"request"` or `"timer"`). The only behavioral difference is the log level of a stopped jj step:
  - `error` for a request, as now;
  - for the timer, `warn` "the jj step of timed GC stopped short; the next run retries", with `error`. A busy checkout or a workspace that would not settle is expected and transient unattended.

  `JjStep.error`'s text keeps "run reef ws gc again when it finishes": that is what a person reading a manual report should do.

### The jj step unattended

It runs, with no more care than a manual run takes. #94's protections are what make that safe:

- the user's checkout is locked before the head is loaded, and a busy lock means nothing is abandoned (logged at `warn`; the next run retries, at most one interval later; the undo window is 7 days, so a day's delay costs nothing);
- the checkout is re-checked before collecting;
- a root stranded by the remaining race (a `jj` command committing before GC loads the head and checking out after the re-check) is repaired by the next run when its files match (ADR 0040's `ToHead`), or handed to jj's recovery otherwise.

The timer makes the race daily instead of rare. It also makes the repair automatic instead of waiting for someone to type `reef ws gc`. A user who would rather choose the moment sets `interval = "0s"` and runs `reef ws gc` themselves. ADR 0041 records this as a consequence.

### Freed bytes

- **`BaseCache` measures as it removes.** `fs_util::footprint(path) -> Result<Footprint, BackendError>`, with `Footprint { allocated_bytes: u64, unshared_bytes: Option<u64> }`, replaces the two `allocated_bytes` calls in `evict` and `evict_one`. It makes one walk with `allocated_bytes`' rules (no symlinks followed, hard links counted once). For each regular file on macOS it also asks `apfs::private_bytes(path) -> io::Result<Option<u64>>`:
  - the call is `getattrlist` with `FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED`, `commonattr = ATTR_CMN_RETURNED_ATTRS` and `forkattr = ATTR_CMNEXT_PRIVATESIZE`;
  - it returns `None` when the returned set lacks the bit;
  - directories and symlinks count 0;
  - if any file returns `None`, the entry's `unshared_bytes` is `None`;
  - off macOS it is always `None`.

  `getattrlistbulk` per directory is allowed if per-file calls prove slow. Each eviction already walks the entry once.
- **Order matters, and evict already has it.** `evict_one` measures an entry just before deleting it, so a block shared between two evicted bases is counted when the second goes, once. `Evicted` gains `unshared_bytes: Option<u64>`.
- **A dry run** reports each entry's current private size. That is a lower bound: blocks shared only among the entries that would go are not counted. The proto comment says so.
- **The report.**
  - `GcReport.base_bytes_unshared: Option<u64>` is `Some(sum)` when every evicted entry was measured, `Some(0)` when none was evicted, and `None` otherwise.
  - `base_bytes_freed` is unchanged (allocated, an upper bound).
  - `wc_bytes_freed` and `store_bytes_freed` stay allocated. State directories are small, and stores are built, not cloned.
- **Linux is unmeasured here.** Reflink bases share blocks too. `FIEMAP`'s `FIEMAP_EXTENT_SHARED` could count them, but btrfs updates sharing through delayed references, so a just-deleted sibling can still look shared. That needs its own measurement on CI's loop-mounted btrfs and XFS. Filed as a follow-up (see "Out of scope"). Copy and overlay bases share nothing, so for them `base_bytes_freed` is already exact.

### API: revision 6, additive

In `maintenance.proto`'s `GcReport`, after `dry_run`:

```proto
  // What removing the base cache entries freed on disk: for each, the bytes
  // no other file or filesystem snapshot shared when it was removed
  // (incremental bases share blocks with the base they were cloned from,
  // which base_bytes_freed counts in full). In a dry run, what each entry
  // holds alone now, a lower bound. Unset where the filesystem cannot tell
  // (measured on APFS only); 0 when nothing was removed.
  optional uint64 base_bytes_unshared = 15;
```

Other comments, which do not change the digest:

- the message comment becomes "Byte counts are allocated blocks, except `base_bytes_unshared`";
- `base_bytes_freed` becomes "What they had allocated: an upper bound on what removing them freed (see `base_bytes_unshared`)";
- `CollectGarbage` gains "The daemon also runs it by itself every `gc.interval` (default a day); UNAVAILABLE while that run is in progress too."

Then:

- `API_REVISION` becomes 6 and `API_DIGESTS` gains its entry, with the comment `// #95: GcReport.base_bytes_unshared.` (computed by the failing `api_digest_matches_revision`);
- `buf breaking` passes, since a new optional field is additive;
- no field is renamed (ADR 0029 item 17);
- v0 is frozen and untouched.

### CLI and client

- **`reef_client::json::GcReport`** gains `base_bytes_unshared: Option<u64>` (JSON `null` when unset), so `reef ws gc -o json` gains one key. Existing keys are unchanged.
- **The text line** (`render.rs`'s `gc_line`):
  - with the field set: `bases 85 removed (388 MiB; 110 MiB unshared)`;
  - unset: as today, `bases 85 removed (388 MiB)`;
  - the total shows the unshared sum only when every report has one.
- **`reef ws gc --help`** gains one sentence: "The daemon also runs this every `gc.interval` (1 day; `0s` turns it off)."

### Logs

New lines use conforming names (a `_count`, `_ms` or `_bytes` suffix):

- start: `info` "GC timer started" (`interval_ms`, `wait_ms` until the first run), or `info` "GC timer off";
- lock busy: `info` "timed GC deferred: maintenance in progress" (`retry_ms`);
- run start: `info` "timed GC started" (`repo_count`);
- run end: `info` "timed GC finished" (`repo_count`, `failed_count`, `elapsed_ms`);
- a repository's failure: `error` "timed GC failed for this repository; the next run retries" (`repo`, `error`);
- stopped for shutdown: `info` "timed GC stopped for shutdown" (`skipped_count`);
- a failed `last_gc_at` write: `warn` "cannot record when GC ran; the next start runs it again" (`error`).

`run_repo`'s "garbage collection finished" line keeps its report-named fields and adds `base_bytes_unshared`, mirroring the report as its siblings do. `tracing` records nothing for `None`. `evict_one`'s "base evicted" line adds `unshared_bytes`. The telemetry test covers reef-lib only, and nothing in reef-lib changes.

### No cap on create

The issue's optional cap (evict when a create materializes a base past `max_entries`) is not built:

- **It needs the create lock exclusively.** That is `evict`'s contract (`base_cache.rs:455-456`), and without it a concurrent create can lose the base it is cloning from. Every create that makes a new base would then stall the creates behind it until the eviction finishes.
- **It needs the maintenance lock.** A quiesce promises no deletions under a filesystem snapshot, so the cap would have to take the lock too. A `reef ws gc` or `reef backup` right after a create would then get `UNAVAILABLE`.
- **The timer already bounds the cache** at `max_entries` plus one interval's new bases (about nine a day here). A user who creates many trees can shorten `interval`.

See open question 2.

### Text

- **`maintenance/gc.rs` module doc**, first paragraph: "Garbage collection (R1.17): on request (`Maintenance.Gc`, `reef ws gc`) and every `gc.interval` from the daemon's timer (`maintenance::timer`, #95, ADR 0041)." Add a paragraph on `meta.last_gc_at`, the timed run going on past a failed repository, and `base_bytes_unshared`.
- **`maintenance/mod.rs` module doc**: name the timer as a maintenance task and say the busy message names the holder.
- **`daemon.rs` module doc**: step 9, "Start the GC timer (#95) unless `gc.interval` is `0s`; its first run is 10 minutes later at the earliest." In the shutdown paragraph, after the quiesce resumes: "a timed GC in progress finishes the repository it is on".

## Deliverables

### `crates/reef-daemon/`

- `src/maintenance/timer.rs` (new), sketched in code after this list.

  ```rust
  pub struct Limits { pub start_delay: Duration, pub busy_retry: Duration, pub check_every: Duration } // 10 min each by default
  pub(crate) enum Ran { Done, Busy }
  pub(crate) trait Runner: Send + Sync + 'static {
      fn now(&self) -> SystemTime;
      fn last_run(&self) -> Option<SystemTime>;
      fn run(&self) -> BoxFuture<'_, Ran>;
  }
  pub(crate) fn wait(interval: Duration, now: SystemTime, last: Option<SystemTime>, earliest: SystemTime) -> Duration;
  pub struct GcTimer { /* handle: Mutex<Option<JoinHandle<()>>> */ }
  impl GcTimer {
      pub fn new() -> Self;
      pub(crate) fn start(&self, interval: Duration, limits: Limits, runner: impl Runner, stop: CancellationToken); // no-op at interval 0
      pub async fn join(&self);
  }
  struct DaemonRunner(Weak<AppState>); // last_run reads meta; run takes try_acquire(Task::TimedGc), then gc::run_timed
  ```

  The loop is generic over `Runner`, so unit tests drive it under a paused tokio clock with a fake wall clock and count runs.
- `src/maintenance/mod.rs`:
  - `pub mod timer`;
  - `Task`, holder tracking, and `try_acquire(task) -> Result<Guard, Busy>`;
  - `Busy { holder }` and its `Display`;
  - callers updated (`service/maintenance.rs:93`, `:127`; `quiesce.rs:161`).
- `src/maintenance/gc.rs`:
  - `Trigger` and the `trigger` span field;
  - `run_timed` and `TimedRun`;
  - `last_gc_at` read and write, and `run` writing it as above;
  - `GcReport.base_bytes_unshared`, filled in;
  - the log changes and the module doc.
- `src/db/mod.rs`: `META_LAST_GC_AT = "last_gc_at"` beside `META_LAST_BACKUP_AT`, documented the same way ("written by GC's full runs, read by the GC timer, #95").
- `src/backend/fs_util.rs`: `Footprint` and `footprint`. `allocated_bytes` stays for its other callers.
- `src/backend/apfs.rs`: `private_bytes`, with the crate's `#[allow(unsafe_code, reason = …)]` and a `// SAFETY:` comment, as `clonefile` has (`:55-74`).
- `src/backend/base_cache.rs`: `evict` and `evict_one` use `footprint`, `Evicted.unshared_bytes`, the `unshared_bytes` log field, and `evict`'s doc on the order.
- `src/service/maintenance.rs`: `From<&GcReport> for v1::GcReport` sets the new field.
- `src/config.rs`: `GcSection.interval`, `KEYS`, `config show`, `validate`, and `Config.gc_timer` (in `with_defaults` and `load`).
- `src/daemon.rs`:
  - in `bring_up` step 6, a `GcTimer` and its "gc timer" hook added right after "quiesce";
  - after the lease loop (step 8), as step 9, `timer.start(..)` with `DaemonRunner` and `state.shutdown`;
  - the module doc.
- `src/testing.rs`: `TestDaemon::start_with` sets `config.file.gc.interval = Duration::ZERO` before `configure`.
- `tests/common/spawned.rs`: the written `FileConfig` has `gc.interval = 0s` unless the caller set one.

### `crates/reef-api/`

- `proto/reef/api/v1/maintenance.proto`: the field and comments above.
- `src/lib.rs`: `API_REVISION = 6` and the digest entry.

### `crates/reef-client/` and `crates/reef-vcs/`

- `reef-client/src/json.rs`: `GcReport.base_bytes_unshared`.
- `reef-vcs/src/cli/render.rs`: `gc_line` and the totals.
- `reef-vcs/src/cli/ws.rs`: the help sentence.

### Documentation

- **`docs/adr/0041-gc-on-a-timer.md`** (accepted, ticket issue #95), indexed in `docs/adr/README.md`:
  - **Context:** #95's numbers (93 bases, 388 MiB reported against about 110 MiB gained), R1.17's "explicit only", and #94 making the jj step safe unattended.
  - **Decision:**
    - `gc.interval` and its validation;
    - the due time from `meta.last_gc_at`, the start delay, and the wall-clock check;
    - the maintenance lock with a busy retry, and the holder in the busy message;
    - all four steps, every repository, going on past failures;
    - the jj step's busy-checkout behavior;
    - shutdown;
    - `base_bytes_unshared` measured with APFS private size just before each removal;
    - no cap on create.
  - **Consequences:**
    - a repository's creates, moves and writes wait out a timed run once a day (seconds on the bench, longer with a large `git gc`);
    - ADR 0040's residual race becomes daily, and its repair automatic;
    - `git gc --prune` runs daily in the user's colocated `.git` (S-13), pruning unreachable objects older than the undo window (git's own default is 2 weeks; reflog-reachable objects are kept by git);
    - subscriptions see GC's rewrite daily (R2.09 already moves caught-up ones);
    - `base_bytes_unshared` is unset on Linux until the follow-up.
  - **Alternatives:**
    - `statvfs` before and after (noise from concurrent writers);
    - renaming `base_bytes_freed` (ADR 0029);
    - a cap on create (above);
    - only steps 2 to 4 on the timer (#94 is fixed);
    - an in-memory clock (a restarting daemon never collects);
    - `"off"` as a spelling;
    - jitter.
- **`docs/dev-tickets/phase-1/README.md`**:
  - the config schema row (`:111`) gains `interval = "1d"` in `[gc]`, with "(#95: `0s` turns the daemon's timer off)", and the row's ticket column gains `#95`;
  - the GC row (`:206`) ends "Never runs automatically in Phase 1; #95 adds `gc.interval`."
- **`CHANGELOG.md`**, under Unreleased:
  - In "Added":
    - "The daemon runs garbage collection by itself every `[gc] interval` (default a day; `0s` turns it off), over every repository, 10 minutes after it starts at the earliest and a full interval after the last full run, timed or `reef ws gc` (#95, ADR 0041, linked). Its reports go to the daemon's log."
    - "API revision 6: `GcReport.base_bytes_unshared`, what removing the base cache entries freed on disk, counting blocks shared between incremental bases once (measured on APFS). `reef ws gc` prints it beside the allocated size."
  - In "Changed": "A GC, backup or quiesce refused because maintenance is running now names what is running, such as a timed GC."
  - In "Upgrading from 0.1": "The daemon now collects garbage by itself, first 10 minutes after the new daemon starts; set `[gc] interval = "0s"` to keep it manual. That first run also repairs a checkout a 0.1 `reef ws gc` broke, as described above."
- **`README.md:45`**: "… and reports what it freed; the daemon runs it daily by itself."
- **`docs/quickstart.md:247-250`**: the `[gc]` example gains `interval = "1d"     # how often the daemon collects garbage by itself; "0s" turns it off`. The sentence after it becomes "The daemon collects garbage daily; `reef ws gc` runs it now, and `--dry-run` reports what it would free."
- **`docs/threat-model.md`**: S-13's row (`:85`) and the paragraph at `:40` say `git gc` runs from `reef ws gc` and from the daemon's GC timer. The rating stays Low; nothing new reaches a repository the user does not own.
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #95 | [issue-95-gc-on-a-timer.md](issue-95-gc-on-a-timer.md) | M | R1.17, #94 | none listed |`.
- **Unchanged:**
  - `docs/dev-phases.md` (no row);
  - R1.17's brief (a record of what was decided then);
  - ADR 0013 (no new section; the key is recorded in ADR 0041 and the Phase 1 table);
  - ADR 0015 (its "`base_cache.max_entries` and R1.17's GC bound them" still holds);
  - ADR 0040;
  - `docs/benchmarks.md`.

## Tests

### Unit tests

In `src/maintenance/timer.rs`. Wall-clock arithmetic is plain `#[test]`; the loop tests are `#[tokio::test(start_paused = true)]` with a `FakeRunner` (a shared `SystemTime` the test advances with `tokio::time::advance`, a run counter, a scripted `Busy` count, and an optional gate that holds a run open).

1. **`wait_is_the_start_delay_without_a_last_run`**: `last = None` gives `earliest - now`.
2. **`wait_counts_from_the_last_run_but_not_before_the_start_delay`**: interval 1 d.
   - Last 2 h ago: 22 h.
   - Last 3 d ago with `earliest` 10 min ahead: 10 min.
   - Last 3 d ago and past `earliest`: zero.
3. **`a_last_run_in_the_future_counts_as_now`**: last = now + 5 d gives exactly the interval.
4. **`runs_after_the_start_delay_then_every_interval`**:
   - advance 10 min: one run;
   - advance 1 d − 1 min: still one;
   - advance 11 min: two.
5. **`a_busy_lock_retries_after_busy_retry`**: the first run answers `Busy`. Advance 10 min: none done. Advance another 10 min: one done.
6. **`a_slept_machine_runs_within_check_every`**: the fake wall clock jumps 3 d while tokio's clock advances only `check_every`. One run follows.
7. **`a_manual_run_postpones_the_next`**: the runner's `last_run` is set to "now" by the test at 12 h. No run at 1 d; one at 1 d 12 h.
8. **`stop_waits_for_a_run_in_progress`**: a gated run is started and the stop token cancelled. `join` is pending until the gate opens, then completes. No second run starts.
9. **`interval_zero_starts_nothing`**: `start` with `Duration::ZERO` spawns no task. `join` returns at once, and the runner is never called.

In `src/maintenance/mod.rs`:

10. **`busy_names_the_holder`**: `try_acquire(Task::TimedGc)`, then a second `try_acquire(Task::Gc)` is `Err(Busy)` whose message contains "a timed GC". After the drop, it is free and `Busy`'s holder is cleared.

In `src/config/tests.rs`:

11. **Updated `defaults_load_with_no_file`**: `f.gc.interval == days(1)`; the source count still equals `KEYS`.
12. **`gc_interval_is_off_or_at_least_an_hour`**:
    - `interval = "0s"` and `"1h"` load;
    - `"30m"` is `ConfigError::Invalid` with key `gc.interval` and the reason "must be 0s (off) or at least 1h";
    - `config show` prints `interval = "1d"` under `[gc]` after `undo_window` (the existing ordered-keys test covers the order).

In `src/backend/apfs.rs` (`cfg(target_os = "macos")`, a temporary directory on the APFS data volume):

13. **`footprint_counts_a_clone_once`**: an 8 MiB file of random bytes in `a/`, `sync_all`, `clone_dir(a, b)`, one byte rewritten in `b/big`.
    - `footprint(a).allocated_bytes >= 8 MiB` and `footprint(b).allocated_bytes >= 8 MiB`;
    - both `unshared_bytes` are `Some(x)` with `x < 1 MiB`;
    - after `remove_dir_all(a)`, `footprint(b).unshared_bytes >= Some(8 MiB)`.
14. **`private_bytes_is_none_off_apfs`**: `private_bytes("/dev/null")` is `Ok(None)` (devfs).

In `src/backend/fs_util.rs`:

15. **`footprint_is_unmeasured_off_macos`** (`cfg(not(target_os = "macos"))`): a written file gives `unshared_bytes == None`, and `allocated_bytes` matches `allocated_bytes()`.

In `src/backend/base_cache.rs` (`cfg(target_os = "macos")`):

16. **`evict_counts_blocks_shared_by_cloned_entries_once`**: entry X materialized with an 8 MiB random file and a small one, then entry Y by `ensure_from(X)` with the APFS cloner and a check-out that rewrites only the small file.
    - `evict(&{}, 0, now, dry_run = true)` reports two entries whose `unshared_bytes` sum is below 1 MiB (the lower bound) and whose `bytes` sum is at least 16 MiB.
    - The real `evict` reports an `unshared_bytes` sum of at least 8 MiB and below the `bytes` sum.

### `crates/reef-daemon/tests/gc.rs`

17. **Updated `base_cache_evicts_unreferenced`** (test 3): `report.base_bytes_unshared.is_some() == cfg!(target_os = "macos")`. When set, it is above 0 and at most `base_bytes_freed`. `three_trees` builds `b` and `c` incrementally on APFS.
18. **Updated `gc_refuses_during_maintenance`** (test 7): holds `try_acquire(Task::TimedGc)`. The error is `Unavailable`, and its message contains "maintenance in progress" and "timed GC".
19. **`timed_gc_runs_after_a_restart_and_records_its_time`**:
    - Setup: `fixture()` (timer off, `undo_window = 0s`, `max_entries = 1`), `three_trees`, then release `a` and `b`. Then set `daemon.config.file.gc.interval = 1h` and `daemon.config.gc_timer.start_delay = 0`, and `restart()`.
    - Poll `state().db.meta(META_LAST_GC_AT)` every 100 ms for up to 60 s. Once it is set:
      - `base_dirs` holds only `c`'s base;
      - `a`'s and `b`'s rows are gone (`workspace(id)` is `NotFound`);
      - `c`'s workspace still snapshots.
    - `restart()` again with the same config, then wait 2 s: `last_gc_at` is unchanged (not due for an hour).
20. **`only_a_full_manual_gc_records_its_time`**:
    - `gc --repo <id>`: no `last_gc_at`;
    - a dry run of every repository: none;
    - `collect_garbage("", false)`: set, and within the last minute.
21. **`timed_gc_with_a_locked_root_abandons_nothing_and_finishes`**:
    - Setup: `old_root_fixture()` (#94's, default undo window over backdated history), with `base_cache.max_entries = 1` set on `daemon.config` before the restart, and two workspaces on two trees with the first released. Restart with the timer on and `start_delay = 0`, while the test holds the root's `working_copy.lock` (`FileLock::lock`).
    - Once `last_gc_at` is set:
      - `op_count(root)` is unchanged;
      - the daemon still serves `daemon_status`;
      - the released workspace's base is evicted (step 2 ran).
    - After the lock is dropped, a manual `gc` reports `jj_error == ""` and `op_count` drops.
22. **`timed_gc_goes_on_past_a_failed_repository`** (skipped when `geteuid() == 0`):
    - Setup: two synthetic repositories, A registered first. A has a released workspace whose `wc/<id>` directory holds a file and is made `0o500`, so `remove_dir_all` fails with `EACCES`. B has two released workspaces on two trees. Then restart with the timer on.
    - Once `last_gc_at` is set:
      - B's released bases are evicted;
      - A's released row is still there.
    - The mode is restored for cleanup.
23. **`timer_off_runs_nothing`**: `fixture()`, one released workspace, restart with `interval = 0s` and `start_delay = 0`. After 2 s there is no `last_gc_at` and the base is still there.

### `crates/reef-api/`

24. **`api_digest_matches_revision`**, unchanged code with the new revision 6 and digest.

### `crates/reef-vcs/src/cli/render.rs`

25. **Updated `gc_reports_read_as_one_line_each_and_a_total`**:
    - one report has `base_bytes_unshared: Some(110 << 20)` and prints `bases 3 removed (1.2 GiB; 110 MiB unshared)`;
    - the other is `None` and prints as today;
    - the total omits the unshared figure;
    - the JSON has `"base_bytes_unshared": 115343360` and `null`.
    - A second case, with both set, shows the summed unshared figure in the total.

Every other existing test passes unchanged. In particular: R1.17's GC tests (with test 7's acquire updated), #94's `root_jj_works_after_gc` and `gc_abandons_nothing_while_the_root_is_locked`, both subscription GC tests, `backup.rs` (the backup's busy path through the new `try_acquire`), and the quiesce tests.

## Implementation notes

- **Order:**
  1. `Task` and `Busy` with test 10, and the callers;
  2. `footprint`, `private_bytes` and eviction with tests 13 to 16;
  3. the proto field, revision, client and CLI with tests 17, 24 and 25;
  4. the config key with tests 11 and 12;
  5. `timer.rs` with tests 1 to 9;
  6. `run_timed`, `last_gc_at`, `Trigger`, and the daemon wiring and harness defaults, with tests 18 to 23;
  7. ADR 0041 and the documents.
- **Turn the timer off in both test harnesses first.** Otherwise a long test (the load tests, R1.23) gets a GC 10 minutes in.
- **Never `select!` over a run.** Only the sleep is raced against the stop token.
- **`private_bytes` on a path that is not a regular file** is not called. The walk already has the file type from `walkdir`'s metadata.
- **Linux:** `footprint`'s non-macOS branch and the timer are not `cfg(target_os = "linux")`-only, but macOS never compiles the `cfg(not(target_os = "macos"))` branch. Run `cargo clippy -p reef-daemon --all-targets --all-features -D warnings` and `cargo nextest run -p reef-daemon --lib timer footprint` plus `--test gc` in the privileged `ubuntu:24.04` container before pushing (`docs/benchmarks.md`, "Linux"). Test 22 skips there as root.
- **Timing tests flake under load** (lease, snapshot and `ws_run` tests do on `main`). Tests 19, 21 and 22 poll with a 60 s deadline and never assert durations. Tests 19 and 23 use a 2 s negative wait, which can only pass falsely, never fail falsely.
- **`buf`** is not installed on the Mac. CI's `buf lint` and `buf breaking` check the proto.

## Acceptance criteria

- [ ] With the default config, a daemon runs GC over every repository at most 10 minutes after start when none has run, then every day, and a restart does not reset that (tests 4, 19).
- [ ] `gc.interval = "0s"` runs nothing; values under an hour are refused at load (tests 9, 12, 23).
- [ ] A manual full GC postpones the timer, and a timed run postpones itself while maintenance runs (tests 5, 7, 20).
- [ ] A timed run with the user's checkout locked abandons nothing, finishes the other steps, and the next run abandons (test 21). A failed repository does not stop the others (test 22).
- [ ] Shutdown waits for a run in progress to finish its repository (test 8).
- [ ] `GcReport.base_bytes_unshared` counts blocks shared between cloned bases once on APFS and is unset elsewhere (tests 13, 16, 17). It is API revision 6, additive, `buf breaking` clean.
- [ ] A busy maintenance refusal names a timed GC (tests 10, 18).
- [ ] ADR 0041 written and indexed; the config table, help, module docs, CHANGELOG, README, quickstart, threat model and phase README updated; `just ci` passes; the Linux container run passes.

## Verification

```sh
cargo nextest run -p reef-daemon --lib timer maintenance footprint base_cache config
cargo nextest run -p reef-daemon --test gc --test backup --test daemon
cargo nextest run -p reef-api -p reef-client -p reef-vcs
just ci
# By hand: a scratch daemon with interval = "1h" and a bloated base cache.
# The log shows "GC timer started", then "timed GC finished"
# 10 minutes later, and `reef ws gc -o json` afterwards shows
# base_bytes_unshared near the free-space change.
```

## Inherited scope

- **From R1.17:** the four steps, `keep_newer`, the maintenance lock, the exclusive create lock, the report, and eviction's order. This ticket adds a second trigger and a measured figure. It drops "nothing runs it on a timer".
- **From #94 and ADR 0040:** the busy-checkout refusal, the re-check before collecting, and repair by the next run. They are why the jj step runs unattended.
- **From R2.09 and ADR 0036:** GC's callback moves caught-up subscriptions; a timed run uses the same path.

## Out of scope

- **A cap on create** (above; open question 2).
- **Measuring unshared bytes on Linux reflink filesystems** (`FIEMAP_EXTENT_SHARED`, with btrfs's delayed references checked on CI's loop-mounted images). File it as an issue when this lands, linked from ADR 0041.
- **Reporting the last and next GC** in `GetDaemonStatus` or `reef daemon status` (open question 3).
- **Presence for a timed GC** (a `PROGRESS` entry while it holds creates off). Agents see a create wait, not fail.
- **Disk-pressure triggers** (run when the volume is nearly full), which R1.17 also deferred.
- **S-13's hardening of `git gc`** (#64), which the timer makes more routine but not more exposed.

## Known limits

- **A timed run holds a repository's creates, moves and writes off** while it runs, and its repository thread's jobs queue behind `git gc`. That is a few seconds a day on the bench, and more for a large repository's `git gc`.
- **ADR 0040's residual race** can now happen at a moment nobody chose. The next run (a day later) or a manual `reef ws gc` repairs it.
- **`base_bytes_unshared` is unset on Linux.** In a dry run it is a lower bound.
- **A daemon that never stays up 10 minutes never collects by itself.**
- **The interval is read at start**, as every other key is: changing it needs a daemon restart.

## What later tickets inherit

- `maintenance::timer`'s `Runner` loop (wall-clock due time, busy retry, shutdown) for any later periodic maintenance, such as automatic backups.
- `fs_util::footprint` for anything that reports what a delete frees (workspace release, `ws list`'s disk usage), and the Linux follow-up fills its `None`.
- `Task` in the maintenance lock, so later tasks are named in `Busy`.

## Open questions

1. Accept `"off"` as well as `"0s"` for `gc.interval`, as the issue suggested? Default: no. One duration grammar (ADR 0013), and `0s` is how other keys say none. `config show` would have to print one of two spellings back.
2. Build the cap on create after all? Default: no. It needs the create lock exclusively per new base and the maintenance lock against quiesce. The timer bounds the cache to a day's growth (about nine bases, roughly 12 MiB at the issue's numbers), and a shorter `interval` is the lever.
3. Should `GetDaemonStatus` report `last_gc_at` and the next due time (API revision 7, a `daemon status` line)? Default: not in this ticket. The log carries both, and `meta.last_gc_at` is in the database for a later ticket.
4. Should the first run on a state directory with no `last_gc_at` wait a full interval instead of 10 minutes? Default: 10 minutes. The users who need it most are upgrading from 0.1 with a bloated cache and possibly a checkout a 0.1 GC broke.
