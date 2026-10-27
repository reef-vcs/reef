//! Presence (R2.10, tests 1 to 16): reports, clears and subscriptions
//! through the daemon, the daemon's own lease, editing and progress
//! entries, delivery, both channels on one connection, and the shared
//! subscription cap.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, GetWorkspaceRequest, PresenceEndReason,
    PresenceKind, ReportPresenceRequest, SubscribeOperationsRequest, SubscribePresenceRequest,
    SubscribePresenceResponse, move_workspace_request,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use tokio::time::Instant;
use tonic::{Code, Streaming};

const SECOND: Duration = Duration::from_secs(1);

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

async fn create(client: &Client, repo: &SyntheticRepo, owner: &str) -> v1::Workspace {
    create_with(client, repo, owner, None).await
}

async fn create_with(
    client: &Client,
    repo: &SyntheticRepo,
    owner: &str,
    lease: Option<Duration>,
) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: owner.to_owned(),
            lease_length: lease.map(|lease| prost_types::Duration {
                seconds: i64::try_from(lease.as_secs()).unwrap(),
                nanos: 0,
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn activity(agent: &str, text: &str) -> ReportPresenceRequest {
    ReportPresenceRequest {
        agent: agent.to_owned(),
        kind: PresenceKind::Activity.into(),
        activity: text.to_owned(),
        ..ReportPresenceRequest::default()
    }
}

async fn report(client: &Client, request: ReportPresenceRequest) -> v1::Presence {
    client
        .report_presence(request)
        .await
        .unwrap()
        .presence
        .unwrap()
}

async fn refused(client: &Client, request: ReportPresenceRequest) -> ClientError {
    client.report_presence(request).await.unwrap_err()
}

fn ttl(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap(),
        nanos: i32::try_from(duration.subsec_nanos()).unwrap(),
    }
}

async fn subscribe(
    client: &Client,
    request: SubscribePresenceRequest,
) -> Streaming<SubscribePresenceResponse> {
    client.subscribe_presence(request).await.unwrap()
}

fn kinds(kinds: &[PresenceKind]) -> SubscribePresenceRequest {
    SubscribePresenceRequest {
        kinds: kinds.iter().map(|kind| i32::from(*kind)).collect(),
        ..SubscribePresenceRequest::default()
    }
}

/// The next frame, within `within`.
async fn frame(
    stream: &mut Streaming<SubscribePresenceResponse>,
    within: Duration,
) -> SubscribePresenceResponse {
    tokio::time::timeout(within, stream.message())
        .await
        .expect("a frame in time")
        .expect("no error")
        .expect("the stream is open")
}

/// The frames that complete the current set, and its events.
async fn set(stream: &mut Streaming<SubscribePresenceResponse>) -> Vec<v1::PresenceEvent> {
    let mut events = Vec::new();
    loop {
        let next = frame(stream, 2 * SECOND).await;
        events.extend(next.events);
        if next.synced {
            return events;
        }
    }
}

/// Events from frames until `done` holds for what arrived, within
/// `within`.
async fn events_until(
    stream: &mut Streaming<SubscribePresenceResponse>,
    within: Duration,
    done: impl Fn(&[v1::PresenceEvent]) -> bool,
) -> Vec<v1::PresenceEvent> {
    let deadline = Instant::now() + within;
    let mut got = Vec::new();
    while !done(&got) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "not done in time: {got:#?}");
        let next = frame(stream, left).await;
        got.extend(next.events);
    }
    got
}

/// The error that ends the stream, within `within`.
async fn ending(
    stream: &mut Streaming<SubscribePresenceResponse>,
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

fn presence(event: &v1::PresenceEvent) -> &v1::Presence {
    event.presence.as_ref().unwrap()
}

fn millis(timestamp: Option<&prost_types::Timestamp>) -> i64 {
    let timestamp = timestamp.unwrap();
    timestamp.seconds * 1000 + i64::from(timestamp.nanos / 1_000_000)
}

fn system_time(timestamp: Option<&prost_types::Timestamp>) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(millis(timestamp)).unwrap())
}

