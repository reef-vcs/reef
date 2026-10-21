//! Workspace creation benchmarks (R1.12): how long making a workspace takes
//! on each backend this machine has, how much disk each one costs, and the
//! phase 1 exit criterion, twenty workspaces on the `large` preset each
//! created in under a second. `docs/benchmarks.md` says what each group
//! measures and how to run them.
//!
//! - `create/<preset>/<kind>/<n>`: `n` workspaces materialized one after
//!   another through the backend layer from a base built once in setup.
//!   Tearing them down happens outside the timer.
//! - `tear_down/<preset>/<kind>`: tearing down one workspace, deletion
//!   included.
//! - `create_api/<preset>/<kind>/<n>`: `n` `Workspaces.Create` calls to an
//!   in-process daemon on the `auto` backend, which resolves to `<kind>`.
//!   This is the exit criterion's measurement.
//!
//! Criterion's times are per workspace: each iteration's total divided by
//! `n`. After each `create` and `create_api` benchmark, a disk pass creates
//! `n` more workspaces, measures free space before and after, modifies 50
//! files in each, measures again, and tears them down one at a time. Its
//! results go to stderr as a Markdown row and to `target/bench-ws.json` as
//! a JSON line, with a `target:` line for every `n = 20`. Nothing fails on
//! a missed target; RX.06 adds the gate.

#![allow(
    clippy::expect_used,
    reason = "a failed step aborts the benchmark run; there is no caller to report to"
)]
#![allow(
    clippy::print_stderr,
    reason = "rows, targets, and setup times are reported on stderr"
)]
#![allow(
    clippy::cast_precision_loss,
    reason = "byte counts become MiB figures for reading, where f64 is plenty"
)]

mod common;

use std::cell::{OnceCell, RefCell};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use criterion::{BenchmarkId, Criterion, SamplingMode, criterion_group, criterion_main};
use reef_daemon::backend::fs_util::{allocated_bytes, free_bytes};
use reef_daemon::backend::{
    Backend, BackendKind, OverlayConfig, WorkspacePaths, backend_for, probe,
};
use reef_daemon::proto::v0::register_request::IfNotJj;
use reef_daemon::proto::v0::{CreateRequest, CreateResponse, Workspace};
use reef_daemon::testing::{TestDaemon, short_tmp};
use reef_testing::synth::{self, MutateOps, Preset};

use common::{BenchBase, Fixture};

/// Text files the disk pass modifies in each workspace.
const MODIFY: u32 = 50;
/// The `n` the phase target is stated for.
const TARGET_N: u32 = 20;
/// The phase target per workspace.
const TARGET: Duration = Duration::from_secs(1);
/// `create` counts, before [`counts`] filters them.
const CREATE_COUNTS: [u32; 3] = [1, 20, 100];
/// `create_api` counts, before [`counts`] filters them.
const API_COUNTS: [u32; 2] = [1, 20];
/// How long to wait for the daemon's background deletions after a release.
const TRASH_WAIT: Duration = Duration::from_mins(15);
/// The owner of the daemon's workspaces.
const OWNER: &str = "bench@reef.invalid";
const MIB: f64 = (1u64 << 20) as f64;

/// One target for all three groups, so the base and its temporary state
/// directory are built once and removed at the end (a `static` is never
/// dropped).
fn workspace_benches(c: &mut Criterion) {
    let fixture = common::fixture();
    eprintln!(
        "bench fixture: {} at {}, {} tracked files",
        fixture.preset_name(),
        fixture.root.display(),
        fixture.files
    );
    let base = BenchBase::new(fixture);
    let kinds = common::available_kinds(&base);
    bench_create(c, fixture, &base, &kinds);
    bench_tear_down(c, fixture, &base, &kinds);
    bench_create_api(c, fixture);
}

