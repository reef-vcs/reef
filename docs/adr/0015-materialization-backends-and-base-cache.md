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

This ADR does not cover any primitive's details (R1.07 to R1.10, ADR 0016 (R1.09) for overlayfs), base cache eviction (R1.17), or incremental bases cloned from a nearby one (R1.08).

## Consequences

- A tree is written once per machine, whatever the number of workspaces on it. The first workspace on a new tree pays a full check-out (0.8 s for the 1,014-file `small` preset in a debug build; the `large` figure is in the R1.02 PR), later ones pay only the backend's primitive.
- The first snapshot of a new workspace is a scan with no hashing, provided the backend kept mtimes and sizes. A backend that cannot keep them is still correct, only slower: the fallback is `locked_wc.reset`, which makes the next snapshot rehash every file.
- Backends are small: they move bytes and never touch jj. Adding one is an arm in `backend_for`, a probe, and the conformance suite.
- `DiskUsage` is an estimate: overlay backends count the upper directory's allocated blocks; clone and copy backends count the allocated blocks of files whose mtime or size differs from the base's `tree_state`, by a stat-only walk. Phase targets are measured by free-space deltas instead.
- Bases accumulate one per tree ever materialized. `base_cache.max_entries` and R1.17's GC bound them; a base a workspace still uses is never evicted.
- Reading the base's `tree_state` uses jj-lib's public protobuf message (`jj_lib::protos::local_working_copy::TreeState`). A jj-lib upgrade that changes it breaks the disk usage estimate, not correctness, and only `reef_lib::jj::materialize` changes.
- Conflicted trees work like any other: their key contains `+`, which is safe in a file name.

## Alternatives considered

**Backends build trees themselves.** Each backend would capture its writable layer (the overlay's upper directory, or a clone's changed files) and write the tree. Rejected: it reimplements jj's snapshot (ignores, file states, conflict handling, executable bits) once per backend, and diverges from what `jj` itself would record.

**Base cache keyed by commit.** Simpler to look up from a workspace's commit. Rejected: rewriting a commit (a description change, a rebase that does not touch files) changes its id but not its tree, so every rewrite would materialize an identical tree again.

**Materialize by walking `MergedTree` by hand.** A Reef walker writing blobs straight from the store. Rejected: it loses jj's conflict materialization, its EOL and executable-bit handling, and the `tree_state` it writes as a side effect, which is what makes seeding possible.

**No base cache: clone or check out straight from the store per workspace.** Rejected: a check-out per workspace is O(repository) every time (seconds on the `large` preset), and clone and overlay primitives need a directory to clone or lay over.
