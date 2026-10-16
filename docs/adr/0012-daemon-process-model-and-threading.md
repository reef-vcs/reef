# 0012. Daemon process model and threading

- Status: accepted
- Date: 2026-10-16
- Ticket: R1.03

## Context

Agents and the CLI talk to one local daemon per machine over gRPC on a Unix socket ([0004](0004-grpc-over-unix-socket.md)). That daemon serves hundreds of workspaces across a few repositories, each workspace snapshotted on every burst of edits, so it is concurrent by nature: a gRPC server, timers, a lease loop, and watcher events on one side; jj scans, check-outs, and transactions on the other.

jj-lib does not fit an async runtime ([0008](0008-jj-lib-embedding-findings.md)). Its futures are not `Send`, so tokio cannot move them between workers, and they do blocking file I/O, so polling them on a worker stalls every other task on it. Its `Workspace` (holding the working copy) is `Send` but not `Sync`. Its `ReadonlyRepo` is `Send + Sync`, and so is `Arc<ReadonlyRepo>` (every field is declared so in jj-lib 0.46.0, including the index trait objects and the `once_cell::sync::OnceCell` holding the change id index), as is `MergedTree`.

Metadata lives in one SQLite file with one writer ([0003](0003-sqlite-for-daemon-metadata.md)). Two daemons on one state directory would corrupt each other's view of leases and workspaces, and an integration test suite that spawns a binary per test would be slow and hard to debug.

## Decision

We will run the daemon as one process with this threading model:

- **tokio for I/O.** A multi-threaded tokio runtime hosts the gRPC server, timers, the lease loop, and watcher event handling. Nothing from jj-lib runs on a tokio worker.
- **One thread per repository.** Each registered repository gets a dedicated `std::thread` owning a `RepoHead`: the user's default workspace loaded at head, the jj settings, its root, and whether it is colocated. Every jj transaction, every reload, every `git::import_refs`, and the GC of that repository run on this thread, as boxed `FnOnce(&mut RepoHead)` jobs sent through an `mpsc` channel, each answered through a oneshot. Jobs run one at a time in the order sent, and the thread reloads at head after any job that reports it committed an operation. The thread is the repository's write lock: a backup pauses it (with a deadline) to copy a consistent set of operation heads.
- **Blocking threads for workspaces.** A workspace's scan, check-out, and `finish` run on `tokio::task::spawn_blocking`, driven by `pollster::block_on`, holding that workspace's `Arc<std::sync::Mutex<..>>`. Their concurrency is bounded by a semaphore (R1.13).
- **What crosses threads.** Ids, plain structs, and `Arc<ReadonlyRepo>`. A static assertion that `Arc<ReadonlyRepo>` and `MergedTree` are `Send + Sync` makes a jj-lib upgrade that breaks this a compile error.
- **Single instance.** An advisory `flock` on `<state>/daemon.lock`, holding the owner's pid, taken before anything else is touched. A second daemon on the same state directory exits 1 naming the holder's pid. A stale socket file is removed only with the lock held, and only if nothing answers on it, so a daemon on another state directory configured with the same socket is refused rather than displaced.
- **Graceful shutdown.** SIGTERM and SIGINT stop the server accepting connections and give in-flight requests 5 s; then shutdown hooks run (final snapshots, R1.13 and R1.15), every repository thread drains its queue and stops, and the socket and pid files are removed. The lock file stays; the kernel drops the lock with the process, so a crash never leaves a stale lock.
- **In-process start.** `Daemon::start(Config)` runs the same start sequence as the binary on the caller's runtime and returns a handle (socket path, shutdown, join). Integration tests run daemons in temporary state directories without spawning a binary; the binary is `run_main` around it, which `reef daemon run` (R1.22) also calls.

This ADR does not cover the configuration file (R1.04, ADR 0013), the API's services and error mapping (R1.05, ADR 0017), or the watcher (R1.13).

## Consequences

- Every jj mutation of a repository is serialized on its thread. That is the property R1.18's quiesce relies on, and it means no two Reef operations race to write jj operation heads; divergent heads only arise from the `jj` CLI used on the registered root.
- Scans, the expensive part of a snapshot, parallelize across workspaces; only the short commit half queues on the repository thread.
- A slow transaction (a large rebase, a GC) delays every snapshot commit of that repository behind it. R1.23 measures the queueing under load; if it matters, GC moves to a reloaded `ReadonlyRepo` on another thread with only its final commit on the repository thread.
- One loaded repository (index, view, store caches) per repository, shared by its workspaces through `Arc<ReadonlyRepo>`, not one per workspace.
- A job that panics fails alone: its caller gets an error and the thread reloads at head and keeps serving.
- Code that calls jj-lib must run as a repository job or a blocking task; a jj-lib future awaited directly in an async handler is a bug (and usually fails to compile, since handlers must be `Send`).
- Tests start real daemons cheaply, so every service is tested end to end over the socket.
- Shutdown can exceed 5 s by the time the hooks and the last queued jobs take; the 5 s bounds only in-flight RPCs.

## Alternatives considered

**One thread per workspace, each holding its own loaded repository.** Workspaces would never wait on each other. Rejected: N loaded indexes and views for N workspaces of one repository, N reloads after every operation, and transactions on one repository racing each other's operation heads, which jj then merges as divergent operations.

**jj-lib on tokio with `spawn_local`.** A `LocalSet` per thread can run non-`Send` futures. Rejected: the futures still block on file I/O, so each local set needs its own thread anyway, and the result is the repository thread with an extra runtime inside it.

**A process per repository.** Isolation between repositories, and a crash takes down one. Rejected: SQLite would have several writers, the CLI would need one socket per repository and a broker to find them, and leases and GC would coordinate across processes.

**No single-instance lock; rely on the socket.** A daemon that finds a live socket exits. Rejected: the socket may live outside the state directory (`REEF_SOCKET`, `$XDG_RUNTIME_DIR`), so two daemons on one state directory with different sockets would both run; and a socket file survives a crash, while a `flock` does not.