/// `create/<preset>/<kind>/<n>` for every available kind and every count
/// [`counts`] allows.
fn bench_create(c: &mut Criterion, fixture: &Fixture, base: &BenchBase, kinds: &[BackendKind]) {
    let mut group = c.benchmark_group("create");
    group.sampling_mode(SamplingMode::Flat);
    let preset = fixture.preset_name();
    for &kind in kinds {
        let backend = backend_for(kind, &OverlayConfig::default());
        let backend = backend.as_ref();
        let counts = counts(fixture.preset, kind, &CREATE_COUNTS);
        for &n in &counts {
            let created = RefCell::new(Vec::new());
            let id = BenchmarkId::new(format!("{preset}/{kind}"), n);
            group.bench_function(id, |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let mut workspaces = Vec::new();
                        for _ in 0..n {
                            let ws = base.workspace(next_workspace());
                            let started = Instant::now();
                            backend
                                .materialize(base.entry(), &ws)
                                .expect("materialize a workspace");
                            let took = started.elapsed();
                            total += took;
                            created.borrow_mut().push(took);
                            workspaces.push(ws);
                        }
                        tear_down_all(backend, &workspaces);
                    }
                    total / n
                });
            });
            let created = created.into_inner();
            // Empty when a filter left this benchmark out.
            if created.is_empty() {
                continue;
            }
            let disk = disk_pass(
                fixture,
                &base.ws_dir(),
                n,
                || {
                    let ws = base.workspace(next_workspace());
                    backend
                        .materialize(base.entry(), &ws)
                        .expect("materialize a workspace");
                    (ws.root.clone(), ws)
                },
                |ws| {
                    let started = Instant::now();
                    backend.tear_down(&ws).expect("tear down a workspace");
                    started.elapsed()
                },
            );
            let row = Row {
                group: "create",
                preset,
                kind: kind.as_str(),
                n,
                p50_create: p50(created),
                p50_materialize: None,
                watcher: None,
                target: target_for(n, &counts),
                disk,
            };
            report(&row, Some(&base.entry().path));
        }
    }
    group.finish();
}

/// `tear_down/<preset>/<kind>`: one workspace's tear-down, materialized
/// outside the timer. Without a tokio runtime the backends delete inline,
/// so this is the whole cost of reclaiming a workspace; the daemon's call
/// returns once the root is renamed away and deletes in the background.
fn bench_tear_down(c: &mut Criterion, fixture: &Fixture, base: &BenchBase, kinds: &[BackendKind]) {
    let mut group = c.benchmark_group("tear_down");
    group.sampling_mode(SamplingMode::Flat);
    for &kind in kinds {
        let backend = backend_for(kind, &OverlayConfig::default());
        let id = BenchmarkId::new(fixture.preset_name(), kind);
        group.bench_function(id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let ws = base.workspace(next_workspace());
                    backend
                        .materialize(base.entry(), &ws)
                        .expect("materialize a workspace");
                    let started = Instant::now();
                    backend.tear_down(&ws).expect("tear down a workspace");
                    total += started.elapsed();
                }
                total
            });
        });
    }
    group.finish();
}

