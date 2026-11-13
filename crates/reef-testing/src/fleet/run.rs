//! The scenario: solo tickets, five at once, idle, release, gc, with the
//! sampler and canary running throughout. Everything goes through the
//! `reef` CLI as an agent would use it, against a daemon with the run's own
//! state directory.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::report::{self, RepoReport, Report};
use super::repos::{self, Prepared, RepoSpec};
use super::sample::{
    self, BuildSampler, Canary, CanaryReadings, DiskWatch, Phase, Sampler, Tracked, millis,
};
use super::seed::{self, quote};
use super::{
    DaemonWorkspaceDisk, Host, LimitMode, Profile, TicketRun, Toggle, Variant, disk, outcomes,
};

/// What a run does.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// The directory holding the `reef` and `reef-daemon` to measure.
    pub reef_bin_dir: PathBuf,
    /// The repositories, in order.
    pub repos: Vec<RepoSpec>,
    /// `fleet/patches`.
    pub patches_root: PathBuf,
    /// Reef's own checkout, for the `self` repository.
    pub reef_checkout: PathBuf,
    /// The run's directory: the daemon's state (a short path: the socket
    /// lives there).
    pub work_root: PathBuf,
    /// Where clones are kept between runs.
    pub cache: PathBuf,
    /// Where `report.json`, `summary.md` and the logs go.
    pub out: PathBuf,
    /// The profile the run stands for.
    pub profile: Profile,
    /// What it varies.
    pub variant: Variant,
    /// Tickets per repository.
    pub tickets: u32,
    /// How many tickets also run alone first (the correctness reference);
    /// at least 1, the "one workspace" of every ratio.
    pub solo_tickets: u32,
    /// How many times solo ticket 1 runs; the report takes the median
    /// (#271's lone-build criterion). At least 1.
    pub solo_runs: u32,
    /// Which build limits to measure (#271).
    pub limits: LimitMode,
    /// How long the five stay idle before release.
    pub idle: Duration,
    /// The five tickets start this far apart.
    pub stagger: Duration,
    /// Keep the run's state directory (the clones are always kept).
    pub keep_state: bool,
    /// `[disk] floor` for the run's daemon (#272), written to its
    /// `config.toml`; `None` for the daemon's default. The smoke test sets
    /// `"0"`, since a CI runner's free space may be under any floor.
    pub disk_floor: Option<String>,
}

/// Prefixes of variables the harness drops from its own environment before
/// running anything: Reef's (a harness run inside a workspace), and build
/// settings that would make the repositories' builds not their own.
pub const DROPPED_PREFIXES: [&str; 6] = [
    "REEF_",
    "CARGO_PROFILE_",
    "CARGO_BUILD_",
    "CARGO_TARGET_",
    "GIT_",
    "NEXTEST_",
];
/// Variables dropped by name, for the same reason as [`DROPPED_PREFIXES`].
pub const DROPPED: [&str; 12] = [
    "CARGO_INCREMENTAL",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC_WRAPPER",
    // Colored output would hide cargo's lines from the log parser.
    "CARGO_TERM_COLOR",
    // A harness started inside `reef ws run` measures its own daemon's
    // build limit, not the one it runs under (#271).
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "RUST_TEST_THREADS",
    "GOFLAGS",
    "VITEST_MAX_WORKERS",
];

/// The environment every command gets: the harness's own, less
/// [`DROPPED_PREFIXES`] and [`DROPPED`], with the run's state directory
/// and the build under test first on `PATH`.
pub fn environment(state: &Path, reef_bin_dir: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .filter(|(name, _)| {
            !DROPPED.contains(&name.as_str())
                && !DROPPED_PREFIXES.iter().any(|p| name.starts_with(p))
                && name != "PATH"
        })
        .collect();
    let inherited = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let path = std::env::join_paths(
        std::iter::once(reef_bin_dir.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .unwrap_or(inherited);
    env.push(("PATH".to_owned(), path));
    env.push(("REEF_STATE_DIR".to_owned(), state.into()));
    env
}

struct Ctx<'a> {
    config: &'a RunConfig,
    env: Vec<(String, OsString)>,
    state: PathBuf,
    tracked: Tracked,
    epoch: Instant,
    logs: PathBuf,
    /// Whether this scenario's `ws run`s use the build limit (#271).
    limit: Toggle,
}

impl Ctx<'_> {
    fn reef(&self) -> Command {
        let mut command = Command::new(self.config.reef_bin_dir.join("reef"));
        command.env_clear().envs(self.env.iter().cloned());
        command
    }

    fn reef_ok(&self, args: &[&str], cwd: &Path) -> Result<String, String> {
        crate::agent::process::output(self.reef().args(args).current_dir(cwd))
    }
}

