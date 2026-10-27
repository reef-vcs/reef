//! A store build (R1.20): the daemon runs a `[[store]]`'s `build` argv once
//! and seals what it wrote.
//!
//! 1. Resolve the revision, read its `reef.toml`, and hash the store's
//!    lockfile in its tree. A sealed store for the hash is the answer
//!    unless `force`, which active workspaces mounting it refuse
//!    (`Conflict`).
//! 2. Mark the build running (one per store and hash) and record the row
//!    unsealed; a forced rebuild keeps the sealed row and the store until
//!    the new one is sealed.
//! 3. Create a scratch workspace through the same path as any create: a
//!    new change on the revision, owner `reef-store-build`, the copy
//!    backend (a build must not depend on a clone or overlay backend), the
//!    other stores mounted where built, and its watcher snapshots paused.
//!    Link the store's mount path to `<hash>.building/<leaf>/`, `<leaf>`
//!    being the mount path's last component (see the module docs of
//!    [`super`]).
//! 4. Run the argv with the scratch root as its working directory, in its
//!    own process group, with `PATH`, `HOME`, `LANG`, and `TMPDIR` from the
//!    daemon, then `[store.env]`, then `REEF_STORE_DIR` (the contents
//!    directory the link points at) and `REEF_STORE_NAME`. Output lines are forwarded as they
//!    come; lines a slow client cannot take are dropped and counted (by the
//!    service, at the client's channel). The scratch lease is renewed while
//!    it runs. When it exits, past the timeout, or when the daemon shuts
//!    down, whatever is left of the process group is killed.
//! 5. If the command exited 0 but replaced the link with a directory of its
//!    own (`npm ci` deletes `node_modules` before installing), move that
//!    directory into `<hash>.building/<leaf>/`; anything else in the link's place
//!    fails the build. Unlink the store, release the scratch workspace
//!    without a final snapshot (`reclaims.detail = 'store build'`), and
//!    seal (exit 0) or move the directory aside as `<hash>.failed-<ms>/`
//!    (anything else).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reef_lib::error::ErrorChain;
use reef_lib::spec::StoreSpec;
use reef_lib::telemetry::elapsed_ms;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MountMode, StoreError, StoreKey, StoreManager, mount};
use crate::backend::{BackendChoice, BackendKind};
use crate::model::{ReclaimReason, RepoId, StoreRecord, WorkspaceId};
use crate::presence::Report;
use crate::presence::publish::Progress;
use crate::workspace::{
    CreateSpec, Manager, ManagerError, MissingStores, ReleaseSpec, Start, StoreTarget,
};

/// The owner of a store build's scratch workspace.
pub const BUILD_OWNER: &str = "reef-store-build";

/// The `reclaims.detail` of a store build's scratch workspace.
const BUILD_DETAIL: &str = "store build";

/// The variables a build inherits from the daemon; everything else is
/// cleared.
const INHERITED_ENV: [&str; 4] = ["PATH", "HOME", "LANG", "TMPDIR"];

/// How long to wait for the output readers once the build exited: a
/// process that left the group can hold the pipes open forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The build timeout when the request gives none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(30);

/// What to build.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// The repository whose `reef.toml` declares the store.
    pub repo: RepoId,
    /// The store's name.
    pub name: String,
    /// The revision to build from; `None` for a create's default start
    /// (R1.31).
    pub rev: Option<String>,
    /// How long the build command may run.
    pub timeout: Duration,
    /// Build even if a sealed store exists, replacing it.
    pub force: bool,
    /// The operations' `reef.agent`; `None` for [`BUILD_OWNER`] (R2.03).
    pub agent: Option<String>,
}

/// Which stream an output line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// What a build sends its caller while it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildOutput {
    /// The command is about to run; sent once, before any line (RX.01,
    /// S-03).
    Started(BuildStarted),
    /// A line of its output.
    Line(OutputLine),
}

