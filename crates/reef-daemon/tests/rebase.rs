//! `ChangeService.RebaseChange` and `GitService.FetchGitRemote` (#259, ADR
//! 0071) end to end: a daemon over a colocated git checkout whose `origin`
//! is a bare git repository on disk, advanced from a second clone. Each
//! test prints a note and returns when `git` is not on `PATH`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]
#![allow(clippy::print_stderr, reason = "a skip note when git is missing")]

use std::path::{Path, PathBuf};
use std::process::Command;

use reef_api::v1::{
    self, CreateWorkspaceRequest, FetchGitRemoteRequest, GetChangeRequest, GetWorkspaceRequest,
    IfNotJj, LifecycleState, OperationKind, RebaseChangeRequest, RebaseMode,
    TransitionChangeRequest, UndoOperationRequest, create_workspace_request,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::git::init_repo;
use tonic::Code;

const NAME: &str = "Pat Example";
const EMAIL: &str = "pat@example.com";

fn has_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not on PATH");
    }
    found
}

/// Runs git in `dir` (a work tree, or with `--git-dir` a bare repository),
/// with an identity: CI runners have none.
fn git_in(dir: &Path, bare: bool, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if bare {
        command.arg("--git-dir").arg(dir);
    } else {
        command.arg("-C").arg(dir);
    }
    let output = command
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", NAME)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_COMMITTER_NAME", NAME)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    daemon: TestDaemon,
    repo: SyntheticRepo,
    /// The bare remote.
    remote: PathBuf,
    /// A second clone of the remote, which advances it.
    other: PathBuf,
    /// The commit `main` holds at the start.
    base: String,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::with(|_| {}).await
    }

    async fn with(configure: impl FnOnce(&mut Config)) -> Fixture {
        let daemon = TestDaemon::start_with(|config| {
            no_automatic_snapshots(config);
            configure(config);
        })
        .await;
        let root = daemon.tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let base = init_repo(&root, &[("README.md", "hello\n"), ("a.txt", "a\n")]);
        let remote = daemon.tmp.path().join("remote.git");
        let status = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&remote)
            .status()
            .unwrap();
        assert!(status.success());
        let store = root.join(".git");
        git_in(
            &store,
            true,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_in(&store, true, &["push", "--quiet", "origin", "main"]);
        git_in(&remote, true, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        // Reef fetched nothing yet: the remote-tracking ref is the push's.
        git_in(&store, true, &["fetch", "--quiet", "origin"]);
        let other = daemon.tmp.path().join("other");
        let status = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&remote)
            .arg(&other)
            .status()
            .unwrap();
        assert!(status.success());
        let registered = daemon
            .client
            .register_repository(&root, IfNotJj::InitAlongsideGit)
            .await
            .unwrap();
        let repo = SyntheticRepo {
            root: root.canonicalize().unwrap(),
            id: registered.repository.unwrap().id,
        };
        Fixture {
            daemon,
            repo,
            remote,
            other,
            base,
        }
    }

    fn client(&self) -> &Client {
        &self.daemon.client
    }

    /// Commits `files` in the second clone and pushes `main`; the new
    /// commit.
    fn advance_remote(&self, files: &[(&str, &str)]) -> String {
        for (path, contents) in files {
            let path = self.other.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        git_in(&self.other, false, &["add", "-A"]);
        git_in(&self.other, false, &["commit", "--quiet", "-m", "upstream"]);
        git_in(&self.other, false, &["push", "--quiet", "origin", "main"]);
        git_in(&self.other, false, &["rev-parse", "HEAD"])
    }

    async fn workspace_on(&self, on: &str) -> v1::Workspace {
        self.client()
            .create_workspace(CreateWorkspaceRequest {
                repo: self.repo.id.clone(),
                owner: "agent@test".to_owned(),
                start: Some(create_workspace_request::Start::NewOn(on.to_owned())),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .workspace
            .unwrap()
    }

    async fn rebase(
        &self,
        request: RebaseChangeRequest,
    ) -> Result<v1::RebaseChangeResponse, ClientError> {
        self.client().rebase_change(request).await
    }

    async fn at(&self, ws: &v1::Workspace, revision: &str) -> v1::Change {
        self.client()
            .get_change(GetChangeRequest {
                workspace: ws.id.clone(),
                revision: revision.to_owned(),
                skip_snapshot: true,
                ..GetChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
    }

    async fn behind(&self, ws: &v1::Workspace) -> Option<u32> {
        self.client()
            .get_workspace(GetWorkspaceRequest {
                workspace: ws.id.clone(),
                skip_disk_usage: true,
                ..GetWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .workspace
            .unwrap()
            .behind_trunk
    }
}

fn of(ws: &v1::Workspace) -> RebaseChangeRequest {
    RebaseChangeRequest {
        workspace: ws.id.clone(),
        revision: "@".to_owned(),
        ..RebaseChangeRequest::default()
    }
}

fn write(ws: &v1::Workspace, path: &str, contents: &str) {
    let path = Path::new(&ws.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn read(ws: &v1::Workspace, path: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(&ws.path).join(path)).ok()
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn next(error: &ClientError) -> Vec<String> {
    error.advice().map(|advice| advice.next).unwrap_or_default()
}

// 11, 18, 22. From a workspace behind the remote's main: the fetch, then
// the rebase; the files on disk are trunk's plus the agent's edit; the
// remote bookmark is the destination, the checked-out local `main` left
// behind; a second rebase records nothing; the rebase undoes, the fetch
// does not.
#[tokio::test(flavor = "multi_thread")]
async fn from_a_workspace() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "mine.txt", "mine\n");
    assert_eq!(fixture.behind(&ws).await, Some(0));
    let upstream = fixture.advance_remote(&[("src/b.txt", "b\n")]);

    let rebased = fixture.rebase(of(&ws)).await.unwrap();
    let fetched = rebased.fetched.as_ref().unwrap();
    assert_eq!(fetched.git_remote, "origin");
    assert_eq!(fetched.branches[0].branch, "main");
    assert_eq!(fetched.branches[0].commit_id, upstream);
    assert!(!fetched.branches[0].local_moved, "{fetched:?}");
    let fetch_op = rebased.fetch_operation.as_ref().unwrap();
    assert_eq!(fetch_op.kind(), OperationKind::Fetch);
    let op = rebased.operation.as_ref().unwrap();
    assert_eq!(op.kind(), OperationKind::Rebase);
    assert_eq!(rebased.onto, "main@origin");
    assert_eq!(rebased.onto_commit_id, upstream);
    assert!(
        rebased
            .notes
            .iter()
            .any(|note| note.contains("left as it is")),
        "{:?}",
        rebased.notes
    );
    assert_eq!(rebased.rebased_count, 1);
    assert_eq!(rebased.rebased[0].workspace_ids, vec![ws.id.clone()]);
    assert!(!rebased.up_to_date);
    let change = rebased.change.unwrap();
    assert_eq!(change.parent_commit_ids, vec![upstream.clone()]);
    // The workspace's files: trunk's new file and its own edit.
    assert_eq!(read(&ws, "src/b.txt").as_deref(), Some("b\n"));
    assert_eq!(read(&ws, "mine.txt").as_deref(), Some("mine\n"));
    assert_eq!(fixture.behind(&ws).await, Some(0));

    // Up to date: nothing recorded.
    let again = fixture.rebase(of(&ws)).await.unwrap();
    assert!(again.up_to_date);
    assert!(again.operation.is_none());
    assert!(again.fetch_operation.is_none(), "nothing new to fetch");

    // Undo: the rebase reverts, the files follow; the fetch refuses.
    fixture
        .client()
        .undo_operation(UndoOperationRequest {
            repo: fixture.repo.id.clone(),
            workspace: ws.id.clone(),
            operation_id: op.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(read(&ws, "src/b.txt"), None);
    assert_eq!(read(&ws, "mine.txt").as_deref(), Some("mine\n"));
    let refused = fixture
        .client()
        .undo_operation(UndoOperationRequest {
            repo: fixture.repo.id.clone(),
            operation_id: fetch_op.id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&refused),
        (Code::InvalidArgument, "OPERATION_NOT_UNDOABLE".to_owned())
    );
    assert_eq!(fixture.behind(&ws).await, Some(1));
}

// 13. A stack: B's change on A's. From B, branch mode refuses to move A's
// live work and names it; from A both move; `--source` from B moves B only.
#[tokio::test(flavor = "multi_thread")]
async fn stack() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let a = fixture.workspace_on(&fixture.base).await;
    write(&a, "a2.txt", "a2\n");
    let a_change = fixture.at(&a, "@").await;
    let b = fixture.workspace_on(&a_change.commit_id).await;
    write(&b, "b2.txt", "b2\n");
    let upstream = fixture.advance_remote(&[("up.txt", "up\n")]);

    let refused = fixture.rebase(of(&b)).await.unwrap_err();
    assert_eq!(
        reason(&refused),
        (Code::FailedPrecondition, "INCLUDES_WORKING_COPY".to_owned())
    );
    assert_eq!(next(&refused), ["reef", "rebase", "--workspace", &a.id]);

    let moved = fixture.rebase(of(&a)).await.unwrap();
    assert_eq!(moved.rebased_count, 2);
    assert_eq!(read(&a, "up.txt").as_deref(), Some("up\n"));
    assert_eq!(read(&b, "up.txt").as_deref(), Some("up\n"));
    assert_eq!(read(&b, "a2.txt").as_deref(), Some("a2\n"));
    assert_eq!(read(&b, "b2.txt").as_deref(), Some("b2\n"));
    let a_after = fixture.at(&a, "@").await;
    assert_eq!(a_after.parent_commit_ids, vec![upstream.clone()]);

    // Source mode from B, onto the base: only B moves, without A's edit.
    let source = fixture
        .rebase(RebaseChangeRequest {
            onto: fixture.base.clone(),
            mode: RebaseMode::Source.into(),
            skip_fetch: true,
            ..of(&b)
        })
        .await
        .unwrap();
    assert_eq!(source.rebased_count, 1);
    assert_eq!(read(&b, "a2.txt"), None);
    assert_eq!(read(&b, "b2.txt").as_deref(), Some("b2\n"));
    assert_eq!(read(&a, "a2.txt").as_deref(), Some("a2\n"));
}

// 17. A conflicting rebase is written and succeeds: the conflict is
// returned, the files carry jj's markers.
#[tokio::test(flavor = "multi_thread")]
async fn conflict() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "README.md", "mine\n");
    fixture.advance_remote(&[("README.md", "theirs\n")]);
    let rebased = fixture.rebase(of(&ws)).await.unwrap();
    assert!(rebased.operation.is_some());
    assert_eq!(rebased.conflict_count, 1);
    let conflict = &rebased.conflicts[0];
    assert_eq!(conflict.paths, vec!["README.md".to_owned()]);
    assert!(rebased.rebased[0].conflicted);
    let on_disk = read(&ws, "README.md").unwrap();
    assert!(on_disk.contains("<<<<<<<"), "{on_disk}");
}

// 19. A failed fetch fails the rebase with a reason and the hint to rebase
// without it; `--no-fetch` rebases onto what Reef last saw. A disabled
// fetch is refused.
#[tokio::test(flavor = "multi_thread")]
async fn fetch_failures() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "mine.txt", "mine\n");
    let gone = fixture.daemon.tmp.path().join("gone.git");
    git_in(
        &fixture.repo.root.join(".git"),
        true,
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );
    let failed = fixture.rebase(of(&ws)).await.unwrap_err();
    let (code, why) = reason(&failed);
    assert!(
        matches!(why.as_str(), "GIT_FETCH_FAILED" | "GIT_AUTH_FAILED"),
        "{code:?} {why}: {failed:?}"
    );
    let offline = fixture
        .rebase(RebaseChangeRequest {
            skip_fetch: true,
            ..of(&ws)
        })
        .await
        .unwrap();
    assert!(offline.fetched.is_none());
    assert!(offline.up_to_date);

    let disabled = Fixture::with(|config| config.file.fetch.enabled = false).await;
    let refused = disabled
        .client()
        .fetch_git_remote(FetchGitRemoteRequest {
            repo: disabled.repo.id.clone(),
            ..FetchGitRemoteRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&refused),
        (Code::PermissionDenied, "FETCH_DISABLED".to_owned())
    );
}

// 2 (daemon side). `FetchGitRemote` moves the remote-tracking bookmark,
// leaves git's checked-out local `main`, and records one operation; a
// second fetch records none; a branch the remote lacks is reported
// missing.
#[tokio::test(flavor = "multi_thread")]
async fn fetch_git_remote() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let upstream = fixture.advance_remote(&[("x.txt", "x\n")]);
    let fetch = |branches: Vec<String>| FetchGitRemoteRequest {
        repo: fixture.repo.id.clone(),
        branches,
        ..FetchGitRemoteRequest::default()
    };
    let first = fixture
        .client()
        .fetch_git_remote(fetch(Vec::new()))
        .await
        .unwrap();
    let fetched = first.fetched.unwrap();
    assert_eq!(fetched.branches[0].commit_id, upstream);
    assert_eq!(fetched.branches[0].previous_commit_id, fixture.base);
    assert!(!fetched.branches[0].local_moved);
    assert_eq!(first.operation.unwrap().kind(), OperationKind::Fetch);
    let main = fixture
        .client()
        .get_change(GetChangeRequest {
            repo: fixture.repo.id.clone(),
            revision: "main".to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(main.commit_id, fixture.base, "the checked-out main moved");
    let second = fixture
        .client()
        .fetch_git_remote(fetch(Vec::new()))
        .await
        .unwrap();
    assert!(second.operation.is_none());
    let missing = fixture
        .client()
        .fetch_git_remote(fetch(vec!["nope".to_owned()]))
        .await
        .unwrap();
    assert!(missing.fetched.unwrap().branches[0].missing);
    let _ = &fixture.remote;
}

// 16. A proposed change rebased: no transition, its proposal reads amended,
// a note says to propose again, and the CLI's `next` is `reef propose`.
#[tokio::test(flavor = "multi_thread")]
async fn lifecycle() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "mine.txt", "mine\n");
    fixture
        .client()
        .transition_change(TransitionChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            to: LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    fixture.advance_remote(&[("up.txt", "up\n")]);
    let rebased = fixture.rebase(of(&ws)).await.unwrap();
    let lifecycle = rebased.change.unwrap().lifecycle.unwrap();
    assert_eq!(lifecycle.state(), LifecycleState::Proposed);
    assert!(lifecycle.amended);
    assert!(
        rebased
            .notes
            .iter()
            .any(|note| note.contains("propose it again")),
        "{:?}",
        rebased.notes
    );
}

/// `REEF_REBASE_PRESET` (small, medium, large; default large).
fn resource_preset() -> reef_testing::synth::Preset {
    use reef_testing::synth::Preset;
    match std::env::var("REEF_REBASE_PRESET").as_deref() {
        Ok("small") => Preset::Small,
        Ok("medium") => Preset::Medium,
        _ => Preset::Large,
    }
}

// The resource criterion (#259, #264's budget): a workspace rebased over a
// trunk that changed 100 files of 4 KiB rewrites only those files, its disk
// grows by about their bytes, and the rebase takes under 500 ms on `large`
// (100k files). Manual: `cargo test --release -p reef-daemon --test rebase
// -- --ignored rebase_resource --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement; run by hand with --release"]
async fn rebase_resource() {
    use reef_daemon::testing::no_automatic_snapshots;
    let preset = resource_preset();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(preset, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    let base = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: "root()+".to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .commit_id;
    // Trunk: 100 new files of 4 KiB.
    let body = "x".repeat(4096);
    let trunk = client
        .edit_files(v1::EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(v1::edit_files_request::Target::NewOn(base.clone())),
            edits: (0..100)
                .map(|i| v1::FileEdit {
                    path: format!("trunk/{i:03}.txt"),
                    action: Some(v1::file_edit::Action::Content(body.clone().into_bytes())),
                    executable: None,
                })
                .collect(),
            ..v1::EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .commit_id;
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "agent@test".to_owned(),
            start: Some(create_workspace_request::Start::NewOn(base.clone())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    write(&ws, "mine.txt", "mine\n");
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let disk = async || {
        client
            .get_workspace(GetWorkspaceRequest {
                workspace: ws.id.clone(),
                ..GetWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .disk_usage
            .unwrap()
    };
    let before = disk().await;
    let started = std::time::Instant::now();
    let rebased = client
        .rebase_change(RebaseChangeRequest {
            onto: trunk.clone(),
            skip_fetch: true,
            ..of(&ws)
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let after = disk().await;
    assert_eq!(rebased.rebased_count, 1);
    assert_eq!(read(&ws, "trunk/042.txt").as_deref(), Some(body.as_str()));
    let grown = after.allocated_bytes.saturating_sub(before.allocated_bytes);
    eprintln!(
        "rebase_resource {preset:?}: {} ms; files changed {} -> {}; allocated {} -> {} bytes (+{grown}, 100 x 4 KiB = {} bytes)",
        elapsed.as_millis(),
        before.files_changed,
        after.files_changed,
        before.allocated_bytes,
        after.allocated_bytes,
        100 * 4096
    );
    assert_eq!(after.files_changed, before.files_changed + 100);
    assert!(grown <= 100 * 4096 * 11 / 10 + 64 * 1024, "+{grown} bytes");
}

// #248 and #259 with a git user's trunk: the workspace's commit landed on
// the remote's `main` (pushed there, as a host's fast-forward merge does)
// while the checked-out local `main` stays behind. After the fetch the
// change is landed: the workspace ends on a new empty change on it, never
// "up to date" on the landed commit, and its next edit does not amend it.
#[tokio::test(flavor = "multi_thread")]
async fn landed_on_the_remote() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "mine.txt", "mine\n");
    fixture
        .client()
        .snapshot_workspace(&fixture.repo.id, &ws.id, "")
        .await
        .unwrap();
    let landed = fixture.at(&ws, "@").await;
    git_in(
        &fixture.repo.root.join(".git"),
        true,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("{}:refs/heads/main", landed.commit_id),
        ],
    );
    let rebased = fixture.rebase(of(&ws)).await.unwrap();
    // The rebase started the new change, or the observer did first.
    assert!(
        rebased.started_new_change || rebased.up_to_date,
        "{rebased:?}"
    );
    let at = fixture.at(&ws, "@").await;
    assert_ne!(at.change_id, landed.change_id, "still on the landed change");
    assert_eq!(at.parent_commit_ids, vec![landed.commit_id.clone()]);
    assert!(at.empty);
    write(&ws, "after.txt", "after\n");
    fixture
        .client()
        .snapshot_workspace(&fixture.repo.id, &ws.id, "")
        .await
        .unwrap();
    let after = fixture.at(&ws, "@").await;
    assert_eq!(after.parent_commit_ids, vec![landed.commit_id.clone()]);
    let still = fixture.at(&ws, &landed.commit_id).await;
    assert_eq!(
        still.commit_id, landed.commit_id,
        "the landed commit was rewritten"
    );
    assert_eq!(fixture.behind(&ws).await, Some(0));
}

// A change whose edits trunk already has (a host's squash merge) is kept,
// reported empty, with a note.
#[tokio::test(flavor = "multi_thread")]
async fn became_empty() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let ws = fixture.workspace_on(&fixture.base).await;
    write(&ws, "same.txt", "same\n");
    fixture.advance_remote(&[("same.txt", "same\n")]);
    let rebased = fixture.rebase(of(&ws)).await.unwrap();
    assert!(rebased.rebased[0].became_empty, "{rebased:?}");
    assert!(
        rebased
            .notes
            .iter()
            .any(|note| note.contains("is now empty")),
        "{:?}",
        rebased.notes
    );
}

// Refusals: onto one of the moved commits' descendants; a commit on trunk
// named explicitly in source mode.
#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    if !has_git() {
        return;
    }
    let fixture = Fixture::new().await;
    let snapshot = async |ws: &v1::Workspace| {
        fixture
            .client()
            .snapshot_workspace(&fixture.repo.id, &ws.id, "")
            .await
            .unwrap();
    };
    let a = fixture.workspace_on(&fixture.base).await;
    write(&a, "a.txt", "a\n");
    snapshot(&a).await;
    let a_change = fixture.at(&a, "@").await;
    let b = fixture.workspace_on(&a_change.commit_id).await;
    write(&b, "b.txt", "b\n");
    snapshot(&b).await;
    let b_change = fixture.at(&b, "@").await;
    let onto_descendant = fixture
        .rebase(RebaseChangeRequest {
            onto: b_change.commit_id.clone(),
            mode: RebaseMode::Source.into(),
            skip_fetch: true,
            ..of(&a)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&onto_descendant),
        (Code::InvalidArgument, "REBASE_ONTO_DESCENDANT".to_owned())
    );
    let immutable = fixture
        .rebase(RebaseChangeRequest {
            revision: fixture.base.clone(),
            onto: a_change.commit_id.clone(),
            mode: RebaseMode::Source.into(),
            skip_fetch: true,
            ..of(&a)
        })
        .await
        .unwrap_err();
    let (_, why) = reason(&immutable);
    assert!(
        matches!(
            why.as_str(),
            "IMMUTABLE_REVISION" | "REBASE_ONTO_DESCENDANT"
        ),
        "{immutable:?}"
    );
}