/// Runs every repository's scenario and returns the report; also writes
/// `report.json` and `summary.md` to `config.out`. A repository that fails
/// is recorded with its error, and the run goes on to the next.
pub fn run(config: &RunConfig) -> Result<Report, String> {
    if config.tickets == 0 {
        return Err("--tickets must be at least 1".to_owned());
    }
    fs::create_dir_all(&config.out).map_err(|e| format!("{}: {e}", config.out.display()))?;
    let root = crate::agent::env::short_tempdir(&config.work_root, "f")?;
    let state = root.join("s");
    fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    if let Some(floor) = &config.disk_floor {
        let text = format!("[disk]\nfloor = {}\n", toml_string(floor));
        fs::write(state.join("config.toml"), text)
            .map_err(|e| format!("{}: {e}", state.display()))?;
    }
    let logs = config.out.join("logs");
    fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    let ctx = Ctx {
        config,
        env: environment(&state, &config.reef_bin_dir),
        state: state.clone(),
        tracked: Tracked::default(),
        epoch: Instant::now(),
        logs,
        limit: Toggle::On,
    };
    let (_, started_at) = crate::agent::process::today_and_now();
    let host = host(&ctx, &state);
    // A host that is not the profile's machine never decides a gate: its
    // run is `dev`, with the reason recorded.
    let mismatch = profile_mismatch(config.profile, host.cores, host.memory_bytes, &host.os);
    let profile = if mismatch.is_some() {
        Profile::Dev
    } else {
        config.profile
    };
    ctx.reef_ok(&["daemon", "start"], &root)?;
    // Stops the daemon and removes the run's directory however the run ends.
    let _guard = DaemonGuard {
        ctx: &ctx,
        root: &root,
        keep: config.keep_state,
    };
    let mut report = Report {
        schema: report::SCHEMA,
        started_at,
        profile,
        profile_note: mismatch.map(|why| format!("asked for {}: {why}", config.profile)),
        variant: config.variant,
        host,
        tickets: config.tickets,
        idle_secs: config.idle.as_secs(),
        repos: vec![],
    };
    for spec in &config.repos {
        // With `--limit both`, the scenario without the limit first, then
        // with it; the report judges the second and compares the two.
        let mut runs = Vec::new();
        for &limit in config.limits.runs() {
            let logs = if config.limits == LimitMode::Both {
                ctx.logs.join(format!("limit-{limit}"))
            } else {
                ctx.logs.clone()
            };
            let ctx = Ctx {
                config,
                env: ctx.env.clone(),
                state: ctx.state.clone(),
                tracked: Tracked::default(),
                epoch: ctx.epoch,
                logs,
                limit,
            };
            let mut repo = scenario(&ctx, spec);
            repo.limit = Some(limit);
            repo.summary = report::summarize(&repo, report.host.cores);
            runs.push(repo);
        }
        let Some(mut repo) = runs.pop() else {
            continue;
        };
        repo.off = runs.pop().map(Box::new);
        report.repos.push(repo);
        write(&report, &config.out)?;
    }
    Ok(report)
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

struct DaemonGuard<'a> {
    ctx: &'a Ctx<'a>,
    root: &'a Path,
    keep: bool,
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = self.ctx.reef_ok(&["daemon", "stop"], self.root);
        if !self.keep {
            let _ = fs::remove_dir_all(self.root);
        }
    }
}

/// Free space that moves by less than this between readings is steady.
const SETTLED_WITHIN: u64 = 256 << 20;

/// Waits, up to two minutes, until the daemon has finished deleting
/// released workspaces (no `.trash-` entry left in the state directory)
/// and the free space has held steady for three readings, so a disk reading
/// does not count a workspace that is already released.
fn settle(state: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last = None;
    let mut still = 0;
    while Instant::now() < deadline {
        let trash = walkdir::WalkDir::new(state)
            .max_depth(5)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".trash-"));
        // Within 256 MiB: other processes write too.
        let free = disk::free_bytes(state).ok();
        let steady = free
            .zip(last)
            .is_some_and(|(now, then)| now.abs_diff(then) < SETTLED_WITHIN);
        still = if !trash && steady { still + 1 } else { 0 };
        if still >= 2 {
            return;
        }
        last = free;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Clones and sets up every repository without running a scenario: warms
