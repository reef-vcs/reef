//! `GitService.PublishChange` (#258, ADR 0070) end to end: a daemon over a
//! colocated git checkout whose `origin` is a bare git repository on disk.
//! Each test prints a note and returns when `git` is not on `PATH`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]
#![allow(clippy::print_stderr, reason = "a skip note when git is missing")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, IfNotJj, ListOperationsRequest,
    OperationFilter, OperationKind, PublicationUpdate, PublishChangeRequest, UndoOperationRequest,
    create_workspace_request,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::model::{RepoId, WorkspaceId};
use reef_daemon::snapshot::index;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_lib::jj::settings::Identity;
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

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // CI runners have no git identity; `commit-tree` needs one.
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

/// A daemon with a real identity and no automatic snapshots.
fn with_identity(config: &mut Config) {
    no_automatic_snapshots(config);
    config.identity = Identity {
        name: NAME.to_owned(),
        email: EMAIL.to_owned(),
    };
    NAME.clone_into(&mut config.file.identity.name);
    config.file.identity.email = Some(EMAIL.to_owned());
}

struct Fixture {
    daemon: TestDaemon,
    repo: SyntheticRepo,
    /// The bare remote.
    remote: PathBuf,
    /// The repository's git directory.
    store: PathBuf,
    /// The commit `main` holds on the remote.
    base: String,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::with(with_identity).await
    }

    async fn with(configure: impl FnOnce(&mut Config)) -> Fixture {
        // A git checkout with `origin`, as a person has it, registered
        // colocated as `reef init` does.
        let daemon = TestDaemon::start_with(configure).await;
        let root = daemon.tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let base = init_repo(&root, &[("README.md", "hello\n")]);
        let remote = daemon.tmp.path().join("remote.git");
        let status = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&remote)
            .status()
            .unwrap();
        assert!(status.success());
        let store = root.join(".git");
        git(
            &store,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&store, &["push", "--quiet", "origin", "main"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
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
            store,
            base,
        }
    }

    fn client(&self) -> &Client {
        &self.daemon.client
    }

    async fn workspace(&self) -> v1::Workspace {
        self.client()
            .create_workspace(CreateWorkspaceRequest {
                repo: self.repo.id.clone(),
                owner: "agent@test".to_owned(),
                start: Some(create_workspace_request::Start::NewOn(self.base.clone())),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap()
            .workspace
            .unwrap()
    }

    async fn publish(
        &self,
        request: PublishChangeRequest,
    ) -> Result<v1::PublishChangeResponse, ClientError> {
        self.client().publish_change(request).await
    }

    /// The remote's commit for `branch`, if it has the branch.
    fn remote_commit(&self, branch: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.remote)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }
}

fn request(ws: &v1::Workspace) -> PublishChangeRequest {
    PublishChangeRequest {
        workspace: ws.id.clone(),
        revision: "@".to_owned(),
        ..PublishChangeRequest::default()
    }
}

