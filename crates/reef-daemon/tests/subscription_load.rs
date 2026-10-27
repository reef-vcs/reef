//! The subscription load test (R2.22): 500 operation-log and 500 presence
//! subscribers on one daemon while 1,000 operations a minute are recorded,
//! the delivery latency of every event, and a check that no subscriber
//! missed, repeated or reordered an operation. See `docs/load-tests.md`,
//! "Subscriptions (R2.22)", for the profile, the gate and the baselines.
//!
//! `#[ignore]`d, and a no-op unless `REEF_LOAD=1`:
//!
//! ```sh
//! REEF_LOAD=1 cargo nextest run --release -p reef-daemon --test subscription_load --run-ignored all --no-capture
//! ```
//!
//! Knobs: `REEF_SUBLOAD_SUBSCRIBERS` (500), `REEF_SUBLOAD_PRESENCE_SUBSCRIBERS`
//! (500), `REEF_SUBLOAD_CONNECTIONS` (500), `REEF_SUBLOAD_OPS_PER_MIN`
//! (1,000), `REEF_SUBLOAD_WORKSPACES` (20), `REEF_SUBLOAD_REPOS` (1),
//! `REEF_SUBLOAD_REPORTERS` (20), `REEF_SUBLOAD_STEADY_SECS` (300),
//! `REEF_SUBLOAD_PRESET` (`small`, `medium` or `large`). The report is
//! `target/load/subscriptions-<os>-<timestamp>.json`, and a Markdown summary
//! is printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a measurement program: a panic is the failure report, and it prints its results"
)]

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::load::{
    Monitor, keep_daemon_log, machine, mb, millis, open_descriptors, per_mille, percentile,
    process_cpu_secs, raise_open_file_limit, summary_lines, utc_stamp, write_report,
};
use common::spawned::SpawnedDaemon;
use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, ListOperationsRequest, OperationFilter,
    OperationKind, PresenceKind, ReportPresenceRequest, SubscribeOperationsRequest,
    SubscribePresenceRequest, SubscribePresenceResponse, subscribe_operations_request::Start,
};
use reef_daemon::client::Client;
use reef_testing::synth::Preset;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The gate: steady-state delivery p99, for both channels.
const GATE_P99: Duration = Duration::from_millis(100);
/// The gate: no steady-state delivery slower than this (a stuck stream).
const GATE_MAX: Duration = Duration::from_secs(2);
/// The gate: the steady state must reach this share of the target rate.
const GATE_RATE: f64 = 0.95;
/// Subscriptions opened per step while subscribing.
const SUBSCRIBE_BATCH: usize = 50;
const SUBSCRIBE_EVERY: Duration = Duration::from_millis(100);
const WARM_UP: Duration = Duration::from_secs(30);
const BURST: Duration = Duration::from_secs(10);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);
const REPORT_EVERY: Duration = Duration::from_secs(1);
const REPORT_TTL: Duration = Duration::from_secs(30);
/// Unfiltered subscribers that stop reading for a while mid-run.
const STALLED: usize = 10;
/// Unfiltered subscribers that drop their stream and resume by cursor.
const RECONNECTED: usize = 100;
/// How long subscribers get to receive the last operation after the writes.
const DRAIN: Duration = Duration::from_secs(30);
const OWNER: &str = "load@host";

/// The run's parameters, from the environment.
#[derive(Clone, Debug)]
struct Params {
    subscribers: usize,
    presence_subscribers: usize,
    connections: usize,
    ops_per_min: u64,
    workspaces: usize,
    repos: usize,
    reporters: usize,
    steady: Duration,
    preset: Preset,
}

impl Params {
    fn from_env() -> Params {
        let number = |name: &str, default: u64| {
            std::env::var(name).ok().map_or(default, |text| {
                text.parse().unwrap_or_else(|_| panic!("{name}={text}"))
            })
        };
        let count = |name: &str, default: u64| usize::try_from(number(name, default)).unwrap();
        let preset = match std::env::var("REEF_SUBLOAD_PRESET").as_deref() {
            Err(_) | Ok("" | "small") => Preset::Small,
            Ok("medium") => Preset::Medium,
            Ok("large") => Preset::Large,
            Ok(other) => panic!("REEF_SUBLOAD_PRESET={other}: expected small, medium or large"),
        };
        let params = Params {
            subscribers: count("REEF_SUBLOAD_SUBSCRIBERS", 500),
            presence_subscribers: count("REEF_SUBLOAD_PRESENCE_SUBSCRIBERS", 500),
            connections: count("REEF_SUBLOAD_CONNECTIONS", 500).max(1),
            ops_per_min: number("REEF_SUBLOAD_OPS_PER_MIN", 1000).max(1),
            workspaces: count("REEF_SUBLOAD_WORKSPACES", 20).max(1),
            repos: count("REEF_SUBLOAD_REPOS", 1).max(1),
            reporters: count("REEF_SUBLOAD_REPORTERS", 20),
            steady: Duration::from_secs(number("REEF_SUBLOAD_STEADY_SECS", 300).max(10)),
            preset,
        };
        assert!(
            params.workspaces >= params.repos,
            "REEF_SUBLOAD_WORKSPACES must be at least REEF_SUBLOAD_REPOS"
        );
        params
    }

    fn to_json(&self) -> Value {
        json!({
            "subscribers": self.subscribers,
            "presence_subscribers": self.presence_subscribers,
            "connections": self.connections,
            "ops_per_min": self.ops_per_min,
            "workspaces": self.workspaces,
            "repos": self.repos,
            "reporters": self.reporters,
            "steady_secs": self.steady.as_secs(),
            "warm_up_secs": WARM_UP.as_secs(),
            "burst_secs": BURST.as_secs(),
            "stalled": STALLED,
            "reconnected": RECONNECTED,
            "preset": format!("{:?}", self.preset).to_lowercase(),
        })
    }
}

/// Microseconds since the Unix epoch, on the clock the daemon stamps
/// `ended_at` and `updated_at` with.
fn now_us() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
}

fn timestamp_us(at: Option<&prost_types::Timestamp>) -> i64 {
    at.map_or(0, |at| at.seconds * 1_000_000 + i64::from(at.nanos) / 1_000)
}

/// Operation ids as small numbers, shared by every subscriber.
#[derive(Debug, Default)]
struct Interner(RwLock<HashMap<String, u32>>);

