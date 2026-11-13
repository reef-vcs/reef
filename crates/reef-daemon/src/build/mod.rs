//! The machine-wide build limit (#271, ADR 0074; `docs/build-limit.md`).
//!
//! Five agents building at once each assume they own the machine. The
//! daemon bounds them together:
//!
//! - `jobserver`: one FIFO, `~/.reef/.build/jobserver` (else
//!   `<state>/build/jobserver`), holding `jobs - 1`
//!   tokens in GNU make's protocol, handed to cargo as `CARGO_MAKEFLAGS`.
//!   Every client has one implicit token, so a lone build runs `jobs` jobs
//!   and five run at most `jobs + 4`. A killed build loses its tokens; the
//!   check below writes them back once no process holds the FIFO
//!   (`holders`), and drains any surplus.
//! - `env`: job counts for tools without a jobserver (`share = max(2,
//!   jobs / builders)`), never overriding a setting already made.
//! - `cargo_config`: `~/.reef/.cargo/config.toml` for cargo commands Reef
//!   did not start, rewritten as builders come and go.
//! - `make`: a dependency that hands the jobserver to a `make` older than
//!   4.4 breaks its build, so such a workspace gets the counts only.
//!
//! `builders` are the workspaces whose leases are heartbeating (a `reef ws
//! run`, a provider follower, `reef mcp`), machine-wide.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use reef_api::v1::{self, JobserverSkip};
use reef_lib::spec::BuildSpec;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{BuildSection, Config, Source, WORKSPACE_BASE_DIR};
use crate::model::WorkspaceId;
use crate::presence::PresenceHub;

pub(crate) mod cargo_config;
pub(crate) mod env;
pub(crate) mod holders;
pub(crate) mod jobserver;
pub(crate) mod make;

use cargo_config::CargoFile;
use jobserver::Jobserver;

/// The FIFO's directory in the state directory, for a daemon without the
/// default workspace root.
pub const BUILD_DIR: &str = "build";

/// The FIFO's directory under the default workspace root, `~/.reef/.build`:
/// its path has no space, which the jobserver's argument cannot carry
/// (cargo splits it at the first one), as the macOS state directory's has.
pub const BASE_BUILD_DIR: &str = ".build";

/// Specs remembered by base tree id, at most.
const SPEC_CACHE: usize = 256;

/// How long the pool may stay short while processes hold the FIFO before
/// `reef daemon status` names them.
const HELD_SHORT_NOTE_AFTER: Duration = Duration::from_mins(10);

/// The build limit, in [`crate::AppState::build`].
#[derive(Debug)]
pub struct BuildLimit {
    section: BuildSection,
    jobs: u32,
    /// Where the FIFO lives, or why it cannot.
    fifo_dir: Result<PathBuf, String>,
    /// The default workspace root, `~/.reef`, which the FIFO and the cargo
    /// file wait for: a start never makes it (S-185), the first create
    /// does.
    base: Option<PathBuf>,
    /// The jobserver; replaced when its path stops naming it.
    jobserver: RwLock<Option<Arc<Jobserver>>>,
    /// Why there is no jobserver, when there is not.
    jobserver_note: Mutex<String>,
    /// Why there is no cargo file, or why the pool stays short.
    other_note: Mutex<String>,
    generation: AtomicU32,
    top_ups: AtomicU64,
    tokens_restored: AtomicU64,
    tokens_drained: AtomicU64,
    /// Consecutive checks that found the pool short.
    short_checks: AtomicU32,
    /// Since when the pool has been short while processes held the FIFO.
    held_short_since: Mutex<Option<Instant>>,
    cargo_file: Option<CargoFile>,
    make_jobserver: bool,
    make: make::MakeProbe,
    presence: Arc<PresenceHub>,
    interval: Duration,
    specs: Mutex<HashMap<String, BuildSpec>>,
}

/// What one token check did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recovery {
    /// Surplus tokens read out.
    pub drained: u32,
    /// Lost tokens written back.
    pub restored: u32,
    /// The pool was short, and these processes held the FIFO, so nothing
    /// was written back.
    pub held_by: Vec<u32>,
}