/// `create_api/<preset>/<kind>/<n>`: the full create path through a daemon
/// (resolve the revision, ensure the base, materialize, `jj workspace`
/// init, seed `tree_state`, write the lease row), one daemon for the group,
/// started when the first benchmark in it runs. Its first create, which
/// builds the daemon's base, happens then too, outside the timer.
fn bench_create_api(c: &mut Criterion, fixture: &Fixture) {
    let kind = auto_kind();
    let counts = counts(fixture.preset, kind, &API_COUNTS);
    let started = OnceCell::new();
    let api = || started.get_or_init(|| Api::start(fixture, kind));
    let mut group = c.benchmark_group("create_api");
    group.sampling_mode(SamplingMode::Flat);
    let preset = fixture.preset_name();
    for &n in &counts {
        let created = RefCell::new(Vec::new());
        let materialized = RefCell::new(Vec::new());
        let id = BenchmarkId::new(format!("{preset}/{kind}"), n);
        group.bench_function(id, |b| {
            let api = api();
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let mut workspaces = Vec::new();
                    for _ in 0..n {
                        let started = Instant::now();
                        let (ws, response) = api.create_with_response();
                        let took = started.elapsed();
                        total += took;
                        created.borrow_mut().push(took);
                        materialized
                            .borrow_mut()
                            .push(Duration::from_millis(response.materialize_ms));
                        workspaces.push(ws);
                    }
                    for ws in &workspaces {
                        api.release(ws);
                    }
                    api.wait_for_trash();
                }
                total / n
            });
        });
        let created = created.into_inner();
        if created.is_empty() {
            continue;
        }
        let api = api();
        let disk = disk_pass(
            fixture,
            &api.ws_dir,
            n,
            || {
                let ws = api.create();
                (PathBuf::from(&ws.path), ws)
            },
            |ws| {
                let started = Instant::now();
                api.release(&ws);
                started.elapsed()
            },
        );
        api.wait_for_trash();
        let row = Row {
            group: "create_api",
            preset,
            kind: kind.as_str(),
            n,
            p50_create: p50(created),
            p50_materialize: Some(p50(materialized.into_inner())),
            watcher: Some(&api.watcher),
            target: target_for(n, &counts),
            disk,
        };
        report(&row, None);
    }
    group.finish();
    if let Some(api) = started.into_inner() {
        api.stop();
    }
}

/// The backend `auto` picks for a daemon's workspaces under the temporary
/// directory `TestDaemon` uses, as `backend::select` would: the first
/// available in `auto` order. [`Api::start`] checks the daemon agrees.
fn auto_kind() -> BackendKind {
    probe(&short_tmp(), None, &OverlayConfig::default())
        .into_iter()
        .find(|probe| probe.availability.is_available())
        .map_or(BackendKind::Copy, |probe| probe.kind)
}

/// An in-process daemon with the fixture registered, for `create_api`.
struct Api {
    rt: tokio::runtime::Runtime,
    daemon: TestDaemon,
    repo: String,
    /// Where its workspace roots are, and their trash while it deletes.
    /// (`Release` removes working-copy states inline.)
    ws_dir: PathBuf,
    /// The watcher the daemon gave its workspaces (`notify` or `watchman`).
    watcher: String,
}

impl Api {
    /// Starts the daemon, registers the fixture, and creates and releases
    /// one workspace, which builds the daemon's base for the fixture's
    /// working-copy commit. Panics if the daemon's backend is not `kind`.
    fn start(fixture: &Fixture, kind: BackendKind) -> Api {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let (daemon, repo) = rt.block_on(async {
            let daemon = TestDaemon::start().await;
            let registered = daemon
                .client
                .register_repository(&fixture.root, IfNotJj::Fail)
                .await
                .expect("register the fixture");
            let repo = registered.repository.expect("a repository").id;
            (daemon, repo)
        });
        let mut api = Api {
            rt,
            daemon,
            repo,
            ws_dir: PathBuf::new(),
            watcher: String::new(),
        };
        let started = Instant::now();
        let (first, response) = api.create_with_response();
        eprintln!(
            "bench base: the daemon's first create took {:.1} s (materialize {} ms, base cached: {}), watcher {}",
            started.elapsed().as_secs_f64(),
            response.materialize_ms,
            response.base_was_cached,
            first.watcher,
        );
        assert_eq!(
            first.backend,
            kind.as_str(),
            "the daemon's auto backend differs from the probe's"
        );
        Path::new(&first.path)
            .parent()
            .expect("a workspace directory")
            .clone_into(&mut api.ws_dir);
        api.watcher.clone_from(&first.watcher);
        api.release(&first);
        api.wait_for_trash();
        api
    }

    fn create(&self) -> Workspace {
        self.create_with_response().0
    }

    fn create_with_response(&self) -> (Workspace, CreateResponse) {
        let request = CreateRequest {
            repo: self.repo.clone(),
            owner: OWNER.to_owned(),
            purpose: "bench".to_owned(),
            ..CreateRequest::default()
        };
        let mut response = self
            .rt
            .block_on(self.daemon.client.create_workspace(request))
            .expect("create a workspace");
        let ws = response.workspace.take().expect("a workspace");
        (ws, response)
    }

