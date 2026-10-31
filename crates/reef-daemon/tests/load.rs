//! The Phase 1 load test (R1.23): two hundred workspaces with active
//! watchers on one daemon, and the time from a write to the snapshot that
//! holds it. See `docs/load-tests.md` for the profile, the gate, and the
//! baselines.
//!
//! `#[ignore]`d, and a no-op unless `REEF_LOAD=1`:
//!
//! ```sh
//! REEF_LOAD=1 cargo nextest run -p reef-daemon --test load --run-ignored all --no-capture
//! ```
//!
//! Knobs: `REEF_LOAD_PRESET` (`medium`, or `large`), `REEF_LOAD_WORKSPACES`
//! (200), `REEF_LOAD_STEADY_SECS` (300), `REEF_LOAD_WRITE_PERIOD_SECS`
//! (60), `REEF_LOAD_WATCHER` (`notify`, or `watchman`),
//! `REEF_LOAD_NARROWED_MAX_PATHS` (the daemon's default; `0` scans every
//! tracked file on every snapshot, as before R1.30), `REEF_LOAD_SYMBOLS`
//! (`0`; `1` indexes every snapshot's tree in the background, R2.14),
//! `REEF_LOAD_HISTORY` (`1`, the daemon's default; `0` turns the history
//! index off, R2.16). The report is
//! `target/load/<os>-<preset>-<timestamp>.json`, and a Markdown summary is
//! printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    reason = "a measurement program: a panic is the failure report, and it prints its results"
)]

mod common;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::load::{
    Monitor, gb, keep_daemon_log, machine, mb, mean, millis, now_ms, percentile, summary_lines,
    utc_stamp, write_report,
};
use common::spawned::{SpawnedDaemon, free_bytes, workspace_dirs};
use common::{SyntheticRepo, watcher_name};
use futures::StreamExt as _;
use reef_api::v1::{
    CreateTimings, CreateWorkspaceRequest, CreateWorkspaceResponse, ListWorkspacesRequest,
    WatchState, Workspace, WorkspaceState,
};
use reef_daemon::client::Client;
use reef_daemon::client::json::backend_name;
use reef_daemon::config::{FileConfig, WatcherBackend};
use reef_testing::synth::Preset;
use serde_json::{Value, json};

/// The phase exit criterion: steady-state write to snapshot, p95.
const GATE_P95: Duration = Duration::from_secs(5);
/// No write may still be unsnapshotted this long after it was made.
const GATE_MAX: Duration = Duration::from_secs(30);
/// The creation criterion, on the copy-on-write tiers.
const CREATE_TARGET: Duration = Duration::from_secs(1);
/// Workspaces created on `large` in every run.
const LARGE_CREATES: usize = 20;
/// Creates in flight at once while making the N workspaces.
const CREATE_CONCURRENCY: usize = 8;
const SAMPLE_EVERY: Duration = Duration::from_millis(250);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);
const OWNER: &str = "load@host";

/// The run's parameters, from the environment.
#[derive(Clone, Debug)]
struct Params {
    preset: Preset,
    workspaces: usize,
    steady: Duration,
    period: Duration,
    watcher: WatcherBackend,
    narrowed_max_paths: Option<usize>,
    symbols: bool,
    history: bool,
}

impl Params {
    fn from_env() -> Params {
        let number = |name: &str, default: u64| {
            std::env::var(name).ok().map_or(default, |text| {
                text.parse().unwrap_or_else(|_| panic!("{name}={text}"))
            })
        };
        let preset = match std::env::var("REEF_LOAD_PRESET").as_deref() {
            Err(_) | Ok("" | "medium") => Preset::Medium,
            Ok("large") => Preset::Large,
            Ok(other) => panic!("REEF_LOAD_PRESET={other}: expected medium or large"),
        };
        let watcher = match std::env::var("REEF_LOAD_WATCHER").as_deref() {
            Err(_) | Ok("" | "notify") => WatcherBackend::Notify,
            Ok("watchman") => WatcherBackend::Watchman,
            Ok(other) => panic!("REEF_LOAD_WATCHER={other}: expected notify or watchman"),
        };
        Params {
            preset,
            workspaces: usize::try_from(number("REEF_LOAD_WORKSPACES", 200)).unwrap(),
            steady: Duration::from_secs(number("REEF_LOAD_STEADY_SECS", 300)),
            period: Duration::from_secs(number("REEF_LOAD_WRITE_PERIOD_SECS", 60)),
            watcher,
            narrowed_max_paths: std::env::var("REEF_LOAD_NARROWED_MAX_PATHS")
                .ok()
                .map(|text| {
                    text.parse()
                        .unwrap_or_else(|_| panic!("REEF_LOAD_NARROWED_MAX_PATHS={text}"))
                }),
            symbols: number("REEF_LOAD_SYMBOLS", 0) == 1,
            history: number("REEF_LOAD_HISTORY", 1) == 1,
        }
    }
}

