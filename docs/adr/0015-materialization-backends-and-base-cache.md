# 0015. Materialization backends and the base cache

- Status: accepted
- Date: 2026-10-16
- Ticket: R1.02

## Context

One daemon serves many workspaces per machine, often several on one tree, and each must be cheap to create: under a second on a 100,000-file tree ([Workspaces](../design.md#workspaces): "Creation shares every unchanged byte with the store"). Every platform has a different copy-on-write primitive: `clonefile(2)` on APFS, `FICLONE` on btrfs and XFS, overlayfs (kernel or FUSE) on any Linux filesystem, and a plain copy where nothing else works. A Reef workspace is a jj workspace whose working-copy state lives outside the tree ([0014](0014-workspace-model-on-jj.md)), so jj-lib's working copy scans the directory and builds the tree on every snapshot; nothing below jj needs to capture a tree.

A primitive needs a source. A clone backend clones a directory that already holds the tree, and an overlay needs a lower layer. jj can write a tree into a directory (`LocalWorkingCopy::check_out`), and in doing so records every file's mtime and size in its `tree_state` file, which holds no absolute path and so is valid for any directory with byte-identical files and the same mtimes and sizes (verified in jj-lib 0.46.0 for R1.02).

## Decision

We will put a `Backend` trait between the daemon and the primitives, and feed every backend from a per-machine base cache materialized through jj-lib.

- **The trait** (`reef_daemon::backend::Backend`): `materialize(base, ws)` makes a workspace root hold a base's files with mtimes and sizes preserved; `disk_usage(base, ws)` estimates what the workspace adds; `swap_base(ws, from, to)` moves it to another tree, returning `TreeReplaced` (overlays: swap the lower layer, empty the upper; the daemon resets the working copy) or `NeedsCheckout` (clones and copies: the daemon applies the diff with jj's check-out); `tear_down(ws)` removes the root, upper, and work directories without following symlinks. Methods are synchronous and run on blocking threads. Backends never capture a tree.
- **Kinds and selection.** `copy`, `apfs`, `reflink`, `overlayfs`, `fuse-overlayfs`, stored by those names. `probe(dir)` reports for each kind whether it can serve a directory and why not. `auto` takes the first available in the order overlayfs, fuse-overlayfs, apfs, reflink, copy; a named backend that is unavailable is an error naming the reason, never a silent fallback. Copy is always available.
- **The base cache.** A tree is materialized once per machine into `<state>/repos/<repo-id>/bases/<tree-id>/` by a throwaway `LocalWorkingCopy` (initialized on the empty tree, checked out to the commit, finished), whose state directory becomes `bases/<tree-id>.state/`. The key is the tree id in hex, or a conflicted tree's ids joined with `+` in jj's `Merge` order; a conflicted tree is written with jj's conflict markers, as in any jj working copy. Both directories are written under `.tmp` names, renamed state first, and recorded in the `base_cache` table last; a directory without a row is incomplete and swept at daemon start. Concurrent requests for one tree materialize it once.
- **Seeding.** A workspace's `tree_state` starts as a copy of its base's, so its first snapshot is a no-op scan rather than a rehash of every file (R1.05). This is why backends must preserve mtimes and sizes, which R1.11 tests.
- **Immutability by convention.** Nothing writes under a base after its rename. Permissions stay as jj wrote them, because backends read them and overlayfs needs them readable.

This ADR does not cover any primitive's details (R1.07 to R1.10, ADR 0016 (R1.09) for overlayfs) or base cache eviction (R1.17). Incremental bases cloned from a nearby one (R1.08) are in the amendment at the end.

## Consequences

- A tree is written once per machine, whatever the number of workspaces on it. The first workspace on a new tree pays a full check-out (40.5 s for the 100,014-file `large` preset in a release build on an M2 Max, measured for R1.02), later ones pay only the backend's primitive.
- The first snapshot of a new workspace is a scan with no hashing, provided the backend kept mtimes and sizes. A backend that cannot keep them is still correct, only slower: the fallback is `locked_wc.reset`, which makes the next snapshot rehash every file.
- Backends are small: they move bytes and never touch jj. Adding one is an arm in `backend_for`, a probe, and the conformance suite.
- `DiskUsage` is an estimate: overlay backends count the upper directory's allocated blocks; clone and copy backends count the allocated blocks of files whose mtime or size differs from the base's `tree_state`, by a stat-only walk. Phase targets are measured by free-space deltas instead.
- Bases accumulate one per tree ever materialized. `base_cache.max_entries` and R1.17's GC bound them; a base a workspace still uses is never evicted.
- Reading the base's `tree_state` uses jj-lib's public protobuf message (`jj_lib::protos::local_working_copy::TreeState`). A jj-lib upgrade that changes it breaks the disk usage estimate, not correctness, and only `reef_lib::jj::materialize` changes.
- Conflicted trees work like any other: their key contains `+`, which is safe in a file name.
- Logs name a base by its key in a `tree_id` field, which this decision adds to the field names of [0009](0009-error-and-logging-conventions.md) (`reef_lib::telemetry::fields::TREE_ID`).
- The base cache is crash-safe against the daemon dying, not against power loss: jj writes the tree without fsync, so a power cut can leave a recorded entry with a truncated `tree_state` or file. The start-up sweep drops an entry whose `tree_state` does not decode to the recorded file count; a truncated tree file whose state entry survived is not detected (fsyncing every file would add seconds to a `large` materialization).
- An entry whose tree is on disk without its state is reported as incomplete and left in place while the daemon runs, because an overlay may be using the tree as its lower layer; the start-up sweep, which runs before overlay mounts are re-established, drops it.

## Alternatives considered

**Backends build trees themselves.** Each backend would capture its writable layer (the overlay's upper directory, or a clone's changed files) and write the tree. Rejected: it reimplements jj's snapshot (ignores, file states, conflict handling, executable bits) once per backend, and diverges from what `jj` itself would record.

**Base cache keyed by commit.** Simpler to look up from a workspace's commit. Rejected: rewriting a commit (a description change, a rebase that does not touch files) changes its id but not its tree, so every rewrite would materialize an identical tree again.

**Materialize by walking `MergedTree` by hand.** A Reef walker writing blobs straight from the store. Rejected: it loses jj's conflict materialization, its EOL and executable-bit handling, and the `tree_state` it writes as a side effect, which is what makes seeding possible.

**No base cache: clone or check out straight from the store per workspace.** Rejected: a check-out per workspace is O(repository) every time (seconds on the `large` preset), and clone and overlay primitives need a directory to clone or lay over.

## Amendment, 2026-10-17 (R1.08): directory clones on APFS, and incremental bases

- **The `apfs` backend clones the whole base directory with one `clonefile(2)` call.** `man 2 clonefile` (macOS 27) says cloning directory hierarchies "is strongly discouraged" and recommends `copyfile(3)`, which clones item by item. On the 102,005-file `large` tree on an M2 Max, one directory call takes 0.93 to 0.97 s with a warm cache (1.6 s on the first, cold run) and about 30 MiB of free space, against 13.3 s for a per-file `clonefile` walk and 15.2 s for `cp -cR`. Nothing else meets the one-second creation criterion, so the directory call is used despite the man page, and the per-file walk (`reflink_copy::reflink` per file under `copy_tree`) is only the fallback when the directory call fails. `ENOTSUP` and `EXDEV` make the backend unavailable rather than falling back, because per-file clones fail the same way and a plain copy under the `apfs` name would hide the cost. If a macOS release breaks or slows directory clones, the fallback keeps workspaces working at about 13 s each and the criterion fails; the alternative left is an APFS snapshot or volume per workspace, which needs `diskutil` and root.
- **Clone backends probe against the base cache.** A clone needs the base and the workspace on one volume, so `probe` takes the base cache directory and the `apfs` probe clones a one-byte `bases/.probe` file into the workspace directory (removed again). A `--path` on another volume gets `apfs` unavailable with "different volume", and `auto` falls through to `copy` with the usual warning.
- **Incremental bases.** A backend with a whole-directory clone (`Backend::dir_cloner`) builds a new base from the most recently used one: `BaseCache::ensure_from` clones that base's tree and state into the `.tmp` names, and `Repo::materialize_tree_from` loads a throwaway working copy on them and checks out the new commit, so jj writes only the files that differ. A check-out that skips files is an error, since a base must be complete. Any failure removes the temporary directories and the daemon falls back to the full check-out. Bases are still never written after their rename, so cloning one while workspaces are cloned from it is safe.