fn write(ws: &v1::Workspace, path: &str, contents: &str) {
    let path = Path::new(&ws.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn metadata(error: &ClientError) -> BTreeMap<String, String> {
    match error {
        ClientError::Rpc { metadata, .. } => metadata.clone(),
        other => panic!("{other:?}"),
    }
}

fn next(error: &ClientError) -> Vec<String> {
    error.advice().map(|advice| advice.next).unwrap_or_default()
}

fn publication(response: &v1::PublishChangeResponse) -> &v1::Publication {
    response.publication.as_ref().unwrap()
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn from_a_workspace() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    // A draft snapshot whose content the publish does not carry.
    write(&ws, "notes/publish.txt", "draft\n");
    fx.client()
        .snapshot_workspace(&fx.repo.id, &ws.id, "")
        .await
        .unwrap();
    write(&ws, "notes/publish.txt", "first\n");
    let response = fx
        .publish(PublishChangeRequest {
            description: "Add a note\n\nSo it can be reviewed.\n".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    let published = publication(&response);
    assert_eq!(published.update(), PublicationUpdate::Created);
    assert_eq!(published.git_remote, "origin");
    assert!(
        published.branch.starts_with("reef/"),
        "{}",
        published.branch
    );
    assert_eq!(published.branch.len(), "reef/".len() + 12);
    assert_eq!(published.commit_count, 1);
    assert!(
        response.snapshot.is_some(),
        "the edit was snapshotted first"
    );
    assert!(
        response.describe.is_some(),
        "the change was described first"
    );
    let operation = response.operation.as_ref().unwrap();
    assert_eq!(operation.kind(), OperationKind::Publish);
    assert!(
        operation
            .description
            .contains(&format!("to origin/{}", published.branch)),
        "{}",
        operation.description
    );
    assert_eq!(
        fx.remote_commit(&published.branch).as_deref(),
        Some(published.commit_id.as_str())
    );
    // The pushed commit is the snapshot, described, on main.
    let message = git(&fx.remote, &["log", "-1", "--format=%B", &published.branch]);
    assert_eq!(message, "Add a note\n\nSo it can be reviewed.");
    let parent = git(
        &fx.remote,
        &["rev-parse", &format!("{}^", published.branch)],
    );
    assert_eq!(parent, fx.base);
    let author = git(
        &fx.remote,
        &["log", "-1", "--format=%an <%ae>", &published.branch],
    );
    assert_eq!(author, format!("{NAME} <{EMAIL}>"));
    // A file remote is no host Reef knows.
    assert!(published.pull_request.is_none());
    // The describe rewrote the edit's snapshot into the pushed commit, with
    // the same tree: that snapshot has left the machine. The draft has not.
    assert_eq!(published.local_snapshots_published, 1);
    let local = fx
        .client()
        .inspect_workspace(&fx.repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
        .local_snapshot_count;
    assert_eq!(local, 1, "the draft stays local");

    // The operation log has the publish.
    let listed = fx
        .client()
        .list_operations_page(ListOperationsRequest {
            repo: fx.repo.id.clone(),
            filter: Some(OperationFilter {
                kinds: vec![OperationKind::Publish.into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(listed.operations.len(), 1);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn publish_again_updates() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/again.txt", "one\n");
    let first = fx
        .publish(PublishChangeRequest {
            description: "Add a note".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    let branch = publication(&first).branch.clone();

    write(&ws, "notes/again.txt", "two\n");
    let second = fx.publish(request(&ws)).await.unwrap();
    let published = publication(&second);
    assert_eq!(published.update(), PublicationUpdate::Rewritten);
    assert_eq!(published.branch, branch);
    assert_eq!(published.previous_commit_id, publication(&first).commit_id);
    assert_eq!(
        fx.remote_commit(&branch).as_deref(),
        Some(published.commit_id.as_str())
    );
    // The pushed commit is the edit's snapshot: its row has left the machine.
    assert_eq!(published.local_snapshots_published, 1);

    let third = fx.publish(request(&ws)).await.unwrap();
    assert_eq!(publication(&third).update(), PublicationUpdate::UpToDate);
    assert!(third.operation.is_none(), "nothing new, no operation");
    assert_eq!(third.notes.len(), 1, "{:?}", third.notes);
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;

    // Empty: nothing edited yet.
    let error = fx.publish(request(&ws)).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "CHANGE_EMPTY".to_owned())
    );
    assert_eq!(next(&error), ["reef", "publish", "@-"]);

    // Undescribed.
    write(&ws, "notes/refused.txt", "x\n");
    let error = fx.publish(request(&ws)).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "CHANGE_UNDESCRIBED".to_owned())
    );
    assert!(metadata(&error).contains_key("commit_id"));

    // Another workspace's working copy in the range.
    let other = fx.workspace().await;
    write(&other, "notes/other.txt", "y\n");
    fx.client()
        .snapshot_workspace(&fx.repo.id, &other.id, "")
        .await
        .unwrap();
    fx.client()
        .describe_change(DescribeChangeRequest {
            workspace: other.id.clone(),
            revision: "@".to_owned(),
            description: "Other agent's work".to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let error = fx
        .publish(PublishChangeRequest {
            revision: format!("{}@", "reef-".to_owned() + &other.id),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "INCLUDES_WORKING_COPY".to_owned())
    );
    assert_eq!(metadata(&error)["workspace"], other.id);
    assert_eq!(next(&error)[..3], ["reef", "publish", "--workspace"]);
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn signoff_and_trailers() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/signed.txt", "s\n");
    let text = "Fix it\n\nCo-Authored-By: Someone <someone@example.com>\n";
    let response = fx
        .publish(PublishChangeRequest {
            description: text.to_owned(),
            signoff: true,
            ..request(&ws)
        })
        .await
        .unwrap();
    let branch = &publication(&response).branch;
    let message = git(&fx.remote, &["log", "-1", "--format=%B", branch]);
    assert_eq!(
        message,
        format!(
            "Fix it\n\nCo-Authored-By: Someone <someone@example.com>\nSigned-off-by: {NAME} <{EMAIL}>"
        )
    );

    // A second sign-off adds nothing; describe keeps the description.
    let described = fx
        .client()
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            signoff: true,
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    assert!(described.operation.is_none(), "already signed off");
}

// 12, the default identity
#[tokio::test(flavor = "multi_thread")]
async fn identity_unset() {
    if !has_git() {
        return;
    }
    let fx = Fixture::with(no_automatic_snapshots).await;
    let ws = fx.workspace().await;
    write(&ws, "notes/default.txt", "d\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Add".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "IDENTITY_UNSET".to_owned())
    );
    let error = fx
        .client()
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: "Add".to_owned(),
            signoff: true,
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "IDENTITY_UNSET".to_owned())
    );
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn dry_run() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/dry.txt", "d\n");
    fx.client()
        .snapshot_workspace(&fx.repo.id, &ws.id, "")
        .await
        .unwrap();
    let response = fx
        .publish(PublishChangeRequest {
            description: "Planned".to_owned(),
            dry_run: true,
            ..request(&ws)
        })
        .await
        .unwrap();
    let published = publication(&response);
    assert!(published.dry_run);
    assert_eq!(published.update(), PublicationUpdate::Created);
    assert!(response.operation.is_none());
    assert!(response.describe.is_none(), "a dry run describes nothing");
    assert_eq!(fx.remote_commit(&published.branch), None);
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn remotes() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/remotes.txt", "r\n");
    let described = |remote: &str| PublishChangeRequest {
        description: "Remotes".to_owned(),
        git_remote: remote.to_owned(),
        ..request(&ws)
    };
    let error = fx.publish(described("upstream")).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::NotFound, "GIT_REMOTE_NOT_FOUND".to_owned())
    );
    for bad in ["git", "https://example.com/r.git", "--upload-pack=x", "a b"] {
        let error = fx.publish(described(bad)).await.unwrap_err();
        assert_eq!(
            reason(&error),
            (Code::InvalidArgument, "INVALID_GIT_REMOTE".to_owned()),
            "{bad}"
        );
    }
    for bad in [
        "+main", "a:b", "-x", "HEAD", "a..b", "x@{u}", "x.lock", "a b",
    ] {
        let error = fx
            .publish(PublishChangeRequest {
                branch: bad.to_owned(),
                ..described("")
            })
            .await
            .unwrap_err();
        assert_eq!(
            reason(&error),
            (Code::InvalidArgument, "INVALID_BRANCH".to_owned()),
            "{bad}"
        );
    }
    // Trunk's branch is never a target.
    let error = fx
        .publish(PublishChangeRequest {
            branch: "main".to_owned(),
            ..described("")
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::FailedPrecondition,
            "BRANCH_NOT_PUBLISHABLE".to_owned()
        )
    );

    // A second remote and no `origin`'s choice: still origin.
    let second = fx.daemon.tmp.path().join("second.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&second)
            .status()
            .unwrap()
            .success()
    );
    git(
        &fx.store,
        &["remote", "add", "second", second.to_str().unwrap()],
    );
    let response = fx.publish(described("")).await.unwrap();
    assert_eq!(publication(&response).git_remote, "origin");
}

// 14, `[publish]`
#[tokio::test(flavor = "multi_thread")]
async fn publish_config() {
    if !has_git() {
        return;
    }
    let fx = Fixture::with(|config| {
        with_identity(config);
        config.file.publish.remotes = vec!["upstream".to_owned()];
    })
    .await;
    let ws = fx.workspace().await;
    write(&ws, "notes/config.txt", "c\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Config".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::PermissionDenied, "GIT_REMOTE_NOT_ALLOWED".to_owned())
    );

    let fx = Fixture::with(|config| {
        with_identity(config);
        config.file.publish.enabled = false;
    })
    .await;
    let ws = fx.workspace().await;
    let error = fx.publish(request(&ws)).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::PermissionDenied, "PUBLISH_DISABLED".to_owned())
    );
}

// 2 and 4: the lease, someone else's branch, and --force
#[tokio::test(flavor = "multi_thread")]
async fn lease_and_force() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    // A branch on the remote Reef never fetched, holding a person's commit.
    let tree = format!("{}^{{tree}}", fx.base);
    let theirs = Command::new("git")
        .arg("--git-dir")
        .arg(&fx.remote)
        .args(["commit-tree", &tree, "-p", &fx.base, "-m", "Their work"])
        .env("GIT_AUTHOR_NAME", "Robin")
        .env("GIT_AUTHOR_EMAIL", "robin@example.com")
        .env("GIT_COMMITTER_NAME", "Robin")
        .env("GIT_COMMITTER_EMAIL", "robin@example.com")
        .output()
        .unwrap();
    assert!(theirs.status.success());
    let theirs = String::from_utf8(theirs.stdout).unwrap().trim().to_owned();
    git(&fx.remote, &["update-ref", "refs/heads/taken", &theirs]);
    let ws = fx.workspace().await;
    write(&ws, "notes/lease.txt", "l\n");
    let request = PublishChangeRequest {
        description: "Lease".to_owned(),
        branch: "taken".to_owned(),
        ..request(&ws)
    };
    let error = fx.publish(request.clone()).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "BRANCH_MOVED".to_owned())
    );
    assert_eq!(fx.remote_commit("taken").as_deref(), Some(theirs.as_str()));
    // The advice is Reef's own fetch (#259), which works.
    assert_eq!(
        next(&error),
        [
            "reef",
            "fetch",
            "--git-remote",
            "origin",
            "--branch",
            "taken"
        ]
    );

    // Fetched, it holds another change's commit: refused without --force.
    fx.client()
        .fetch_git_remote(v1::FetchGitRemoteRequest {
            repo: fx.repo.id.clone(),
            git_remote: "origin".to_owned(),
            branches: vec!["taken".to_owned()],
            ..v1::FetchGitRemoteRequest::default()
        })
        .await
        .unwrap();
    let error = fx.publish(request.clone()).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "BRANCH_DIVERGED".to_owned())
    );
    let found = metadata(&error);
    assert_eq!(found["remote_summary"], "Their work");
    assert_eq!(found["remote_author"], "Robin <robin@example.com>");
    assert_eq!(found["remote_commit_id"], theirs);
    let response = fx
        .publish(PublishChangeRequest {
            force: true,
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(publication(&response).update(), PublicationUpdate::Forced);
    assert!(
        response
            .operation
            .as_ref()
            .unwrap()
            .description
            .ends_with(" (forced)")
    );

    // 18: not undoable.
    let error = fx
        .client()
        .undo_operation(UndoOperationRequest {
            repo: fx.repo.id.clone(),
            operation_id: response.operation.as_ref().unwrap().id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::InvalidArgument, "OPERATION_NOT_UNDOABLE".to_owned())
    );
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn auth_failure() {
    if !has_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("git");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = push ]; then\n    echo 'git@github.com: Permission denied (publickey).' >&2\n    echo 'fatal: Could not read from remote repository https://user:secret@example.com/r.git' >&2\n    exit 128\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = fake.to_str().unwrap().to_owned();
    let fx = Fixture::with(move |config| {
        with_identity(config);
        config.file.gc.git_executable = path;
    })
    .await;
    let ws = fx.workspace().await;
    write(&ws, "notes/auth.txt", "a\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Auth".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::PermissionDenied, "GIT_AUTH_FAILED".to_owned())
    );
    let found = metadata(&error);
    assert!(matches!(found["ssh_auth_sock"].as_str(), "set" | "unset"));
    let stderr = &found["git_stderr"];
    assert!(stderr.contains("Permission denied"), "{stderr}");
    assert!(!stderr.contains("secret"), "{stderr}");
}