fn preset_name(preset: Preset) -> &'static str {
    match preset {
        Preset::Small => "small",
        Preset::Medium => "medium",
        Preset::Large => "large",
        Preset::Xl => "xl",
    }
}

/// Rough bytes a preset needs: its tree, its git objects, one base, and
/// the working-copy state of `workspaces` workspaces (`docs/benchmarks.md`
/// and the brief's disk budget).
fn bytes_needed(preset: Preset, workspaces: usize) -> u64 {
    let (tree, tree_state) = match preset {
        Preset::Large => (592 << 20, 15 << 20),
        _ => (60 << 20, 2 << 20),
    };
    3 * tree + workspaces as u64 * tree_state
}

fn configure(config: &mut FileConfig, params: &Params) {
    // Notify unless asked: `auto` would pick Watchman for `large` where it is
    // installed (R1.30 compares the two).
    config.watcher.backend = params.watcher;
    if let Some(max) = params.narrowed_max_paths {
        config.watcher.narrowed_max_paths = max;
    }
    config.symbols.background = params.symbols;
    config.history.enabled = params.history;
    // Longer than the run: the heartbeats stop after the burst, and
    // creating and releasing 200 `large` workspaces on macOS each take more
    // than the default 30 minutes. An expiry would reclaim a workspace
    // mid-run and fail its release.
    config.lease.default = Duration::from_hours(4);
}