fn ended_with(event: &v1::PresenceEvent, reason: PresenceEndReason) -> bool {
    event.ended && event.end_reason() == reason
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn first_frame_is_the_current_set() {
    let (daemon, _repo) = fixture_with(|config| config.presence.frame_events = 2).await;
    let client = &daemon.client;
    let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
    let first = frame(&mut stream, SECOND).await;
    assert_eq!(first.events.len(), 0);
    assert!(first.synced);
    let mut ids = Vec::new();
    for n in 0..3 {
        ids.push(report(client, activity("bot", &n.to_string())).await.id);
    }
    ids.sort();
    let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
    let first = frame(&mut stream, SECOND).await;
    let second = frame(&mut stream, SECOND).await;
    assert_eq!((first.events.len(), first.synced), (2, false));
    assert_eq!((second.events.len(), second.synced), (1, true));
    let got: Vec<String> = first
        .events
        .iter()
        .chain(&second.events)
        .inspect(|event| assert!(!event.ended))
        .map(|event| presence(event).id.clone())
        .collect();
    assert_eq!(got, ids);
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn report_replace_clear() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
    set(&mut stream).await;
    let before = SystemTime::now();
    let reported = report(client, activity("bot", "running tests")).await;
    assert_eq!(reported.id.len(), 16);
    assert!(reported.id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!reported.by_daemon);
    let updated = millis(reported.updated_at.as_ref());
    assert_eq!(millis(reported.expires_at.as_ref()) - updated, 30_000);
    assert!(system_time(reported.updated_at.as_ref()) + SECOND >= before);
    let seen = events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    assert_eq!(presence(&seen[0]), &reported);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let replaced = report(
        client,
        ReportPresenceRequest {
            presence_id: reported.id.clone(),
            ..activity("bot", "linting")
        },
    )
    .await;
    assert_eq!(replaced.id, reported.id);
    let seen = events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(presence(&seen[0]).activity, "linting");
    client.clear_presence(&reported.id).await.unwrap();
    let seen = events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    assert!(ended_with(&seen[0], PresenceEndReason::Cleared));
    assert_eq!(presence(&seen[0]).activity, "linting");
    client.clear_presence(&reported.id).await.unwrap();
    let error = refused(
        client,
        ReportPresenceRequest {
            presence_id: reported.id.clone(),
            ..activity("bot", "again")
        },
    )
    .await;
    assert_eq!(
        reason(&error),
        (Code::NotFound, "PRESENCE_NOT_FOUND".to_owned())
    );
    assert_eq!(metadata(&error, "presence_id"), reported.id);
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn ttl_expires_and_clamps() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
    set(&mut stream).await;
    let started = Instant::now();
    let short = report(
        client,
        ReportPresenceRequest {
            ttl: Some(ttl(SECOND)),
            ..activity("bot", "short")
        },
    )
    .await;
    let seen = events_until(&mut stream, 3 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    let elapsed = started.elapsed();
    let end = seen.iter().find(|event| event.ended).unwrap();
    assert_eq!(presence(end).id, short.id);
    assert!(ended_with(end, PresenceEndReason::Expired));
    assert!(elapsed >= SECOND, "{elapsed:?}");
    assert!(elapsed < 2 * SECOND, "{elapsed:?}");
    let span = |presence: &v1::Presence| {
        millis(presence.expires_at.as_ref()) - millis(presence.updated_at.as_ref())
    };
    assert_eq!(
        span(&report(client, activity("bot", "unset")).await),
        30_000
    );
    let long = ReportPresenceRequest {
        ttl: Some(ttl(Duration::from_mins(20))),
        ..activity("bot", "long")
    };
    assert_eq!(span(&report(client, long).await), 600_000);
    let tiny = ReportPresenceRequest {
        ttl: Some(ttl(Duration::from_millis(100))),
        ..activity("bot", "tiny")
    };
    assert_eq!(span(&report(client, tiny).await), 1_000);
    let negative = ReportPresenceRequest {
        ttl: Some(prost_types::Duration {
            seconds: -1,
            nanos: 0,
        }),
        ..activity("bot", "negative")
    };
    let error = refused(client, negative).await;
    assert_eq!(reason(&error), (Code::InvalidArgument, String::new()));
}

// 4
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one case per refusal")]
async fn reports_are_validated() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let expect = |error: ClientError, code: Code, why: &str| {
        assert_eq!(reason(&error), (code, why.to_owned()), "{error:?}");
        error
    };
    expect(
        refused(client, activity("", "x")).await,
        Code::InvalidArgument,
        "INVALID_AGENT",
    );
    let unspecified = ReportPresenceRequest {
        kind: 0,
        ..activity("bot", "")
    };
    expect(
        refused(client, unspecified).await,
        Code::InvalidArgument,
        "",
    );
    let paths_on_activity = ReportPresenceRequest {
        paths: vec!["a".to_owned()],
        ..activity("bot", "x")
    };
    expect(
        refused(client, paths_on_activity).await,
        Code::InvalidArgument,
        "",
    );
    let lease = ReportPresenceRequest {
        kind: PresenceKind::Lease.into(),
        ..activity("bot", "")
    };
    let error = expect(
        refused(client, lease).await,
        Code::InvalidArgument,
        "DAEMON_PRESENCE",
    );
    assert_eq!(metadata(&error, "kind"), "PRESENCE_KIND_LEASE");
    let long = activity("bot", &"x".repeat(1_025));
    let error = expect(
        refused(client, long).await,
        Code::InvalidArgument,
        "PRESENCE_TOO_LARGE",
    );
    assert_eq!(metadata(&error, "field"), "activity");
    let editing = |paths: Vec<String>| ReportPresenceRequest {
        kind: PresenceKind::Editing.into(),
        paths,
        ..activity("bot", "")
    };
    let error = expect(
        refused(client, editing((0..65).map(|n| n.to_string()).collect())).await,
        Code::InvalidArgument,
        "PRESENCE_TOO_LARGE",
    );
    assert_eq!(metadata(&error, "field"), "paths");
    let error = expect(
        refused(client, editing(vec!["../x".to_owned()])).await,
        Code::InvalidArgument,
        "INVALID_PATH",
    );
    assert_eq!(metadata(&error, "path"), "../x");
    let ws = create(client, &repo, "owner").await;
    let prefix = ReportPresenceRequest {
        workspace: ws.id.clone(),
        change: ws.change_id[..8].to_owned(),
        ..activity("bot", "x")
    };
    expect(
        refused(client, prefix).await,
        Code::InvalidArgument,
        "INVALID_CHANGE_ID",
    );
    let unanchored = ReportPresenceRequest {
        change: ws.change_id.clone(),
        ..activity("bot", "x")
    };
    expect(refused(client, unanchored).await, Code::InvalidArgument, "");
    let unknown = ReportPresenceRequest {
        repo: "0123456789abcdef".to_owned(),
        ..activity("bot", "x")
    };
    expect(
        refused(client, unknown).await,
        Code::NotFound,
        "REPOSITORY_NOT_FOUND",
    );
    let in_ws = report(
        client,
        ReportPresenceRequest {
            workspace: ws.id.clone(),
            change: ws.change_id.clone(),
            ..activity("bot", "x")
        },
    )
    .await;
    assert_eq!(in_ws.repo_id, repo.id);
    assert_eq!(in_ws.workspace_id, ws.id);
    assert_eq!(in_ws.change_id, ws.change_id);
    let other_kind = ReportPresenceRequest {
        presence_id: in_ws.id.clone(),
        workspace: ws.id.clone(),
        kind: PresenceKind::Editing.into(),
        paths: vec!["a".to_owned()],
        ..activity("bot", "")
    };
    let error = expect(
        refused(client, other_kind).await,
        Code::InvalidArgument,
        "PRESENCE_MISMATCH",
    );
    assert_eq!(metadata(&error, "field"), "kind");
    let malformed = ReportPresenceRequest {
        presence_id: "xyz".to_owned(),
        ..activity("bot", "x")
    };
    expect(
        refused(client, malformed).await,
        Code::InvalidArgument,
        "INVALID_PRESENCE_ID",
    );
    let lease_entry = daemon
        .handle
        .state()
        .presence
        .entries()
        .into_iter()
        .find(|entry| entry.kind() == PresenceKind::Lease)
        .expect("the create's lease entry");
    let error = expect(
        refused(
            client,
            ReportPresenceRequest {
                presence_id: lease_entry.id.clone(),
                ..activity(&lease_entry.agent, "")
            },
        )
        .await,
        Code::InvalidArgument,
        "DAEMON_PRESENCE",
    );
    assert_eq!(metadata(&error, "presence_id"), lease_entry.id);
    let error = client.clear_presence(&lease_entry.id).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::InvalidArgument, "DAEMON_PRESENCE".to_owned())
    );
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let released = ReportPresenceRequest {
        workspace: ws.id.clone(),
        ..activity("bot", "x")
    };
    expect(
        refused(client, released).await,
        Code::FailedPrecondition,
        "",
    );
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn caps() {
    let (daemon, _repo) = fixture_with(|config| {
        config.presence.max_entries = 5;
        config.presence.max_entries_per_agent = 3;
    })
    .await;
    let client = &daemon.client;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(report(client, activity("a", "x")).await.id);
    }
    let error = refused(client, activity("a", "x")).await;
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "TOO_MANY_PRESENCE_ENTRIES".to_owned())
    );
    assert_eq!(metadata(&error, "agent"), "a");
    assert_eq!(metadata(&error, "limit"), "3");
    report(client, activity("b", "x")).await;
    report(client, activity("c", "x")).await;
    let error = refused(client, activity("d", "x")).await;
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "TOO_MANY_PRESENCE_ENTRIES".to_owned())
    );
    assert_eq!(metadata(&error, "agent"), "");
    assert_eq!(metadata(&error, "limit"), "5");
    client.clear_presence(&held[0]).await.unwrap();
    report(client, activity("d", "x")).await;
}