/// The command a build runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildStarted {
    /// The `[[store]] build` argv.
    pub argv: Vec<String>,
    /// `[store.env]`, in key order.
    pub env: Vec<(String, String)>,
    /// The commit it runs on.
    pub rev: String,
}

/// One line of the build command's output, without its line ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLine {
    /// The text, lossily decoded as UTF-8.
    pub text: String,
    /// Where it came from.
    pub stream: OutputStream,
}

/// How a build ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildResult {
    /// The store and its lockfile's hash.
    pub key: StoreKey,
    /// The store's directory once sealed; a failed build's directory, kept
    /// for inspection, otherwise (empty if it left none).
    pub path: PathBuf,
    /// Bytes the sealed store allocates; 0 for a failed build.
    pub bytes: u64,
    /// Whether the store is sealed now.
    pub sealed: bool,
    /// The build command's exit code; -1 if a signal ended it. 0 when an
    /// existing store answered without a build.
    pub exit_code: i32,
    /// The operation that released the scratch workspace, if one was
    /// recorded; `None` when no build ran.
    pub op_id: Option<String>,
    /// Output lines dropped because the client read too slowly.
    pub dropped_lines: u64,
    /// Whether a sealed store already existed and nothing was built.
    pub already_built: bool,
}

/// Runs the build; see the module docs. `lines` gets what the command is
/// ([`BuildOutput::Started`]) and then its output, best effort. A command that exits non-zero is a result with `sealed =
/// false`; a timeout, a shutdown, or a command that cannot start is an
/// error.
#[allow(
    clippy::too_many_lines,
    reason = "the build's steps in order; R2.03's agent tipped it over"
)]
#[tracing::instrument(skip_all, fields(repo = %request.repo, store = %request.name, force = request.force))]
pub async fn run(
    manager: Arc<Manager>,
    request: BuildRequest,
    lines: mpsc::Sender<BuildOutput>,
    shutdown: CancellationToken,
) -> Result<BuildResult, ManagerError> {
    let begun = Instant::now();
    // Ended when dropped, whatever the outcome (R2.10); an existing store
    // answers within the delay, so it shows nothing.
    let progress = Progress::start(
        manager.presence(),
        Report {
            agent: request
                .agent
                .clone()
                .unwrap_or_else(|| BUILD_OWNER.to_owned()),
            repo: Some(request.repo),
            ..Report::default()
        },
        "resolve",
        0,
        "lines",
    );
    // 1.
    let (commit, store, target) = manager
        .resolve_store(
            request.repo,
            request.rev.clone(),
            request.name.clone(),
            request.agent.clone(),
        )
        .await?;
    let stores = Arc::clone(manager.stores());
    let key = target.key.clone();
    if !request.force
        && let Some(row) = stores.sealed(&key, &target.mount)?
    {
        tracing::info!(lockfile_hash = %key.lockfile_hash, "store is already built");
        return Ok(BuildResult {
            key,
            path: row.path,
            bytes: 0,
            sealed: true,
            exit_code: 0,
            op_id: None,
            dropped_lines: 0,
            already_built: true,
        });
    }

    // A forced rebuild moves the store aside when it seals, which a bind
    // mount would not follow.
    if let Some(&count) = manager.stores_in_use().get(&key) {
        return Err(StoreError::InUse {
            name: key.name,
            hash: key.lockfile_hash,
            count,
        }
        .into());
    }

    progress.phase("create", 0);

    // 2.
    let _running = stores.start_build(&key)?;
    let inserted = record_building(&manager, &stores, &key, &target.mount, request.repo)?;
    let building = stores.building_dir(&key);
    let contents = stores.building_contents(&key, &target.mount);
    let ran = async {
        prepare_building(&building, &contents)?;
        build_in_scratch(
            &manager, &request, &commit, &store, &target, &contents, lines, &progress, &shutdown,
        )
        .await
    }
    .await;

    // 5.
    let (status, op_id) = match ran {
        Ok(ran) => ran,
        Err(error) => {
            discard(&stores, &manager, &key, &building, inserted);
            return Err(error);
        }
    };
    let mut result = BuildResult {
        key,
        path: PathBuf::new(),
        bytes: 0,
        sealed: false,
        exit_code: status.code().unwrap_or(-1),
        op_id,
        // The service counts what its client could not take.
        dropped_lines: 0,
        already_built: false,
    };
    if status.success() {
        progress.phase("seal", 0);
        let sealed = tokio::task::spawn_blocking({
            let stores = Arc::clone(&stores);
            let (building, key) = (building.clone(), result.key.clone());
            let mount = target.mount.clone();
            move || stores.seal(&building, &key, &mount)
        })
        .await
        .map_err(ManagerError::from)
        .and_then(|sealed| sealed.map_err(ManagerError::from));
        let (row, bytes) = match sealed {
            Ok(sealed) => sealed,
            Err(error) => {
                discard(&stores, &manager, &result.key, &building, inserted);
                return Err(error);
            }
        };
        tracing::info!(
            lockfile_hash = %result.key.lockfile_hash,
            path = %row.path.display(),
            bytes,
            elapsed_ms = elapsed_ms(begun),
            "store built and sealed"
        );
        result.path = row.path;
        result.bytes = bytes;
        result.sealed = true;
        return Ok(result);
    }
    let failed = discard(&stores, &manager, &result.key, &building, inserted);
    tracing::warn!(
        lockfile_hash = %result.key.lockfile_hash,
        exit_code = result.exit_code,
        path = ?failed.as_ref().map(|path| path.display().to_string()),
        elapsed_ms = elapsed_ms(begun),
        "store build failed; nothing sealed"
    );
    result.path = failed.unwrap_or_default();
    Ok(result)
}