#[allow(
    clippy::too_many_lines,
    reason = "the phases in order, as docs/load-tests.md lists them"
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the load test: REEF_LOAD=1 and --run-ignored; see docs/load-tests.md"]
async fn load_profile() {
    if std::env::var("REEF_LOAD").as_deref() != Ok("1") {
        eprintln!("skipped: set REEF_LOAD=1 to run the load test");
        return;
    }
    let params = Params::from_env();
    let started_at = utc_stamp();
    let daemon = SpawnedDaemon::spawn(|config| configure(config, &params)).await;
    let free_at_start = free_bytes(daemon.root());
    let needed = bytes_needed(Preset::Large, LARGE_CREATES)
        + if params.preset == Preset::Large {
            bytes_needed(Preset::Large, params.workspaces) - 3 * (592 << 20)
        } else {
            bytes_needed(params.preset, params.workspaces)
        }
        + (1 << 30);
    assert!(
        free_at_start >= needed,
        "the load test needs about {:.1} GB free under {}; {:.1} GB is",
        gb(needed),
        daemon.root().display(),
        gb(free_at_start)
    );
    let monitor = Monitor::start(daemon.pid);
    let mut report = json!({
        "started_at": started_at,
        "machine": machine(),
        "params": {
            "preset": preset_name(params.preset),
            "workspaces": params.workspaces,
            "watcher": params.watcher.as_str(),
            "narrowed_max_paths": params.narrowed_max_paths,
            "symbols": params.symbols,
            "history": params.history,
            "steady_secs": params.steady.as_secs(),
            "write_period_secs": params.period.as_secs(),
            "create_concurrency": CREATE_CONCURRENCY,
            "sample_every_ms": SAMPLE_EVERY.as_millis(),
            "heartbeat_every_secs": HEARTBEAT_EVERY.as_secs(),
        },
        "config": serde_json::to_value(&daemon.config).unwrap(),
        "free_bytes_at_start": free_at_start,
    });

    // Generate and register.
    let begun = Instant::now();
    let large = daemon.register_synth(Preset::Large).await;
    report["register_large_ms"] = json!(millis(begun.elapsed()));
    let repo = if params.preset == Preset::Large {
        large.clone()
    } else {
        let begun = Instant::now();
        let repo = daemon.register_synth(params.preset).await;
        report["register_preset_ms"] = json!(millis(begun.elapsed()));
        repo
    };
    eprintln!("load: registered");

    // Twenty on `large`, one at a time, after one that builds the base.
    let client = &daemon.client;
    let begun = Instant::now();
    let warm = create(client, &large.id).await;
    let base_ms = millis(begun.elapsed());
    release_all(client, std::slice::from_ref(&warm.id)).await;
    let free_before = free_bytes(daemon.root());
    let mut large_creates = Vec::new();
    let mut large_ids = Vec::new();
    for _ in 0..LARGE_CREATES {
        let begun = Instant::now();
        let ws = create(client, &large.id).await;
        large_creates.push(millis(begun.elapsed()));
        large_ids.push(ws.id);
    }
    let free_after = free_bytes(daemon.root());
    let large_drain_ms = wait_attached(client, &large_ids).await;
    let backend = backend_name(workspace(client, &large_ids[0]).await.backend);
    let cow = matches!(backend, "apfs" | "overlayfs");
    report["large_creates"] = json!({
        "backend": backend,
        "copy_on_write": cow,
        "base_build_ms": base_ms,
        "ms": large_creates,
        "max_ms": large_creates.iter().max(),
        "mean_ms": mean(&large_creates),
        "free_bytes_per_workspace": free_before.saturating_sub(free_after) / LARGE_CREATES as u64,
        "attach": attach_section(&daemon, &large_ids, large_drain_ms),
    });
    release_all(client, &large_ids).await;
    eprintln!("load: {LARGE_CREATES} large creates done");

    // N on the preset, eight at a time.
    let begun = Instant::now();
    let warm = create(client, &repo.id).await;
    let preset_base_ms = millis(begun.elapsed());
    release_all(client, std::slice::from_ref(&warm.id)).await;
    let free_before = free_bytes(daemon.root());
    let begun = Instant::now();
    let created: Vec<(CreateWorkspaceResponse, u64)> = futures::stream::iter(0..params.workspaces)
        .map(|_| {
            let client = client.clone();
            let repo = repo.id.clone();
            async move {
                let begun = Instant::now();
                let response = create_response(&client, &repo).await;
                (response, millis(begun.elapsed()))
            }
        })
        .buffer_unordered(CREATE_CONCURRENCY)
        .collect()
        .await;
    let all_ms = millis(begun.elapsed());
    let free_after = free_bytes(daemon.root());
    let mut create_ms: Vec<u64> = created.iter().map(|(_, ms)| *ms).collect();
    create_ms.sort_unstable();
    let steps = step_percentiles(created.iter().filter_map(|(response, _)| response.timings));
    let workspaces: Vec<Workspace> = created
        .into_iter()
        .map(|(response, _)| response.workspace.unwrap())
        .collect();
    let mut watchers = HashMap::<&str, usize>::new();
    for ws in &workspaces {
        *watchers.entry(watcher_name(ws.watcher())).or_default() += 1;
    }
    // The criterion is at steady state with every watch up (R1.32).
    let ids: Vec<String> = workspaces.iter().map(|ws| ws.id.clone()).collect();
    let drain_ms = wait_attached(client, &ids).await;
    report["creates"] = json!({
        "backend": backend_name(workspaces[0].backend),
        "watchers": watchers,
        "base_build_ms": preset_base_ms,
        "total_ms": all_ms,
        "ms": create_ms,
        "p50_ms": percentile(&create_ms, 50),
        "p95_ms": percentile(&create_ms, 95),
        "max_ms": create_ms.last(),
        "free_bytes_per_workspace": free_before.saturating_sub(free_after) / params.workspaces as u64,
        "steps": steps,
        "attach": attach_section(&daemon, &ids, drain_ms),
    });
    eprintln!("load: {} creates in {all_ms} ms", params.workspaces);

    // Steady state, once every watch is up, then a burst.
    let load = Arc::new(Load::new(&daemon, &repo, &workspaces));
    let heartbeats = load.heartbeat(&workspaces);
    let sampler = load.sample();
    let steady_from = utc_stamp();
    let steady = load.steady(&params).await;
    eprintln!("load: steady state done");
    let burst = load.burst().await;
    eprintln!("load: burst done");
    load.stop.cancel();
    sampler.await.unwrap();
    let heartbeat_count = heartbeats.await.unwrap();

    let steady_stats = steady.stats();
    let burst_stats = burst.stats();
    report["steady"] = steady_stats.to_json();
    report["steady"]["samples"] = steady.samples_json();
    report["burst"] = burst_stats.to_json();
    report["burst"]["samples"] = burst.samples_json();
    report["heartbeat_rpcs"] = json!(heartbeat_count);
    report["list_rpcs"] = json!(load.list_calls.load(Ordering::Relaxed));

    // Limits, then the watcher's own account.
    report["limits"] = limits(&daemon);
    report["watcher_summaries"] = watcher_summaries(&daemon);
    report["watcher_scans"] = watcher_scans(&daemon, &steady_from);

    // Teardown.
    let begun = Instant::now();
    let mut release_ms = Vec::new();
    let mut release_failures = Vec::new();
    for result in release_each(client, &ids).await {
        match result {
            Ok(ms) => release_ms.push(ms),
            Err(error) => release_failures.push(error),
        }
    }
    release_ms.sort_unstable();
    report["teardown"] = json!({
        "total_ms": millis(begun.elapsed()),
        "release_p50_ms": percentile(&release_ms, 50),
        "release_p95_ms": percentile(&release_ms, 95),
        "release_max_ms": release_ms.last(),
        "failures": release_failures,
    });
    let left: Vec<String> = [&large, &repo]
        .iter()
        .flat_map(|repo| workspace_dirs(&daemon.state_dir.join("repos").join(&repo.id).join("ws")))
        .collect();
    report["directories_left"] = json!(left);
    monitor.stop();
    report["limits"]["peak_threads"] = json!(monitor.peak_threads());
    report["limits"]["peak_rss_bytes"] = json!(monitor.peak_rss());

    let create_ok = !cow || large_creates.iter().all(|ms| *ms < millis(CREATE_TARGET));
    let gate = steady_stats.p95_ms <= millis(GATE_P95) && steady_stats.unsnapshotted == 0;
    report["pass"] = json!({ "gate": gate, "creates_under_1s": create_ok });
    let name = format!("{}-{}", std::env::consts::OS, preset_name(params.preset));
    let path = write_report(&report, &name, &started_at);
    keep_daemon_log(&daemon, &path);
    println!("{}", markdown(&report));
    println!("report: {}", path.display());

    assert_eq!(release_failures, Vec::<String>::new(), "failed releases");
    assert_eq!(
        left,
        Vec::<String>::new(),
        "workspace directories left after teardown"
    );
    // The creation criterion is R1.12's bench's; here it is a cross-check,
    // reported with the backend, not a gate (the runners are slower than
    // the machines it was measured on).
    if !create_ok {
        eprintln!(
            "note: a {backend} create on large took {} ms, over {CREATE_TARGET:?}",
            large_creates.iter().max().unwrap()
        );
    }
    assert!(
        gate,
        "steady state: p95 {} ms (gate {} ms), {} writes unsnapshotted after {GATE_MAX:?}",
        steady_stats.p95_ms,
        millis(GATE_P95),
        steady_stats.unsnapshotted
    );
}

