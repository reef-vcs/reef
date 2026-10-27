//! The subscription broker (R2.22, tests 7 to 14): fan-out reads the log
//! once per operation, a ring hit sends what a log read would, resuming
//! inside and beyond a ring, a slow subscriber holding nothing, the
//! rings' bounds, rewrites, and rings coming and going. Counters are read
//! from `AppState::broker`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, OperationFilter, OperationKind,
    SubscribeOperationsRequest, SubscribeOperationsResponse, subscribe_operations_request::Start,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::subscribe::broker::BrokerStats;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, init_synthetic_repo, test_settings};
use reef_testing::synth::Preset;
use tokio::time::Instant;
use tonic::{Code, Streaming};

const SECOND: Duration = Duration::from_secs(1);

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await
}

fn stats(daemon: &TestDaemon) -> BrokerStats {
    daemon.handle.state().broker.stats()
}

async fn create(client: &Client, repo: &SyntheticRepo) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "owner".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn describe(client: &Client, ws: &v1::Workspace, text: &str) -> v1::Operation {
    client
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: text.to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap()
        .operation
        .unwrap()
}

/// A file write and an explicit snapshot: one operation.
async fn snapshot(
    client: &Client,
    repo: &SyntheticRepo,
    ws: &v1::Workspace,
    name: &str,
    bytes: usize,
) {
    std::fs::write(Path::new(&ws.path).join(name), vec![b'x'; bytes]).unwrap();
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
}

fn request(repo: &SyntheticRepo) -> SubscribeOperationsRequest {
    SubscribeOperationsRequest {
        repos: vec![repo.id.clone()],
        ..SubscribeOperationsRequest::default()
    }
}

async fn frame(
    stream: &mut Streaming<SubscribeOperationsResponse>,
    within: Duration,
) -> SubscribeOperationsResponse {
    tokio::time::timeout(within, stream.message())
        .await
        .expect("a frame in time")
        .expect("no error")
        .expect("the stream is open")
}

/// Operations from frames until `n` arrived, within `within`, and the last
/// frame's cursor.
async fn operations(
    stream: &mut Streaming<SubscribeOperationsResponse>,
    n: usize,
    within: Duration,
) -> (Vec<v1::Operation>, String) {
    let deadline = Instant::now() + within;
    let mut got = Vec::new();
    let mut cursor = String::new();
    while got.len() < n {
        let next = frame(stream, deadline.saturating_duration_since(Instant::now())).await;
        got.extend(next.operations);
        cursor = next.cursor;
    }
    (got, cursor)
}

fn ids(operations: &[v1::Operation]) -> Vec<String> {
    operations.iter().map(|op| op.id.clone()).collect()
}

/// The error that ends the stream, within `within`.
async fn ending(
    stream: &mut Streaming<SubscribeOperationsResponse>,
    within: Duration,
) -> ClientError {
    let deadline = Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, stream.message())
            .await
            .expect("the stream ends in time")
        {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without an error"),
            Err(status) => return ClientError::from(status),
        }
    }
}

fn reason(error: &ClientError) -> (Code, String) {
    let ClientError::Rpc { status, reason, .. } = error else {
        panic!("{error:?}");
    };
    (status.code(), reason.clone())
}