/// One token check of `jobserver`: drains a surplus; once the pool has been
/// short for two checks in a row (`short_checks`), writes the missing
/// tokens back if no process but the daemon holds the FIFO.
pub(crate) fn recover(
    jobserver: &Jobserver,
    short_checks: &AtomicU32,
) -> std::io::Result<Recovery> {
    let mut recovery = Recovery::default();
    let available = jobserver.available()?;
    let tokens = jobserver.tokens();
    if available >= tokens {
        short_checks.store(0, Ordering::Relaxed);
        if available > tokens {
            recovery.drained = jobserver.drain(available - tokens)?;
        }
        return Ok(recovery);
    }
    if short_checks.fetch_add(1, Ordering::Relaxed) + 1 < 2 {
        return Ok(recovery);
    }
    let (dev, ino) = jobserver.id();
    let held_by = holders::holders(dev, ino)?;
    if !held_by.is_empty() {
        recovery.held_by = held_by;
        return Ok(recovery);
    }
    // Nothing holds it: every missing token is lost.
    let available = jobserver.available()?;
    if available < tokens {
        recovery.restored = jobserver.write_tokens(tokens - available)?;
    }
    short_checks.store(0, Ordering::Relaxed);
    Ok(recovery)
}

impl BuildLimit {
    /// Starts the limit for `config`: the jobserver (reused, topped up and
    /// drained, or made), and the cargo file. Never fails: a jobserver it
    /// cannot keep leaves the counts, with a note, and is tried again at
    /// each check. Blocking.
    pub fn start(config: &Config, presence: Arc<PresenceHub>) -> BuildLimit {
        let section = config.file.build.clone();
        let jobs = section.jobs.resolve(available_cores(), machine_memory());
        let mut limit = BuildLimit {
            jobs,
            fifo_dir: fifo_dir(config),
            base: config.workspace_base.clone(),
            jobserver: RwLock::new(None),
            jobserver_note: Mutex::new(String::new()),
            other_note: Mutex::new(String::new()),
            generation: AtomicU32::new(0),
            top_ups: AtomicU64::new(0),
            tokens_restored: AtomicU64::new(0),
            tokens_drained: AtomicU64::new(0),
            short_checks: AtomicU32::new(0),
            held_short_since: Mutex::new(None),
            cargo_file: None,
            make_jobserver: false,
            make: make::MakeProbe::default(),
            presence,
            interval: config.build_interval,
            specs: Mutex::new(HashMap::new()),
            section,
        };
        let cargo_path = default_cargo_file(config);
        if !limit.section.limit {
            // What an earlier run left: a FIFO nobody fills would make a
            // command that still names it build one job at a time.
            if let Ok(dir) = &limit.fifo_dir {
                jobserver::remove_stale(dir);
            }
            if let Some(path) = &cargo_path {
                cargo_config::remove_if_ours(path);
            }
            return limit;
        }
        limit.open_jobserver(true);
        if limit.section.make_jobserver {
            let make = limit.make.get();
            if make.make.modern {
                limit.make_jobserver = true;
            } else {
                tracing::warn!(
                    found = %make.make.text,
                    "[build] make_jobserver needs GNU make 4.4 or later as make; MAKEFLAGS is not set"
                );
            }
        }
        match (&config.workspace_base, limit.section.cargo_config) {
            (Some(base), true) => {
                let keys = env::UserFiles::read().cargo_file_keys();
                limit.cargo_file = Some(CargoFile::new(base, keys));
                limit.write_cargo_file();
            }
            _ => {
                if let Some(path) = &cargo_path {
                    cargo_config::remove_if_ours(path);
                }
            }
        }
        limit
    }