impl Interner {
    fn id(&self, text: &str) -> u32 {
        if let Some(id) = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(text)
        {
            return *id;
        }
        let mut ids = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let next = u32::try_from(ids.len()).unwrap();
        *ids.entry(text.to_owned()).or_insert(next)
    }

    fn get(&self, text: &str) -> Option<u32> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(text)
            .copied()
    }
}

/// Which operations a subscriber asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Class {
    /// Everything in its repository: a UI, `reef op watch`.
    Unfiltered,
    /// One writer's workspace: an agent's wait tool.
    Workspace,
    /// `kinds: [DESCRIBE]`.
    Kind,
}

impl Class {
    fn of(index: usize) -> Class {
        match index % 10 {
            8 => Class::Workspace,
            9 => Class::Kind,
            _ => Class::Unfiltered,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Class::Unfiltered => "unfiltered",
            Class::Workspace => "workspace",
            Class::Kind => "kind",
        }
    }
}

/// One operation a subscriber received: its interned id, when it ended
/// (µs after the run's origin), and how long after that it arrived.
#[derive(Clone, Copy, Debug)]
struct Delivery {
    id: u32,
    ended_us: i64,
    latency_us: i64,
}

impl Delivery {
    fn received_us(self) -> i64 {
        self.ended_us + self.latency_us
    }
}

/// What the controller asks a subscriber to do.
#[derive(Debug)]
enum Command {
    /// Stop reading for this long.
    Stall(Duration),
    /// Drop the stream and resubscribe from the last frame's cursor.
    Reconnect,
}

/// One operation subscriber's account of its run.
#[derive(Debug)]
struct OpRecord {
    index: usize,
    class: Class,
    repo: usize,
    /// The operation heads its first frame stood at.
    start_heads: Vec<String>,
    first_frame_us: i64,
    deliveries: Vec<Delivery>,
    frames_with_operations: u64,
    /// Receipt windows (µs after the origin) the gate leaves out: a stall
    /// until caught up, a reconnect until its first live frame.
    excluded: Vec<(i64, i64)>,
    /// From the end of a stall to the first frame past it.
    catch_ups_us: Vec<i64>,
    /// From dropping the stream to the first live frame after it.
    reconnects_us: Vec<i64>,
    /// From resubscribing to the first frame.
    reconnect_first_frames_us: Vec<i64>,
    /// Resubscriptions refused for the cap and retried.
    cap_retries: u64,
    error: Option<String>,
}

struct OpSubscriber {
    index: usize,
    class: Class,
    repo: usize,
    request: SubscribeOperationsRequest,
    client: Client,
    interner: Arc<Interner>,
    origin_us: i64,
    /// The interned id of the newest operation received; `u32::MAX` for none.
    last: Arc<AtomicU32>,
    subscribed: Arc<AtomicUsize>,
}

impl OpSubscriber {
    async fn open(
        &self,
        start: Option<Start>,
        cap_retries: &mut u64,
    ) -> Result<tonic::Streaming<v1::SubscribeOperationsResponse>, String> {
        let request = SubscribeOperationsRequest {
            start,
            ..self.request.clone()
        };
        loop {
            match self.client.subscribe_operations(request.clone()).await {
                Ok(stream) => return Ok(stream),
                // A dropped stream's slot is freed when its session sees the
                // close; a resubscription can get there first.
                Err(error) if error.reason() == "TOO_MANY_SUBSCRIPTIONS" && *cap_retries < 100 => {
                    *cap_retries += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one loop: frames, stalls, reconnects"
    )]
    async fn run(self, mut commands: mpsc::Receiver<Command>, stop: CancellationToken) -> OpRecord {
        let mut record = OpRecord {
            index: self.index,
            class: self.class,
            repo: self.repo,
            start_heads: Vec::new(),
            first_frame_us: 0,
            deliveries: Vec::new(),
            frames_with_operations: 0,
            excluded: Vec::new(),
            catch_ups_us: Vec::new(),
            reconnects_us: Vec::new(),
            reconnect_first_frames_us: Vec::new(),
            cap_retries: 0,
            error: None,
        };
        let begun = now_us();
        let mut stream = match self.open(None, &mut record.cap_retries).await {
            Ok(stream) => stream,
            Err(error) => {
                record.error = Some(format!("subscribe: {error}"));
                self.subscribed.fetch_add(1, Ordering::Relaxed);
                return record;
            }
        };
        let mut cursor = match stream.message().await {
            Ok(Some(frame)) => {
                record.first_frame_us = now_us() - begun;
                record.start_heads = frame.head_operation_ids;
                frame.cursor
            }
            other => {
                record.error = Some(format!("first frame: {other:?}"));
                self.subscribed.fetch_add(1, Ordering::Relaxed);
                return record;
            }
        };
        self.subscribed.fetch_add(1, Ordering::Relaxed);
        // A stall's (start, end), until a frame reaches past its end.
        let mut stalled: Option<(i64, i64)> = None;
        // A reconnect's start, until its first live frame.
        let mut reconnected: Option<i64> = None;
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(command) = commands.recv() => match command {
                    Command::Stall(length) => {
                        let from = now_us();
                        tokio::time::sleep(length).await;
                        stalled = Some((from, now_us()));
                    }
                    Command::Reconnect => {
                        let from = now_us();
                        drop(stream);
                        let start = Some(Start::Cursor(cursor.clone()));
                        stream = match self.open(start, &mut record.cap_retries).await {
                            Ok(stream) => stream,
                            Err(error) => {
                                record.error = Some(format!("resubscribe: {error}"));
                                break;
                            }
                        };
                        reconnected = Some(from);
                    }
                },
                next = stream.message() => {
                    let at = now_us();
                    let frame = match next {
                        Ok(Some(frame)) => frame,
                        Ok(None) => {
                            record.error = Some("the stream ended".to_owned());
                            break;
                        }
                        Err(status) => {
                            record.error = Some(format!("{}: {}", status.code(), status.message()));
                            break;
                        }
                    };
                    cursor = frame.cursor;
                    if !frame.operations.is_empty() {
                        record.frames_with_operations += 1;
                    }
                    let mut newest_ended = i64::MIN;
                    for operation in &frame.operations {
                        let ended = timestamp_us(operation.ended_at.as_ref());
                        newest_ended = newest_ended.max(ended);
                        record.deliveries.push(Delivery {
                            id: self.interner.id(&operation.id),
                            ended_us: ended - self.origin_us,
                            latency_us: at - ended,
                        });
                    }
                    if let Some(delivery) = record.deliveries.last() {
                        self.last.store(delivery.id, Ordering::Relaxed);
                    }
                    if let Some(from) = reconnected {
                        if record.reconnect_first_frames_us.len() < record.reconnects_us.len() + 1 {
                            record.reconnect_first_frames_us.push(at - from);
                        }
                        if frame.live {
                            record.excluded.push((from - self.origin_us, at - self.origin_us));
                            record.reconnects_us.push(at - from);
                            reconnected = None;
                        }
                    }
                    if let Some((from, end)) = stalled
                        && newest_ended >= end
                    {
                        record.excluded.push((from - self.origin_us, at - self.origin_us));
                        record.catch_ups_us.push(at - end);
                        stalled = None;
                    }
                }
            }
        }
        record
    }
}