    /// Releases `ws` without a final snapshot. Before returning, the daemon
    /// measures what the workspace frees (a walk of its tree), forgets the
    /// jj workspace, renames the root away, and removes the working-copy
    /// state; the renamed root is deleted in the background.
    fn release(&self, ws: &Workspace) {
        self.rt
            .block_on(
                self.daemon
                    .client
                    .release_workspace(&self.repo, &ws.id, false, false),
            )
            .expect("release a workspace");
    }

    /// Waits for the daemon's background deletions of released workspaces.
    fn wait_for_trash(&self) {
        wait_for_trash(&self.ws_dir);
    }

    fn stop(self) {
        self.rt.block_on(self.daemon.stop());
    }
}

/// Waits until no `*.trash-*` entry is left in `dir`: the daemon's
/// background deletions of released workspaces have finished.
fn wait_for_trash(dir: &Path) {
    let trash_left = || {
        fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".trash-"))
        })
    };
    let deadline = Instant::now() + TRASH_WAIT;
    while trash_left() {
        assert!(
            Instant::now() < deadline,
            "background deletions still running after {TRASH_WAIT:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The counts of `candidates` to benchmark `kind` with on `preset`: those
/// up to `REEF_BENCH_WS_MAX` (default 20 on `large`, 100 otherwise). The
/// copy backend, directly or as what `auto` resolves to, runs only `n = 1`
/// above `small`: its cost per workspace does not depend on `n`, and twenty
/// copies of `large` take minutes per iteration.
fn counts(preset: Preset, kind: BackendKind, candidates: &[u32]) -> Vec<u32> {
    if kind == BackendKind::Copy && preset != Preset::Small {
        return vec![1];
    }
    let max = std::env::var("REEF_BENCH_WS_MAX").map_or_else(
        |_| if preset == Preset::Large { 20 } else { 100 },
        |text| {
            text.parse()
                .expect("REEF_BENCH_WS_MAX must be a whole number")
        },
    );
    candidates.iter().copied().filter(|n| *n <= max).collect()
}

/// Whether the row for `n` (one of `counts`) gets the target line:
/// `Some(false)` for `n = 20`, `Some(true)` for a copy row whose `n = 1`
/// stands in for twenty because it is the only count, `None` otherwise.
fn target_for(n: u32, counts: &[u32]) -> Option<bool> {
    if n == TARGET_N {
        Some(false)
    } else if counts == [1] {
        Some(true)
    } else {
        None
    }
}

/// A process-wide counter for workspace ids, so no two workspaces in one
/// run share a directory.
fn next_workspace() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Tears `workspaces` down on a few threads at once, each deleting inline
/// (no tokio runtime). Outside every timer; parallel only to keep the run
/// short, since deleting a `large` tree takes seconds.
fn tear_down_all(backend: &dyn Backend, workspaces: &[WorkspacePaths]) {
    let threads = thread::available_parallelism()
        .map_or(4, usize::from)
        .clamp(1, 8)
        .min(workspaces.len().max(1));
    let chunk = workspaces.len().div_ceil(threads).max(1);
    thread::scope(|scope| {
        for part in workspaces.chunks(chunk) {
            scope.spawn(move || {
                for ws in part {
                    backend.tear_down(ws).expect("tear down a workspace");
                }
            });
        }
    });
}

/// What one disk pass measured.
struct Disk {
    /// Free space lost per workspace by creating them.
    per_ws: f64,
    /// [`Disk::per_ws`] plus the free space lost per workspace by
    /// modifying [`MODIFY`] files in each.
    per_ws_modified: f64,
    /// The modified files' allocated size, averaged over the workspaces.
    modified_files: f64,
    /// Median time to tear one workspace down (for `create_api`, the
    /// `Release` call).
    tear_down: Duration,
}

/// Creates `n` workspaces with `create` (returning each one's root), and
/// measures free space on `dir`'s volume before and after; copies the
/// generator's manifest into each root (not measured) and has
/// `synth::mutate` modify [`MODIFY`] text files in each, measuring again;
/// then tears each down with `tear_down`, timing it.
fn disk_pass<W>(
    fixture: &Fixture,
    dir: &Path,
    n: u32,
    mut create: impl FnMut() -> (PathBuf, W),
    mut tear_down: impl FnMut(W) -> Duration,
) -> Disk {
    let before = free_median(dir);
    let workspaces: Vec<(PathBuf, W)> = (0..n).map(|_| create()).collect();
    let created = free_median(dir);
    for (root, _) in &workspaces {
        fs::copy(fixture.manifest(), root.join(synth::MANIFEST_FILE))
            .expect("copy the manifest into a workspace");
    }
    let copied = free_median(dir);
    let mut modified_bytes = 0;
    for (seed, (root, _)) in (1..).zip(&workspaces) {
        let ops = MutateOps {
            modify: MODIFY,
            ..MutateOps::default()
        };
        let mutation = synth::mutate(root, seed, ops).expect("modify a workspace");
        for path in &mutation.modified {
            modified_bytes += allocated_bytes(&root.join(path)).expect("measure a modified file");
        }
    }
    let modified = free_median(dir);
    let mut tear_downs: Vec<Duration> = workspaces
        .into_iter()
        .map(|(_, ws)| tear_down(ws))
        .collect();
    tear_downs.sort();
    // Signed: other activity on the volume can free more than this adds.
    let per = |bytes: f64| bytes / f64::from(n) / MIB;
    let lost_creating = before as f64 - created as f64;
    let lost_modifying = copied as f64 - modified as f64;
    Disk {
        per_ws: per(lost_creating),
        per_ws_modified: per(lost_creating + lost_modifying),
        modified_files: per(modified_bytes as f64),
        tear_down: tear_downs[tear_downs.len().saturating_sub(1) / 2],
    }
}

/// The median of three `free_bytes` samples on `dir`'s volume, after a
/// `sync`: other activity on a shared machine moves free space too.
fn free_median(dir: &Path) -> u64 {
    nix::unistd::sync();
    let mut samples = [0; 3];
    for (i, sample) in samples.iter_mut().enumerate() {
        if i > 0 {
            thread::sleep(Duration::from_millis(100));
        }
        *sample = free_bytes(dir).expect("read free space");
    }
    samples.sort_unstable();
    samples[1]
}

/// The median of `times`.
fn p50(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len().saturating_sub(1) / 2]
}

/// One benchmark's row.
struct Row<'a> {
    group: &'static str,
    preset: &'static str,
    kind: &'a str,
    n: u32,
    /// Median of every workspace the benchmark created, warm-up included.
    p50_create: Duration,
    /// For `create_api`, the median of the daemon's `materialize_ms` (the
    /// base cache lookup and the backend's materialization) over the same
    /// creates; the rest of a create is resolving the start, the jj
    /// workspace, seeding `tree_state`, the row, and the RPC.
    p50_materialize: Option<Duration>,
    /// For `create_api`, the daemon's watcher, which starts within the
    /// create.
    watcher: Option<&'a str>,
    /// Whether the row gets the target line, and whether its `n = 1`
    /// stands in for twenty ([`target_for`]).
    target: Option<bool>,
    disk: Disk,
}