/// the download caches (`reef-fleet run --setup-only`).
pub fn setup_only(config: &RunConfig) -> Result<(), String> {
    let env = environment(&config.work_root.join("unused"), &config.reef_bin_dir);
    for spec in &config.repos {
        let prepared = repos::prepare(
            spec,
            &config.cache,
            &config.patches_root,
            &config.reef_checkout,
            config.tickets,
            &env,
        )?;
        repos::check_patches(&prepared, &config.cache, config.tickets, &env)?;
    }
    Ok(())
}

/// Writes `report.json` and `summary.md`.
pub fn write(report: &Report, out: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    fs::write(out.join("report.json"), json).map_err(|e| format!("report.json: {e}"))?;
    fs::write(out.join("summary.md"), report::render(report))
        .map_err(|e| format!("summary.md: {e}"))
}

fn scenario(ctx: &Ctx<'_>, spec: &RepoSpec) -> RepoReport {
    let mut repo = RepoReport {
        name: spec.name.clone(),
        stack: Some(spec.stack),
        url: spec.url.clone(),
        rev: spec.rev.clone(),
        gated: spec.gated,
        ..RepoReport::default()
    };
    if let Err(error) = scenario_inner(ctx, spec, &mut repo) {
        repo.error = Some(error);
    }
    repo
}

/// Per-ticket labels, for the sampler's RSS readings.
fn label(run: &str, ticket: u32, round: u32) -> String {
    format!("{run}-{ticket}-{round}")
}