/// One presence subscriber's account of its run.
#[derive(Debug, Default)]
struct PresenceRecord {
    first_frame_us: i64,
    /// Receipt minus `updated_at` for every event after the set was synced,
    /// with the event's `updated_at` (µs after the origin).
    latencies: Vec<(i64, i64)>,
    events: u64,
    frames: u64,
    error: Option<String>,
}

/// A presence subscriber's live set: entry id to `updated_at` (µs).
type PresenceSet = Arc<Mutex<BTreeMap<String, i64>>>;

async fn run_presence_subscriber(
    client: Client,
    origin_us: i64,
    set: PresenceSet,
    subscribed: Arc<AtomicUsize>,
    stop: CancellationToken,
) -> PresenceRecord {
    let mut record = PresenceRecord::default();
    let begun = now_us();
    let mut stream = match client
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            record.error = Some(format!("subscribe: {error}"));
            subscribed.fetch_add(1, Ordering::Relaxed);
            return record;
        }
    };
    let mut synced = false;
    loop {
        let next = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            next = stream.message() => next,
        };
        let at = now_us();
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                record.error = Some("the stream ended".to_owned());
                break;
            }
            Err(status) => {
                record.error = Some(format!("{}: {}", status.code(), status.message()));
                break;
            }
        };
        record.frames += 1;
        apply_presence(&set, &frame);
        if synced {
            for event in &frame.events {
                let updated = event
                    .presence
                    .as_ref()
                    .map_or(0, |presence| timestamp_us(presence.updated_at.as_ref()));
                if !event.ended {
                    record.latencies.push((updated - origin_us, at - updated));
                }
                record.events += 1;
            }
        } else if frame.synced {
            synced = true;
            record.first_frame_us = at - begun;
            subscribed.fetch_add(1, Ordering::Relaxed);
        }
    }
    if !synced {
        subscribed.fetch_add(1, Ordering::Relaxed);
    }
    record
}

fn apply_presence(set: &PresenceSet, frame: &SubscribePresenceResponse) {
    let mut set = set.lock().unwrap_or_else(PoisonError::into_inner);
    for event in &frame.events {
        let Some(presence) = &event.presence else {
            continue;
        };
        if event.ended {
            set.remove(&presence.id);
        } else {
            set.insert(
                presence.id.clone(),
                timestamp_us(presence.updated_at.as_ref()),
            );
        }
    }
}

/// A fresh subscription's synced set, then the stream dropped.
async fn presence_now(client: &Client) -> BTreeMap<String, i64> {
    let mut stream = client
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap();
    let set: PresenceSet = Arc::default();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("a presence frame in time")
            .unwrap()
            .unwrap();
        apply_presence(&set, &frame);
        if frame.synced {
            break;
        }
    }
    set.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// A writer: one workspace, its operations in order.
struct Writer {
    client: Client,
    repo: String,
    workspace: v1::Workspace,
    done: AtomicU64,
}

/// One operation a writer ran.
#[derive(Clone, Copy, Debug)]
struct WriteRecord {
    /// When it was due, µs after the origin (when it started, in the burst).
    due_us: i64,
    /// How late it started.
    slip_us: i64,
    /// The RPCs' time.
    rpc_us: i64,
    /// When it returned, µs after the origin.
    returned_us: i64,
    describe: bool,
    failed: bool,
}

impl Writer {
    /// A describe of the workspace's `@`, or a file write and an explicit
    /// snapshot, alternately: one operation each.
    async fn operation(&self, origin_us: i64, due_us: i64) -> WriteRecord {
        let n = self.done.fetch_add(1, Ordering::Relaxed);
        let started = now_us();
        let describe = n.is_multiple_of(2);
        let failed = if describe {
            self.client
                .describe_change(DescribeChangeRequest {
                    workspace: self.workspace.id.clone(),
                    revision: "@".to_owned(),
                    description: format!("load {n}"),
                    ..DescribeChangeRequest::default()
                })
                .await
                .is_err()
        } else {
            let path = std::path::Path::new(&self.workspace.path).join("load.txt");
            std::fs::write(&path, format!("{n} {started}\n")).unwrap();
            self.client
                .snapshot_workspace(&self.repo, &self.workspace.id, "")
                .await
                .is_err()
        };
        let returned = now_us();
        WriteRecord {
            due_us,
            slip_us: (started - origin_us - due_us).max(0),
            rpc_us: returned - started,
            returned_us: returned - origin_us,
            describe,
            failed,
        }
    }
}

/// Runs `writer`'s scheduled operations in order until the schedule closes.
async fn run_writer(
    writer: Arc<Writer>,
    origin_us: i64,
    mut jobs: mpsc::UnboundedReceiver<i64>,
) -> Vec<WriteRecord> {
    let mut records = Vec::new();
    while let Some(due_us) = jobs.recv().await {
        records.push(writer.operation(origin_us, due_us).await);
    }
    records
}

/// Each writer back to back until `until`.
async fn burst(writers: &[Arc<Writer>], origin_us: i64, until: Instant) -> Vec<WriteRecord> {
    let mut tasks = tokio::task::JoinSet::new();
    for writer in writers {
        let writer = Arc::clone(writer);
        tasks.spawn(async move {
            let mut records = Vec::new();
            while Instant::now() < until {
                let due = now_us() - origin_us;
                records.push(writer.operation(origin_us, due).await);
            }
            records
        });
    }
    let mut records = Vec::new();
    while let Some(done) = tasks.join_next().await {
        records.extend(done.unwrap());
    }
    records
}