/// Prints `row` as a Markdown row (the header before the first one) with
/// the modified-files check and any target line, and appends it to
/// `target/bench-ws.json`.
/// `tree` is the base, for the copy backend's check (`create_api` does not
/// know where the daemon's base is).
fn report(row: &Row<'_>, tree: Option<&Path>) {
    static HEADER: AtomicBool = AtomicBool::new(false);
    if !HEADER.swap(true, Ordering::Relaxed) {
        eprintln!(
            "| group | preset | kind | n | p50 create ms/ws | disk MiB/ws | disk MiB/ws after modify {MODIFY} | tear_down ms/ws |"
        );
        eprintln!("| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |");
    }
    let disk = &row.disk;
    eprintln!(
        "| {} | {} | {} | {} | {:.1} | {:.2} | {:.2} | {:.1} |",
        row.group,
        row.preset,
        row.kind,
        row.n,
        millis(row.p50_create),
        disk.per_ws,
        disk.per_ws_modified,
        millis(disk.tear_down),
    );
    let check = if row.kind == BackendKind::Copy.as_str() {
        let tree = tree.map_or_else(String::new, |tree| {
            format!(", the tree allocates {:.2} MiB", tree_allocated(tree))
        });
        format!(
            "{:.2} MiB/ws after modify {MODIFY}{tree}",
            disk.per_ws_modified
        )
    } else {
        let bound = 2.0 * (disk.per_ws + disk.modified_files);
        format!(
            "{:.2} MiB/ws after modify {MODIFY} against 2 × ({:.2} + {:.2} modified) = {bound:.2} MiB: {}",
            disk.per_ws_modified,
            disk.per_ws,
            disk.modified_files,
            pass_or_miss(disk.per_ws_modified <= bound)
        )
    };
    eprintln!(
        "proportional ({}/{}/{}/{}): {check}",
        row.group, row.preset, row.kind, row.n
    );
    if let Some(materialize) = row.p50_materialize {
        eprintln!(
            "breakdown ({}/{}/{}/{}): p50 materialize {:.1} ms of {:.1} ms per create, watcher {}",
            row.group,
            row.preset,
            row.kind,
            row.n,
            millis(materialize),
            millis(row.p50_create),
            row.watcher.unwrap_or("unknown"),
        );
    }
    let target = row.target.map(|from_one| {
        print_target(row, from_one);
        pass_or_miss(row.p50_create < TARGET)
    });
    let line = serde_json::json!({
        "group": row.group,
        "preset": row.preset,
        "kind": row.kind,
        "n": row.n,
        "os": std::env::consts::OS,
        "privileged": common::privileged(),
        "p50_create_ms": millis(row.p50_create),
        "p50_materialize_ms": row.p50_materialize.map(millis),
        "watcher": row.watcher,
        "disk_per_ws_mib": disk.per_ws,
        "disk_per_ws_modified_mib": disk.per_ws_modified,
        "modified_files_mib": disk.modified_files,
        "tear_down_ms": millis(disk.tear_down),
        "target": target,
        "unix_time": SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    });
    let path = common::target_dir().join("bench-ws.json");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open target/bench-ws.json");
    writeln!(file, "{line}").expect("append to target/bench-ws.json");
}