async fn create(client: &Client, repo: &str) -> Workspace {
    create_response(client, repo).await.workspace.unwrap()
}

async fn create_response(client: &Client, repo: &str) -> CreateWorkspaceResponse {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
}

/// The p50 and p95 of each step the daemon timed (R1.29), over `timings`.
fn step_percentiles(timings: impl Iterator<Item = CreateTimings>) -> Value {
    let timings: Vec<CreateTimings> = timings.collect();
    let step = |get: fn(&CreateTimings) -> u64| {
        let mut values: Vec<u64> = timings.iter().map(get).collect();
        values.sort_unstable();
        json!([percentile(&values, 50), percentile(&values, 95)])
    };
    json!({
        "resolve_ms": step(|t| t.resolve_ms),
        "base_ms": step(|t| t.base_ms),
        "materialize_ms": step(|t| t.materialize_ms),
        "mount_ms": step(|t| t.mount_ms),
        "jj_ms": step(|t| t.jj_ms),
        "seed_ms": step(|t| t.seed_ms),
        "insert_ms": step(|t| t.insert_ms),
        "watch_ms": step(|t| t.watch_ms),
        "total_ms": step(|t| t.total_ms),
    })
}

/// Waits until none of `ids` is `attaching` (its watch queued or being
/// attached in the background, Linux notify, R1.32), polling `List` every
/// 250 ms, and returns how long that took. Fails past `max(60 s, 2 s ×
/// count)`, naming the workspaces still attaching.
async fn wait_attached(client: &Client, ids: &[String]) -> u64 {
    let count = u32::try_from(ids.len()).unwrap_or(u32::MAX);
    let bound = Duration::from_mins(1).max(Duration::from_secs(2).saturating_mul(count));
    let begun = Instant::now();
    loop {
        let listed = client
            .list_workspaces(ListWorkspacesRequest::default())
            .await
            .unwrap();
        let attaching: Vec<&str> = listed
            .iter()
            .filter(|ws| ws.watch_state() == WatchState::Attaching && ids.contains(&ws.id))
            .map(|ws| ws.id.as_str())
            .collect();
        if attaching.is_empty() {
            return millis(begun.elapsed());
        }
        assert!(
            begun.elapsed() < bound,
            "{} workspaces still attaching after {bound:?}: {attaching:?}",
            attaching.len()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The background attaches of `ids` (R1.32): the drain [`wait_attached`]
/// measured, and the queue wait and attach time of each from the daemon's
/// `workspace watch attached` lines. Zeros where watches attach inside the
/// create (macOS).
fn attach_section(daemon: &SpawnedDaemon, ids: &[String], drain_ms: u64) -> Value {
    let lines: Vec<Value> = daemon
        .log_lines("workspace watch attached")
        .into_iter()
        .filter(|event| {
            event["workspace_id"]
                .as_str()
                .is_some_and(|id| ids.iter().any(|ws| ws == id))
        })
        .collect();
    let field = |name: &str| {
        let mut values: Vec<u64> = lines
            .iter()
            .filter_map(|event| event[name].as_u64())
            .collect();
        values.sort_unstable();
        json!({ "p50": percentile(&values, 50), "max": values.last().copied().unwrap_or(0) })
    };
    json!({
        "drain_ms": drain_ms,
        "count": lines.len(),
        "queued_ms": field("queued_ms"),
        "attach_ms": field("attach_ms"),
    })
}

async fn workspace(client: &Client, id: &str) -> Workspace {
    client
        .inspect_workspace("", id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Releases `ids`, eight at a time, each after its final snapshot.
/// Returns each release's time.
async fn release_all(client: &Client, ids: &[String]) -> Vec<u64> {
    release_each(client, ids)
        .await
        .into_iter()
        .map(|result| result.unwrap_or_else(|error| panic!("{error}")))
        .collect()
}

/// As [`release_all`], but a failed release comes back as `release <id>:
/// <error>`, so that the teardown's report is written before the test fails.
async fn release_each(client: &Client, ids: &[String]) -> Vec<Result<u64, String>> {
    futures::stream::iter(ids.iter().cloned())
        .map(|id| async move {
            let begun = Instant::now();
            client
                .release_workspace("", &id, true, false)
                .await
                .map_err(|error| format!("release {id}: {error}"))?;
            Ok(millis(begun.elapsed()))
        })
        .buffer_unordered(CREATE_CONCURRENCY)
        .collect()
        .await
}

/// One write the test made, and when the snapshot holding it was recorded.
#[derive(Clone, Debug)]
struct Write {
    workspace: usize,
    seq: u64,
    at_ms: i64,
    /// The workspace's commit when the write was made.
    commit_before: String,
    snapshot_ms: Option<i64>,
}

/// The writes of one phase.
#[derive(Debug, Default)]
struct Writes(Mutex<Vec<Write>>);

impl Writes {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Write>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> usize {
        self.lock()
            .iter()
            .filter(|write| write.snapshot_ms.is_none())
            .count()
    }

    fn stats(&self) -> Stats {
        let writes = self.lock();
        let now = now_ms();
        let mut latencies: Vec<u64> = writes
            .iter()
            .filter_map(|write| {
                write
                    .snapshot_ms
                    .map(|at| u64::try_from(at - write.at_ms).unwrap_or(0))
            })
            .collect();
        latencies.sort_unstable();
        let unsnapshotted = writes
            .iter()
            .filter(|write| {
                write
                    .snapshot_ms
                    .map_or(now - write.at_ms, |at| at - write.at_ms)
                    > i64::try_from(GATE_MAX.as_millis()).unwrap()
            })
            .count();
        Stats {
            writes: writes.len(),
            p50_ms: percentile(&latencies, 50),
            p95_ms: percentile(&latencies, 95),
            max_ms: latencies.last().copied().unwrap_or(0),
            unsnapshotted,
        }
    }

    fn samples_json(&self) -> Value {
        self.lock()
            .iter()
            .map(|write| json!([write.workspace, write.seq, write.at_ms, write.snapshot_ms]))
            .collect()
    }
}

#[derive(Debug)]
struct Stats {
    writes: usize,
    p50_ms: u64,
    p95_ms: u64,
    max_ms: u64,
    /// Writes snapshotted more than [`GATE_MAX`] after they were made, or
    /// not at all.
    unsnapshotted: usize,
}

impl Stats {
    fn to_json(&self) -> Value {
        json!({
            "writes": self.writes,
            "p50_ms": self.p50_ms,
            "p95_ms": self.p95_ms,
            "max_ms": self.max_ms,
            "unsnapshotted": self.unsnapshotted,
        })
    }
}

/// The steady state and the burst over the N workspaces.
struct Load {
    client: Client,
    repo: String,
    paths: Vec<PathBuf>,
    ids: Vec<String>,
    /// Each workspace's commit and last snapshot time, as last listed.
    latest: Arc<Mutex<HashMap<String, (String, i64)>>>,
    /// The phase being written: the sampler fills in its snapshots.
    current: Arc<Mutex<Arc<Writes>>>,
    list_calls: Arc<AtomicU64>,
    stop: tokio_util::sync::CancellationToken,
}

impl Load {
    fn new(daemon: &SpawnedDaemon, repo: &SyntheticRepo, workspaces: &[Workspace]) -> Load {
        let latest = workspaces
            .iter()
            .map(|ws| (ws.id.clone(), (ws.commit_id.clone(), 0)))
            .collect();
        Load {
            client: daemon.client.clone(),
            repo: repo.id.clone(),
            paths: workspaces
                .iter()
                .map(|ws| PathBuf::from(&ws.path))
                .collect(),
            ids: workspaces.iter().map(|ws| ws.id.clone()).collect(),
            latest: Arc::new(Mutex::new(latest)),
            current: Arc::new(Mutex::new(Arc::default())),
            list_calls: Arc::new(AtomicU64::new(0)),
            stop: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Heartbeats every workspace every [`HEARTBEAT_EVERY`] through the one
    /// client until stopped; returns the RPCs sent.
    fn heartbeat(&self, workspaces: &[Workspace]) -> tokio::task::JoinHandle<u64> {
        let client = self.client.clone();
        let stop = self.stop.clone();
        let leases: Vec<(String, String)> = workspaces
            .iter()
            .map(|ws| (ws.id.clone(), ws.lease.as_ref().unwrap().id.clone()))
            .collect();
        tokio::spawn(async move {
            let mut sent = 0;
            let mut every = tokio::time::interval(HEARTBEAT_EVERY);
            loop {
                tokio::select! {
                    () = stop.cancelled() => return sent,
                    _ = every.tick() => {}
                }
                for (id, lease) in &leases {
                    client
                        .renew_lease("", id, lease)
                        .await
                        .unwrap_or_else(|error| panic!("heartbeat {id}: {error:?}"));
                    sent += 1;
                }
            }
        })
    }

    /// Lists the workspaces every [`SAMPLE_EVERY`] until stopped, and gives
    /// each pending write the snapshot time of the first commit after it.
    /// `List` carries what `Inspect(snapshot_first = false)` does, for all
    /// of them in one call.
    fn sample(&self) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let repo = self.repo.clone();
        let ids = self.ids.clone();
        let latest = Arc::clone(&self.latest);
        let current = Arc::clone(&self.current);
        let calls = Arc::clone(&self.list_calls);
        let stop = self.stop.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(SAMPLE_EVERY);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = every.tick() => {}
                }
                let listed = client
                    .list_workspaces(ListWorkspacesRequest {
                        repo: repo.clone(),
                        states: vec![WorkspaceState::Active.into()],
                        ..ListWorkspacesRequest::default()
                    })
                    .await
                    .unwrap();
                calls.fetch_add(1, Ordering::Relaxed);
                let by_id: HashMap<&str, (&str, i64)> = listed
                    .iter()
                    .map(|ws| {
                        let at = ws.last_snapshot_at.as_ref().map_or(0, |time| {
                            time.seconds * 1000 + i64::from(time.nanos) / 1_000_000
                        });
                        (ws.id.as_str(), (ws.commit_id.as_str(), at))
                    })
                    .collect();
                {
                    let mut latest = latest.lock().unwrap_or_else(PoisonError::into_inner);
                    for (id, (commit, at)) in &by_id {
                        latest.insert((*id).to_owned(), ((*commit).to_owned(), *at));
                    }
                }
                let writes = Arc::clone(&current.lock().unwrap_or_else(PoisonError::into_inner));
                for write in writes.lock().iter_mut() {
                    if write.snapshot_ms.is_some() {
                        continue;
                    }
                    if let Some((commit, at)) = by_id.get(ids[write.workspace].as_str())
                        && *commit != write.commit_before
                        && *at >= write.at_ms
                    {
                        write.snapshot_ms = Some(*at);
                    }
                }
            }
        })
    }

    /// Writes `write-<seq>.txt` (its content the write's time, so jj sees a
    /// change) in workspace `index` and records it in `writes`.
    fn write(&self, writes: &Writes, index: usize, seq: u64) {
        let commit_before = self
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&self.ids[index])
            .map(|(commit, _)| commit.clone())
            .unwrap_or_default();
        let at_ms = now_ms();
        std::fs::write(
            self.paths[index].join(format!("write-{seq}.txt")),
            format!("{at_ms}\n"),
        )
        .unwrap();
        writes.lock().push(Write {
            workspace: index,
            seq,
            at_ms,
            commit_before,
            snapshot_ms: None,
        });
    }

    fn begin(&self) -> Arc<Writes> {
        let writes = Arc::new(Writes::default());
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&writes);
        writes
    }

    /// Waits until no write of `writes` is pending, or [`GATE_MAX`] has
    /// passed since the last of them.
    async fn settle(&self, writes: &Writes) {
        let begun = Instant::now();
        while writes.pending() > 0 && begun.elapsed() < GATE_MAX {
            tokio::time::sleep(SAMPLE_EVERY).await;
        }
    }

    /// Each workspace writes once per period at its own offset, for the
    /// steady-state duration.
    async fn steady(self: &Arc<Self>, params: &Params) -> Arc<Writes> {
        let writes = self.begin();
        let start = tokio::time::Instant::now();
        let end = start + params.steady;
        let n = self.paths.len();
        let mut tasks = tokio::task::JoinSet::new();
        let this = Arc::clone(self);
        for index in 0..n {
            let offset = params.period.mul_f64(index as f64 / n as f64);
            let (this, writes, period) = (Arc::clone(&this), Arc::clone(&writes), params.period);
            tasks.spawn(async move {
                let mut every = tokio::time::interval_at(start + offset, period);
                let mut seq = 0;
                loop {
                    let at = every.tick().await;
                    if at >= end {
                        return;
                    }
                    this.write(&writes, index, seq);
                    seq += 1;
                }
            });
        }
        while let Some(done) = tasks.join_next().await {
            done.unwrap();
        }
        self.settle(&writes).await;
        writes
    }

    /// Every workspace writes once, all within one second.
    async fn burst(&self) -> Arc<Writes> {
        let writes = self.begin();
        let n = self.paths.len();
        let start = tokio::time::Instant::now();
        for index in 0..n {
            tokio::time::sleep_until(
                start + Duration::from_secs(1).mul_f64(index as f64 / n as f64),
            )
            .await;
            self.write(&writes, index, 1_000_000);
        }
        self.settle(&writes).await;
        writes
    }
}

