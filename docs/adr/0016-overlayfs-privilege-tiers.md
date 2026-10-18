# 0016. overlayfs on Linux: privilege tiers

- Status: accepted
- Date: 2026-10-17
- Ticket: R1.09

## Context

On Linux the cheapest workspace is an overlay mount: the base cache entry ([0015](0015-materialization-backends-and-base-cache.md)) is the read-only lower layer and a per-workspace upper directory takes the writes ([Workspaces](../design.md#workspaces): "Linux: overlayfs: read-only snapshot as lower layer, thin writable upper layer. Cost to create: mount, milliseconds. Disk per workspace: modified files only."). A mount costs the same whatever the tree's size, and the upper directory is exactly what the workspace adds.

Mounting needs privilege, and machines differ in what they give a daemon:

- **Kernel overlayfs** needs `CAP_SYS_ADMIN` in the initial user namespace for a mount everyone can see. A daemon run as root (a CI runner with passwordless `sudo`, a dedicated build VM, a container started `--privileged`) has it; a daemon run as the developer does not.
- **`fuse-overlayfs`** (`containers/fuse-overlayfs`, packaged by Ubuntu 24.04 as 1.13) mounts unprivileged through FUSE: it needs the binary, `/dev/fuse`, and the setuid `fusermount3` to unmount. It costs a user-space server process per mount and a context switch per file operation.
- **User namespaces** let an unprivileged process mount kernel overlayfs (`userxattr`), but the mount exists only inside the new mount namespace. An agent must be able to `cd` into its workspace from any shell, which a private namespace does not allow.

The kernel rules constrain the rest of the design. Changing a lower layer while it is mounted is undefined. The upper and work directories must be on one filesystem that supports `trusted.*` (or `user.*`) xattrs and `d_type`, so not NFS and not another overlay. Without `redirect_dir=on`, renaming a directory that came from the lower layer fails with `EXDEV`.

## Decision

We will support two tiers behind one backend module, `reef_daemon::backend::overlay`, and no user namespaces in Phase 1.

1. **Kernel tier** (`BackendKind::Overlayfs`): `mount(2)` of `lowerdir=<base>,upperdir=<root>.upper,workdir=<root>.work,redirect_dir=on,index=off,metacopy=off`, with `,`, `:` and `\` in paths escaped by a backslash. `redirect_dir=on` lets agents rename unmodified directories, which build tools expect to work. `index=off` and `metacopy=off` keep the lower layer swappable and never written. A kernel that refuses `redirect_dir=on` with `EINVAL` gets the mount without it, and the daemon warns once that directory renames across layers fail. `userxattr` is not used: the daemon is privileged. `MS_NOSUID` only if `workspace.overlay_nosuid` is set (default off, since build tools may need setuid helpers).
2. **FUSE tier** (`BackendKind::FuseOverlayfs`): the `workspace.fuse_overlayfs` binary (default `fuse-overlayfs` on `PATH`) with the same three layer options and no `-f`, so it daemonizes and its mount survives a daemon restart like a kernel mount. The daemon waits up to 5 s for the mount to appear in `/proc/self/mounts`. Unmounting is `fusermount3 -u` (or `fusermount`), `-uz` for a lazy unmount.
3. **The probe** tries the kernel tier first, then FUSE. Kernel: `CapEff` in `/proc/self/status` has bit 21, then a trial mount of an empty overlay in a `.reef-probe-*` directory where workspaces go. FUSE: the binary is found, `/dev/fuse` opens read-write, then the same trial mount within 5 s. Each unavailable tier reports its reason (`no CAP_SYS_ADMIN`, `trial mount failed: EPERM: ...`, `ENODEV (overlay module missing)`, `fuse-overlayfs not on PATH`, `/dev/fuse: ...`, `trial mount timed out`). The trial catches an upper filesystem overlayfs refuses before any workspace is created. `auto` falls through to reflink (R1.10) and then the copy.
4. **One code path otherwise.** Materialize creates the root, `<root>.upper` (`0755`, since the merged root takes the upper root's attributes) and `<root>.work` (`0700`) and mounts. Tear-down unmounts, lazily (`MNT_DETACH`, `fusermount3 -uz`) if the mount is busy, then removes the empty root and deletes the layers in the background. Swapping the base unmounts, replaces both layers with empty ones, and mounts the new base (`SwapOutcome::TreeReplaced`). Disk usage is exact: the allocated blocks under the upper directory, and its files, symlinks, and whiteouts.
5. **Mounts are state the daemon reconciles at start**, before it loads workspaces. An `active` or `releasing` overlay workspace whose mount is gone, or whose FUSE server died (`ENOTCONN`), is mounted again over its existing upper directory. One whose upper directory is gone is marked `attention = 'upper-missing'`; one that cannot be mounted again (no privilege any more, base missing) is marked `attention = 'mount-failed'`. Overlay mounts under the state directory's `repos/` or a workspace root directory that no such workspace owns, probe mounts included, are unmounted lazily and removed.
6. **A snapshot refuses an unmounted overlay.** Every snapshot first asks the backend whether the workspace's root holds its files (`Backend::check_materialized`); the overlay backends check the root is mounted with their filesystem type. A scan of the bare mountpoint would otherwise record every file as deleted.

## Consequences

- Creating a workspace on Linux costs one mount, independent of the tree's size, when either tier is available. Disk per workspace is what it changed.
- A daemon run as root serves kernel mounts. An unprivileged daemon needs `fuse-overlayfs` installed, which Reef does not ship, or falls back to reflink or copy; `reef daemon status` shows why.
- Lower layers must never change while mounted. Base cache entries are immutable once built (0015), and GC never evicts a base an `active` or `releasing` workspace uses (R1.17). Any future feature that edits a base in place must unmount its users first.
- Mounts outlive the daemon, by design, so a restart keeps every workspace usable. The cost is reconciliation at start and the stale-mount cleanup, which only looks under the state directory's `repos/` and the configured workspace roots: a crashed `--path` workspace elsewhere keeps its mount until someone unmounts it.
- A lazy unmount keeps the kernel mount alive while a process still holds a file or a cwd in it, so its disk is freed when that process exits. The lease model assumes agents' processes die (R1.23 kills them).
- The FUSE tier costs a server process per workspace and is slower per file operation than the kernel tier. R1.12 and R1.23 measure it.
- The upper directory must be on a filesystem overlayfs accepts: the state directory on NFS, or a daemon inside a container whose root filesystem is itself an overlay, makes both probes fail. Configuring `workspace.root` onto a suitable volume fixes it.
- CI runs the kernel tier as root in a dedicated `overlayfs` job (`--features privileged-tests`) and the FUSE tier unprivileged in the same job.

## Alternatives considered

- **User namespaces, with `setns` from `reef ws run`.** An unprivileged daemon creates a user and mount namespace per workspace and mounts kernel overlayfs with `userxattr` inside it; `reef ws run` joins the namespace before running the agent's command. It needs no privilege and no FUSE, but a shell that did not come through `reef ws run` sees an empty directory, which breaks the basic promise that a workspace is a directory. It returns as an option for a sandboxed `ws run`, where the private namespace is the point.
- **A setuid mount helper** that mounts overlays only under the state directory. It gives kernel performance to an unprivileged daemon, but Reef would ship and maintain a setuid binary, with the review burden and distribution packaging that brings. `fusermount3` already is the setuid helper FUSE needs.
- **`bubblewrap`** to set up the namespace and mounts. It is the user-namespace alternative with a well-reviewed tool doing the setup, and has the same visibility problem.
- **Only the copy and reflink backends on Linux.** No privilege question at all, but creation is O(tree) on ext4 (the most common Linux filesystem), and the 100,000-file target would cost seconds per workspace.