#[allow(clippy::too_many_lines, reason = "the scenario, step by step")]
fn scenario_inner(ctx: &Ctx<'_>, spec: &RepoSpec, repo: &mut RepoReport) -> Result<(), String> {
    let config = ctx.config;
    let prepared = repos::prepare(
        spec,
        &config.cache,
        &config.patches_root,
        &config.reef_checkout,
        config.tickets,
        &ctx.env,
    )?;
    let mut prepared = prepared;
    repo.rev.clone_from(&prepared.rev);
    if prepared.spec.url == repos::SELF {
        "self".clone_into(&mut repo.url);
    }
    if config.variant.stores == Toggle::On {
        // Workspaces start on the commit that declares the stores.
        prepared.head = declare_stores(ctx, &prepared)?;
    }
    // `--limit both` runs the scenario twice against one daemon: the
    // second finds the repository registered.
    match ctx.reef_ok(
        &["repo", "register", &prepared.dir.to_string_lossy()],
        &prepared.dir,
    ) {
        Err(error) if error.contains("already registered") => {}
        other => {
            other?;
        }
    }
    if config.variant.stores == Toggle::On {
        build_stores(ctx, &prepared)?;
    }
    for line in &spec.clean {
        repos::shell(line, &prepared.dir, &ctx.env, &spec.env)
            .map_err(|e| format!("clean: {e}"))?;
    }

    // Watch the state directory's volume (the workspaces are under it) and
    // the clone's, if different.
    // Two APFS volumes in one container share their free space: count it
    // once.
    let mut volumes = vec![ctx.state.clone()];
    if disk::device(&prepared.dir).ok() != disk::device(&ctx.state).ok()
        && disk::free_bytes(&prepared.dir).ok() != disk::free_bytes(&ctx.state).ok()
    {
        volumes.push(prepared.dir.clone());
    }
    repo.volumes = volumes.iter().map(|v| v.display().to_string()).collect();
    repo.volume_bytes = volumes
        .iter()
        .map(|v| disk::volume_bytes(v).unwrap_or(0))
        .collect();

    let epoch = Instant::now();
    let ctx = Ctx {
        config,
        env: ctx.env.clone(),
        state: ctx.state.clone(),
        tracked: Tracked::default(),
        epoch,
        logs: ctx.logs.join(&spec.name),
        limit: ctx.limit,
    };
    fs::create_dir_all(&ctx.logs).map_err(|e| format!("{}: {e}", ctx.logs.display()))?;
    settle(&ctx.state);
    let kept_start = kept(&ctx);
    let caches_start = caches(spec);
    let sampler = Sampler::start(volumes, ctx.tracked.clone(), epoch);
    let canary = {
        let program = config.reef_bin_dir.join("reef");
        let env = ctx.env.clone();
        Canary::start(
            move || {
                let mut command = Command::new(&program);
                command
                    .env_clear()
                    .envs(env.iter().cloned())
                    .args(["ws", "list", "-o", "json"]);
                command
            },
            epoch,
        )
    };
    let build_sampler = {
        let program = config.reef_bin_dir.join("reef");
        let env = ctx.env.clone();
        BuildSampler::start(
            move || {
                let mut command = Command::new(&program);
                command
                    .env_clear()
                    .envs(env.iter().cloned())
                    .args(["daemon", "status", "-o", "json"]);
                command
            },
            epoch,
        )
    };
    // The daemon's own disk figures (#272), which G1 and G4 read.
    let disk_watch = {
        let program = config.reef_bin_dir.join("reef");
        let env = ctx.env.clone();
        DiskWatch::start(
            move || {
                let mut command = Command::new(&program);
                command
                    .env_clear()
                    .envs(env.iter().cloned())
                    .args(["daemon", "status", "-o", "json"]);
                command
            },
            epoch,
        )
    };
    let mut phases = Vec::new();
    let mark = |phases: &mut Vec<Phase>, name: &str, start: u64| {
        phases.push(Phase {
            name: name.to_owned(),
            start_ms: start,
            end_ms: millis(epoch.elapsed()),
        });
    };
    // Let the sampler take a reading before anything runs.
    std::thread::sleep(Duration::from_millis(1500));

    let result = (|| -> Result<(), String> {
        // 1. Solo, one ticket at a time.
        let solo_start = millis(epoch.elapsed());
        for ticket in 1..=config.solo_tickets.clamp(1, config.tickets) {
            // Ticket 1, the ratios' reference, runs `--solo-runs` times
            // for its median; a repeat logs as `solo<n>`.
            let attempts = if ticket == 1 {
                config.solo_runs.max(1)
            } else {
                1
            };
            for attempt in 1..=attempts {
                let start = millis(epoch.elapsed());
                let name = if attempt == 1 {
                    "solo".to_owned()
                } else {
                    format!("solo{attempt}")
                };
                let run = ticket_run(&ctx, &prepared, &name, ticket, 1, None, false, "")?;
                if ticket == 1 && attempt == 1 {
                    mark(&mut phases, "solo-1", start);
                }
                if run.workspace_id.is_none() {
                    repo.creates_refused += 1;
                    repo.refused_for_disk += u32::from(run.refused_for_disk);
                }
                repo.solo.push(run);
            }
        }
        mark(&mut phases, "solo", solo_start);

        // The spike's seed source, kept until the release.
        let mut seed_source = None;
        let source_script = seed::source_script(
            config.variant.seed,
            &prepared.patch(1, 1),
            &spec.build,
            spec.seed_prepare.as_deref(),
        )?;
        if let Some(script) = source_script {
            let start = millis(epoch.elapsed());
            let run = seed_run(&ctx, &prepared, &script)?;
            mark(&mut phases, "seed", start);
            seed_source = run
                .workspace_id
                .clone()
                .zip(workspace_path(&ctx, run.workspace_id.as_deref()));
            repo.seed = Some(run);
            if seed_source.is_none() {
                return Err("the seed's source workspace was not kept".to_owned());
            }
        }
        let seed_lines = seed_source
            .as_ref()
            .map(|(_, path)| seed::clone_lines(path, &spec.build_dirs))
            .unwrap_or_default();

        // 2. Five at once.
        settle(&ctx.state);
        let five_start = millis(epoch.elapsed());
        let runs: Vec<Vec<TicketRun>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=config.tickets)
                .map(|ticket| {
                    let ctx = &ctx;
                    let prepared = &prepared;
                    let seed_lines = &seed_lines;
                    let delay = config.stagger * (ticket - 1) / config.tickets.max(1);
                    scope.spawn(move || {
                        std::thread::sleep(delay);
                        let mut out = Vec::new();
                        let first =
                            ticket_run(ctx, prepared, "five", ticket, 1, None, true, seed_lines);
                        let first = first.unwrap_or_else(|error| TicketRun {
                            ticket,
                            round: 1,
                            error: Some(error),
                            ..TicketRun::default()
                        });
                        let workspace = first.workspace_id.clone();
                        out.push(first);
                        if let Some(id) = workspace
                            && let Ok(second) =
                                ticket_run(ctx, prepared, "five", ticket, 2, Some(&id), true, "")
                        {
                            out.push(second);
                        }
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        mark(&mut phases, "five", five_start);
        for runs in runs {
            for run in runs {
                if run.round == 1 && run.workspace_id.is_none() {
                    repo.creates_refused += 1;
                    repo.refused_for_disk += u32::from(run.refused_for_disk);
                }
                repo.five.push(run);
            }
        }

        // Idle, workspaces kept.
        let idle_start = millis(epoch.elapsed());
        std::thread::sleep(config.idle);
        mark(&mut phases, "idle", idle_start);

        // Release, then gc.
        let release_start = millis(epoch.elapsed());
        let mut kept: Vec<String> = repo
            .five
            .iter()
            .filter(|t| t.round == 1)
            .filter_map(|t| t.workspace_id.clone())
            .collect();
        kept.extend(seed_source.map(|(id, _)| id));
        // Every one is released even if one fails, so none holds disk
        // through the next repository's scenario.
        let mut errors: Vec<String> = kept
            .iter()
            .filter_map(|id| {
                ctx.reef_ok(&["ws", "release", id], &prepared.dir)
                    .err()
                    .map(|e| format!("releasing {id}: {e}"))
            })
            .collect();
        mark(&mut phases, "release", release_start);
        let gc_start = millis(epoch.elapsed());
        // `--limit both`'s first scenario keeps its objects: a `ws gc`
        // between two scenarios on one repository left the second's
        // identical trees unwritten (a pruned tree a commit still names),
        // and the off run only provides G8's reference.
        let gc_now = !(config.limits == LimitMode::Both && ctx.limit == Toggle::Off);
        if gc_now
            && let Err(e) = ctx.reef_ok(
                &["ws", "gc", "--repo", &prepared.dir.to_string_lossy()],
                &prepared.dir,
            )
        {
            errors.push(format!("ws gc: {e}"));
        }
        // The released workspaces are deleted in the background.
        settle(&ctx.state);
        std::thread::sleep(Duration::from_millis(1500));
        mark(&mut phases, "gc", gc_start);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    })();

    let readings = canary.finish();
    repo.daemon_disk = disk_watch.finish();
    let taken = sampler.finish();
    repo.build_samples = build_sampler.finish();
    repo.lateness_p99_us = lateness_by_phase(&readings, &phases);
    repo.list_ms = readings.list_ms;
    repo.list_failures = readings.list_failures;
    // Each run's peak RSS, from the sampler's readings.
    let mut peaks: HashMap<String, u64> = HashMap::new();
    for s in &taken {
        for (label, rss) in &s.ticket_rss {
            let peak = peaks.entry(label.clone()).or_default();
            *peak = (*peak).max(*rss);
        }
    }
    // Each run's own label is its log's name: a repeated solo run's
    // (`solo2-1-1`) is not the first's.
    for t in repo.solo.iter_mut().chain(repo.five.iter_mut()) {
        let own = Path::new(&t.log)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        t.peak_rss_bytes = peaks.get(&own).copied().unwrap_or(0);
    }
    if let Some(t) = &mut repo.seed {
        t.peak_rss_bytes = peaks.get(&label("seed", 0, 1)).copied().unwrap_or(0);
    }
    repo.samples = taken;
    repo.phases = phases;
    repo.kept = growth(&kept_start, &kept(&ctx));
    repo.caches = growth(&caches_start, &caches(spec));
    result
}

fn lateness_by_phase(readings: &CanaryReadings, phases: &[Phase]) -> BTreeMap<String, u32> {
    phases
        .iter()
        .filter_map(|phase| {
            let values: Vec<f64> = readings
                .lateness_us
                .iter()
                .filter(|(t, _)| *t >= phase.start_ms && *t <= phase.end_ms)
                .map(|(_, us)| f64::from(*us))
                .collect();
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "microseconds of a sleep's lateness"
            )]
            let p99 = sample::percentile(values, 99.0)? as u32;
            Some((phase.name.clone(), p99))
        })
        .collect()
}

/// What `reef ws run -o json` prints on exit.
#[derive(Debug, Default, Deserialize)]
struct WsRunReport {
    workspace_id: Option<String>,
    path: Option<String>,
    exit_code: Option<i32>,
    error: Option<serde_json::Value>,
}

/// One `reef ws run`: ticket `ticket`'s round `round`, in a new workspace
/// or in `workspace`, applying its patch after `pre` (the seed's clone
/// lines) and running the build. `Err` only when the harness itself fails
/// (a log it cannot write).
#[allow(clippy::too_many_arguments, reason = "one call per ticket round")]
fn ticket_run(
    ctx: &Ctx<'_>,
    prepared: &Prepared,
    run: &str,
    ticket: u32,
    round: u32,
    workspace: Option<&str>,
    keep: bool,
    pre: &str,
) -> Result<TicketRun, String> {
    let patch = prepared.patch(ticket, round);
    let script = format!(
        "{pre}git apply --whitespace=nowarn {} || {{ echo 'reef-fleet: the patch did not apply'; exit 90; }}\n{}{}\n",
        quote(&patch.to_string_lossy()),
        exports(&prepared.spec.env),
        prepared.spec.build,
    );
    ws_run(ctx, prepared, run, ticket, round, workspace, keep, &script)
}

/// The seed's source: a kept workspace that ran `script`.
fn seed_run(ctx: &Ctx<'_>, prepared: &Prepared, script: &str) -> Result<TicketRun, String> {
    let script = format!("{}{script}", exports(&prepared.spec.env));
    ws_run(ctx, prepared, "seed", 0, 1, None, true, &script)
}

fn exports(env: &BTreeMap<String, String>) -> String {
    env.iter().fold(String::new(), |mut out, (k, v)| {
        let _ = writeln!(out, "export {k}={}", quote(v));
        out
    })
}

#[allow(clippy::too_many_arguments, reason = "one call per ticket round")]
fn ws_run(
    ctx: &Ctx<'_>,
    prepared: &Prepared,
    run: &str,
    ticket: u32,
    round: u32,
    workspace: Option<&str>,
    keep: bool,
    body: &str,
) -> Result<TicketRun, String> {
    let label = label(run, ticket, round);
    let log = ctx.logs.join(format!("{label}.log"));
    // Append mode: `ws run`'s own stderr shares this file with the
    // script's `>>`, and must not write over the build's first lines.
    let _ = fs::remove_file(&log);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let _ = writeln!(file, "== reef-fleet {} {label}", prepared.spec.name);
    let script = format!("exec >>{} 2>&1\n{body}", quote(&log.to_string_lossy()));
    let purpose = format!("fleet {} {label}", prepared.spec.name);
    let dir = prepared.dir.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["ws", "run", "-o", "json"];
    match workspace {
        Some(id) => args.extend(["--workspace", id]),
        // Long enough for both rounds and the idle period.
        None => args.extend([
            "--lease",
            "6h",
            "--repo",
            &dir,
            "--new-on",
            &prepared.head,
            "--purpose",
            &purpose,
        ]),
    }
    if keep {
        args.push("--keep");
    }
    if ctx.limit == Toggle::Off {
        args.push("--no-build-limit");
    }
    args.extend(["--", "sh", "-c", &script]);
    let started_ms = millis(ctx.epoch.elapsed());
    let started = Instant::now();
    let stderr = file
        .try_clone()
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let child = ctx
        .reef()
        .args(&args)
        .current_dir(&prepared.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("reef ws run: {e}"))?;
    ctx.tracked.add(&label, child.id());
    let output = child.wait_with_output();
    ctx.tracked.remove(&label);
    let wall_ms = millis(started.elapsed());
    let output = output.map_err(|e| format!("reef ws run: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: WsRunReport = serde_json::from_str(stdout.trim()).unwrap_or_default();
    let text = fs::read_to_string(&log).unwrap_or_default();
    let error = parsed.error.as_ref().map(ToString::to_string).or_else(|| {
        (parsed.workspace_id.is_none()).then(|| {
            text.lines()
                .rev()
                .find(|l| l.starts_with("reef:") || l.contains("error"))
                .unwrap_or("reef ws run printed no report")
                .to_owned()
        })
    });
    let disk = if keep {
        parsed
            .path
            .as_deref()
            .and_then(|p| disk::workspace(Path::new(p), &prepared.spec.build_dirs).ok())
    } else {
        None
    };
    let (daemon_disk, refused_for_disk) = daemon_figures(ctx, &parsed, keep);
    Ok(TicketRun {
        ticket,
        round,
        workspace_id: parsed.workspace_id,
        exit_code: parsed.exit_code,
        wall_ms,
        started_ms,
        outcomes: outcomes::parse(&text),
        units_compiled: outcomes::units_compiled(&text),
        peak_rss_bytes: 0,
        disk,
        daemon_disk,
        refused_for_disk,
        enospc: outcomes::enospc(&text),
        error,
        log: format!("logs/{}/{label}.log", prepared.spec.name),
    })
}

/// The daemon's figure for a kept workspace, beside the harness's own walk,
/// and whether `ws run`'s create was refused for disk (#272).
fn daemon_figures(
    ctx: &Ctx<'_>,
    parsed: &WsRunReport,
    keep: bool,
) -> (Option<DaemonWorkspaceDisk>, bool) {
    let daemon_disk = if keep {
        parsed
            .workspace_id
            .as_deref()
            .and_then(|id| daemon_workspace_disk(ctx, id))
    } else {
        None
    };
    let refused = parsed
        .error
        .as_ref()
        .and_then(|error| error.get("reason"))
        .and_then(serde_json::Value::as_str)
        == Some("DISK_SPACE_LOW");
    (daemon_disk, refused)
}

/// `reef ws inspect <id>`'s `disk`: the daemon walks the workspace and
/// keeps the figure (#272). `None` from a daemon before it.
fn daemon_workspace_disk(ctx: &Ctx<'_>, id: &str) -> Option<DaemonWorkspaceDisk> {
    #[derive(Deserialize)]
    struct Inspect {
        disk: Option<DaemonWorkspaceDisk>,
    }
    let json = ctx
        .reef_ok(
            &["ws", "inspect", id, "--no-snapshot", "-o", "json"],
            &ctx.state,
        )
        .ok()?;
    serde_json::from_str::<Inspect>(&json).ok()?.disk
}

fn workspace_path(ctx: &Ctx<'_>, id: Option<&str>) -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Inspect {
        path: String,
    }
    let json = ctx
        .reef_ok(&["ws", "inspect", id?, "-o", "json"], &ctx.state)
        .ok()?;
    serde_json::from_str::<Inspect>(&json)
        .ok()
        .map(|i| PathBuf::from(i.path))
}

/// Commits the repository's `reef.toml` to the clone (`--stores on`) on
/// top of the set-up commit, and returns the new commit.
fn declare_stores(ctx: &Ctx<'_>, prepared: &Prepared) -> Result<String, String> {
    let text = prepared.spec.reef_toml.as_deref().ok_or_else(|| {
        format!(
            "--stores on: `{}` declares no reef_toml in repos.toml",
            prepared.spec.name
        )
    })?;
    fs::write(prepared.dir.join("reef.toml"), text).map_err(|e| format!("reef.toml: {e}"))?;
    repos::git(&["add", "reef.toml"], &prepared.dir, &ctx.env)?;
    repos::git(
        &["commit", "--quiet", "-m", "reef-fleet: declare stores"],
        &prepared.dir,
        &ctx.env,
    )?;
    repos::git(&["rev-parse", "HEAD"], &prepared.dir, &ctx.env)
}

/// Builds each store `reef.toml` declares.
fn build_stores(ctx: &Ctx<'_>, prepared: &Prepared) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Declared {
        #[serde(default)]
        store: Vec<Store>,
    }
    #[derive(Deserialize)]
    struct Store {
        name: String,
    }
    let text = prepared.spec.reef_toml.as_deref().unwrap_or_default();
    let declared: Declared = toml::from_str(text).map_err(|e| format!("reef_toml: {e}"))?;
    for store in declared.store {
        ctx.reef_ok(
            &[
                "store",
                "build",
                &store.name,
                "--repo",
                &prepared.dir.to_string_lossy(),
            ],
            &prepared.dir,
        )
        .map_err(|e| format!("reef store build {}: {e}", store.name))?;
    }
    Ok(())
}

/// What Reef keeps on purpose: its bases and stores, bytes by name
/// (never workspaces or trash).
fn kept(ctx: &Ctx<'_>) -> BTreeMap<String, u64> {
    let mut kept = BTreeMap::new();
    let mut bases = 0;
    if let Ok(repos) = fs::read_dir(ctx.state.join("repos")) {
        for repo in repos.flatten() {
            if let Ok(f) = disk::footprint(&repo.path().join("bases")) {
                bases += f.freeable_bytes();
            }
        }
    }
    kept.insert("reef bases".to_owned(), bases);
    if let Ok(f) = disk::footprint(&ctx.state.join("stores")) {
        kept.insert("reef stores".to_owned(), f.freeable_bytes());
    }
    kept
}

/// The repository's shared tool caches (`~` expanded), allocated bytes by
/// name.
fn caches(spec: &RepoSpec) -> BTreeMap<String, u64> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    spec.caches
        .iter()
        .filter_map(|cache| {
            let path = match (cache.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => PathBuf::from(cache),
            };
            let f = disk::footprint(&path).ok()?;
            Some((cache.clone(), f.allocated_bytes))
        })
        .collect()
}