/// The limits the brief has the report record.
fn limits(daemon: &SpawnedDaemon) -> Value {
    let max_scans = daemon
        .config
        .watcher
        .max_concurrent_scans
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        });
    let mut limits = json!({ "max_concurrent_scans": max_scans });
    if cfg!(target_os = "linux") {
        let sysctl = |name: &str| {
            std::fs::read_to_string(format!("/proc/sys/fs/inotify/{name}"))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
        };
        let (instances, watches) = inotify_usage(daemon.pid);
        limits["inotify_max_user_watches"] = json!(sysctl("max_user_watches"));
        limits["inotify_max_user_instances"] = json!(sysctl("max_user_instances"));
        limits["inotify_instances"] = json!(instances);
        limits["inotify_watches"] = json!(watches);
    }
    limits
}

/// The daemon's inotify instances and the watches they hold, from
/// `/proc/<pid>/fd` and `fdinfo`.
fn inotify_usage(pid: u32) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return (0, 0);
    };
    let mut instances = 0;
    let mut watches = 0;
    for entry in entries.flatten() {
        if std::fs::read_link(entry.path())
            .is_ok_and(|target| target.to_string_lossy() == "anon_inode:inotify")
        {
            instances += 1;
            let info = std::fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                entry.file_name().to_string_lossy()
            ))
            .unwrap_or_default();
            watches += info
                .lines()
                .filter(|line| line.starts_with("inotify wd:"))
                .count();
        }
    }
    (instances, watches)
}