// 6
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one case per filter field")]
async fn filters() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let second = daemon.tmp.path().join("second");
    let second_repo = {
        let at = second.clone();
        tokio::task::spawn_blocking(move || {
            reef_daemon::testing::init_synthetic_repo(&at, Preset::Small);
        })
        .await
        .unwrap();
        client
            .register_repository(&second, v1::IfNotJj::Fail)
            .await
            .unwrap()
            .repository
            .unwrap()
            .id
    };
    let ws1 = create(client, &repo, "o1").await;
    let ws2 = create(client, &repo, "o2").await;
    // Only client entries: leave the daemon's lease entries out.
    let base = || SubscribePresenceRequest {
        kinds: vec![PresenceKind::Activity.into(), PresenceKind::Editing.into()],
        ..SubscribePresenceRequest::default()
    };
    let entries = [
        ("a", "", ws1.id.as_str(), PresenceKind::Activity),
        ("b", "", ws2.id.as_str(), PresenceKind::Editing),
        ("a", second_repo.as_str(), "", PresenceKind::Editing),
        ("b", repo.id.as_str(), "", PresenceKind::Activity),
    ];
    let request =
        |(agent, repo, workspace, kind): (&str, &str, &str, PresenceKind)| ReportPresenceRequest {
            agent: agent.to_owned(),
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            kind: kind.into(),
            activity: if kind == PresenceKind::Activity {
                format!("{agent}{repo}{workspace}")
            } else {
                String::new()
            },
            paths: if kind == PresenceKind::Editing {
                vec![format!("{agent}.txt")]
            } else {
                Vec::new()
            },
            ..ReportPresenceRequest::default()
        };
    let mut ids = Vec::new();
    for entry in entries {
        ids.push(report(client, request(entry)).await.id);
    }
    let cases: Vec<(SubscribePresenceRequest, Vec<usize>)> = vec![
        (base(), vec![0, 1, 2, 3]),
        (
            SubscribePresenceRequest {
                repos: vec![repo.id.clone()],
                ..base()
            },
            vec![0, 1, 3],
        ),
        (
            SubscribePresenceRequest {
                workspaces: vec![ws2.id.clone()],
                ..base()
            },
            vec![1],
        ),
        (
            SubscribePresenceRequest {
                agents: vec!["a".to_owned()],
                ..base()
            },
            vec![0, 2],
        ),
        (kinds(&[PresenceKind::Editing]), vec![1, 2]),
        (
            SubscribePresenceRequest {
                agents: vec!["b".to_owned()],
                repos: vec![repo.id.clone()],
                ..kinds(&[PresenceKind::Activity])
            },
            vec![3],
        ),
    ];
    for (filter, expected) in cases {
        let mut stream = subscribe(client, filter.clone()).await;
        let got: HashSet<String> = set(&mut stream)
            .await
            .iter()
            .map(|event| presence(event).id.clone())
            .collect();
        let want: HashSet<String> = expected.iter().map(|n| ids[*n].clone()).collect();
        assert_eq!(got, want, "{filter:?}");
        // Later changes too: replacing every entry reaches exactly these.
        for (n, entry) in entries.into_iter().enumerate() {
            report(
                client,
                ReportPresenceRequest {
                    presence_id: ids[n].clone(),
                    ..request(entry)
                },
            )
            .await;
        }
        let changed = events_until(&mut stream, 2 * SECOND, |got| {
            want.iter()
                .all(|id| got.iter().any(|event| &presence(event).id == id))
        })
        .await;
        for event in changed {
            assert!(want.contains(&presence(&event).id), "{filter:?}: {event:?}");
        }
    }
    let error = client
        .subscribe_presence(SubscribePresenceRequest {
            workspaces: vec!["ffffffffffffffff".to_owned()],
            ..SubscribePresenceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::NotFound, "WORKSPACE_NOT_FOUND".to_owned())
    );
    let error = client
        .subscribe_presence(SubscribePresenceRequest {
            kinds: vec![99],
            ..SubscribePresenceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), (Code::InvalidArgument, String::new()));
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn coalescing() {
    let (daemon, _repo) = fixture_with(|config| {
        config.presence.frame_interval = Duration::from_millis(200);
        config.presence.publish_interval = Duration::from_millis(200);
    })
    .await;
    let client = &daemon.client;
    let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
    set(&mut stream).await;
    let first = report(client, activity("bot", "0")).await;
    events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    for n in 1..=50 {
        report(
            client,
            ReportPresenceRequest {
                presence_id: first.id.clone(),
                ..activity("bot", &n.to_string())
            },
        )
        .await;
    }
    let burst = events_until(&mut stream, 2 * SECOND, |got| {
        got.last()
            .is_some_and(|event| presence(event).activity == "50")
    })
    .await;
    assert!(
        burst.len() <= 4,
        "{} events for 50 replacements",
        burst.len()
    );
    // A brief entry the subscriber was never sent never reaches it.
    let brief = report(client, activity("brief", "x")).await;
    client.clear_presence(&brief.id).await.unwrap();
    tokio::time::sleep(SECOND).await;
    let started = Instant::now();
    report(
        client,
        ReportPresenceRequest {
            presence_id: first.id.clone(),
            ..activity("bot", "quiet")
        },
    )
    .await;
    let quiet = events_until(&mut stream, SECOND, |got| {
        got.iter().any(|event| presence(event).activity == "quiet")
    })
    .await;
    // Not held for the 200 ms frame interval: the last frame was a second
    // ago.
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(
        quiet.iter().all(|event| presence(event).id != brief.id),
        "{quiet:?}"
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn slow_subscriber_converges() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    let mut slow = subscribe(client, kinds(&[PresenceKind::Activity])).await;
    set(&mut slow).await;
    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(report(client, activity(&format!("bot-{n}"), "0")).await.id);
    }
    for round in 1..=100 {
        for (n, id) in ids.iter().enumerate() {
            report(
                client,
                ReportPresenceRequest {
                    presence_id: id.clone(),
                    ..activity(&format!("bot-{n}"), &round.to_string())
                },
            )
            .await;
        }
    }
    for id in &ids[..5] {
        client.clear_presence(id).await.unwrap();
    }
    // Now read: everything converges on the current state.
    let mut state: BTreeMap<String, v1::PresenceEvent> = BTreeMap::new();
    let deadline = Instant::now() + 10 * SECOND;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "never converged: {state:#?}");
        let next = frame(&mut slow, left).await;
        let mut seen = HashSet::new();
        for event in next.events {
            let id = presence(&event).id.clone();
            assert!(seen.insert(id.clone()), "{id} twice in a frame");
            state.insert(id, event);
        }
        let converged = ids[..5]
            .iter()
            .all(|id| state.get(id).is_some_and(|event| event.ended))
            && ids[5..].iter().all(|id| {
                state
                    .get(id)
                    .is_some_and(|event| !event.ended && presence(event).activity == "100")
            });
        if converged {
            break;
        }
    }
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn lease_heartbeats() {
    let (daemon, repo) = fixture_with(|config| config.file.lease.heartbeat = SECOND).await;
    let client = &daemon.client;
    let mut stream = subscribe(client, kinds(&[PresenceKind::Lease])).await;
    set(&mut stream).await;
    let ws = create_with(client, &repo, "holder", Some(10 * SECOND)).await;
    let lease_id = ws.lease.as_ref().unwrap().id.clone();
    let seen = events_until(&mut stream, 2 * SECOND, |got| !got.is_empty()).await;
    let first = presence(&seen[0]).clone();
    assert!(first.by_daemon);
    assert_eq!(first.agent, "holder");
    assert_eq!(first.workspace_id, ws.id);
    assert_eq!(first.repo_id, repo.id);
    assert_eq!(first.lease.as_ref().unwrap().id, lease_id);
    assert_eq!(
        millis(first.expires_at.as_ref()) - millis(first.updated_at.as_ref()),
        2_000
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.renew_lease("", &ws.id, &lease_id).await.unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| !got.is_empty()).await;
    let renewed = presence(&seen[0]);
    assert_eq!(renewed.id, first.id);
    assert!(
        millis(renewed.lease.as_ref().unwrap().expires_at.as_ref())
            > millis(first.lease.as_ref().unwrap().expires_at.as_ref())
    );
    let started = Instant::now();
    let seen = events_until(&mut stream, 3 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    assert!(started.elapsed() <= Duration::from_millis(2_500));
    assert!(ended_with(
        seen.iter().find(|e| e.ended).unwrap(),
        PresenceEndReason::Expired
    ));
    let inspected = client
        .get_workspace(GetWorkspaceRequest {
            workspace: ws.id.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(
        inspected.lease.unwrap().state(),
        v1::LeaseState::Active,
        "the lease outlives its heartbeat entry"
    );
    client.renew_lease("", &ws.id, &lease_id).await.unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| !got.is_empty()).await;
    let again = presence(&seen[0]).id.clone();
    assert_ne!(again, first.id, "a new entry after the lapse");
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    let end = seen.iter().find(|e| e.ended).unwrap();
    assert_eq!(presence(end).id, again);
    assert!(ended_with(end, PresenceEndReason::Gone));
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn editing_follows_snapshots() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_millis(50);
        config.presence.editing_ttl = 2 * SECOND;
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo, "editor").await;
    let root = Path::new(&ws.path);
    std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
    let mut stream = subscribe(
        client,
        SubscribePresenceRequest {
            workspaces: vec![ws.id.clone()],
            ..kinds(&[PresenceKind::Editing])
        },
    )
    .await;
    set(&mut stream).await;
    let paths_of = |event: &v1::PresenceEvent| presence(event).paths.clone();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    let seen = events_until(&mut stream, 5 * SECOND, |got| {
        got.iter().any(|event| {
            let paths = paths_of(event);
            paths.contains(&"a.txt".to_owned()) && paths.contains(&"b.txt".to_owned())
        })
    })
    .await;
    let entry = presence(seen.last().unwrap()).clone();
    assert!(entry.by_daemon);
    assert_eq!(entry.agent, "editor");
    assert_eq!(entry.workspace_id, ws.id);
    assert_eq!(entry.repo_id, repo.id);
    assert_eq!(entry.change_id.len(), 32);
    std::fs::write(root.join("ignored.log"), "noise").unwrap();
    std::fs::write(root.join("c.txt"), "c").unwrap();
    let seen = events_until(&mut stream, 5 * SECOND, |got| {
        got.iter()
            .any(|event| paths_of(event).first().map(String::as_str) == Some("c.txt"))
    })
    .await;
    for event in &seen {
        assert!(
            !paths_of(event).contains(&"ignored.log".to_owned()),
            "{event:?}"
        );
    }
    std::fs::write(root.join("d.txt"), "d").unwrap();
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
    events_until(&mut stream, 2 * SECOND, |got| {
        got.iter()
            .any(|event| paths_of(event).first().map(String::as_str) == Some("d.txt"))
    })
    .await;
    let seen = events_until(&mut stream, 4 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    assert!(ended_with(
        seen.iter().find(|e| e.ended).unwrap(),
        PresenceEndReason::Expired
    ));
    std::fs::write(root.join("e.txt"), "e").unwrap();
    events_until(&mut stream, 5 * SECOND, |got| !got.is_empty()).await;
    client
        .move_workspace(v1::MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(move_workspace_request::Target::NewOn(ws.commit_id.clone())),
            ..v1::MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    assert!(ended_with(
        seen.iter().find(|e| e.ended).unwrap(),
        PresenceEndReason::Cleared
    ));
    std::fs::write(root.join("f.txt"), "f").unwrap();
    events_until(&mut stream, 5 * SECOND, |got| !got.is_empty()).await;
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    assert!(ended_with(
        seen.iter().find(|e| e.ended).unwrap(),
        PresenceEndReason::Gone
    ));
}

const CREATE_PHASES: [&str; 8] = [
    "resolve",
    "base",
    "materialize",
    "mount",
    "jj",
    "seed",
    "insert",
    "watch",
];

/// Whether `seen` is a subsequence of `all`.
fn subsequence(seen: &[String], all: &[&str]) -> bool {
    let mut rest = all.iter();
    seen.iter()
        .all(|phase| rest.any(|candidate| candidate == phase))
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn create_progress() {
    let (daemon, repo) =
        fixture_with(|config| config.presence.progress_after = Duration::ZERO).await;
    let client = &daemon.client;
    let mut stream = subscribe(client, kinds(&[PresenceKind::Progress])).await;
    set(&mut stream).await;
    let ws = create(client, &repo, "creator").await;
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    let mut phases = Vec::new();
    for event in &seen {
        let entry = presence(event);
        assert!(entry.by_daemon);
        assert_eq!(entry.repo_id, repo.id);
        assert_eq!(entry.workspace_id, ws.id);
        assert_eq!(entry.agent, "creator");
        let progress = entry.progress.as_ref().unwrap();
        assert_eq!((progress.total, progress.unit.as_str()), (8, "steps"));
        if !event.ended && phases.last() != Some(&progress.phase) {
            phases.push(progress.phase.clone());
        }
    }
    assert!(subsequence(&phases, &CREATE_PHASES), "{phases:?}");
    assert!(ended_with(seen.last().unwrap(), PresenceEndReason::Cleared));
    let failed = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "creator".to_owned(),
            start: Some(v1::create_workspace_request::Start::NewOn(
                "no-such-revision".to_owned(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await;
    assert!(failed.is_err());
    // Its entry ended with it; a subscriber is not sent an entry that
    // ended before it could be sent.
    let left = daemon
        .handle
        .state()
        .presence
        .entries()
        .into_iter()
        .filter(|entry| entry.kind() == PresenceKind::Progress)
        .count();
    assert_eq!(left, 0);
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn presence_delivery_latency() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    for subscribers in [1, 10] {
        let mut streams = Vec::new();
        for _ in 0..subscribers {
            let mut stream = subscribe(client, SubscribePresenceRequest::default()).await;
            set(&mut stream).await;
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
                        got.extend(next.events.into_iter().map(|event| (event, at)));
                    }
                    got
                })
            })
            .collect();
        let mut sent = Vec::new();
        let mut baselines = Vec::new();
        for n in 0..40 {
            let agent = format!("bot-{subscribers}-{n}");
            sent.push(report(client, activity(&agent, "x")).await.id);
            let before = std::time::Instant::now();
            client.daemon_status(false).await.unwrap();
            baselines.push(before.elapsed());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        baselines.sort();
        let baseline = baselines[baselines.len() / 2];
        for receiver in receivers {
            let got = receiver.await.unwrap();
            assert_eq!(
                got.iter()
                    .map(|(event, _)| presence(event).id.clone())
                    .collect::<Vec<_>>(),
                sent
            );
            let mut latencies: Vec<Duration> = got
                .iter()
                .map(|(event, at)| {
                    at.duration_since(system_time(presence(event).updated_at.as_ref()))
                        .unwrap_or_default()
                })
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
        for id in sent {
            client.clear_presence(&id).await.unwrap();
        }
    }
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn both_channels_on_one_connection() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "owner").await;
    let mut operations = client
        .subscribe_operations(SubscribeOperationsRequest {
            repos: vec![repo.id.clone()],
            ..SubscribeOperationsRequest::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(SECOND, operations.message())
        .await
        .unwrap()
        .unwrap();
    let mut presence_stream = subscribe(client, kinds(&[PresenceKind::Activity])).await;
    set(&mut presence_stream).await;
    let describe = |n: usize| DescribeChangeRequest {
        workspace: ws.id.clone(),
        revision: "@".to_owned(),
        description: format!("d{n}"),
        ..DescribeChangeRequest::default()
    };
    let mut reported = 0;
    let mut described = 0;
    for n in 0..20 {
        client.describe_change(describe(n)).await.unwrap();
        report(client, activity(&format!("bot-{n}"), "x")).await;
        let got = events_until(&mut presence_stream, 2 * SECOND, |got| !got.is_empty()).await;
        reported += got.len();
        while described <= n {
            let next = tokio::time::timeout(2 * SECOND, operations.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            described += next.operations.len();
        }
    }
    assert_eq!(reported, 20);
    // The operation stream left unread: presence still flows.
    for n in 0..200 {
        client.describe_change(describe(100 + n)).await.unwrap();
    }
    for n in 0..20 {
        let started = Instant::now();
        let mut request = activity(&format!("late-{n}"), "x");
        request.ttl = Some(ttl(60 * SECOND));
        report(client, request).await;
        events_until(&mut presence_stream, 2 * SECOND, |got| !got.is_empty()).await;
        assert!(started.elapsed() < 2 * SECOND);
    }
    drop(operations);
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn ends_with_workspace_and_repository() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "owner").await;
    let mut stream = subscribe(
        client,
        SubscribePresenceRequest {
            repos: vec![repo.id.clone()],
            ..kinds(&[PresenceKind::Activity])
        },
    )
    .await;
    set(&mut stream).await;
    let in_ws = report(
        client,
        ReportPresenceRequest {
            workspace: ws.id.clone(),
            ..activity("bot", "in ws")
        },
    )
    .await;
    events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    let end = seen.iter().find(|e| e.ended).unwrap();
    assert_eq!(presence(end).id, in_ws.id);
    assert!(ended_with(end, PresenceEndReason::Gone));
    let error = refused(
        client,
        ReportPresenceRequest {
            workspace: ws.id.clone(),
            ..activity("bot", "late")
        },
    )
    .await;
    assert_eq!(reason(&error).0, Code::FailedPrecondition);
    let in_repo = report(
        client,
        ReportPresenceRequest {
            repo: repo.id.clone(),
            ..activity("bot", "in repo")
        },
    )
    .await;
    events_until(&mut stream, SECOND, |got| !got.is_empty()).await;
    client.forget_repository(&repo.id).await.unwrap();
    let seen = events_until(&mut stream, 2 * SECOND, |got| got.iter().any(|e| e.ended)).await;
    let end = seen.iter().find(|e| e.ended).unwrap();
    assert_eq!(presence(end).id, in_repo.id);
    assert!(ended_with(end, PresenceEndReason::Gone));
    // The stream stays open: a keepalive or nothing, never an error.
    let open = tokio::time::timeout(Duration::from_millis(300), stream.message()).await;
    assert!(open.is_err() || open.unwrap().is_ok());
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_and_restart() {
    let (daemon, _repo) = fixture().await;
    let held = report(&daemon.client, activity("bot", "x")).await;
    let mut stream = subscribe(&daemon.client, SubscribePresenceRequest::default()).await;
    assert_eq!(set(&mut stream).await.len(), 1);
    let started = Instant::now();
    daemon.handle.shutdown();
    let error = ending(&mut stream, 2 * SECOND).await;
    assert!(started.elapsed() < 2 * SECOND);
    assert_eq!(reason(&error), (Code::Unavailable, String::new()));
    let daemon = daemon.restart().await;
    let mut stream = subscribe(&daemon.client, kinds(&[PresenceKind::Activity])).await;
    let first = frame(&mut stream, SECOND).await;
    assert!(first.synced);
    assert_eq!(first.events.len(), 0, "nothing survives a restart");
    let error = refused(
        &daemon.client,
        ReportPresenceRequest {
            presence_id: held.id.clone(),
            ..activity("bot", "x")
        },
    )
    .await;
    assert_eq!(
        reason(&error),
        (Code::NotFound, "PRESENCE_NOT_FOUND".to_owned())
    );
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_share_the_cap() {
    let (daemon, repo) = fixture_with(|config| config.subscribe.max_subscriptions = 3).await;
    let client = &daemon.client;
    let operations = SubscribeOperationsRequest {
        repos: vec![repo.id.clone()],
        ..SubscribeOperationsRequest::default()
    };
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut stream = client
            .subscribe_operations(operations.clone())
            .await
            .unwrap();
        tokio::time::timeout(SECOND, stream.message())
            .await
            .unwrap()
            .unwrap();
        held.push(stream);
    }
    let mut presence_stream = subscribe(client, SubscribePresenceRequest::default()).await;
    set(&mut presence_stream).await;
    let error = client
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "TOO_MANY_SUBSCRIPTIONS".to_owned())
    );
    let error = client
        .subscribe_operations(operations.clone())
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "TOO_MANY_SUBSCRIPTIONS".to_owned())
    );
    drop(presence_stream);
    let subscriptions = &daemon.handle.state().subscriptions;
    let deadline = Instant::now() + SECOND;
    while subscriptions.active() != 2 {
        assert!(Instant::now() < deadline, "{}", subscriptions.active());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    subscribe(client, SubscribePresenceRequest::default()).await;
}