/// Step 2's row: inserted unsealed unless the store has one (a forced
/// rebuild keeps the sealed row). Returns whether it inserted one.
fn record_building(
    manager: &Manager,
    stores: &StoreManager,
    key: &StoreKey,
    mount: &str,
    repo: RepoId,
) -> Result<bool, ManagerError> {
    if stores.db.store(&key.name, &key.lockfile_hash)?.is_some() {
        return Ok(false);
    }
    manager.db().insert_store(&StoreRecord {
        repo_id: repo,
        name: key.name.clone(),
        lockfile_hash: key.lockfile_hash.clone(),
        path: stores.content_dir(key, mount),
        built_at: None,
        sealed: false,
    })?;
    Ok(true)
}

/// An empty `<hash>.building/<leaf>/`; a leftover `<hash>.building/` (which the start-time sweep
/// should have moved) is removed first.
fn prepare_building(
    building: &std::path::Path,
    contents: &std::path::Path,
) -> Result<(), StoreError> {
    let io = |op, source| StoreError::Io {
        op,
        path: building.to_owned(),
        source,
    };
    if std::fs::symlink_metadata(building).is_ok() {
        crate::backend::fs_util::remove_all(building).map_err(|source| io("remove", source))?;
    }
    if let Some(parent) = building.parent() {
        std::fs::create_dir_all(parent).map_err(|source| io("create directory", source))?;
    }
    std::fs::create_dir(building).map_err(|source| io("create directory", source))?;
    std::fs::create_dir(contents).map_err(|source| StoreError::Io {
        op: "create directory",
        path: contents.to_owned(),
        source,
    })
}

/// A build that will not be sealed: its directory goes aside as failed and
/// the row it inserted is deleted. Failures are logged. Returns where the
/// directory went.
fn discard(
    stores: &StoreManager,
    manager: &Manager,
    key: &StoreKey,
    building: &std::path::Path,
    inserted: bool,
) -> Option<PathBuf> {
    let failed = match stores.fail(building, key) {
        Ok(failed) => failed,
        Err(error) => {
            tracing::error!(error = %ErrorChain(&error), "cannot move the failed build's directory aside");
            None
        }
    };
    if inserted && let Err(error) = manager.db().delete_store(&key.name, &key.lockfile_hash) {
        tracing::error!(error = %ErrorChain(&error), "cannot delete the failed build's row");
    }
    failed
}

