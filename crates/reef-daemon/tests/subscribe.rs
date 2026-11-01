//! `SubscribeOperations` (R2.09, tests 4 to 15): the stream of the
//! operation log through the daemon, its filters, resuming, frames,
//! backpressure, shutdown and the subscription cap. Since R2.22 sessions
//! are served from the broker's rings; tests 5, 10 and 11 also run with
//! the broker off (R2.22's test 9), so the log path stays tested.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use prost::Message as _;
use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, IfNotJj, ListOperationsRequest,
    OperationFilter, OperationKind, SubscribeOperationsRequest, SubscribeOperationsResponse,
    subscribe_operations_request::Start,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, init_synthetic_repo, test_settings};
use reef_testing::synth::Preset;
use tokio::time::Instant;
use tonic::{Code, Streaming};

/// A daemon whose watcher never snapshots on its own, over a small
/// synthetic repository, with the configuration changed by `configure`.
async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

/// Whether sessions are served from R2.22's broker or read the log
/// themselves, as before it (test 9 runs some of R2.09's tests both ways).
#[derive(Clone, Copy, Debug)]
enum Served {
    Broker,
    Log,
}

impl Served {
    fn configure(self, config: &mut Config) {
        if let Served::Log = self {
            config.subscribe.ring_operations = 0;
        }
    }
}

async fn create(client: &Client, repo: &SyntheticRepo, owner: &str) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: owner.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Describes `ws`'s working-copy change: exactly one operation.
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

fn request(repo: &SyntheticRepo) -> SubscribeOperationsRequest {
    SubscribeOperationsRequest {
        repos: vec![repo.id.clone()],
        ..SubscribeOperationsRequest::default()
    }
}

async fn subscribe(
    client: &Client,
    request: SubscribeOperationsRequest,
) -> Streaming<SubscribeOperationsResponse> {
    client.subscribe_operations(request).await.unwrap()
}

/// The next frame, within `within`.
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

/// The error that ends the stream, within `within`.
async fn ending(
    stream: &mut Streaming<SubscribeOperationsResponse>,
    within: Duration,
) -> ClientError {
    let deadline = Instant::now() + within;
    loop {
        let next = tokio::time::timeout_at(deadline, stream.message())
            .await
            .expect("the stream ends in time");
        match next {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without an error"),
            Err(status) => return ClientError::from(status),
        }
    }
}

/// Operations from frames until `n` arrived, within `within`.
async fn operations(
    stream: &mut Streaming<SubscribeOperationsResponse>,
    n: usize,
    within: Duration,
) -> (Vec<v1::Operation>, Vec<SubscribeOperationsResponse>) {
    let deadline = Instant::now() + within;
    let mut got = Vec::new();
    let mut frames = Vec::new();
    while got.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        let next = frame(stream, left).await;
        got.extend(next.operations.iter().cloned());
        frames.push(next);
    }
    (got, frames)
}