/// What one subscriber received against what it should have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Accounting {
    missing: usize,
    duplicated: usize,
    out_of_order: usize,
    unexpected: usize,
}

impl Accounting {
    fn clean(self) -> bool {
        self == Accounting::default()
    }
}

/// Compares `received` with `expected`, both interned ids, `expected` in
/// log order.
fn account(expected: &[u32], received: &[u32]) -> Accounting {
    let position: HashMap<u32, usize> = expected
        .iter()
        .enumerate()
        .map(|(at, id)| (*id, at))
        .collect();
    let mut seen = HashSet::new();
    let mut accounting = Accounting::default();
    let mut last = None;
    for id in received {
        if !seen.insert(*id) {
            accounting.duplicated += 1;
            continue;
        }
        let Some(at) = position.get(id) else {
            accounting.unexpected += 1;
            continue;
        };
        if last.is_some_and(|last| *at < last) {
            accounting.out_of_order += 1;
        }
        last = Some(*at);
    }
    accounting.missing = expected.iter().filter(|id| !seen.contains(id)).count();
    accounting
}

/// Percentiles of latencies in µs, in milliseconds.
fn latency_json(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    let ms = |us: u64| (us as f64 / 1000.0 * 10.0).round() / 10.0;
    let mean = values.iter().sum::<u64>() as f64 / values.len().max(1) as f64;
    json!({
        "count": values.len(),
        "p50_ms": ms(percentile(&values, 50)),
        "p95_ms": ms(percentile(&values, 95)),
        "p99_ms": ms(percentile(&values, 99)),
        "p999_ms": ms(per_mille(&values, 999)),
        "max_ms": ms(values.last().copied().unwrap_or(0)),
        "mean_ms": ms(mean as u64),
    })
}