/// A git that sleeps `seconds` before a push, then runs the real git: a
/// slow remote. Returns its path, in `dir`.
fn slow_git(dir: &Path, seconds: u32) -> String {
    let fake = dir.join("git");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = push ]; then\n    sleep {seconds}\n    break\n  fi\ndone\nexec git \"$@\"\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fake.to_str().unwrap().to_owned()
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn timeout() {
    if !has_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = slow_git(tmp.path(), 14);
    let fx = Fixture::with(move |config| {
        with_identity(config);
        config.file.gc.git_executable = path;
        config.file.publish.timeout = std::time::Duration::from_secs(10);
    })
    .await;
    let ws = fx.workspace().await;
    write(&ws, "notes/slow.txt", "s\n");
    let publish = PublishChangeRequest {
        description: "Slow".to_owned(),
        ..request(&ws)
    };
    let error = fx.publish(publish.clone()).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "PUBLISH_TIMEOUT".to_owned())
    );
    // The push goes on and holds its branch.
    let error = fx
        .publish(PublishChangeRequest {
            description: String::new(),
            ..publish
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::Unavailable, "PUBLISH_IN_PROGRESS".to_owned())
    );
    // When git exits, the push is recorded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let listed = fx
            .client()
            .list_operations_page(ListOperationsRequest {
                repo: fx.repo.id.clone(),
                filter: Some(OperationFilter {
                    kinds: vec![OperationKind::Publish.into()],
                    ..OperationFilter::default()
                }),
                ..ListOperationsRequest::default()
            })
            .await
            .unwrap();
        if !listed.operations.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the late push was never recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn rewritten_during_push() {
    if !has_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = slow_git(tmp.path(), 3);
    let fx = Fixture::with(move |config| {
        with_identity(config);
        config.file.gc.git_executable = path;
    })
    .await;
    let ws = fx.workspace().await;
    write(&ws, "notes/during.txt", "one\n");
    let publishing = {
        let client = fx.client().clone();
        let publish = PublishChangeRequest {
            description: "During".to_owned(),
            ..request(&ws)
        };
        tokio::spawn(async move { client.publish_change(publish).await })
    };
    // While git sleeps, the agent writes again and the workspace is
    // snapshotted: the published commit is rewritten.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    write(&ws, "notes/during.txt", "two\n");
    fx.client()
        .snapshot_workspace(&fx.repo.id, &ws.id, "")
        .await
        .unwrap();
    let response = publishing.await.unwrap().unwrap();
    let published = publication(&response);
    // The remote has what was pushed; the local bookmark followed the
    // change to its new commit, and the change is not divergent.
    assert_eq!(
        fx.remote_commit(&published.branch).as_deref(),
        Some(published.commit_id.as_str())
    );
    let workspace = fx
        .client()
        .inspect_workspace(&fx.repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert!(!workspace.divergent, "{workspace:?}");
    assert_ne!(workspace.commit_id, published.commit_id);
    let effects = response.operation.unwrap().effects.unwrap();
    let local = effects
        .bookmarks
        .iter()
        .find(|bookmark| bookmark.remote.is_empty())
        .unwrap();
    assert_eq!(local.commit_id, workspace.commit_id);
}

// 14: several remotes and none named `origin`.
#[tokio::test(flavor = "multi_thread")]
async fn remote_required() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    git(&fx.store, &["remote", "rename", "origin", "fork"]);
    git(
        &fx.store,
        &["remote", "add", "upstream", fx.remote.to_str().unwrap()],
    );
    let ws = fx.workspace().await;
    write(&ws, "notes/two.txt", "t\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Two remotes".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "GIT_REMOTE_REQUIRED".to_owned())
    );
    assert_eq!(metadata(&error)["remotes"], "fork,upstream");
    let response = fx
        .publish(PublishChangeRequest {
            description: "Two remotes".to_owned(),
            git_remote: "upstream".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    assert_eq!(publication(&response).git_remote, "upstream");
}

// 12: a sign-off needs a description.
#[tokio::test(flavor = "multi_thread")]
async fn signoff_without_description() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/bare.txt", "b\n");
    let error = fx
        .publish(PublishChangeRequest {
            signoff: true,
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::FailedPrecondition,
            "SIGNOFF_WITHOUT_DESCRIPTION".to_owned()
        )
    );
}

