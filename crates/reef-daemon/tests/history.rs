//! `reef.api.v1.HistoryService` through the daemon (R2.16, tests 6 to 20):
//! following the operation log, the direct-write rule, outcomes and
//! landing, symbol and path touches, the throttle, paging and grouping,
//! backfill, GC's rewrite of operation ids, other jj writers, the disabled
//! index, invalid queries, and the file's lifecycle.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, AbandonChangeRequest, ChangeDimension, ChangeOutcome, CreateWorkspaceRequest,
    DescribeChangeRequest, EditFilesRequest, FileEdit, HistoryChange, IfNotJj, QueryChangesRequest,
    QueryChangesResponse, SymbolChangeKind, TouchState, UndoOperationRequest, edit_files_request,
    file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::TestDaemon;
use reef_testing::git::init_repo;

const PARSER: &str = "class Parser:\n    def parse(self):\n        return 1\n";
const PARSER_2: &str = "class Parser:\n    def parse(self):\n        return 2\n\n    def extra(self):\n        return 3\n";

struct Fixture {
    daemon: TestDaemon,
    repo: String,
    root: PathBuf,
}

async fn start(configure: impl FnOnce(&mut reef_daemon::config::Config)) -> Fixture {
    let daemon = TestDaemon::start_with(|config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await;
    // Touches are computed at once in tests.
    daemon
        .handle
        .state()
        .history
        .set_throttle(Duration::ZERO, Duration::ZERO);
    let root = daemon.tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    init_repo(&root, &[("README.md", "# fixture\n")]);
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    Fixture {
        repo: registered.repository.unwrap().id,
        root: root.canonicalize().unwrap(),
        daemon,
    }
}

impl Fixture {
    fn client(&self) -> &Client {
        &self.daemon.client
    }

    fn as_agent(&self, agent: &str) -> Client {
        self.daemon.client.clone().with_agent(agent).unwrap()
    }

    async fn workspace(&self, owner: &str) -> v1::Workspace {
        self.client()
            .create_workspace(CreateWorkspaceRequest {
                repo: self.repo.clone(),
                owner: owner.into(),
                start: Some(v1::create_workspace_request::Start::NewOn("main".into())),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .workspace
            .unwrap()
    }

    async fn write(&self, ws: &v1::Workspace, path: &str, contents: &str) -> String {
        let file = Path::new(&ws.path).join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
        self.client()
            .snapshot_workspace(&self.repo, &ws.id, "")
            .await
            .unwrap()
            .commit_id
    }

    async fn query(
        &self,
        request: QueryChangesRequest,
    ) -> Result<QueryChangesResponse, ClientError> {
        self.client()
            .query_changes(QueryChangesRequest {
                repo: self.repo.clone(),
                ..request
            })
            .await
    }

    /// Waits up to 20 s for `check` to hold of the answer to `request`.
    async fn wait_for(
        &self,
        request: QueryChangesRequest,
        check: impl Fn(&QueryChangesResponse) -> bool,
    ) -> QueryChangesResponse {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let answer = self.query(request.clone()).await.unwrap();
            if check(&answer) {
                return answer;
            }
            assert!(Instant::now() < deadline, "never held: {answer:#?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The change `change_id` once `check` holds of it.
    async fn change_when(
        &self,
        change_id: &str,
        check: impl Fn(&HistoryChange) -> bool,
    ) -> HistoryChange {
        let request = QueryChangesRequest {
            changes: vec![change_id.to_owned()],
            include_empty: true,
            ..QueryChangesRequest::default()
        };
        let answer = self
            .wait_for(request, |a| a.changes.first().is_some_and(&check))
            .await;
        answer.changes[0].clone()
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

fn reason(error: &ClientError) -> Option<String> {
    Some(error.reason().to_owned()).filter(|r| !r.is_empty())
}

fn field(error: &ClientError) -> Option<String> {
    error.metadata().get("field").cloned()
}

/// Stops `daemon`, runs `between` with no daemon running, then starts it
/// again with `configure`d configuration.
async fn restart(
    daemon: TestDaemon,
    configure: impl FnOnce(&mut reef_daemon::config::Config),
    between: impl FnOnce(),
) -> TestDaemon {
    let mut stopped = daemon.stop_keeping().await;
    configure(&mut stopped.config);
    between();
    let daemon = stopped.start().await;
    daemon
        .handle
        .state()
        .history
        .set_throttle(Duration::ZERO, Duration::ZERO);
    daemon
}

/// Test 6: activity per agent and workspace, and the direct-write rule.
#[tokio::test(flavor = "multi_thread")]
async fn follows_operations() {
    let fx = start(|_| {}).await;
    let ws1 = fx.workspace("bot-1").await;
    let ws2 = fx.workspace("bot-2").await;
    fx.write(&ws1, "a.txt", "one\n").await;
    fx.write(&ws1, "a.txt", "two\n").await;
    fx.write(&ws2, "b.txt", "three\n").await;

    let one = fx
        .change_when(&ws1.change_id, |c| {
            c.activity.iter().any(|a| a.operations >= 3)
        })
        .await;
    assert_eq!(one.activity.len(), 1, "{one:#?}");
    let activity = &one.activity[0];
    assert_eq!(activity.agent, "bot-1");
    assert_eq!(activity.workspace_id, ws1.id);
    // Its create and two snapshots.
    assert_eq!(activity.operations, 3);
    assert!(activity.first_at.unwrap().seconds <= activity.last_at.unwrap().seconds);
    let two = fx
        .change_when(&ws2.change_id, |c| {
            !c.activity.is_empty() && c.activity[0].operations >= 2
        })
        .await;
    assert_eq!(two.activity[0].agent, "bot-2");

    // A describe by a third agent credits it with the change it named,
    // not with the descendant the describe rebased.
    let parent = fx
        .client()
        .edit_files(EditFilesRequest {
            repo: fx.repo.clone(),
            target: Some(edit_files_request::Target::NewOn("main".into())),
            edits: vec![FileEdit {
                path: "p.txt".into(),
                action: Some(file_edit::Action::Content(b"parent\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    let child = fx
        .client()
        .edit_files(EditFilesRequest {
            repo: fx.repo.clone(),
            target: Some(edit_files_request::Target::NewOn(parent.commit_id.clone())),
            edits: vec![FileEdit {
                path: "c.txt".into(),
                action: Some(file_edit::Action::Content(b"child\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    fx.as_agent("bot-3")
        .describe_change(DescribeChangeRequest {
            repo: fx.repo.clone(),
            revision: parent.commit_id.clone(),
            description: "the parent".into(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let parent_row = fx
        .change_when(&parent.change_id, |c| {
            c.activity.iter().any(|a| a.agent == "bot-3")
        })
        .await;
    assert_eq!(parent_row.subject, "the parent");
    let child_row = fx
        .change_when(&child.change_id, |c| c.commit_id != child.commit_id)
        .await;
    assert!(
        child_row.activity.iter().all(|a| a.agent != "bot-3"),
        "a rebase adds no activity: {child_row:#?}"
    );
}

/// Test 7: open, landed, abandoned by a rewind, abandoned and back, undone.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one fixture, every outcome")]
async fn outcomes() {
    let fx = start(|_| {}).await;
    let ws = fx.workspace("bot-1").await;
    let commit = fx.write(&ws, "a.txt", "one\n").await;
    let row = fx
        .change_when(&ws.change_id, |c| !c.activity.is_empty())
        .await;
    assert_eq!(row.outcome(), ChangeOutcome::Open);

    // Landing: main moves onto the change (git, then the import a create
    // runs), and back.
    let main_before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "refs/heads/main"])
            .current_dir(&fx.root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    fx.git(&["update-ref", "refs/heads/main", &commit]);
    let _trigger = fx.workspace("importer").await;
    let landed = fx
        .change_when(&ws.change_id, |c| c.outcome() == ChangeOutcome::Landed)
        .await;
    assert!(landed.landed_at.is_some(), "seen landing: {landed:#?}");
    // Moving main back has jj's import abandon what only the old target
    // reached, the change included (descendants are rebased): the outcome
    // follows the repository.
    fx.git(&["update-ref", "refs/heads/main", &main_before]);
    let _trigger = fx.workspace("importer").await;
    let rewound = fx
        .change_when(&ws.change_id, |c| c.outcome() != ChangeOutcome::Landed)
        .await;
    assert_eq!(rewound.outcome(), ChangeOutcome::Abandoned, "{rewound:#?}");
    assert!(rewound.landed_at.is_none());

    // Abandoned, then the abandon undone.
    let edit = fx
        .client()
        .edit_files(EditFilesRequest {
            repo: fx.repo.clone(),
            target: Some(edit_files_request::Target::NewOn("main".into())),
            edits: vec![FileEdit {
                path: "x.txt".into(),
                action: Some(file_edit::Action::Content(b"x\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
    let change = edit.change.unwrap();
    let abandoned = fx
        .client()
        .abandon_change(AbandonChangeRequest {
            repo: fx.repo.clone(),
            revision: change.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let row = fx
        .change_when(&change.change_id, |c| {
            c.outcome() == ChangeOutcome::Abandoned
        })
        .await;
    assert!(row.ended_at.is_some());
    fx.client()
        .undo_operation(UndoOperationRequest {
            repo: fx.repo.clone(),
            operation_id: abandoned.operation.unwrap().id,
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap();
    let row = fx
        .change_when(&change.change_id, |c| c.outcome() == ChangeOutcome::Open)
        .await;
    assert!(row.ended_at.is_none());

    // Undoing the edit that created a change: undone.
    let created = fx
        .client()
        .edit_files(EditFilesRequest {
            repo: fx.repo.clone(),
            target: Some(edit_files_request::Target::NewOn("main".into())),
            edits: vec![FileEdit {
                path: "y.txt".into(),
                action: Some(file_edit::Action::Content(b"y\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
    let y = created.change.unwrap();
    fx.client()
        .undo_operation(UndoOperationRequest {
            repo: fx.repo.clone(),
            operation_id: created.operation.unwrap().id,
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap();
    fx.change_when(&y.change_id, |c| c.outcome() == ChangeOutcome::Undone)
        .await;

    let by_outcome = fx
        .query(QueryChangesRequest {
            outcomes: vec![
                ChangeOutcome::Undone.into(),
                ChangeOutcome::Abandoned.into(),
            ],
            include_empty: true,
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert!(
        by_outcome
            .changes
            .iter()
            .any(|c| c.change_id == y.change_id)
    );
    assert!(
        by_outcome
            .changes
            .iter()
            .all(|c| c.change_id != change.change_id)
    );
}

/// Tests 8 and 9: symbol touches by symbol, suffix and name; paths; the
/// net diff; stale rows.
#[tokio::test(flavor = "multi_thread")]
async fn symbol_touches() {
    let fx = start(|_| {}).await;
    let ws = fx.workspace("bot-1").await;
    fx.write(&ws, "src/parser.py", PARSER).await;
    fx.write(&ws, "README.md", "# changed\n").await;
    // Both snapshots followed: the first one's touches may be current
    // before the second is read.
    let row = fx
        .change_when(&ws.change_id, |c| {
            c.touches_state() == TouchState::Current
                && c.symbols_changed > 0
                && c.paths_changed == 2
        })
        .await;
    assert_eq!(row.paths_changed, 2, "{row:#?}");

    for entry in ["parse", "Parser#parse()", "Parser#parse()."] {
        let answer = fx
            .query(QueryChangesRequest {
                symbols: vec![entry.into()],
                ..QueryChangesRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(answer.changes.len(), 1, "{entry}: {answer:#?}");
        let touch = &answer.changes[0].touches[0];
        assert_eq!(touch.name, "parse");
        assert_eq!(touch.path, "src/parser.py");
        assert_eq!(touch.change(), SymbolChangeKind::Added);
    }
    let not_boundary = fx
        .query(QueryChangesRequest {
            symbols: vec!["arser#parse()".into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert!(not_boundary.changes.is_empty(), "{not_boundary:#?}");
    let narrowed = fx
        .query(QueryChangesRequest {
            symbols: vec!["parse".into()],
            symbol_changes: vec![SymbolChangeKind::Removed.into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert!(narrowed.changes.is_empty(), "it was added, not removed");
    let by_path = fx
        .query(QueryChangesRequest {
            paths: vec!["README.md".into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(by_path.changes[0].matched_paths, ["README.md"]);

    // The net diff: a function added and then removed leaves no touch.
    fx.write(&ws, "src/parser.py", PARSER_2).await;
    fx.change_when(&ws.change_id, |c| {
        c.touches_state() == TouchState::Current && c.symbols_changed >= 3
    })
    .await;
    fx.write(&ws, "src/parser.py", PARSER).await;
    fx.change_when(&ws.change_id, |c| {
        c.touches_state() == TouchState::Current && c.symbols_changed == 2
    })
    .await;
    let extra = fx
        .query(QueryChangesRequest {
            symbols: vec!["extra".into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert!(extra.changes.is_empty(), "{extra:#?}");
}

/// Test 10: the throttle computes a busy change rarely, and once quiet.
#[tokio::test(flavor = "multi_thread")]
async fn throttle() {
    let fx = start(|_| {}).await;
    let history = &fx.daemon.handle.state().history;
    // The default quiet time: a snapshot on a slow runner can take longer
    // than a shorter one, and then every write looks quiet.
    history.set_throttle(Duration::from_secs(5), Duration::from_mins(1));
    let ws = fx.workspace("bot-1").await;
    // Settle the create first.
    fx.change_when(&ws.change_id, |c| c.touches_state() == TouchState::Current)
        .await;
    let before = history.computed();
    for i in 0..20 {
        fx.write(&ws, "a.txt", &format!("{i}\n")).await;
    }
    let busy = history.computed() - before;
    assert!(busy <= 2, "computed {busy} times while busy");
    let row = fx
        .change_when(&ws.change_id, |c| c.touches_state() == TouchState::Current)
        .await;
    assert_eq!(row.paths_changed, 1);
    assert!(history.computed() - before <= 3);
}

/// Tests 12 and 13: paging, and grouping by agent and outcome.
#[tokio::test(flavor = "multi_thread")]
async fn paging_and_grouping() {
    let fx = start(|_| {}).await;
    let mut ids = Vec::new();
    for i in 0..12 {
        let agent = if i % 3 == 0 { "bot-a" } else { "bot-b" };
        let change = fx
            .as_agent(agent)
            .edit_files(EditFilesRequest {
                repo: fx.repo.clone(),
                target: Some(edit_files_request::Target::NewOn("main".into())),
                edits: vec![FileEdit {
                    path: format!("f{i}.txt"),
                    action: Some(file_edit::Action::Content(b"x\n".to_vec())),
                    executable: None,
                }],
                ..EditFilesRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap();
        ids.push(change.change_id);
    }
    let all = fx
        .wait_for(
            QueryChangesRequest {
                agents: vec!["bot-a".into(), "bot-b".into()],
                page_size: 1000,
                ..QueryChangesRequest::default()
            },
            |a| a.changes.len() == 12,
        )
        .await;
    let mut paged = Vec::new();
    let mut token = String::new();
    loop {
        let page = fx
            .query(QueryChangesRequest {
                agents: vec!["bot-a".into(), "bot-b".into()],
                page_size: 5,
                page_token: token.clone(),
                ..QueryChangesRequest::default()
            })
            .await
            .unwrap();
        paged.extend(page.changes);
        token = page.next_page_token;
        if token.is_empty() {
            break;
        }
    }
    let ids_of = |changes: &[HistoryChange]| -> Vec<String> {
        changes.iter().map(|c| c.change_id.clone()).collect()
    };
    assert_eq!(ids_of(&paged), ids_of(&all.changes));
    assert_eq!(
        all.changes[0].change_id,
        *ids.last().unwrap(),
        "newest first"
    );
    let error = fx
        .query(QueryChangesRequest {
            agents: vec!["bot-a".into()],
            page_token: token_of(&fx).await,
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).as_deref(), Some("INVALID_PAGE_TOKEN"));

    let groups = fx
        .query(QueryChangesRequest {
            agents: vec!["bot-a".into(), "bot-b".into()],
            group_by: vec![
                ChangeDimension::Agent.into(),
                ChangeDimension::Outcome.into(),
            ],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap();
    assert!(groups.changes.is_empty(), "{groups:#?}");
    let count = |agent: &str| {
        groups
            .groups
            .iter()
            .find(|g| g.agent == agent)
            .map(|g| (g.count, g.outcome()))
    };
    assert_eq!(count("bot-a"), Some((4, ChangeOutcome::Open)));
    assert_eq!(count("bot-b"), Some((8, ChangeOutcome::Open)));
    let error = fx
        .query(QueryChangesRequest {
            group_by: vec![ChangeDimension::Agent.into()],
            page_token: "x".into(),
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).as_deref(), Some("INVALID_HISTORY_QUERY"));
    assert_eq!(field(&error).as_deref(), Some("page_token"));
}

/// A page token issued for another filter.
async fn token_of(fx: &Fixture) -> String {
    fx.query(QueryChangesRequest {
        page_size: 1,
        include_empty: true,
        ..QueryChangesRequest::default()
    })
    .await
    .unwrap()
    .next_page_token
}

/// Test 14: operations recorded before the index existed are backfilled,
/// and a backfill resumes after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn backfill_and_restart() {
    let fx = start(|config| config.file.history.enabled = false).await;
    let ws = fx.workspace("bot-1").await;
    fx.write(&ws, "a.txt", "before\n").await;
    // A change created and undone before the index existed: the backfill
    // reads the undo before the creation (review finding).
    let created = fx
        .client()
        .edit_files(EditFilesRequest {
            repo: fx.repo.clone(),
            target: Some(edit_files_request::Target::NewOn("main".into())),
            edits: vec![FileEdit {
                path: "gone.txt".into(),
                action: Some(file_edit::Action::Content(b"gone\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
    let gone = created.change.unwrap();
    fx.client()
        .undo_operation(UndoOperationRequest {
            repo: fx.repo.clone(),
            operation_id: created.operation.unwrap().id,
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap();
    let error = fx.query(QueryChangesRequest::default()).await.unwrap_err();
    assert_eq!(reason(&error).as_deref(), Some("HISTORY_DISABLED"));
    let path = fx
        .daemon
        .handle
        .state()
        .history
        .path_of(fx.repo.parse().unwrap());
    assert!(!path.exists(), "a disabled index creates no file");

    // Turned on: the backfill finds the workspace's writes.
    let daemon = restart(
        fx.daemon,
        |config| config.file.history.enabled = true,
        || {},
    )
    .await;
    let fx = Fixture {
        daemon,
        repo: fx.repo,
        root: fx.root,
    };
    let row = fx
        .change_when(&ws.change_id, |c| !c.activity.is_empty())
        .await;
    assert_eq!(row.activity[0].agent, "bot-1");
    let undone = fx
        .change_when(&gone.change_id, |c| !c.activity.is_empty())
        .await;
    assert_eq!(undone.outcome(), ChangeOutcome::Undone, "{undone:#?}");
    let answer = fx
        .wait_for(QueryChangesRequest::default(), |a| {
            a.coverage.as_ref().is_some_and(|c| !c.backfilling)
        })
        .await;
    assert!(answer.coverage.unwrap().oldest_at.is_some());
}

/// Test 15: GC's rewrite of operation ids is mapped; the next snapshot is
/// followed without duplicate activity, and the row survives.
#[tokio::test(flavor = "multi_thread")]
async fn gc_rewrite() {
    let fx = start(|config| config.file.gc.undo_window = Duration::ZERO).await;
    let ws = fx.workspace("bot-1").await;
    fx.write(&ws, "a.txt", "one\n").await;
    let before = fx
        .change_when(&ws.change_id, |c| {
            c.activity.first().is_some_and(|a| a.operations == 2)
        })
        .await;
    // `git gc --prune=@<seconds>` keeps what was written this second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let report = fx.client().collect_garbage(&fx.repo, false).await.unwrap();
    assert!(
        report.reports.iter().all(|r| r.jj_error.is_empty()),
        "{report:?}"
    );
    fx.write(&ws, "a.txt", "two\n").await;
    let after = fx
        .change_when(&ws.change_id, |c| {
            c.activity.first().is_some_and(|a| a.operations >= 3)
        })
        .await;
    assert_eq!(
        after.activity[0].operations,
        before.activity[0].operations + 1,
        "{after:#?}"
    );
    assert_eq!(after.first_at, before.first_at, "the row survives GC");
    // The last operation it names loads after GC.
    let op = &after.activity[0].last_operation_id;
    assert!(!op.is_empty(), "{after:#?}");
    fx.client().get_operation(&fx.repo, op).await.unwrap();
}

/// Test 18: with the symbol index off, paths and outcomes still work.
#[tokio::test(flavor = "multi_thread")]
async fn symbols_disabled() {
    let fx = start(|config| config.file.symbols.enabled = false).await;
    let ws = fx.workspace("bot-1").await;
    fx.write(&ws, "src/parser.py", PARSER).await;
    // The snapshot followed: the create's empty change may be current first.
    let row = fx
        .change_when(&ws.change_id, |c| {
            c.touches_state() == TouchState::Current && c.paths_changed == 1
        })
        .await;
    assert_eq!(row.symbols_coverage(), v1::HistorySymbolCoverage::Disabled);
    assert_eq!(row.paths_changed, 1);
    let error = fx
        .query(QueryChangesRequest {
            symbols: vec!["parse".into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).as_deref(), Some("SYMBOLS_DISABLED"));
    let answer = fx.query(QueryChangesRequest::default()).await.unwrap();
    assert!(!answer.coverage.unwrap().symbols);
}

/// Test 19: invalid queries name their field.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_queries() {
    let fx = start(|_| {}).await;
    let ts = |seconds| Some(prost_types::Timestamp { seconds, nanos: 0 });
    let cases: Vec<(QueryChangesRequest, &str)> = vec![
        (
            QueryChangesRequest {
                since: ts(200),
                until: ts(100),
                ..QueryChangesRequest::default()
            },
            "since",
        ),
        (
            QueryChangesRequest {
                symbols: (0..101).map(|i| format!("s{i}")).collect(),
                ..QueryChangesRequest::default()
            },
            "symbols",
        ),
        (
            QueryChangesRequest {
                paths: vec![String::new()],
                ..QueryChangesRequest::default()
            },
            "paths",
        ),
        (
            QueryChangesRequest {
                symbol_changes: vec![SymbolChangeKind::Added.into()],
                ..QueryChangesRequest::default()
            },
            "symbol_changes",
        ),
        (
            QueryChangesRequest {
                outcomes: vec![ChangeOutcome::Unspecified.into()],
                ..QueryChangesRequest::default()
            },
            "outcomes",
        ),
    ];
    for (request, expected) in cases {
        let error = fx.query(request).await.unwrap_err();
        assert_eq!(
            reason(&error).as_deref(),
            Some("INVALID_HISTORY_QUERY"),
            "{expected}"
        );
        assert_eq!(field(&error).as_deref(), Some(expected));
    }
    let error = fx
        .query(QueryChangesRequest {
            changes: vec!["not-hex!".into()],
            ..QueryChangesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).as_deref(), Some("INVALID_CHANGE_ID"));
}

/// Test 20: a garbage file is recreated and backfilled at the next start,
/// and forgetting the repository removes the file.
#[tokio::test(flavor = "multi_thread")]
async fn recreate_and_forget() {
    let fx = start(|_| {}).await;
    let ws = fx.workspace("bot-1").await;
    fx.write(&ws, "a.txt", "one\n").await;
    fx.change_when(&ws.change_id, |c| !c.activity.is_empty())
        .await;
    let repo_id = fx.repo.parse().unwrap();
    let path = fx.daemon.handle.state().history.path_of(repo_id);
    assert!(path.exists());
    let daemon = restart(
        fx.daemon,
        |_| {},
        || {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
            std::fs::write(&path, b"garbage garbage garbage garbage garbage garbage").unwrap();
        },
    )
    .await;
    let fx = Fixture {
        daemon,
        repo: fx.repo,
        root: fx.root,
    };
    fx.change_when(&ws.change_id, |c| !c.activity.is_empty())
        .await;

    fx.client()
        .release_workspace(&fx.repo, &ws.id, true, false)
        .await
        .unwrap();
    fx.client().forget_repository(&fx.repo).await.unwrap();
    assert!(!path.exists(), "forgotten with its repository");
}