/// Steps 3 to 5 up to the seal: the scratch workspace, the command, and the
/// release. Returns the command's status and the release operation.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site, the build's own state"
)]
async fn build_in_scratch(
    manager: &Arc<Manager>,
    request: &BuildRequest,
    commit: &reef_lib::jj::CommitIdHex,
    store: &StoreSpec,
    target: &StoreTarget,
    contents: &std::path::Path,
    lines: mpsc::Sender<BuildOutput>,
    progress: &Progress,
    shutdown: &CancellationToken,
) -> Result<(std::process::ExitStatus, Option<String>), ManagerError> {
    // 3.
    let created = manager
        .create(CreateSpec {
            repo: request.repo,
            start: Start::NewOn(commit.to_string()),
            owner: BUILD_OWNER.to_owned(),
            purpose: format!("store build {}", store.name),
            backend: Some(BackendChoice::Named(BackendKind::Copy)),
            path: None,
            lease: None,
            missing_stores: MissingStores::Skip,
            skip_store: Some(store.name.clone()),
            agent: request.agent.clone(),
        })
        .await?;
    let ws = created.record.id;
    let root = created.record.path.clone();
    // No watcher snapshot of what the build writes; the release takes none
    // either, so nothing the build does reaches the repository.
    let pause = manager.scheduler().pause(ws);
    let linked = mount::symlink_mount(contents, &root, &target.mount);
    let ran = match linked {
        Ok(()) => {
            let lease = (
                created.lease.id,
                manager.leases().heartbeat_every(created.lease.length),
            );
            announce(store, commit, &lines).await;
            progress.phase("build", 0);
            run_command(
                manager,
                ws,
                lease,
                store,
                &root,
                contents,
                request.timeout,
                lines,
                progress,
                shutdown,
            )
            .await
        }
        Err(error) => Err(error),
    };
    // 5.
    let adopted = match &ran {
        Ok(status) if status.success() => {
            mount::adopt_replaced_link(contents, &root, &target.mount, &store.name)
        }
        _ => Ok(false),
    };
    match &adopted {
        Ok(true) => tracing::info!(
            mount = %target.mount,
            "the build replaced the store's link with a directory; sealing that directory"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            mount = %target.mount,
            error = %ErrorChain(error),
            "the build exited 0 but left nothing to seal"
        ),
    }
    if let Err(error) = mount::unmount(&root, &target.mount, MountMode::Symlink) {
        tracing::warn!(error = %ErrorChain(&error), "cannot unlink the store from the scratch workspace");
    }
    let spec = ReleaseSpec {
        force: true,
        final_snapshot: false,
        detail: Some(BUILD_DETAIL),
        agent: request.agent.clone(),
    };
    let op_id = match manager
        .leases()
        .reclaim(ws, ReclaimReason::Released, spec)
        .await
    {
        Ok(outcome) => outcome.release_op_id,
        Err(error) => {
            tracing::error!(workspace_id = %ws, error = %ErrorChain(&error), "cannot release the store build's scratch workspace");
            None
        }
    };
    drop(pause);
    let status = ran?;
    adopted?;
    Ok((status, op_id))
}

/// Logs the command a build is about to run and sends it to the caller
/// (RX.01, S-03): the build runs the repository's own argv, so the user
/// sees it before any of its output.
async fn announce(
    store: &StoreSpec,
    commit: &reef_lib::jj::CommitIdHex,
    lines: &mpsc::Sender<BuildOutput>,
) {
    let env_keys: Vec<&String> = store.env.keys().collect();
    tracing::info!(
        argv = ?store.build,
        ?env_keys,
        rev = %commit,
        "running the store's build command"
    );
    let started = BuildStarted {
        argv: store.build.clone(),
        env: store
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        rev: commit.to_string(),
    };
    // A caller that is gone does not stop the build.
    let _ = lines.send(BuildOutput::Started(started)).await;
}