    /// Opens (or reopens) the jobserver; at start, counts a refill of a
    /// reused FIFO as tokens restored.
    fn open_jobserver(&self, at_start: bool) {
        let dir = match &self.fifo_dir {
            Ok(dir) => dir,
            Err(why) => {
                self.set_jobserver_note(format!("no jobserver: {why}"));
                return;
            }
        };
        if !self.base_ready() {
            self.set_jobserver_note(
                "no jobserver until the first workspace makes ~/.reef".to_owned(),
            );
            return;
        }
        match Jobserver::open(dir, self.jobs.saturating_sub(1)) {
            Ok(opened) => {
                if opened.created {
                    self.generation.fetch_add(1, Ordering::Relaxed);
                } else if at_start && opened.restored > 0 {
                    // Refilling a reused FIFO rescues tokens lost while the
                    // daemon was down; filling a new one restores nothing.
                    self.tokens_restored
                        .fetch_add(u64::from(opened.restored), Ordering::Relaxed);
                }
                self.tokens_drained
                    .fetch_add(u64::from(opened.drained), Ordering::Relaxed);
                tracing::info!(
                    path = %opened.jobserver.path().display(),
                    jobs = self.jobs,
                    tokens = opened.jobserver.tokens(),
                    created = opened.created,
                    restored_count = opened.restored,
                    drained_count = opened.drained,
                    "the build limit's jobserver is ready"
                );
                *self
                    .jobserver
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(opened.jobserver));
                self.set_jobserver_note(String::new());
            }
            Err(error) => {
                let note = format!("no jobserver: {error}");
                if self.jobserver_note() != note {
                    tracing::warn!(dir = %dir.display(), %error, "no jobserver: builds get job counts only");
                }
                *self
                    .jobserver
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                self.set_jobserver_note(note);
            }
        }
    }

    fn jobserver(&self) -> Option<Arc<Jobserver>> {
        self.jobserver
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `[build] limit`.
    pub fn enabled(&self) -> bool {
        self.section.limit
    }

    /// The machine's job count.
    pub fn jobs(&self) -> u32 {
        self.jobs
    }

    /// The niceness commands run at; 0 when the limit is off.
    pub fn nice(&self) -> u32 {
        if self.section.limit {
            self.section.nice
        } else {
            0
        }
    }

    /// Workspaces whose leases are heartbeating, `asking` counted once even
    /// before its first heartbeat; at least 1.
    pub fn builders(&self, asking: Option<WorkspaceId>) -> u32 {
        let mut leased = self.presence.leased_workspaces();
        if let Some(asking) = asking
            && !leased.contains(&asking)
        {
            leased.push(asking);
        }
        u32::try_from(leased.len()).unwrap_or(u32::MAX).max(1)
    }

    /// max(2, jobs / builders).
    pub fn share(&self, builders: u32) -> u32 {
        share(self.jobs, builders)
    }

    /// The base-tree `reef.toml`'s `[build]` remembered for `tree`.
    pub(crate) fn cached_spec(&self, tree: &str) -> Option<BuildSpec> {
        self.specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(tree)
            .cloned()
    }

    /// Remembers `spec` for `tree`.
    pub(crate) fn remember_spec(&self, tree: &str, spec: BuildSpec) {
        let mut specs = self.specs.lock().unwrap_or_else(PoisonError::into_inner);
        if specs.len() >= SPEC_CACHE {
            specs.clear();
        }
        specs.insert(tree.to_owned(), spec);
    }

    /// The environment for a command in `workspace` (its directory and
    /// id), or the machine's defaults for none. Blocking: it reads the
    /// workspace's and the user's files.
    pub fn environment(
        &self,
        workspace: Option<(&Path, WorkspaceId)>,
        spec: &BuildSpec,
    ) -> v1::GetBuildEnvironmentResponse {
        let builders = self.builders(workspace.map(|(_, id)| id));
        let share = self.share(builders);
        if !self.section.limit {
            return v1::GetBuildEnvironmentResponse {
                enabled: false,
                jobs: self.jobs,
                share,
                builders,
                jobserver_skip: JobserverSkip::Disabled.into(),
                ..v1::GetBuildEnvironmentResponse::default()
            };
        }
        let user = env::UserFiles::read();
        // `~/.reef` may have been made since the last check, by the create
        // of the workspace asking.
        if self.jobserver().is_none() && self.fifo_dir.is_ok() && self.base_ready() {
            self.open_jobserver(false);
        }
        let current = self.jobserver();
        let jobserver = match &current {
            Some(jobserver) => Ok(jobserver.path()),
            None => Err((JobserverSkip::Unavailable, self.jobserver_note())),
        };
        let dir = workspace.map(|(dir, _)| dir);
        let cargo_file = self
            .cargo_file
            .as_ref()
            .filter(|file| dir.is_some_and(|dir| file.reaches(dir)))
            .and_then(CargoFile::active_keys);
        let probe = || self.make.get();
        let computed = env::compute(&env::Inputs {
            jobs: self.jobs,
            share,
            jobserver,
            make_jobserver: self.make_jobserver,
            spec,
            workspace: dir,
            user: &user,
            cargo_file,
            make: &probe,
        });
        let offered = computed.skip == JobserverSkip::Unspecified;
        v1::GetBuildEnvironmentResponse {
            enabled: true,
            variables: computed.variables,
            jobs: self.jobs,
            share,
            builders,
            nice: self.section.nice,
            jobserver: match (&current, offered) {
                (Some(jobserver), true) => jobserver.path().display().to_string(),
                _ => String::new(),
            },
            jobserver_skip: computed.skip.into(),
            note: computed.note,
        }
    }

    /// What a remote caller gets: the jobserver is on this machine and the
    /// builders are this machine's, so nothing applies to its commands.
    pub fn remote_environment(&self) -> v1::GetBuildEnvironmentResponse {
        v1::GetBuildEnvironmentResponse {
            enabled: self.section.limit,
            jobserver_skip: JobserverSkip::Remote.into(),
            note: "asked through the remote listener: the build limit is the daemon machine's"
                .to_owned(),
            ..v1::GetBuildEnvironmentResponse::default()
        }
    }

    /// The limit's figures for `reef daemon status`.
    pub fn status(&self) -> v1::BuildStatus {
        let builders = self.builders(None);
        let current = self.jobserver();
        let notes: Vec<String> = [self.jobserver_note(), self.other_note()]
            .into_iter()
            .filter(|note| !note.is_empty())
            .collect();
        v1::BuildStatus {
            enabled: self.section.limit,
            jobs: self.jobs,
            tokens: current.as_deref().map_or(0, Jobserver::tokens),
            available: current
                .as_deref()
                .and_then(|jobserver| jobserver.available().ok())
                .unwrap_or(0),
            builders,
            share: self.share(builders),
            jobserver: current
                .as_deref()
                .map(|jobserver| jobserver.path().display().to_string())
                .unwrap_or_default(),
            generation: self.generation.load(Ordering::Relaxed),
            top_ups: self.top_ups.load(Ordering::Relaxed),
            tokens_restored: self.tokens_restored.load(Ordering::Relaxed),
            tokens_drained: self.tokens_drained.load(Ordering::Relaxed),
            cargo_config: self
                .cargo_file
                .as_ref()
                .filter(|file| file.active_keys().is_some())
                .map(|file| file.path().display().to_string())
                .unwrap_or_default(),
            note: notes.join("; "),
        }
    }

    /// One check: reopens a jobserver whose path no longer names it (or
    /// that could not be opened), drains a surplus, tops up a pool short
    /// for two checks in a row once no process but the daemon holds the
    /// FIFO, and rewrites the cargo file for the current share. Blocking.
    pub fn check(&self) {
        match self.jobserver() {
            Some(jobserver) if jobserver.at_path() => self.check_tokens(&jobserver),
            Some(jobserver) => {
                tracing::warn!(
                    path = %jobserver.path().display(),
                    "the jobserver FIFO was removed or replaced; making a new one"
                );
                self.open_jobserver(false);
            }
            None if self.fifo_dir.is_ok() => self.open_jobserver(false),
            None => {}
        }
        self.write_cargo_file();
    }

    fn check_tokens(&self, jobserver: &Jobserver) {
        let recovery = match recover(jobserver, &self.short_checks) {
            Ok(recovery) => recovery,
            Err(error) => {
                tracing::warn!(%error, "the build limit's token check failed");
                return;
            }
        };
        self.tokens_drained
            .fetch_add(u64::from(recovery.drained), Ordering::Relaxed);
        if recovery.restored > 0 {
            self.top_ups.fetch_add(1, Ordering::Relaxed);
            self.tokens_restored
                .fetch_add(u64::from(recovery.restored), Ordering::Relaxed);
            tracing::info!(
                restored_count = recovery.restored,
                "the build limit restored tokens no process held"
            );
        }
        // A process that keeps the FIFO open (a build server, a watcher)
        // blocks the refill: say so once it has lasted.
        let mut since = self
            .held_short_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if recovery.held_by.is_empty() {
            if since.take().is_some() {
                self.set_other_note(String::new());
            }
        } else {
            let started = *since.get_or_insert_with(Instant::now);
            if started.elapsed() >= HELD_SHORT_NOTE_AFTER {
                self.set_other_note(format!(
                    "the pool has been short for {} min while pids {:?} hold the jobserver; tokens they lost come back when they exit",
                    started.elapsed().as_secs() / 60,
                    recovery.held_by
                ));
            }
        }
    }

    /// Whether the default workspace root exists, when there is one.
    fn base_ready(&self) -> bool {
        self.base.as_ref().is_none_or(|base| base.exists())
    }

    fn write_cargo_file(&self) {
        let Some(file) = &self.cargo_file else {
            return;
        };
        if !self.base_ready() {
            return;
        }
        let share = self.share(self.builders(None));
        match file.write(share) {
            Ok(()) => {
                if self.other_note().starts_with("no cargo file") {
                    self.set_other_note(String::new());
                }
            }
            Err(error) => {
                let note = format!("no cargo file: {error}");
                if self.other_note() != note {
                    tracing::warn!(path = %file.path().display(), %error, "cannot write the build limit's cargo file");
                    self.set_other_note(note);
                }
            }
        }
    }

    /// Runs [`BuildLimit::check`] every interval until `shutdown`.
    pub fn spawn_checks(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        let limit = Arc::clone(self);
        tokio::spawn(async move {
            if !limit.section.limit {
                return;
            }
            let mut ticks = tokio::time::interval(limit.interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    _ = ticks.tick() => {}
                }
                let checked = Arc::clone(&limit);
                if let Err(error) = tokio::task::spawn_blocking(move || checked.check()).await {
                    tracing::error!(%error, "the build limit's check panicked");
                }
            }
        })
    }

    /// At a graceful stop: removes the FIFO's path. Running builds keep
    /// their own descriptors.
    pub fn stop(&self) {
        if let Some(jobserver) = self.jobserver() {
            jobserver.unlink();
        }
    }

    /// The jobserver's path, if there is one.
    pub fn jobserver_path(&self) -> Option<PathBuf> {
        self.jobserver()
            .map(|jobserver| jobserver.path().to_owned())
    }

    fn jobserver_note(&self) -> String {
        self.jobserver_note
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_jobserver_note(&self, note: String) {
        *self
            .jobserver_note
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = note;
    }

    fn other_note(&self) -> String {
        self.other_note
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_other_note(&self, note: String) {
        *self
            .other_note
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = note;
    }
}

/// Where the FIFO lives: `~/.reef/.build` under the default workspace
/// root, else `<state>/build`. A path with whitespace cannot be handed to
/// cargo, which reads the jobserver's path up to the first space.
fn fifo_dir(config: &Config) -> Result<PathBuf, String> {
    let dir = match &config.workspace_base {
        // `~/.reef` may be a symlink (S-187), and a start never makes it
        // (S-185): the FIFO waits for the first create.
        Some(base) => base.join(BASE_BUILD_DIR),
        None => config.paths.state_dir.join(BUILD_DIR),
    };
    without_whitespace(dir)
}

/// `dir`, or why cargo could not be handed a FIFO in it.
fn without_whitespace(dir: PathBuf) -> Result<PathBuf, String> {
    if dir.to_string_lossy().chars().any(char::is_whitespace) {
        return Err(format!(
            "{} has whitespace in its path, which cargo cannot read in a jobserver's",
            dir.display()
        ));
    }
    Ok(dir)
}

/// The cargo file the default workspace root would have: also when the
/// root is configured, so that a file an earlier run wrote is removed. A
/// daemon given its own state directory has none.
fn default_cargo_file(config: &Config) -> Option<PathBuf> {
    let base = config.workspace_base.clone().or_else(|| {
        (config.origin.state_dir == Source::Default)
            .then(|| std::env::var_os("HOME"))
            .flatten()
            .map(|home| PathBuf::from(home).join(WORKSPACE_BASE_DIR))
    })?;
    Some(base.join(".cargo").join("config.toml"))
}

/// max(2, jobs / builders).
pub fn share(jobs: u32, builders: u32) -> u32 {
    (jobs / builders.max(1)).max(2)
}

/// Logical cores, honouring a cgroup's CPU quota.
fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// The machine's memory in bytes: `hw.memsize` on macOS; on Linux the
/// smaller of physical memory and the cgroup's `memory.max`.
fn machine_memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut length = size_of::<u64>();
        // SAFETY: the name is NUL-terminated, the output buffer is a u64 of
        // `length` bytes, and no new value is given.
        #[allow(unsafe_code, reason = "sysctlbyname(hw.memsize)")]
        let result = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut size).cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            return size;
        }
        16 << 30
    }
    #[cfg(target_os = "linux")]
    {
        let physical = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| meminfo_total(&text))
            .unwrap_or(16 << 30);
        let cgroup = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|text| cgroup_max(&text));
        cgroup.map_or(physical, |max| physical.min(max))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        16 << 30
    }
}