/// Waits until `check` holds of the broker's stats, within `within`.
async fn until(
    daemon: &TestDaemon,
    within: Duration,
    check: impl Fn(&BrokerStats) -> bool,
) -> BrokerStats {
    let deadline = Instant::now() + within;
    loop {
        let now = stats(daemon);
        if check(&now) {
            return now;
        }
        assert!(
            Instant::now() < deadline,
            "broker stats never matched: {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn ended_at(operation: &v1::Operation) -> SystemTime {
    let at = operation.ended_at.as_ref().unwrap();
    SystemTime::UNIX_EPOCH
        + Duration::new(
            u64::try_from(at.seconds).unwrap(),
            u32::try_from(at.nanos).unwrap(),
        )
}

/// Writes an operation on the registered root through jj-lib, as another
/// `jj` process would; its id.
fn outside(root: &Path, description: &str) -> String {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::workspace::Workspace;
    let workspace = Workspace::load(
        &test_settings(),
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_git_ref_target(
        jj_lib::ref_name::GitRefName::new(&format!("refs/reef-test/{description}")),
        jj_lib::op_store::RefTarget::absent(),
    );
    pollster::block_on(tx.commit(description))
        .unwrap()
        .op_id()
        .hex()
}

/// Replaces the op head `dropped` with `kept`, as `jj op abandon` leaves it.
fn drop_head(root: &Path, dropped: &str, kept: &str) {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::OperationId;
    use jj_lib::workspace::Workspace;
    let workspace = Workspace::load(
        &test_settings(),
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    pollster::block_on(workspace.repo_loader().op_heads_store().update_op_heads(
        &[OperationId::try_from_hex(dropped).unwrap()],
        &OperationId::try_from_hex(kept).unwrap(),
    ))
    .unwrap();
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn fan_out_reads_the_log_once_per_operation() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let ws = create(&daemon.client, &repo).await;
    let mut streams = Vec::new();
    for _ in 0..50 {
        let mut stream = daemon
            .client
            .subscribe_operations(request(&repo))
            .await
            .unwrap();
        frame(&mut stream, SECOND).await;
        streams.push(stream);
    }
    // Every ring is primed before the count starts.
    until(&daemon, 5 * SECOND, |now| {
        now.topics == 1 && now.ingest_reads > 0
    })
    .await;
    let before = stats(&daemon);
    let receivers: Vec<_> = streams
        .into_iter()
        .map(|mut stream| {
            tokio::spawn(async move { operations(&mut stream, 40, 30 * SECOND).await.0 })
        })
        .collect();
    let mut sent = Vec::new();
    for n in 0..40 {
        sent.push(describe(&daemon.client, &ws, &format!("d{n}")).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for receiver in receivers {
        assert_eq!(ids(&receiver.await.unwrap()), ids(&sent));
    }
    let after = stats(&daemon);
    let (ingested, log_reads, ring_reads) = (
        after.ingested - before.ingested,
        after.log_reads - before.log_reads,
        after.ring_reads - before.ring_reads,
    );
    println!("ingested={ingested} log_reads={log_reads} ring_reads={ring_reads}");
    assert_eq!(ingested, 40, "each operation read from the log once");
    assert!(log_reads <= 50, "{log_reads} session reads of the log");
    // A frame may carry two operations; most carry one.
    assert!(ring_reads >= 50 * 40 / 2, "{ring_reads} ring reads");
}

// 8
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one write per kind of operation")]
async fn ring_and_log_send_the_same_frames() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let ws = create(&daemon.client, &repo).await;
    let snapshots = SubscribeOperationsRequest {
        filter: Some(OperationFilter {
            kinds: vec![OperationKind::Snapshot.into()],
            ..OperationFilter::default()
        }),
        ..request(&repo)
    };
    let mut all = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    let start = frame(&mut all, SECOND).await.cursor;
    let mut only = daemon
        .client
        .subscribe_operations(snapshots.clone())
        .await
        .unwrap();
    frame(&mut only, SECOND).await;
    until(&daemon, 5 * SECOND, |now| now.ingest_reads > 0).await;
    let before = stats(&daemon);

    // Thirty operations of every kind a stream carries.
    let client = &daemon.client;
    let mut recorded = 0;
    for n in 0..8 {
        describe(client, &ws, &format!("d{n}")).await;
        snapshot(client, &repo, &ws, &format!("f{n}.txt"), 100).await;
        recorded += 2;
    }
    let described = describe(client, &ws, "to undo").await;
    client
        .undo_operation(v1::UndoOperationRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            operation_id: described.id.clone(),
        })
        .await
        .unwrap();
    recorded += 2;
    let child = client
        .edit_files(v1::EditFilesRequest {
            workspace: ws.id.clone(),
            target: Some(v1::edit_files_request::Target::NewOn("@".to_owned())),
            edits: vec![v1::FileEdit {
                path: "child.txt".to_owned(),
                action: Some(v1::file_edit::Action::Content(b"child".to_vec())),
                ..v1::FileEdit::default()
            }],
            ..v1::EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    client
        .abandon_change(v1::AbandonChangeRequest {
            workspace: ws.id.clone(),
            revision: child.commit_id.clone(),
            ..v1::AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    recorded += 2;
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || outside(&root, "outside"))
        .await
        .unwrap();
    recorded += 1;
    while recorded < 30 {
        describe(client, &ws, &format!("e{recorded}")).await;
        recorded += 1;
    }
    let (ring_all, ring_cursor) = operations(&mut all, 30, 10 * SECOND).await;
    let mut ring_only = Vec::new();
    let expected_snapshots = ring_all
        .iter()
        .filter(|op| op.kind() == OperationKind::Snapshot)
        .count();
    if expected_snapshots > 0 {
        ring_only = operations(&mut only, expected_snapshots, 10 * SECOND)
            .await
            .0;
    }
    let after = stats(&daemon);
    assert!(after.ring_reads > before.ring_reads, "served from the ring");
    assert_eq!(after.log_reads, before.log_reads, "and never from the log");
    drop((all, only));

    // The same from the log alone.
    let stopped = daemon.stop_keeping().await;
    let mut config = stopped.config.clone();
    config.subscribe.ring_operations = 0;
    let daemon = reef_daemon::testing::StoppedDaemon {
        tmp: stopped.tmp,
        config,
    }
    .start()
    .await;
    assert!(!daemon.handle.state().broker.enabled());
    let resumed = |request: SubscribeOperationsRequest| SubscribeOperationsRequest {
        start: Some(Start::Cursor(start.clone())),
        ..request
    };
    let mut all = daemon
        .client
        .subscribe_operations(resumed(request(&repo)))
        .await
        .unwrap();
    let (log_all, log_cursor) = operations(&mut all, 30, 10 * SECOND).await;
    assert_eq!(log_all, ring_all, "effects included");
    assert_eq!(log_cursor, ring_cursor);
    let mut only = daemon
        .client
        .subscribe_operations(resumed(snapshots))
        .await
        .unwrap();
    let log_only = operations(&mut only, expected_snapshots, 10 * SECOND)
        .await
        .0;
    assert_eq!(log_only, ring_only);
    assert_eq!(stats(&daemon).ring_reads, 0);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn resume_inside_and_beyond_the_ring() {
    let (daemon, repo) = fixture_with(|config| {
        config.subscribe.ring_operations = 8;
        config.subscribe.max_behind = 100;
    })
    .await;
    let ws = create(&daemon.client, &repo).await;
    // A live subscriber keeps the ring followed, and its frames' cursors
    // are the points to resume from.
    let mut live = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    let oldest = frame(&mut live, SECOND).await.cursor;
    let mut cursors = Vec::new();
    let mut sent = Vec::new();
    for n in 0..40 {
        sent.push(describe(&daemon.client, &ws, &format!("d{n}")).await);
        // One frame per operation, so each cursor is after exactly one.
        let (got, cursor) = operations(&mut live, 1, 5 * SECOND).await;
        assert_eq!(got.len(), 1);
        cursors.push(cursor);
    }
    until(&daemon, 5 * SECOND, |now| now.items == 8).await;

    // 5 back: in the ring.
    let before = stats(&daemon);
    let mut stream = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor(cursors[34].clone())),
            ..request(&repo)
        })
        .await
        .unwrap();
    let (got, _) = operations(&mut stream, 5, 5 * SECOND).await;
    assert_eq!(ids(&got), ids(&sent[35..]));
    assert_eq!(stats(&daemon).log_reads, before.log_reads, "no log read");

    // 30 back: the log to the ring's base, then the ring.
    let before = stats(&daemon);
    let mut stream = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor(cursors[9].clone())),
            ..request(&repo)
        })
        .await
        .unwrap();
    let (got, _) = operations(&mut stream, 30, 5 * SECOND).await;
    assert_eq!(ids(&got), ids(&sent[10..]));
    let reads = stats(&daemon).log_reads - before.log_reads;
    assert!((1..=3).contains(&reads), "{reads} log reads");

    // Past `max_behind`: 101 operations after the start's cursor.
    for n in 40..101 {
        describe(&daemon.client, &ws, &format!("d{n}")).await;
    }
    let error = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor(oldest)),
            ..request(&repo)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "TOO_FAR_BEHIND".to_owned())
    );
    assert_eq!(error.metadata().get("repo_id"), Some(&repo.id));
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_subscriber_holds_nothing() {
    let (daemon, repo) = fixture_with(|config| config.subscribe.ring_operations = 16).await;
    let ws = create(&daemon.client, &repo).await;
    let mut slow = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    frame(&mut slow, SECOND).await;
    let mut fast = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    frame(&mut fast, SECOND).await;
    let receiver = tokio::spawn(async move {
        let mut got = Vec::new();
        while got.len() < 200 {
            let next = frame(&mut fast, 10 * SECOND).await;
            let at = SystemTime::now();
            got.extend(next.operations.into_iter().map(|op| (op, at)));
        }
        got
    });
    let mut sent = Vec::new();
    for n in 0..200 {
        sent.push(describe(&daemon.client, &ws, &format!("d{n}")).await);
        assert!(
            stats(&daemon).items <= 16,
            "the ring never holds more than its bound"
        );
    }
    let got = receiver.await.unwrap();
    assert_eq!(
        got.iter().map(|(op, _)| op.id.clone()).collect::<Vec<_>>(),
        ids(&sent)
    );
    let mut latencies: Vec<Duration> = got
        .iter()
        .map(|(op, at)| at.duration_since(ended_at(op)).unwrap_or_default())
        .collect();
    latencies.sort();
    let (p50, max) = (
        latencies[latencies.len() / 2],
        latencies[latencies.len() - 1],
    );
    println!("fast subscriber: p50={p50:?} max={max:?}");
    assert!(p50 <= Duration::from_millis(100), "p50 {p50:?}");
    assert!(max <= 2 * SECOND, "max {max:?}");

    // The slow one gets every operation once, in order. It is not pushed
    // off the ring at this size: the transport's windows (2 MiB a stream)
    // take its frames while it does not read, so its session is never held.
    // Falling off the ring and reading the log is test 10's.
    let (late, _) = operations(&mut slow, 200, 10 * SECOND).await;
    assert_eq!(ids(&late), ids(&sent));
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn memory_stays_bounded() {
    let (daemon, first) = fixture_with(|config| {
        config.subscribe.ring_bytes = 64 << 10;
        config.subscribe.ring_total_bytes = 96 << 10;
    })
    .await;
    let root = daemon.tmp.path().join("second");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let registered = daemon
        .client
        .register_repository(&root, v1::IfNotJj::Fail)
        .await
        .unwrap();
    let second = SyntheticRepo {
        root: std::fs::canonicalize(&root).unwrap(),
        id: registered.repository.unwrap().id,
    };
    let repos = [first, second];
    let mut streams = Vec::new();
    let mut workspaces = Vec::new();
    for repo in &repos {
        workspaces.push(create(&daemon.client, repo).await);
        let mut stream = daemon
            .client
            .subscribe_operations(request(repo))
            .await
            .unwrap();
        frame(&mut stream, SECOND).await;
        streams.push(stream);
    }
    let state = daemon.handle.state();
    let largest = std::sync::atomic::AtomicUsize::new(0);
    let mut peak = (0, 0);
    for n in 0..200 {
        let i = n % 2;
        snapshot(
            &daemon.client,
            &repos[i],
            &workspaces[i],
            &format!("f{n}.txt"),
            4096,
        )
        .await;
        let now = stats(&daemon);
        let item = now.bytes / now.items.max(1);
        largest.fetch_max(item, std::sync::atomic::Ordering::Relaxed);
        let slack = 2 * item.max(1024);
        assert!(now.bytes <= (96 << 10) + slack, "total {now:?}");
        for repo in &repos {
            let id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
            if let Some((_, bytes)) = state.broker.size_of(id) {
                assert!(bytes <= (64 << 10) + slack, "{bytes} bytes in one ring");
                peak.0 = peak.0.max(bytes);
            }
        }
        peak.1 = peak.1.max(now.bytes);
    }
    println!(
        "bytes per item about {}, peak ring {} B, peak total {} B",
        largest.into_inner(),
        peak.0,
        peak.1
    );
    assert!(stats(&daemon).evicted > 0, "the bounds evicted");
}

// 13, an outside rewrite (GC's half is in tests/gc.rs)
#[tokio::test(flavor = "multi_thread")]
async fn rings_follow_the_log_through_an_outside_rewrite() {
    let (daemon, repo) = fixture_with(|_| {}).await;
    let mut stream = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    frame(&mut stream, SECOND).await;
    until(&daemon, 5 * SECOND, |now| now.ingest_reads > 0).await;
    let root = repo.root.clone();
    let (kept, dropped) = tokio::task::spawn_blocking(move || {
        let kept = outside(&root, "kept");
        let dropped = outside(&root, "dropped");
        (kept, dropped)
    })
    .await
    .unwrap();
    let (got, _) = operations(&mut stream, 2, 5 * SECOND).await;
    assert_eq!(ids(&got), vec![kept.clone(), dropped.clone()]);
    let before = stats(&daemon);

    // `dropped` leaves the op heads: the subscriber stood on it.
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || drop_head(&root, &dropped, &kept))
        .await
        .unwrap();
    let error = ending(&mut stream, 5 * SECOND).await;
    assert_eq!(
        reason(&error),
        (
            Code::FailedPrecondition,
            "OPERATION_LOG_REWRITTEN".to_owned()
        )
    );
    let after = until(&daemon, 5 * SECOND, |now| now.resets > before.resets).await;
    assert_eq!(after.resets, before.resets + 1);

    // A new subscription is served from the restarted ring.
    let mut stream = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    frame(&mut stream, SECOND).await;
    let root = repo.root.clone();
    let next = tokio::task::spawn_blocking(move || outside(&root, "next"))
        .await
        .unwrap();
    let (got, _) = operations(&mut stream, 1, 5 * SECOND).await;
    assert_eq!(ids(&got), vec![next]);
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn rings_come_and_go() {
    let (daemon, repo) = fixture_with(|config| {
        config.subscribe.ring_linger = SECOND;
    })
    .await;
    // A ring exists while a subscription follows its repository, and is
    // dropped about a linger after the last one leaves.
    let mut stream = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap();
    frame(&mut stream, SECOND).await;
    until(&daemon, 5 * SECOND, |now| now.topics == 1).await;
    drop(stream);
    tokio::time::sleep(SECOND / 2).await;
    assert_eq!(stats(&daemon).topics, 1, "it lingers");
    until(&daemon, 5 * SECOND, |now| now.topics == 0).await;

    // A subscription to every repository follows one registered later, and
    // its ring is primed.
    let mut every = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest::default())
        .await
        .unwrap();
    frame(&mut every, SECOND).await;
    until(&daemon, 5 * SECOND, |now| {
        now.topics == 1 && now.ingest_reads > 0
    })
    .await;
    let root = daemon.tmp.path().join("later");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let later = daemon
        .client
        .register_repository(&root, v1::IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap()
        .id;
    until(&daemon, 5 * SECOND, |now| now.topics == 2).await;
    let later_repo = SyntheticRepo {
        root: std::fs::canonicalize(&root).unwrap(),
        id: later.clone(),
    };
    let ws = create(&daemon.client, &later_repo).await;
    let (got, _) = operations(&mut every, 1, 5 * SECOND).await;
    assert_eq!(got[0].repo_id, later);
    assert_eq!(got[0].workspace_id, ws.id);

    // Forgetting a repository drops its ring at once, and the stream to
    // every repository stays open.
    daemon
        .client
        .release_workspace(&later, &ws.id, false, false)
        .await
        .unwrap();
    daemon.client.forget_repository(&later).await.unwrap();
    until(&daemon, SECOND, |now| now.topics == 1).await;
    let ws = create(&daemon.client, &repo).await;
    let got = loop {
        let (got, _) = operations(&mut every, 1, 5 * SECOND).await;
        if got.iter().any(|op| op.repo_id == repo.id) {
            break got;
        }
    };
    assert!(got.iter().any(|op| op.workspace_id == ws.id), "{got:?}");

    // Shutdown ends the streams and the supervisor promptly.
    let begun = Instant::now();
    daemon.handle.shutdown();
    let error = ending(&mut every, 2 * SECOND).await;
    assert_eq!(reason(&error).0, Code::Unavailable);
    daemon.handle.join().await.unwrap();
    assert!(
        begun.elapsed() < 2 * SECOND + SECOND,
        "{:?}",
        begun.elapsed()
    );
}