/// Asserts nothing arrives within `quiet` but keepalives.
async fn nothing(stream: &mut Streaming<SubscribeOperationsResponse>, quiet: Duration) {
    let deadline = Instant::now() + quiet;
    while let Ok(next) = tokio::time::timeout_at(deadline, stream.message()).await {
        let next = next.unwrap().unwrap();
        assert!(next.operations.is_empty(), "{:?}", next.operations);
    }
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn metadata(error: &ClientError, key: &str) -> String {
    match error {
        ClientError::Rpc { metadata, .. } => metadata.get(key).cloned().unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

/// Operations `ListOperations` lists with `filter`, oldest first.
async fn listed(
    client: &Client,
    repo: &SyntheticRepo,
    filter: OperationFilter,
) -> Vec<v1::Operation> {
    let mut operations = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(filter),
            page_size: 1000,
            include_effects: true,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    operations.reverse();
    operations
}

fn ids(operations: &[v1::Operation]) -> Vec<String> {
    operations.iter().map(|op| op.id.clone()).collect()
}

/// The newest operation's id.
async fn head(client: &Client, repo: &SyntheticRepo) -> String {
    client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            page_size: 1,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations[0]
        .id
        .clone()
}

/// Writes an operation on the registered root through jj-lib, as another
/// `jj` process would.
fn outside(root: &Path, description: &str) {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
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
    pollster::block_on(tx.commit(description)).unwrap();
}

fn ended_at(operation: &v1::Operation) -> SystemTime {
    let at = operation.ended_at.as_ref().unwrap();
    SystemTime::UNIX_EPOCH
        + Duration::new(
            u64::try_from(at.seconds).unwrap(),
            u32::try_from(at.nanos).unwrap(),
        )
}

const SECOND: Duration = Duration::from_secs(1);

// 4
#[tokio::test(flavor = "multi_thread")]
async fn first_frame_is_immediate() {
    let (daemon, repo) = fixture().await;
    let started = Instant::now();
    let mut stream = subscribe(&daemon.client, request(&repo)).await;
    let first = frame(&mut stream, SECOND).await;
    assert!(started.elapsed() < SECOND);
    assert!(first.live);
    assert!(first.operations.is_empty(), "{:?}", first.operations);
    assert!(!first.cursor.is_empty(), "{:?}", first.cursor);
    assert_eq!(
        first.head_operation_ids,
        vec![head(&daemon.client, &repo).await]
    );
    // Every repository: no head ids.
    let mut all = subscribe(&daemon.client, SubscribeOperationsRequest::default()).await;
    let first = frame(&mut all, SECOND).await;
    assert!(first.live && first.head_operation_ids.is_empty());
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn every_write_path_wakes_subscribers() {
    every_write_path_wakes(Served::Broker).await;
}

// R2.22, 9
#[tokio::test(flavor = "multi_thread")]
async fn every_write_path_wakes_subscribers_from_the_log() {
    every_write_path_wakes(Served::Log).await;
}

#[expect(clippy::too_many_lines, reason = "one write per write path")]
async fn every_write_path_wakes(served: Served) {
    let (daemon, repo) = fixture_with(|config| {
        config.subscribe.outside_poll = Duration::from_hours(1);
        served.configure(config);
    })
    .await;
    let client = daemon.client.clone().with_agent("bot-1").unwrap();
    let mut stream = subscribe(&client, request(&repo)).await;
    let start = frame(&mut stream, SECOND).await.head_operation_ids;

    let ws = create(&client, &repo, "owner").await;
    std::fs::write(Path::new(&ws.path).join("new.txt"), "one").unwrap();
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let described = describe(&client, &ws, "first").await;
    client
        .undo_operation(v1::UndoOperationRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            operation_id: described.id.clone(),
        })
        .await
        .unwrap();
    client
        .edit_files(v1::EditFilesRequest {
            workspace: ws.id.clone(),
            target: Some(v1::edit_files_request::Target::Revision("@".to_owned())),
            edits: vec![v1::FileEdit {
                path: "edited.txt".to_owned(),
                action: Some(v1::file_edit::Action::Content(b"edited".to_vec())),
                ..v1::FileEdit::default()
            }],
            ..v1::EditFilesRequest::default()
        })
        .await
        .unwrap();
    // A new change on `@`, then abandoned.
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
    client
        .move_workspace(v1::MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(v1::move_workspace_request::Target::NewOn("@-".to_owned())),
            ..v1::MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();

    let all = listed(&daemon.client, &repo, OperationFilter::default()).await;
    let after: Vec<v1::Operation> = all
        .iter()
        .skip_while(|op| !start.contains(&op.id))
        .skip(1)
        .cloned()
        .collect();
    let kinds: BTreeSet<OperationKind> = after.iter().map(v1::Operation::kind).collect();
    for kind in [
        OperationKind::CreateWorkspace,
        OperationKind::Snapshot,
        OperationKind::Describe,
        OperationKind::Undo,
        OperationKind::Edit,
        OperationKind::Abandon,
        OperationKind::MoveWorkspace,
        OperationKind::ReleaseWorkspace,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} in {:?}", ids(&after));
    }
    let (got, _) = operations(&mut stream, after.len(), 5 * SECOND).await;
    assert_eq!(ids(&got), ids(&after));
    for (got, listed) in got.iter().zip(&after) {
        let fetched = daemon
            .client
            .get_operation(&repo.id, &got.id)
            .await
            .unwrap()
            .operation
            .unwrap();
        assert_eq!(got, &fetched, "{}", got.description);
        assert_eq!(got.kind, listed.kind);
    }
    assert!(got.iter().all(|op| op.agent == "bot-1"), "{got:?}");
    nothing(&mut stream, Duration::from_millis(300)).await;
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn outside_writes_arrive() {
    let (daemon, repo) = fixture().await;
    let mut stream = subscribe(&daemon.client, request(&repo)).await;
    frame(&mut stream, SECOND).await;
    outside(&repo.root, "outside");
    let (got, _) = operations(&mut stream, 1, 2 * SECOND).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind(), OperationKind::External);
    assert_eq!(got[0].description, "outside");

    // Two concurrent operations from one base, then the merge the daemon
    // commits when it next loads the repository: each after its parents.
    let (left, right) = concurrent(&repo.root);
    let (got, _) = operations(&mut stream, 2, 2 * SECOND).await;
    let mut both: Vec<String> = ids(&got);
    both.sort();
    let mut expected = vec![left.clone(), right.clone()];
    expected.sort();
    assert_eq!(both, expected);
    daemon
        .client
        .get_status(v1::GetStatusRequest {
            repo: repo.id.clone(),
            ..v1::GetStatusRequest::default()
        })
        .await
        .unwrap();
    let (got, _) = operations(&mut stream, 1, 2 * SECOND).await;
    let mut parents = got[0].parent_ids.clone();
    parents.sort();
    assert_eq!(parents, expected, "{:?}", got[0]);
}

/// Two operations from one base through jj-lib, unmerged, as two `jj`
/// processes racing would leave them; their ids.
fn concurrent(root: &Path) -> (String, String) {
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
    let base = pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
    let mut ids = Vec::new();
    for side in ["left", "right"] {
        let mut tx = base.start_transaction();
        tx.repo_mut().set_git_ref_target(
            jj_lib::ref_name::GitRefName::new(&format!("refs/reef-test/{side}")),
            jj_lib::op_store::RefTarget::absent(),
        );
        ids.push(pollster::block_on(tx.commit(side)).unwrap().op_id().hex());
    }
    (ids[0].clone(), ids[1].clone())
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn delivery_latency() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon.client, &repo, "owner").await;
    for subscribers in [1, 10] {
        let mut streams = Vec::new();
        for _ in 0..subscribers {
            let mut stream = subscribe(&daemon.client, request(&repo)).await;
            frame(&mut stream, SECOND).await;
            streams.push(stream);
        }
        let receivers: Vec<_> = streams
            .into_iter()
            .map(|mut stream| {
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    while got.len() < 40 {
                        let next = frame(&mut stream, 10 * SECOND).await;
                        let at = SystemTime::now();
                        got.extend(next.operations.into_iter().map(|op| (op, at)));
                    }
                    got
                })
            })
            .collect();
        let mut sent = Vec::new();
        let mut baselines = Vec::new();
        for n in 0..40 {
            sent.push(describe(&daemon.client, &ws, &format!("d{subscribers}-{n}")).await);
            let before = std::time::Instant::now();
            daemon.client.status().await.unwrap();
            baselines.push(before.elapsed());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        baselines.sort();
        let baseline = baselines[baselines.len() / 2];
        for receiver in receivers {
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
            let (p50, p95, max) = (
                latencies[latencies.len() / 2],
                latencies[latencies.len() * 95 / 100],
                latencies[latencies.len() - 1],
            );
            println!(
                "subscribers={subscribers} p50={p50:?} p95={p95:?} max={max:?} baseline={baseline:?}"
            );
            assert!(
                p50 <= Duration::from_millis(100) + baseline,
                "p50 {p50:?} over 100 ms + {baseline:?}"
            );
            assert!(max <= 2 * SECOND, "max {max:?}");
        }
    }
}

// 8
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one case per filter field")]
async fn filters() {
    let (daemon, repo) = fixture().await;
    let start = head(&daemon.client, &repo).await;
    let bot = daemon.client.clone().with_agent("bot-1").unwrap();
    let ws = create(&daemon.client, &repo, "owner").await;
    let other = create(&daemon.client, &repo, "owner").await;
    describe(&bot, &ws, "by the bot").await;
    describe(&daemon.client, &other, "by the owner").await;
    std::fs::write(Path::new(&ws.path).join("x.txt"), "x").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let change = daemon
        .client
        .inspect_workspace(&repo.id, &other.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
        .change_id;
    let reverse: String = change
        .chars()
        .map(|c| char::from(b'z' - u8::try_from(c.to_digit(16).unwrap()).unwrap()))
        .collect();
    let filters = [
        OperationFilter {
            kinds: vec![OperationKind::Describe.into()],
            ..OperationFilter::default()
        },
        OperationFilter {
            workspaces: vec![ws.id[..8].to_owned()],
            ..OperationFilter::default()
        },
        OperationFilter {
            agents: vec!["bot-1".to_owned()],
            ..OperationFilter::default()
        },
        OperationFilter {
            changes: vec![reverse[..8].to_owned()],
            ..OperationFilter::default()
        },
    ];
    let after: Vec<String> = listed(&daemon.client, &repo, OperationFilter::default())
        .await
        .into_iter()
        .map(|op| op.id)
        .skip_while(|id| *id != start)
        .skip(1)
        .collect();
    for filter in filters {
        let expected: Vec<v1::Operation> = listed(&daemon.client, &repo, filter.clone())
            .await
            .into_iter()
            .filter(|op| after.contains(&op.id))
            .collect();
        assert!(!expected.is_empty(), "{filter:?}");
        let mut stream = subscribe(
            &daemon.client,
            SubscribeOperationsRequest {
                filter: Some(filter.clone()),
                start: Some(Start::AfterOperation(start.clone())),
                ..request(&repo)
            },
        )
        .await;
        let mut got = Vec::new();
        loop {
            let next = frame(&mut stream, 5 * SECOND).await;
            got.extend(next.operations);
            if next.live {
                break;
            }
        }
        assert_eq!(ids(&got), ids(&expected), "{filter:?}");
    }
    let refused = |filter: OperationFilter| {
        let client = daemon.client.clone();
        let request = SubscribeOperationsRequest {
            filter: Some(filter),
            ..request(&repo)
        };
        async move { client.subscribe_operations(request).await.unwrap_err() }
    };
    let error = refused(OperationFilter {
        workspaces: vec!["0000000000000000".to_owned()],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(reason(&error).0, Code::NotFound);
    let error = refused(OperationFilter {
        changes: vec!["zz!".to_owned()],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(
        reason(&error),
        (Code::InvalidArgument, "INVALID_CHANGE_ID".to_owned())
    );
    let error = refused(OperationFilter {
        kinds: vec![99],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(reason(&error).0, Code::InvalidArgument);
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn replay_after_operation() {
    let (daemon, repo) = fixture_with(|config| config.subscribe.max_behind = 3).await;
    let ws = create(&daemon.client, &repo, "owner").await;
    let mut described = Vec::new();
    for n in 0..5 {
        described.push(describe(&daemon.client, &ws, &format!("d{n}")).await);
    }
    let mut stream = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            start: Some(Start::AfterOperation(described[1].id[..12].to_owned())),
            ..request(&repo)
        },
    )
    .await;
    let mut replayed = Vec::new();
    let live = loop {
        let next = frame(&mut stream, SECOND).await;
        if next.live {
            break next;
        }
        replayed.extend(next.operations);
    };
    assert_eq!(ids(&replayed), ids(&described[2..]));
    assert!(live.operations.is_empty(), "{:?}", live.operations);
    assert_eq!(live.head_operation_ids, vec![described[4].id.clone()]);
    let sixth = describe(&daemon.client, &ws, "d5").await;
    let (got, frames) = operations(&mut stream, 1, SECOND).await;
    assert_eq!(ids(&got), vec![sixth.id]);
    assert!(frames.iter().all(|frame| frame.live));

    let refused = |start: Start, repos: Vec<String>| {
        let client = daemon.client.clone();
        async move {
            client
                .subscribe_operations(SubscribeOperationsRequest {
                    repos,
                    start: Some(start),
                    ..SubscribeOperationsRequest::default()
                })
                .await
                .unwrap_err()
        }
    };
    let one = vec![repo.id.clone()];
    let error = refused(Start::AfterOperation("zz".to_owned()), one.clone()).await;
    assert_eq!(
        reason(&error),
        (Code::InvalidArgument, "INVALID_OPERATION_ID".to_owned())
    );
    let error = refused(Start::AfterOperation("f".repeat(128)), one.clone()).await;
    assert_eq!(
        reason(&error),
        (Code::NotFound, "OPERATION_NOT_FOUND".to_owned())
    );
    let error = refused(Start::AfterOperation(described[0].id.clone()), Vec::new()).await;
    assert_eq!(reason(&error), (Code::InvalidArgument, String::new()));
    let root = listed(&daemon.client, &repo, OperationFilter::default()).await[0]
        .id
        .clone();
    let error = refused(Start::AfterOperation(root), one.clone()).await;
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "TOO_FAR_BEHIND".to_owned())
    );
    assert_eq!(metadata(&error, "repo_id"), repo.id);
    // An operation dropped from the op heads, as `jj op abandon` leaves it.
    let (kept, dropped) = concurrent(&repo.root);
    drop_head(&repo.root, &dropped, &kept);
    let error = refused(Start::AfterOperation(dropped.clone()), one).await;
    assert_eq!(
        reason(&error),
        (
            Code::FailedPrecondition,
            "OPERATION_NOT_IN_HISTORY".to_owned()
        )
    );
    assert_eq!(metadata(&error, "operation_id"), dropped);
}

/// Drops `dropped` from the op heads, leaving `kept`.
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

// 10
#[tokio::test(flavor = "multi_thread")]
async fn resume_by_cursor() {
    resume_by_cursor_served(Served::Broker).await;
}

// R2.22, 9
#[tokio::test(flavor = "multi_thread")]
async fn resume_by_cursor_from_the_log() {
    resume_by_cursor_served(Served::Log).await;
}

#[expect(clippy::too_many_lines, reason = "one case per way to resume")]
async fn resume_by_cursor_served(served: Served) {
    let (daemon, repo) = fixture_with(|config| served.configure(config)).await;
    let ws = create(&daemon.client, &repo, "owner").await;
    let mut stream = subscribe(&daemon.client, request(&repo)).await;
    frame(&mut stream, SECOND).await;
    describe(&daemon.client, &ws, "a").await;
    let (_, frames) = operations(&mut stream, 1, SECOND).await;
    let cursor = frames.last().unwrap().cursor.clone();
    drop(stream);
    let bot = daemon.client.clone().with_agent("bot-1").unwrap();
    let mut later = Vec::new();
    for n in 0..4 {
        let client = if n % 2 == 1 { &bot } else { &daemon.client };
        later.push(describe(client, &ws, &format!("later {n}")).await);
    }
    let mut stream = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            start: Some(Start::Cursor(cursor.clone())),
            ..request(&repo)
        },
    )
    .await;
    let mut replayed = Vec::new();
    loop {
        let next = frame(&mut stream, SECOND).await;
        if next.live {
            assert!(next.operations.is_empty(), "{:?}", next.operations);
            break;
        }
        replayed.extend(next.operations);
    }
    assert_eq!(ids(&replayed), ids(&later));

    // Another filter applies from the cursor on.
    let mut stream = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            start: Some(Start::Cursor(cursor.clone())),
            filter: Some(OperationFilter {
                agents: vec!["bot-1".to_owned()],
                ..OperationFilter::default()
            }),
            ..request(&repo)
        },
    )
    .await;
    let mut filtered = Vec::new();
    loop {
        let next = frame(&mut stream, SECOND).await;
        if next.live {
            break;
        }
        filtered.extend(next.operations);
    }
    assert_eq!(ids(&filtered), ids(&[later[1].clone(), later[3].clone()]));

    let error = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor("abc".to_owned())),
            ..request(&repo)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::InvalidArgument, "INVALID_CURSOR".to_owned())
    );

    // A cursor of every repository, resumed after another was registered,
    // follows the new one from its head.
    let mut all = subscribe(&daemon.client, SubscribeOperationsRequest::default()).await;
    let all_cursor = frame(&mut all, SECOND).await.cursor;
    drop(all);
    let second = daemon.tmp.path().join("second");
    init_synthetic_repo(&second, Preset::Small);
    let registered = daemon
        .client
        .register_repository(&second, IfNotJj::Fail)
        .await
        .unwrap();
    let mut all = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            start: Some(Start::Cursor(all_cursor)),
            ..SubscribeOperationsRequest::default()
        },
    )
    .await;
    loop {
        if frame(&mut all, SECOND).await.live {
            break;
        }
    }
    let ws2 = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: registered.repository.unwrap().id,
            owner: "owner".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let (got, _) = operations(&mut all, 1, 2 * SECOND).await;
    assert_eq!(got[0].repo_id, ws2.repo_id);
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn slow_subscriber_is_held_not_dropped() {
    slow_subscriber_is_held(Served::Broker).await;
}