/// How much each entry grew from `start` to `end` (never negative).
fn growth(start: &BTreeMap<String, u64>, end: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    end.iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.saturating_sub(start.get(k).copied().unwrap_or(0)),
            )
        })
        .collect()
}

fn host(ctx: &Ctx<'_>, state: &Path) -> Host {
    let sysctl =
        |name: &str| crate::agent::process::output(Command::new("sysctl").args(["-n", name])).ok();
    let memory_bytes = if cfg!(target_os = "macos") {
        sysctl("hw.memsize").and_then(|v| v.parse().ok())
    } else {
        fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                .map(|kib| kib * 1024)
        })
    };
    let mut tools = BTreeMap::new();
    for (name, args) in [
        ("cargo", &["--version"][..]),
        ("go", &["version"][..]),
        ("node", &["--version"][..]),
        ("npm", &["--version"][..]),
        ("uv", &["--version"][..]),
        ("git", &["--version"][..]),
    ] {
        if let Ok(version) = crate::agent::process::output(
            Command::new(name)
                .args(args)
                .env_clear()
                .envs(ctx.env.iter().cloned()),
        ) {
            tools.insert(
                name.to_owned(),
                version.lines().next().unwrap_or("").to_owned(),
            );
        }
    }
    let local_snapshots = if cfg!(target_os = "macos") {
        crate::agent::process::output(Command::new("tmutil").args(["listlocalsnapshots", "/"]))
            .map(|out| {
                out.lines()
                    .filter(|l| l.contains("com.apple"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    let spotlight = cfg!(target_os = "macos")
        .then(|| {
            crate::agent::process::output(
                Command::new("mdutil").args(["-s", "/System/Volumes/Data"]),
            )
            .ok()
            .and_then(|out| {
                out.lines()
                    .last()
                    .map(|l| l.trim().trim_end_matches('.').to_owned())
            })
        })
        .flatten();
    Host {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        cores: sample::cores(),
        performance_cores: cfg!(target_os = "macos")
            .then(|| sysctl("hw.perflevel0.physicalcpu").and_then(|v| v.parse().ok()))
            .flatten(),
        memory_bytes: memory_bytes.unwrap_or(0),
        volume_bytes: disk::volume_bytes(state).unwrap_or(0),
        reef_version: ctx.reef_ok(&["--version"], state).unwrap_or_default(),
        tools,
        local_snapshots,
        spotlight,
    }
}

/// Why `host` does not look like `profile`'s machine, if it does not.
pub fn profile_mismatch(
    profile: Profile,
    cores: u32,
    memory_bytes: u64,
    os: &str,
) -> Option<String> {
    let gib = memory_bytes >> 30;
    match profile {
        Profile::Standard if os != "macos" || cores != 8 || gib > 16 => Some(format!(
            "the standard profile is macOS with 8 cores and 16 GiB; this host is {os} with {cores} cores and {gib} GiB"
        )),
        Profile::Linux if os != "linux" => {
            Some(format!("the linux profile runs on Linux, not {os}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_drops_reef_and_build_settings() {
        let env = environment(Path::new("/s"), Path::new("/bin-under-test"));
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"REEF_STATE_DIR"));
        assert!(!names.contains(&"CARGO_TARGET_DIR"));
        assert_eq!(names.iter().filter(|n| n.starts_with("REEF_")).count(), 1);
        let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.to_string_lossy().starts_with("/bin-under-test"));
    }

    #[test]
    fn profiles_name_their_machines() {
        assert!(profile_mismatch(Profile::Standard, 8, 16 << 30, "macos").is_none());
        assert!(profile_mismatch(Profile::Standard, 12, 32 << 30, "macos").is_some());
        assert!(profile_mismatch(Profile::Linux, 4, 16 << 30, "macos").is_some());
        assert!(profile_mismatch(Profile::Dev, 12, 32 << 30, "macos").is_none());
    }
}