/// The daemon's per-minute `watcher summary` lines (R1.13), parsed, and
/// their maxima.
fn watcher_summaries(daemon: &SpawnedDaemon) -> Value {
    let lines = summary_lines(daemon, "watcher summary");
    let max = |key: &str| lines.iter().filter_map(|line| line.get(key)).max().copied();
    json!({
        "lines": lines,
        "scans_total": lines.iter().filter_map(|line| line.get("scans")).sum::<u64>(),
        "queued_max": max("queued_max"),
        "wait_max_ms": max("wait_max_ms"),
    })
}

/// The p50 and p95 of the `scan_ms`, `load_ms` and `walk_ms` of the
/// watcher snapshots logged from `from` (a [`utc_stamp`]) on: the steady
/// state, the burst and the teardown's final snapshots are not in it, since
/// those are `final` (R1.30).
fn watcher_scans(daemon: &SpawnedDaemon, from: &str) -> Value {
    let lines: Vec<Value> = daemon
        .log_lines("watcher snapshot taken")
        .into_iter()
        .filter(|event| {
            event["timestamp"]
                .as_str()
                .is_some_and(|at| at.get(..19) >= from.get(..19))
        })
        .collect();
    let field = |name: &str| {
        let mut values: Vec<u64> = lines
            .iter()
            .filter_map(|event| event[name].as_u64())
            .collect();
        values.sort_unstable();
        json!({ "p50": percentile(&values, 50), "p95": percentile(&values, 95), "max": values.last() })
    };
    json!({
        "count": lines.len(),
        "narrowed": lines.iter().filter(|event| event["narrowed"] == true).count(),
        "scan_ms": field("scan_ms"),
        "load_ms": field("load_ms"),
        "walk_ms": field("walk_ms"),
        "paths_count": field("paths_count"),
    })
}