// R2.22, 9
#[tokio::test(flavor = "multi_thread")]
async fn slow_subscriber_is_held_not_dropped_from_the_log() {
    slow_subscriber_is_held(Served::Log).await;
}

async fn slow_subscriber_is_held(served: Served) {
    // Frames 250 ms apart, so the slow reader's operations coalesce however
    // long a describe takes under load: HTTP/2's window takes its 90 small
    // frames while it does not read, so its session is never held (R2.22).
    let (daemon, repo) = fixture_with(|config| {
        config.subscribe.frame_interval = Duration::from_millis(250);
        served.configure(config);
    })
    .await;
    let ws = create(&daemon.client, &repo, "owner").await;
    let mut slow = subscribe(&daemon.client, request(&repo)).await;
    frame(&mut slow, SECOND).await;
    let mut fast = subscribe(&daemon.client, request(&repo)).await;
    frame(&mut fast, SECOND).await;
    let reader = tokio::spawn(async move { operations(&mut fast, 90, 60 * SECOND).await.0 });
    let mut sent = Vec::new();
    for n in 0..90 {
        sent.push(describe(&daemon.client, &ws, &format!("d{n}")).await);
    }
    assert_eq!(ids(&reader.await.unwrap()), ids(&sent));
    let (got, frames) = operations(&mut slow, 90, 10 * SECOND).await;
    assert_eq!(ids(&got), ids(&sent));
    assert!(frames.len() < 90, "{} frames", frames.len());
    assert!(frames.iter().all(|frame| frame.encoded_len() <= 1 << 20));
    // A reader that falls more than `max_behind` back mid-stream is told
    // so: the session's unit tests, since how far back a reader that
    // stops falls depends on what HTTP/2 buffers for it.
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn frames_and_keepalives() {
    let (daemon, repo) = fixture_with(|config| {
        config.subscribe.keepalive = Duration::from_millis(300);
        config.subscribe.frame_operations = 10;
        config.subscribe.outside_poll = Duration::from_hours(1);
    })
    .await;
    let mut stream = subscribe(&daemon.client, request(&repo)).await;
    let first = frame(&mut stream, SECOND).await;
    for _ in 0..3 {
        let next = frame(&mut stream, Duration::from_millis(600)).await;
        assert!(next.operations.is_empty(), "{:?}", next.operations);
        assert_eq!(next.cursor, first.cursor);
    }
    // 25 written by another process with the poll off, then loaded by the
    // daemon in one reload: read at once, in frames of 10.
    for n in 0..25 {
        outside(&repo.root, &format!("burst-{n}"));
    }
    daemon
        .client
        .get_status(v1::GetStatusRequest {
            repo: repo.id.clone(),
            ..v1::GetStatusRequest::default()
        })
        .await
        .unwrap();
    let (got, frames) = operations(&mut stream, 25, 5 * SECOND).await;
    assert_eq!(
        got.iter()
            .map(|op| op.description.clone())
            .collect::<Vec<_>>(),
        (0..25).map(|n| format!("burst-{n}")).collect::<Vec<_>>()
    );
    // Keepalives come every 300 ms meanwhile.
    let sizes: Vec<usize> = frames
        .iter()
        .map(|frame| frame.operations.len())
        .filter(|&size| size > 0)
        .collect();
    assert_eq!(sizes, [10, 10, 5]);
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn repositories_come_and_go() {
    let (daemon, repo) = fixture().await;
    let second = daemon.tmp.path().join("second");
    init_synthetic_repo(&second, Preset::Small);
    let mut all = subscribe(&daemon.client, SubscribeOperationsRequest::default()).await;
    frame(&mut all, SECOND).await;
    let registered = daemon
        .client
        .register_repository(&second, IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap();
    let mut only = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            repos: vec![registered.id.clone()],
            ..SubscribeOperationsRequest::default()
        },
    )
    .await;
    frame(&mut only, SECOND).await;
    let ws = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: registered.id.clone(),
            owner: "owner".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let (got, _) = operations(&mut all, 1, 2 * SECOND).await;
    assert!(got.iter().all(|op| op.repo_id == registered.id));
    daemon
        .client
        .release_workspace(&registered.id, &ws.id, true, false)
        .await
        .unwrap();
    daemon
        .client
        .forget_repository(&registered.id)
        .await
        .unwrap();
    let error = ending(&mut only, 2 * SECOND).await;
    assert_eq!(
        reason(&error),
        (Code::NotFound, "REPOSITORY_NOT_FOUND".to_owned())
    );
    // The other stream stays open and follows the first repository.
    let first = create(&daemon.client, &repo, "owner").await;
    let deadline = Instant::now() + 2 * SECOND;
    loop {
        let next = frame(&mut all, deadline - Instant::now()).await;
        if next.operations.iter().any(|op| op.workspace_id == first.id) {
            break;
        }
    }
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_streams_and_resume_finds_the_rest() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        // Events mark the workspace dirty; no automatic snapshot runs.
        let watcher = &mut config.file.watcher;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_hours(1);
        watcher.fallback_interval = Duration::from_hours(2);
    })
    .await;
    let ws = create(&daemon.client, &repo, "owner").await;
    let mut stream = subscribe(&daemon.client, request(&repo)).await;
    let cursor = frame(&mut stream, SECOND).await.cursor;
    std::fs::write(Path::new(&ws.path).join("unsaved.txt"), "edit").unwrap();
    let id = ws.id.parse().unwrap();
    let deadline = Instant::now() + 10 * SECOND;
    while !daemon
        .handle
        .state()
        .manager
        .scheduler()
        .dirty()
        .contains(&id)
    {
        assert!(Instant::now() < deadline, "the workspace is never dirty");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = Instant::now();
    daemon.handle.shutdown();
    let error = ending(&mut stream, 2 * SECOND).await;
    assert!(started.elapsed() < 2 * SECOND);
    assert_eq!(reason(&error), (Code::Unavailable, String::new()));
    let daemon = daemon.restart().await;
    let mut stream = subscribe(
        &daemon.client,
        SubscribeOperationsRequest {
            start: Some(Start::Cursor(cursor)),
            ..request(&repo)
        },
    )
    .await;
    let mut replayed = Vec::new();
    loop {
        let next = frame(&mut stream, 2 * SECOND).await;
        if next.live {
            break;
        }
        replayed.extend(next.operations);
    }
    assert!(
        replayed
            .iter()
            .any(|op| op.kind() == OperationKind::Snapshot
                && op.snapshot_kind() == v1::SnapshotKind::Shutdown),
        "{replayed:?}"
    );
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_are_capped_and_freed() {
    let (daemon, repo) = fixture_with(|config| config.subscribe.max_subscriptions = 3).await;
    let mut streams = Vec::new();
    for _ in 0..3 {
        let mut stream = subscribe(&daemon.client, request(&repo)).await;
        frame(&mut stream, SECOND).await;
        streams.push(stream);
    }
    let error = daemon
        .client
        .subscribe_operations(request(&repo))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "TOO_MANY_SUBSCRIPTIONS".to_owned())
    );
    streams.pop();
    let subscriptions = &daemon.handle.state().subscriptions;
    let deadline = Instant::now() + SECOND;
    while subscriptions.active() != 2 {
        assert!(Instant::now() < deadline, "{}", subscriptions.active());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    subscribe(&daemon.client, request(&repo)).await;
}