/// Prints the target line for `row`; only `large`'s is the phase exit
/// criterion. `from_one` marks a copy row with `n = 1` standing in for
/// twenty.
fn print_target(row: &Row<'_>, from_one: bool) {
    eprintln!(
        "target ({}/{}/{}/{}): {TARGET_N} workspaces under 1 s each: {} ({:.3} s){}",
        row.group,
        row.preset,
        row.kind,
        row.n,
        pass_or_miss(row.p50_create < TARGET),
        row.p50_create.as_secs_f64(),
        if from_one {
            "; copy runs n = 1 only on this preset"
        } else {
            ""
        }
    );
}

/// The base tree's allocated size in MiB, measured once.
fn tree_allocated(tree: &Path) -> f64 {
    static TREE: OnceLock<f64> = OnceLock::new();
    *TREE.get_or_init(|| allocated_bytes(tree).expect("measure the base") as f64 / MIB)
}

fn pass_or_miss(pass: bool) -> &'static str {
    if pass { "PASS" } else { "MISS" }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Sample size 10 and flat sampling, with a measurement time that grows
/// with the preset. Both can be overridden on the command line.
fn config() -> Criterion {
    let measurement = match common::preset() {
        Preset::Small => 30,
        Preset::Medium => 60,
        Preset::Large | Preset::Xl => 120,
    };
    Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(measurement))
}

criterion_group! {
    name = benches;
    config = config();
    targets = workspace_benches
}
criterion_main!(benches);