/// Step 4: runs the build command to completion, the timeout, or shutdown,
/// then kills whatever is left of its process group either way: a
/// background process the build started must not keep writing into the
/// directory about to be sealed.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site, the build's own state"
)]
async fn run_command(
    manager: &Manager,
    ws: WorkspaceId,
    (lease, heartbeat_every): (crate::model::LeaseId, Duration),
    store: &StoreSpec,
    root: &std::path::Path,
    contents: &std::path::Path,
    timeout: Duration,
    lines: mpsc::Sender<BuildOutput>,
    progress: &Progress,
    shutdown: &CancellationToken,
) -> Result<std::process::ExitStatus, StoreError> {
    let (program, args) = store.build.split_first().ok_or_else(|| StoreError::Spawn {
        name: store.name.clone(),
        program: String::new(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty build command"),
    })?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .envs(&store.env)
        .env("REEF_STORE_DIR", contents)
        .env("REEF_STORE_NAME", &store.name);
    let mut child = command.spawn().map_err(|source| StoreError::Spawn {
        name: store.name.clone(),
        program: program.clone(),
        source,
    })?;
    let pid = child.id();
    tracing::info!(pid, program = %program, "store build started");
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(tokio::spawn(forward(
            stdout,
            OutputStream::Stdout,
            lines.clone(),
            progress.clone(),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(tokio::spawn(forward(
            stderr,
            OutputStream::Stderr,
            lines,
            progress.clone(),
        )));
    }

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut heartbeat = tokio::time::interval(heartbeat_every);
    heartbeat.tick().await;
    let ended = loop {
        tokio::select! {
            status = child.wait() => {
                if kill_group(pid) {
                    tracing::warn!(pid, "the build left processes running; killed them");
                }
                break status.map_err(|source| StoreError::Io {
                    op: "wait for the build",
                    path: root.to_owned(),
                    source,
                });
            }
            () = &mut deadline => {
                kill_group(pid);
                let _ = child.wait().await;
                break Err(StoreError::BuildTimeout {
                    name: store.name.clone(),
                    secs: timeout.as_secs(),
                });
            }
            () = shutdown.cancelled() => {
                kill_group(pid);
                let _ = child.wait().await;
                break Err(StoreError::BuildCancelled { name: store.name.clone() });
            }
            _ = heartbeat.tick() => {
                if let Err(error) = manager.leases().heartbeat(ws, lease) {
                    tracing::warn!(workspace_id = %ws, error = %ErrorChain(&error), "cannot renew the scratch workspace's lease");
                }
            }
        }
    };
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(DRAIN_TIMEOUT, reader).await.is_err() {
            tracing::warn!(
                "a process the build started still holds its output open; no longer reading it"
            );
            abort.abort();
        }
    }
    ended
}

/// Sends `SIGKILL` to the build's process group, whose id is the build
/// command's pid (`process_group(0)`); whether anything was there. On a
/// timeout or shutdown it runs before the command is reaped. After a
/// command that exited, the id stays its group's while any member lives,
/// and with none left the call finds nothing; another group could only
/// take the id if the pid were reused in between.
fn kill_group(pid: Option<u32>) -> bool {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return false;
    };
    match nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    ) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(error) => {
            tracing::warn!(pid, %error, "cannot kill the build's process group");
            false
        }
    }
}

/// Forwards `reader`'s lines to `lines` until EOF. A full channel holds the
/// reader (and so the pipe) back; the service drops lines for a slow client
/// further on, and keeps draining after one disconnects.
async fn forward(
    reader: impl AsyncRead + Unpin,
    stream: OutputStream,
    lines: mpsc::Sender<BuildOutput>,
    progress: Progress,
) {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        while matches!(buffer.last(), Some(b'\n' | b'\r')) {
            buffer.pop();
        }
        progress.add(1);
        let line = OutputLine {
            text: String::from_utf8_lossy(&buffer).into_owned(),
            stream,
        };
        if lines.send(BuildOutput::Line(line)).await.is_err() {
            return;
        }
    }
}