/// Every operation `ListOperations` lists from the heads, or from
/// `at_operation` when it is not empty, following page tokens of
/// `page_size`, newest first.
async fn list_all(
    client: &Client,
    repo: &SyntheticRepo,
    page_size: i32,
    at_operation: &str,
) -> Vec<v1::Operation> {
    let mut out = Vec::new();
    let mut page_token = String::new();
    loop {
        let page = client
            .list_operations_page(ListOperationsRequest {
                repo: repo.id.clone(),
                page_size,
                page_token: page_token.clone(),
                at_operation: at_operation.to_owned(),
                ..ListOperationsRequest::default()
            })
            .await
            .unwrap();
        out.extend(page.operations);
        if page.next_page_token.is_empty() {
            return out;
        }
        page_token = page.next_page_token;
    }
}

// R2.21 test 1: the seam the activity feed's window relies on. A stream
// that is live before a list is read, joined to the list by operation id,
// holds every operation of the log exactly once, whatever the list read
// while writes landed. The log is read at the writer's last operation, so
// anything the daemon records after it cannot fail the comparison.
#[tokio::test(flavor = "multi_thread")]
async fn list_and_stream_cover_the_log() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon.client, &repo, "ann").await;
    let b = create(&daemon.client, &repo, "bob").await;
    // Every repository and no start, as the web UI's one stream is.
    let mut stream = subscribe(&daemon.client, SubscribeOperationsRequest::default()).await;
    assert!(frame(&mut stream, SECOND).await.live);
    let writer = {
        let client = daemon.client.clone();
        tokio::spawn(async move {
            let mut last = None;
            for i in 0..200 {
                let ws = if i % 2 == 0 { &a } else { &b };
                last = Some(describe(&client, ws, &format!("change {i}")).await.id);
            }
            last.expect("200 writes")
        })
    };
    let first = list_all(&daemon.client, &repo, 50, "").await;
    let second = list_all(&daemon.client, &repo, 50, "").await;
    let last = writer.await.unwrap();
    let mut streamed: Vec<String> = Vec::new();
    while !streamed.contains(&last) {
        let next = frame(&mut stream, 30 * SECOND).await;
        streamed.extend(next.operations.iter().map(|op| op.id.clone()));
    }
    let unique: BTreeSet<&String> = streamed.iter().collect();
    assert_eq!(
        unique.len(),
        streamed.len(),
        "an operation came twice on the stream"
    );
    let full: BTreeSet<String> = ids(&list_all(&daemon.client, &repo, 1000, &last).await)
        .into_iter()
        .collect();
    let listed: BTreeSet<String> = ids(&first).into_iter().chain(ids(&second)).collect();
    let missing: Vec<&String> = full
        .iter()
        .filter(|id| !unique.contains(id) && !listed.contains(*id))
        .collect();
    assert!(
        missing.is_empty(),
        "neither listed nor streamed: {missing:?}"
    );
    let joined: BTreeSet<String> = streamed.iter().cloned().chain(ids(&first)).collect();
    assert_eq!(
        joined, full,
        "the stream and the first list, by id, are the log"
    );
}