// 4: a hook on the remote that declines.
#[tokio::test(flavor = "multi_thread")]
async fn remote_rejected() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let hook = fx.remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'reviews only on Tuesdays' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ws = fx.workspace().await;
    write(&ws, "notes/hook.txt", "h\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Declined".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "REMOTE_REJECTED".to_owned())
    );
    assert!(!metadata(&error)["remote_reason"].is_empty(), "{error:?}");
}

// 3: jj's own fetch after a publish changes nothing and conflicts nothing:
// the record is what `push_refs` leaves jj expecting.
#[tokio::test(flavor = "multi_thread")]
async fn jj_fetch_after_publish() {
    if !has_git() {
        return;
    }
    if Command::new("jj").arg("--version").output().is_err() {
        eprintln!("skipping: jj is not on PATH");
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/fetch.txt", "f\n");
    let response = fx
        .publish(PublishChangeRequest {
            description: "Fetched".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    let branch = publication(&response).branch.clone();
    let jj = |args: &[&str]| {
        let output = Command::new("jj")
            .arg("-R")
            .arg(&fx.repo.root)
            .args(["--no-pager", "--color", "never", "--ignore-working-copy"])
            .args(args)
            .env("JJ_CONFIG", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    jj(&["git", "fetch", "--remote", "origin"]);
    let listed = jj(&["bookmark", "list", "--all-remotes", &branch]);
    assert!(!listed.contains("conflict"), "{listed}");
    assert!(
        listed.contains(&publication(&response).commit_id[..8]),
        "{listed}"
    );
}

// 2: git's checked-out branch is never a target.
#[tokio::test(flavor = "multi_thread")]
async fn checked_out_branch() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    git(&fx.store, &["update-ref", "refs/heads/feature", &fx.base]);
    git(&fx.store, &["symbolic-ref", "HEAD", "refs/heads/feature"]);
    let ws = fx.workspace().await;
    write(&ws, "notes/checkout.txt", "c\n");
    let error = fx
        .publish(PublishChangeRequest {
            description: "Checked out".to_owned(),
            branch: "feature".to_owned(),
            force: true,
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::FailedPrecondition,
            "BRANCH_NOT_PUBLISHABLE".to_owned()
        )
    );
    assert_eq!(metadata(&error)["why"], "checked_out");
}

// Review: a commit the remote already has on another branch is still
// checked when it is published to a new one.
#[tokio::test(flavor = "multi_thread")]
async fn empty_range_still_checks() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/again.txt", "a\n");
    fx.publish(PublishChangeRequest {
        description: "First".to_owned(),
        ..request(&ws)
    })
    .await
    .unwrap();
    // Published again to another branch: the remote already has the
    // commit, so nothing is new to push.
    let response = fx
        .publish(PublishChangeRequest {
            branch: "second-home".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    assert_eq!(publication(&response).commit_count, 0);
    assert_eq!(publication(&response).update(), PublicationUpdate::Created);
    // Undescribing it is refused even with nothing new to push.
    fx.client()
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: String::new(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let error = fx
        .publish(PublishChangeRequest {
            branch: "third-home".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "CHANGE_UNDESCRIBED".to_owned())
    );
}

// 2: a person's local bookmark is not moved without --force, and moving it
// with --force is recorded as forced.
#[tokio::test(flavor = "multi_thread")]
async fn local_bookmark_rules() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    // A person's local branch `topic`, at a commit of their own.
    let tree = format!("{}^{{tree}}", fx.base);
    let theirs = git(
        &fx.store,
        &["commit-tree", &tree, "-p", &fx.base, "-m", "Mine"],
    );
    git(&fx.store, &["update-ref", "refs/heads/topic", &theirs]);
    let ws = fx.workspace().await;
    write(&ws, "notes/local.txt", "l\n");
    let publish = PublishChangeRequest {
        description: "Local".to_owned(),
        branch: "topic".to_owned(),
        ..request(&ws)
    };
    let error = fx.publish(publish.clone()).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "BRANCH_DIVERGED".to_owned())
    );
    let response = fx
        .publish(PublishChangeRequest {
            force: true,
            ..publish
        })
        .await
        .unwrap();
    assert_eq!(publication(&response).update(), PublicationUpdate::Created);
    assert!(
        response
            .operation
            .unwrap()
            .description
            .ends_with(" (forced)")
    );
}

// 19: the reindex (after GC or a restore) derives the flag again from the
// remote bookmarks: the pushed tree's snapshot stays published, the draft
// stays local.
#[tokio::test(flavor = "multi_thread")]
async fn reindex_keeps_published() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let ws = fx.workspace().await;
    write(&ws, "notes/reindex.txt", "draft\n");
    fx.client()
        .snapshot_workspace(&fx.repo.id, &ws.id, "")
        .await
        .unwrap();
    write(&ws, "notes/reindex.txt", "final\n");
    let response = fx
        .publish(PublishChangeRequest {
            description: "Add a note".to_owned(),
            ..request(&ws)
        })
        .await
        .unwrap();
    assert_eq!(publication(&response).local_snapshots_published, 1);

    let state = fx.daemon.handle.state();
    let repo: RepoId = fx.repo.id.parse().unwrap();
    let workspace: WorkspaceId = ws.id.parse().unwrap();
    let flags = || {
        let mut rows: Vec<bool> = index::recent(&state.db, workspace, 10)
            .unwrap()
            .into_iter()
            .map(|row| row.local)
            .collect();
        rows.sort_unstable();
        rows
    };
    assert_eq!(flags(), [false, true]);
    assert_eq!(index::clear(&state.db, repo).unwrap(), 2);
    let handle = state.repos.get(repo).unwrap();
    let report = index::reindex(&state.db, &handle, repo, 100_000)
        .await
        .unwrap();
    assert_eq!(report.added, 2);
    assert_eq!(flags(), [false, true], "published survives the reindex");
}