/// The run as the rows of `docs/load-tests.md`'s table.
#[allow(clippy::too_many_lines, reason = "one row per line of the table")]
fn markdown(report: &Value) -> String {
    let large = &report["large_creates"];
    let creates = &report["creates"];
    let limits = &report["limits"];
    let triple = |phase: &Value| {
        format!(
            "{} / {} / {} ms",
            phase["p50_ms"], phase["p95_ms"], phase["max_ms"]
        )
    };
    let inotify = if limits["inotify_watches"].is_null() {
        "n/a".to_owned()
    } else {
        format!(
            "{} / {}",
            limits["inotify_watches"], limits["inotify_max_user_watches"]
        )
    };
    let n = report["params"]["workspaces"].as_u64().unwrap_or(0);
    let preset = report["params"]["preset"].as_str().unwrap_or("?");
    let rows = [
        (
            "backend used".to_owned(),
            format!(
                "{} ({preset}: {}), watchers {}",
                large["backend"].as_str().unwrap_or("?"),
                creates["backend"].as_str().unwrap_or("?"),
                creates["watchers"]
            ),
        ),
        (
            format!("{LARGE_CREATES} × `large` create, max / mean"),
            format!(
                "{} / {:.0} ms",
                large["max_ms"],
                large["mean_ms"].as_f64().unwrap_or(0.0)
            ),
        ),
        (
            format!("{LARGE_CREATES} × `large` free-space delta per workspace"),
            format!(
                "{:.1} MB",
                mb(large["free_bytes_per_workspace"].as_u64().unwrap_or(0))
            ),
        ),
        (
            format!("{n} × `{preset}` create, total"),
            format!(
                "{:.1} s (p95 {} ms)",
                creates["total_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                creates["p95_ms"]
            ),
        ),
        (
            "steady-state write-to-snapshot p50 / p95 / max".to_owned(),
            triple(&report["steady"]),
        ),
        (
            format!("attach drain after the {LARGE_CREATES} `large` / {n} `{preset}` creates"),
            format!(
                "{} / {} ms",
                large["attach"]["drain_ms"], creates["attach"]["drain_ms"]
            ),
        ),
        ("burst p50 / p95 / max".to_owned(), triple(&report["burst"])),
        ("watcher scan p50 / p95: scan, load, walk".to_owned(), {
            let scans = &report["watcher_scans"];
            let pair = |name: &str| format!("{} / {}", scans[name]["p50"], scans[name]["p95"]);
            format!(
                "{}, {}, {} ms ({} scans, {} narrowed)",
                pair("scan_ms"),
                pair("load_ms"),
                pair("walk_ms"),
                scans["count"],
                scans["narrowed"]
            )
        }),
        (
            "scans queued max, wait max".to_owned(),
            format!(
                "{}, {} ms",
                report["watcher_summaries"]["queued_max"],
                report["watcher_summaries"]["wait_max_ms"]
            ),
        ),
        ("inotify watches used / limit".to_owned(), inotify),
        (
            format!("{n} releases, total (p50 / p95 each)"),
            format!(
                "{:.1} s ({} / {} ms)",
                report["teardown"]["total_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                report["teardown"]["release_p50_ms"],
                report["teardown"]["release_p95_ms"]
            ),
        ),
        (
            "daemon peak threads, RSS".to_owned(),
            format!(
                "{}, {:.0} MB",
                limits["peak_threads"],
                mb(limits["peak_rss_bytes"].as_u64().unwrap_or(0))
            ),
        ),
    ];
    let mut out = format!(
        "| Measurement | {} {} |\n| --- | ---: |\n",
        report["machine"]["os"].as_str().unwrap_or("?"),
        report["machine"]["model"].as_str().unwrap_or("")
    );
    for (name, value) in rows {
        writeln!(out, "| {name} | {value} |").unwrap();
    }
    write!(
        out,
        "\nsteady writes {}, unsnapshotted after {}s: {}; gate {}",
        report["steady"]["writes"],
        GATE_MAX.as_secs(),
        report["steady"]["unsnapshotted"],
        if report["pass"]["gate"] == true {
            "passed"
        } else {
            "FAILED"
        }
    )
    .unwrap();
    out
}