fn us(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The filter subscriber `index` of `class` asks for: none, one writer of
/// its repository's workspace, or describes.
fn filter_of(
    class: Class,
    index: usize,
    params: &Params,
    writers: &[Arc<Writer>],
) -> Option<OperationFilter> {
    match class {
        Class::Unfiltered => None,
        Class::Workspace => {
            // Writer `n` is in repository `n % repos`.
            let repo = index % params.repos;
            let per_repo = params.workspaces / params.repos;
            let writer = repo + params.repos * ((index / 10) % per_repo);
            Some(OperationFilter {
                workspaces: vec![writers[writer].workspace.id.clone()],
                ..OperationFilter::default()
            })
        }
        Class::Kind => Some(OperationFilter {
            kinds: vec![OperationKind::Describe.into()],
            ..OperationFilter::default()
        }),
    }
}

/// The operations of `repo` in log order (oldest first) that `filter`
/// keeps.
async fn listed(client: &Client, repo: &str, filter: Option<OperationFilter>) -> Vec<String> {
    let mut operations = client
        .list_operations_limited(
            ListOperationsRequest {
                repo: repo.to_owned(),
                filter,
                ..ListOperationsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    operations.reverse();
    operations
        .into_iter()
        .map(|operation| operation.id)
        .collect()
}

/// The steady state and the burst, as `(from, to)` µs after the origin.
#[derive(Clone, Copy, Debug)]
struct Windows {
    steady: (i64, i64),
    burst: (i64, i64),
}

#[allow(
    clippy::too_many_lines,
    reason = "the phases in order, as docs/load-tests.md lists them"
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the load test: REEF_LOAD=1 and --run-ignored; see docs/load-tests.md"]
async fn subscription_load() {
    if std::env::var("REEF_LOAD").as_deref() != Ok("1") {
        eprintln!("skipped: set REEF_LOAD=1 to run the load test");
        return;
    }
    let params = Params::from_env();
    let started_at = utc_stamp();
    // The test's own connections, and the daemon's, which inherits it.
    let (limit_before, limit) = raise_open_file_limit();
    let daemon = SpawnedDaemon::spawn(|_| {}).await;
    let monitor = Monitor::start(daemon.pid);
    let mut report = json!({
        "started_at": started_at,
        "machine": machine(),
        "params": params.to_json(),
        "config": serde_json::to_value(&daemon.config).unwrap(),
    });

    // 1. Set up.
    let client = &daemon.client;
    let mut repos = Vec::new();
    for n in 0..params.repos {
        repos.push(
            daemon
                .register_synth_at(&format!("repo-{n}"), params.preset)
                .await,
        );
    }
    let mut writers = Vec::new();
    for n in 0..params.workspaces {
        let repo = &repos[n % params.repos];
        let workspace = client
            .create_workspace(CreateWorkspaceRequest {
                repo: repo.id.clone(),
                owner: OWNER.to_owned(),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .workspace
            .unwrap();
        writers.push(Arc::new(Writer {
            client: client.clone(),
            repo: repo.id.clone(),
            workspace,
            done: AtomicU64::new(0),
        }));
    }
    let stop_background = CancellationToken::new();
    let heartbeats = {
        let client = client.clone();
        let stop = stop_background.clone();
        let leases: Vec<(String, String)> = writers
            .iter()
            .map(|writer| {
                let ws = &writer.workspace;
                (ws.id.clone(), ws.lease.as_ref().unwrap().id.clone())
            })
            .collect();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(HEARTBEAT_EVERY);
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = every.tick() => {}
                }
                for (id, lease) in &leases {
                    client.renew_lease("", id, lease).await.unwrap();
                }
            }
        })
    };
    eprintln!(
        "subload: {} workspaces in {} repositories",
        params.workspaces, params.repos
    );

    // 2. Subscribe.
    let mut connections = Vec::new();
    for _ in 0..params.connections {
        connections.push(Client::connect(&daemon.socket).await.unwrap());
    }
    let origin_us = now_us();
    let interner = Arc::new(Interner::default());
    let stop_subscribers = CancellationToken::new();
    let opened = Arc::new(AtomicUsize::new(0));
    let mut op_tasks = Vec::new();
    let mut presence_tasks = Vec::new();
    let mut presence_sets = Vec::new();
    let mut commands = Vec::new();
    let mut newest = Vec::new();
    let total = params.subscribers + params.presence_subscribers;
    let subscribe_begun = Instant::now();
    for n in 0..total {
        if n > 0 && n % SUBSCRIBE_BATCH == 0 {
            tokio::time::sleep(SUBSCRIBE_EVERY).await;
        }
        if n < params.subscribers {
            let index = n;
            let class = Class::of(index);
            let repo = index % params.repos;
            let filter = filter_of(class, index, &params, &writers);
            let (tx, rx) = mpsc::channel(4);
            let last = Arc::new(AtomicU32::new(u32::MAX));
            let subscriber = OpSubscriber {
                index,
                class,
                repo,
                request: SubscribeOperationsRequest {
                    repos: vec![repos[repo].id.clone()],
                    filter,
                    start: None,
                },
                client: connections[index % params.connections].clone(),
                interner: Arc::clone(&interner),
                origin_us,
                last: Arc::clone(&last),
                subscribed: Arc::clone(&opened),
            };
            commands.push(tx);
            newest.push(last);
            op_tasks.push(tokio::spawn(subscriber.run(rx, stop_subscribers.clone())));
        } else {
            let index = n - params.subscribers;
            let set = PresenceSet::default();
            presence_sets.push(Arc::clone(&set));
            presence_tasks.push(tokio::spawn(run_presence_subscriber(
                connections[index % params.connections].clone(),
                origin_us,
                set,
                Arc::clone(&opened),
                stop_subscribers.clone(),
            )));
        }
    }
    while opened.load(Ordering::Relaxed) < total {
        assert!(
            subscribe_begun.elapsed() < Duration::from_secs(120),
            "{} of {total} subscriptions had their first frame after 120 s",
            opened.load(Ordering::Relaxed)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    report["subscribe_total_ms"] = json!(millis(subscribe_begun.elapsed()));
    eprintln!("subload: {total} subscriptions open");

    // Presence reporters.
    let reporters: Vec<_> = (0..params.reporters)
        .map(|n| {
            let client = client.clone();
            let workspace = writers[n % writers.len()].workspace.id.clone();
            let stop = stop_background.clone();
            tokio::spawn(async move {
                let mut id = String::new();
                let mut every = tokio::time::interval(REPORT_EVERY);
                let mut sent = 0_u64;
                loop {
                    tokio::select! {
                        () = stop.cancelled() => break,
                        _ = every.tick() => {}
                    }
                    let response = client
                        .report_presence(ReportPresenceRequest {
                            presence_id: id.clone(),
                            agent: format!("load-reporter-{n}"),
                            workspace: workspace.clone(),
                            kind: PresenceKind::Activity.into(),
                            activity: format!("step {sent}"),
                            ttl: Some(prost_types::Duration::try_from(REPORT_TTL).unwrap()),
                            ..ReportPresenceRequest::default()
                        })
                        .await
                        .unwrap();
                    id = response.presence.unwrap().id;
                    sent += 1;
                }
                if !id.is_empty() {
                    client.clear_presence(&id).await.unwrap();
                }
                sent
            })
        })
        .collect();

    // 3 and 4. Warm up, then the steady state, on one open-loop schedule.
    let period_us = 60_000_000 / i64::try_from(params.ops_per_min).unwrap();
    let schedule_from_us = now_us() - origin_us + 100_000;
    let steady_from_us = schedule_from_us + i64::try_from(WARM_UP.as_micros()).unwrap();
    let steady_to_us = steady_from_us + i64::try_from(params.steady.as_micros()).unwrap();
    let mut queues = Vec::new();
    let mut writer_tasks = Vec::new();
    for writer in &writers {
        let (tx, rx) = mpsc::unbounded_channel();
        queues.push(tx);
        writer_tasks.push(tokio::spawn(run_writer(Arc::clone(writer), origin_us, rx)));
    }
    let at = |offset_us: i64| {
        Instant::now() + Duration::from_micros(us(offset_us - (now_us() - origin_us)))
    };
    // The stall and the reconnect, at the same fractions of any steady state.
    let steady_us = steady_to_us - steady_from_us;
    let stall_from = steady_from_us + steady_us * 2 / 5;
    let stall_length = Duration::from_micros(us(steady_us / 5));
    let reconnect_at = steady_from_us + steady_us * 4 / 5;
    let unfiltered: Vec<usize> = (0..params.subscribers)
        .filter(|index| Class::of(*index) == Class::Unfiltered)
        .collect();
    let stalled: Vec<usize> = unfiltered.iter().copied().take(STALLED).collect();
    let reconnecting: Vec<usize> = unfiltered
        .iter()
        .copied()
        .skip(STALLED)
        .take(RECONNECTED)
        .collect();
    let controller = {
        let commands = commands.clone();
        let (stall_at, reconnect_at) = (at(stall_from), at(reconnect_at));
        tokio::spawn(async move {
            tokio::time::sleep_until(stall_at).await;
            for index in &stalled {
                commands[*index]
                    .send(Command::Stall(stall_length))
                    .await
                    .unwrap();
            }
            tokio::time::sleep_until(reconnect_at).await;
            for index in &reconnecting {
                commands[*index].send(Command::Reconnect).await.unwrap();
            }
        })
    };
    let rtts = {
        let client = client.clone();
        let (from, to) = (at(steady_from_us), at(steady_to_us));
        tokio::spawn(async move {
            tokio::time::sleep_until(from).await;
            let mut rtts = Vec::new();
            let mut every = tokio::time::interval(Duration::from_secs(1));
            while Instant::now() < to {
                every.tick().await;
                let begun = std::time::Instant::now();
                client.status().await.unwrap();
                rtts.push(u64::try_from(begun.elapsed().as_micros()).unwrap());
            }
            rtts
        })
    };
    let descriptors = {
        let (pid, mid) = (daemon.pid, at(i64::midpoint(steady_from_us, steady_to_us)));
        tokio::spawn(async move {
            tokio::time::sleep_until(mid).await;
            tokio::task::spawn_blocking(move || open_descriptors(pid))
                .await
                .unwrap()
        })
    };
    eprintln!("subload: warm-up and steady state");
    let mut cpu_at_steady = (0.0, 0.0);
    let mut k = 0_i64;
    loop {
        let due_us = schedule_from_us + k * period_us;
        if due_us >= steady_to_us {
            break;
        }
        if k > 0 && due_us >= steady_from_us && due_us - period_us < steady_from_us {
            cpu_at_steady = (
                process_cpu_secs(daemon.pid),
                process_cpu_secs(std::process::id()),
            );
        }
        tokio::time::sleep_until(at(due_us)).await;
        queues[usize::try_from(k).unwrap() % queues.len()]
            .send(due_us)
            .unwrap();
        k += 1;
    }
    drop(queues);
    let mut scheduled = Vec::new();
    for task in writer_tasks {
        scheduled.extend(task.await.unwrap());
    }
    let cpu_after_steady = (
        process_cpu_secs(daemon.pid),
        process_cpu_secs(std::process::id()),
    );
    let steady_wall_us = (now_us() - origin_us - steady_from_us).max(steady_us);
    controller.await.unwrap();
    eprintln!("subload: steady state done");

    // 5. Burst.
    let burst_from_us = now_us() - origin_us;
    let burst_writes = burst(&writers, origin_us, Instant::now() + BURST).await;
    let burst_to_us = now_us() - origin_us;
    eprintln!("subload: burst done");
    let windows = Windows {
        steady: (steady_from_us, steady_to_us),
        burst: (burst_from_us, burst_to_us),
    };

    // 6. Check: every subscriber against the log.
    stop_background.cancel();
    heartbeats.await.unwrap();
    let mut reports_sent = 0;
    for reporter in reporters {
        reports_sent += reporter.await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut lists: HashMap<(usize, String), Vec<u32>> = HashMap::new();
    let mut full: Vec<HashMap<u32, usize>> = Vec::new();
    for repo in &repos {
        let ids = listed(client, &repo.id, None).await;
        full.push(
            ids.iter()
                .enumerate()
                .map(|(at, id)| (interner.id(id), at))
                .collect(),
        );
    }
    // Per subscriber, the list it is checked against; wait until each has
    // the newest operation of its list.
    let mut expectations = Vec::new();
    for index in 0..params.subscribers {
        let repo = index % params.repos;
        let filter = filter_of(Class::of(index), index, &params, &writers);
        let key = (repo, format!("{filter:?}"));
        if !lists.contains_key(&key) {
            let ids = listed(client, &repos[repo].id, filter).await;
            lists.insert(key.clone(), ids.iter().map(|id| interner.id(id)).collect());
        }
        expectations.push(key);
    }
    let drain_begun = Instant::now();
    loop {
        let behind = (0..params.subscribers)
            .filter(|index| {
                lists[&expectations[*index]]
                    .last()
                    .is_some_and(|last| newest[*index].load(Ordering::Relaxed) != *last)
            })
            .count();
        if behind == 0 {
            break;
        }
        if drain_begun.elapsed() >= DRAIN {
            eprintln!("subload: {behind} subscribers still behind after {DRAIN:?}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Give stragglers a moment past the newest operation, then stop.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Presence convergence, before the subscribers stop.
    let mut presence_converged = false;
    let mut presence_diverged = 0;
    for _ in 0..5 {
        let fresh = presence_now(client).await;
        presence_diverged = presence_sets
            .iter()
            .filter(|set| *set.lock().unwrap_or_else(PoisonError::into_inner) != fresh)
            .count();
        if presence_diverged == 0 {
            presence_converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    stop_subscribers.cancel();
    let mut op_records = Vec::new();
    for task in op_tasks {
        op_records.push(task.await.unwrap());
    }
    let mut presence_records = Vec::new();
    for task in presence_tasks {
        presence_records.push(task.await.unwrap());
    }
    let rtts = rtts.await.unwrap();
    let descriptors = descriptors.await.unwrap();

    // The accounting, per subscriber.
    let mut totals = Accounting::default();
    let mut dirty = Vec::new();
    for record in &op_records {
        let cache_key = &expectations[record.index];
        let positions = &full[record.repo];
        let start = record
            .start_heads
            .iter()
            .filter_map(|head| interner.get(head))
            .filter_map(|id| positions.get(&id))
            .max()
            .copied();
        let expected: Vec<u32> = lists[cache_key]
            .iter()
            .copied()
            .filter(|id| {
                positions
                    .get(id)
                    .is_some_and(|at| start.is_none_or(|start| *at > start))
            })
            .collect();
        let got: Vec<u32> = record
            .deliveries
            .iter()
            .map(|delivery| delivery.id)
            .collect();
        let accounting = account(&expected, &got);
        totals.missing += accounting.missing;
        totals.duplicated += accounting.duplicated;
        totals.out_of_order += accounting.out_of_order;
        totals.unexpected += accounting.unexpected;
        if !accounting.clean() || record.error.is_some() {
            dirty.push(json!({
                "index": record.index,
                "class": record.class.name(),
                "expected": expected.len(),
                "received": got.len(),
                "missing": accounting.missing,
                "duplicated": accounting.duplicated,
                "out_of_order": accounting.out_of_order,
                "unexpected": accounting.unexpected,
                "error": record.error,
            }));
        }
    }
    let errors = op_records
        .iter()
        .filter(|record| record.error.is_some())
        .count()
        + presence_records
            .iter()
            .filter(|record| record.error.is_some())
            .count();
    report["deliveries"] = json!({
        "missing": totals.missing,
        "duplicated": totals.duplicated,
        "out_of_order": totals.out_of_order,
        "unexpected": totals.unexpected,
        "subscription_errors": errors,
        "subscribers": dirty,
    });

    // Latency over the steady state, gated; the burst, recorded.
    let in_window = |delivery: &Delivery, (from, to): (i64, i64)| {
        delivery.ended_us >= from && delivery.ended_us < to
    };
    let excluded = |record: &OpRecord, delivery: &Delivery| {
        let at = delivery.received_us();
        record
            .excluded
            .iter()
            .any(|(from, to)| at >= *from && at <= *to)
    };
    let mut by_class: BTreeMap<Class, Vec<u64>> = BTreeMap::new();
    let mut all_steady = Vec::new();
    let mut worst_p99 = 0;
    let mut burst_latencies = Vec::new();
    for record in &op_records {
        let mut mine: Vec<u64> = record
            .deliveries
            .iter()
            .filter(|delivery| in_window(delivery, windows.steady) && !excluded(record, delivery))
            .map(|delivery| us(delivery.latency_us))
            .collect();
        burst_latencies.extend(
            record
                .deliveries
                .iter()
                .filter(|delivery| {
                    in_window(delivery, windows.burst) && !excluded(record, delivery)
                })
                .map(|delivery| us(delivery.latency_us)),
        );
        all_steady.extend(mine.iter().copied());
        by_class
            .entry(record.class)
            .or_default()
            .extend(mine.iter().copied());
        mine.sort_unstable();
        worst_p99 = worst_p99.max(percentile(&mine, 99));
    }
    all_steady.sort_unstable();
    let op_p99 = percentile(&all_steady, 99);
    let op_max = all_steady.last().copied().unwrap_or(0);
    let mut operations = latency_json(all_steady);
    operations["worst_subscriber_p99_ms"] = json!(worst_p99 as f64 / 1000.0);
    for (class, values) in by_class {
        operations["by_class"][class.name()] = latency_json(values);
    }
    let mut presence_latencies: Vec<u64> = presence_records
        .iter()
        .flat_map(|record| record.latencies.iter())
        .filter(|(updated, _)| *updated >= windows.steady.0 && *updated < windows.steady.1)
        .map(|(_, latency)| us(*latency))
        .collect();
    presence_latencies.sort_unstable();
    let presence_p99 = percentile(&presence_latencies, 99);
    let presence_max = presence_latencies.last().copied().unwrap_or(0);
    report["steady"] = json!({
        "operations": operations,
        "presence": latency_json(presence_latencies),
    });
    report["steady"]["presence"]["events"] = json!(
        presence_records
            .iter()
            .map(|record| record.events)
            .sum::<u64>()
    );
    report["steady"]["presence"]["converged"] = json!(presence_converged);
    report["steady"]["presence"]["diverged_subscribers"] = json!(presence_diverged);
    report["steady"]["presence"]["reports_sent"] = json!(reports_sent);

    let mut first_frames: Vec<u64> = op_records
        .iter()
        .map(|record| us(record.first_frame_us))
        .chain(
            presence_records
                .iter()
                .map(|record| us(record.first_frame_us)),
        )
        .collect();
    first_frames.sort_unstable();
    report["subscribe"] = json!({
        "first_frame": latency_json(first_frames),
    });
    let unfiltered_records = op_records
        .iter()
        .filter(|record| record.class == Class::Unfiltered);
    let (frames, delivered) = unfiltered_records.fold((0, 0), |(frames, delivered), record| {
        (
            frames + record.frames_with_operations,
            delivered + record.deliveries.len() as u64,
        )
    });
    report["frames_per_operation"] = json!(frames as f64 / delivered.max(1) as f64);
    let collect = |get: fn(&OpRecord) -> &Vec<i64>| -> Vec<u64> {
        op_records
            .iter()
            .flat_map(|record| get(record).iter().map(|value| us(*value)))
            .collect()
    };
    report["stall"] = json!({
        "subscribers": STALLED.min(unfiltered.len()),
        "secs": stall_length.as_secs(),
        "catch_up": latency_json(collect(|record| &record.catch_ups_us)),
    });
    report["reconnect"] = json!({
        "subscribers": RECONNECTED.min(unfiltered.len().saturating_sub(STALLED)),
        "to_live": latency_json(collect(|record| &record.reconnects_us)),
        "first_frame": latency_json(collect(|record| &record.reconnect_first_frames_us)),
        "cap_retries": op_records.iter().map(|record| record.cap_retries).sum::<u64>(),
    });

    // The writes.
    let steady_writes: Vec<&WriteRecord> = scheduled
        .iter()
        .filter(|write| write.due_us >= steady_from_us)
        .collect();
    let achieved = steady_writes.len() as f64 / (steady_wall_us as f64 / 60e6);
    let mut slips: Vec<u64> = steady_writes
        .iter()
        .map(|write| us(write.slip_us))
        .collect();
    slips.sort_unstable();
    let rpc = |describe: bool| {
        latency_json(
            steady_writes
                .iter()
                .filter(|write| write.describe == describe)
                .map(|write| us(write.rpc_us))
                .collect(),
        )
    };
    let failed = scheduled
        .iter()
        .chain(&burst_writes)
        .filter(|write| write.failed)
        .count();
    report["writes"] = json!({
        "steady": steady_writes.len(),
        "achieved_per_min": achieved,
        "slip_p95_ms": percentile(&slips, 95) as f64 / 1000.0,
        "slip_max_ms": slips.last().copied().unwrap_or(0) as f64 / 1000.0,
        "describe_rpc": rpc(true),
        "snapshot_rpc": rpc(false),
        "failed": failed,
        "last_returned_after_steady_ms": scheduled.iter().map(|write| write.returned_us).max().unwrap_or(0).saturating_sub(steady_to_us) / 1000,
    });
    let burst_secs = (burst_to_us - burst_from_us) as f64 / 1e6;
    report["burst"] = json!({
        "operations": burst_writes.len(),
        "per_min": burst_writes.len() as f64 / burst_secs * 60.0,
        "latency": latency_json(burst_latencies),
    });
    let mut rtts = rtts;
    rtts.sort_unstable();
    report["baseline_rtt_ms"] = json!(percentile(&rtts, 50) as f64 / 1000.0);
    let steady_minutes = steady_wall_us as f64 / 60e6;
    monitor.stop();
    report["daemon"] = json!({
        "cpu_secs_per_min": (cpu_after_steady.0 - cpu_at_steady.0) / steady_minutes,
        "peak_rss_bytes": monitor.peak_rss(),
        "peak_threads": monitor.peak_threads(),
        "open_descriptors": descriptors,
        "open_file_limit": daemon_open_file_limit(&daemon),
    });
    report["generator"] = json!({
        "cpu_secs_per_min": (cpu_after_steady.1 - cpu_at_steady.1) / steady_minutes,
        "open_file_limit_before": limit_before,
        "open_file_limit": limit,
    });
    report["broker"] = json!(summary_lines(&daemon, "subscription broker summary"));

    // Teardown.
    for writer in &writers {
        client
            .release_workspace("", &writer.workspace.id, false, false)
            .await
            .unwrap();
    }
    drop(connections);

    let rate_ok = achieved >= GATE_RATE * params.ops_per_min as f64;
    let operations_ok = op_p99 <= millis(GATE_P99) * 1000 && op_max <= millis(GATE_MAX) * 1000;
    let presence_ok =
        presence_p99 <= millis(GATE_P99) * 1000 && presence_max <= millis(GATE_MAX) * 1000;
    let delivered_ok = totals.clean() && errors == 0 && presence_converged && failed == 0;
    report["pass"] = json!({
        "operations": operations_ok,
        "presence": presence_ok,
        "deliveries": delivered_ok,
        "rate": rate_ok,
        "gate": operations_ok && presence_ok && delivered_ok && rate_ok,
    });
    let name = format!("subscriptions-{}", std::env::consts::OS);
    let path = write_report(&report, &name, &started_at);
    keep_daemon_log(&daemon, &path);
    println!("{}", markdown(&report));
    println!("report: {}", path.display());

    assert!(
        delivered_ok,
        "deliveries: {totals:?}, {errors} subscriptions ended in error, presence converged: {presence_converged}, {failed} writes failed"
    );
    assert!(
        rate_ok,
        "achieved {achieved:.0} operations a minute, under {GATE_RATE} × {}",
        params.ops_per_min
    );
    assert!(
        operations_ok,
        "operations: p99 {op_p99} µs, max {op_max} µs (gate p99 {GATE_P99:?}, max {GATE_MAX:?})"
    );
    assert!(
        presence_ok,
        "presence: p99 {presence_p99} µs, max {presence_max} µs (gate p99 {GATE_P99:?}, max {GATE_MAX:?})"
    );
}

/// The daemon's soft open-file limit: from `/proc` on Linux, else from its
/// `open file limit` log line (R2.22), else unknown.
fn daemon_open_file_limit(daemon: &SpawnedDaemon) -> Value {
    if cfg!(target_os = "linux") {
        let limits =
            std::fs::read_to_string(format!("/proc/{}/limits", daemon.pid)).unwrap_or_default();
        return json!(
            limits
                .lines()
                .find_map(|line| line.strip_prefix("Max open files"))
                .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        );
    }
    daemon
        .log_lines("open file limit")
        .last()
        .map_or(Value::Null, |event| event["limit"].clone())
}

/// The run's headline figures, as `docs/load-tests.md`'s table has them.
#[allow(clippy::too_many_lines, reason = "one row per line of the table")]
fn markdown(report: &Value) -> String {
    let params = &report["params"];
    let triple = |latency: &Value| {
        format!(
            "{} / {} / {} ms",
            latency["p50_ms"], latency["p99_ms"], latency["max_ms"]
        )
    };
    let operations = &report["steady"]["operations"];
    let rows = [
        (
            "subscribers (operations + presence) / connections".to_owned(),
            format!(
                "{} + {} / {}",
                params["subscribers"], params["presence_subscribers"], params["connections"]
            ),
        ),
        (
            "operations a minute, target / achieved".to_owned(),
            format!(
                "{} / {:.0}",
                params["ops_per_min"],
                report["writes"]["achieved_per_min"].as_f64().unwrap_or(0.0)
            ),
        ),
        (
            "operation delivery p50 / p99 / max".to_owned(),
            triple(operations),
        ),
        (
            "operation delivery p99, worst subscriber".to_owned(),
            format!("{} ms", operations["worst_subscriber_p99_ms"]),
        ),
        (
            "presence delivery p50 / p99 / max".to_owned(),
            triple(&report["steady"]["presence"]),
        ),
        (
            "missing / duplicated / out of order".to_owned(),
            format!(
                "{} / {} / {}",
                report["deliveries"]["missing"],
                report["deliveries"]["duplicated"],
                report["deliveries"]["out_of_order"]
            ),
        ),
        (
            "frames per operation (unfiltered)".to_owned(),
            format!(
                "{:.2}",
                report["frames_per_operation"].as_f64().unwrap_or(0.0)
            ),
        ),
        (
            "stall catch-up p50 / max".to_owned(),
            format!(
                "{} / {} ms",
                report["stall"]["catch_up"]["p50_ms"], report["stall"]["catch_up"]["max_ms"]
            ),
        ),
        (
            "reconnect to live p50 / max".to_owned(),
            format!(
                "{} / {} ms",
                report["reconnect"]["to_live"]["p50_ms"], report["reconnect"]["to_live"]["max_ms"]
            ),
        ),
        (
            "burst: operations a minute, delivery p99".to_owned(),
            format!(
                "{:.0}, {} ms",
                report["burst"]["per_min"].as_f64().unwrap_or(0.0),
                report["burst"]["latency"]["p99_ms"]
            ),
        ),
        (
            "write RPC p50: describe, snapshot".to_owned(),
            format!(
                "{}, {} ms",
                report["writes"]["describe_rpc"]["p50_ms"],
                report["writes"]["snapshot_rpc"]["p50_ms"]
            ),
        ),
        (
            "baseline round trip p50".to_owned(),
            format!("{} ms", report["baseline_rtt_ms"]),
        ),
        (
            "daemon CPU / generator CPU, s a minute".to_owned(),
            format!(
                "{:.1} / {:.1}",
                report["daemon"]["cpu_secs_per_min"].as_f64().unwrap_or(0.0),
                report["generator"]["cpu_secs_per_min"]
                    .as_f64()
                    .unwrap_or(0.0)
            ),
        ),
        (
            "daemon peak RSS, threads, descriptors".to_owned(),
            format!(
                "{:.0} MB, {}, {}",
                mb(report["daemon"]["peak_rss_bytes"].as_u64().unwrap_or(0)),
                report["daemon"]["peak_threads"],
                report["daemon"]["open_descriptors"]
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
        "\ngate {}",
        if report["pass"]["gate"] == true {
            "passed"
        } else {
            "FAILED"
        }
    )
    .unwrap();
    out
}

// 16
#[test]
fn accounting_is_exact() {
    assert!(account(&[1, 2, 3], &[1, 2, 3]).clean());
    assert_eq!(
        account(&[1, 2, 3], &[1, 3]),
        Accounting {
            missing: 1,
            ..Accounting::default()
        }
    );
    assert_eq!(
        account(&[1, 2, 3], &[1, 2, 2, 3]),
        Accounting {
            duplicated: 1,
            ..Accounting::default()
        }
    );
    assert_eq!(
        account(&[1, 2, 3], &[1, 3, 2]),
        Accounting {
            out_of_order: 1,
            ..Accounting::default()
        }
    );
    assert_eq!(
        account(&[1, 2], &[1, 2, 9]),
        Accounting {
            unexpected: 1,
            ..Accounting::default()
        }
    );

    let values: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile(&values, 50), 50);
    assert_eq!(percentile(&values, 95), 95);
    assert_eq!(percentile(&values, 100), 100);
    assert_eq!(percentile(&[7], 95), 7);
    assert_eq!(percentile(&[], 95), 0);
    let values: Vec<u64> = (1..=1000).collect();
    assert_eq!(per_mille(&values, 999), 999);

    let interner = Interner::default();
    let a = interner.id("a");
    assert_eq!(interner.id("b"), a + 1);
    assert_eq!(interner.id("a"), a);
    assert_eq!(interner.get("c"), None);
}