/// `MemTotal` from `/proc/meminfo`'s text, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// A cgroup v2 `memory.max`: a number of bytes, or `max` for none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_max(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BuildJobs;

    #[test]
    fn jobs_auto() {
        let gib = 1u64 << 30;
        assert_eq!(BuildJobs::Auto.resolve(8, 16 * gib), 8);
        assert_eq!(BuildJobs::Auto.resolve(12, 32 * gib), 12);
        assert_eq!(BuildJobs::Auto.resolve(4, 16 * gib), 4);
        assert_eq!(BuildJobs::Auto.resolve(10, 16 * gib), 8);
        assert_eq!(BuildJobs::Auto.resolve(2, gib), 1);
        // MemTotal is below the nominal size: rounded to the nearest GiB.
        assert_eq!(BuildJobs::Auto.resolve(8, 15 * gib + 600 * (gib >> 10)), 8);
        assert_eq!(BuildJobs::Count(5).resolve(64, 256 * gib), 5);
        assert_eq!(meminfo_total("MemTotal:       16384 kB\n"), Some(16 << 20));
        assert_eq!(cgroup_max("8589934592\n"), Some(8 << 30));
        assert_eq!(cgroup_max("max\n"), None);
        assert!(machine_memory() > 0);
    }

    /// #271 test 5's held case: a process holding the FIFO with tokens
    /// taken blocks the refill; once it exits, the lost tokens come back.
    #[test]
    fn held_pool_is_not_refilled() {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};
        let temp = tempfile::tempdir().unwrap();
        let jobserver = Jobserver::open(temp.path(), 4).unwrap().jobserver;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(r#"exec 3<>"$1"; dd bs=1 count=2 <&3 >/dev/null 2>&1; echo taken; exec sleep 30"#)
            .arg("sh")
            .arg(jobserver.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "taken");
        assert_eq!(jobserver.available().unwrap(), 2);
        let short = AtomicU32::new(0);
        assert_eq!(recover(&jobserver, &short).unwrap(), Recovery::default());
        let held = recover(&jobserver, &short).unwrap();
        assert_eq!(held.restored, 0);
        assert_eq!(held.held_by, [child.id()], "the FIFO's holder is found");
        assert_eq!(jobserver.available().unwrap(), 2);
        child.kill().unwrap();
        child.wait().unwrap();
        let freed = recover(&jobserver, &short).unwrap();
        assert_eq!(freed.restored, 2, "{freed:?}");
        assert_eq!(jobserver.available().unwrap(), 4);
    }

    #[test]
    fn a_path_with_a_space_has_no_jobserver() {
        let macos = PathBuf::from("/Users/x/Library/Application Support/reef/build");
        let error = without_whitespace(macos).unwrap_err();
        assert!(error.contains("whitespace"), "{error}");
        let reef = PathBuf::from("/Users/x/.reef/.build");
        assert_eq!(without_whitespace(reef.clone()), Ok(reef));
    }

    #[test]
    fn shares() {
        assert_eq!(share(8, 1), 8);
        assert_eq!(share(8, 2), 4);
        assert_eq!(share(8, 3), 2);
        assert_eq!(share(8, 5), 2);
        assert_eq!(share(8, 9), 2);
        assert_eq!(share(12, 5), 2);
        assert_eq!(share(12, 4), 3);
        assert_eq!(share(1, 1), 2);
        assert_eq!(share(8, 0), 8);
    }
}
