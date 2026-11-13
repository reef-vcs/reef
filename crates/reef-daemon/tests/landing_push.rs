//! Pushing landed history to a git remote (R3.17, the brief's tests 5 to
//! 14) end to end: a daemon over a colocated git checkout whose `origin` is
//! a bare git repository on disk, and whose `main` has a policy that says
//! `[landing] push = "origin"`. Someone else's push is a commit written
//! into the bare repository directly; an offline remote is the bare
//! repository moved away. Each test prints a note and returns when `git` is
//! not on `PATH`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]
#![allow(clippy::print_stderr, reason = "a skip note when git is missing")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use reef_api::v1::{
    self, EditFilesRequest, FileEdit, GetChangeRequest, IfNotJj, LandChangeRequest,
    LandingOutcome as Outcome, ListOperationsRequest, OperationFilter, OperationKind,
    PushLandedRequest, PushState, TransitionChangeRequest, edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_lib::jj::settings::Identity;
use reef_testing::git::init_repo;

const NAME: &str = "Pat Example";
const EMAIL: &str = "pat@example.com";

const POLICY: &str = r#"version = 1

[owners]
identities = ["pat"]

[landing]
checks = "proposal"
push = "origin"
"#;

fn has_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not on PATH");
    }
    found
}

/// Runs git on the git directory `dir` with an identity; `index` is a
/// private index file for plumbing.
fn git_at(dir: &Path, index: Option<&Path>, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write as _;
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Robin")
        .env("GIT_AUTHOR_EMAIL", "robin@example.com")
        .env("GIT_COMMITTER_NAME", "Robin")
        .env("GIT_COMMITTER_EMAIL", "robin@example.com")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.unwrap_or_default().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, None, args, None)
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
    /// The commit `main` holds, here and on the remote.
    base: String,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::with(with_identity).await
    }

    async fn with(configure: impl FnOnce(&mut Config)) -> Fixture {
        Fixture::with_policy(POLICY, configure).await
    }

    async fn with_policy(policy: &str, configure: impl FnOnce(&mut Config)) -> Fixture {
        let daemon = TestDaemon::start_with(configure).await;
        let root = daemon.tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let base = init_repo(
            &root,
            &[("README.md", "hello\n"), ("reef-policy.toml", policy)],
        );
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
        git(&store, &["fetch", "--quiet", "origin"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        // No branch checked out at the root, as docs/landing.md advises.
        std::fs::write(store.join("HEAD"), format!("{base}\n")).unwrap();
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

    /// The remote's `main`.
    fn remote_main(&self) -> String {
        git(&self.remote, &["rev-parse", "refs/heads/main"])
    }

    /// Someone else pushes a commit writing `file` on top of the remote's
    /// `main`; its id.
    fn foreign_push(&self, file: &str, contents: &str) -> String {
        let index = self.daemon.tmp.path().join("foreign-index");
        let parent = self.remote_main();
        git_at(&self.remote, Some(&index), &["read-tree", &parent], None);
        let blob = git_at(
            &self.remote,
            None,
            &["hash-object", "-w", "--stdin"],
            Some(contents),
        );
        git_at(
            &self.remote,
            Some(&index),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{file}"),
            ],
            None,
        );
        let tree = git_at(&self.remote, Some(&index), &["write-tree"], None);
        let commit = git_at(
            &self.remote,
            None,
            &[
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                &format!("Theirs {file}"),
            ],
            None,
        );
        git(&self.remote, &["update-ref", "refs/heads/main", &commit]);
        let _ = std::fs::remove_file(index);
        commit
    }

    /// The remote taken offline (moved away), and back.
    fn offline(&self) -> PathBuf {
        let away = self.remote.with_extension("away");
        std::fs::rename(&self.remote, &away).unwrap();
        away
    }

    fn online(&self, away: &Path) {
        std::fs::rename(away, &self.remote).unwrap();
    }

    /// A new change on `on` writing `file`, described.
    async fn change(&self, on: &str, file: &str) -> v1::Change {
        self.client()
            .edit_files(EditFilesRequest {
                repo: self.repo.id.clone(),
                target: Some(edit_files_request::Target::NewOn(on.to_owned())),
                edits: vec![FileEdit {
                    path: file.to_owned(),
                    action: Some(file_edit::Action::Content(b"landed\n".to_vec())),
                    executable: None,
                }],
                description: Some(format!("Add {file}")),
                ..EditFilesRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
    }

    async fn propose(&self, commit: &str) {
        self.client()
            .transition_change(TransitionChangeRequest {
                repo: self.repo.id.clone(),
                revision: commit.to_owned(),
                to: v1::LifecycleState::Proposed.into(),
                ..TransitionChangeRequest::default()
            })
            .await
            .unwrap();
    }

    async fn land(&self, commit: &str) -> Result<v1::Landing, ClientError> {
        self.land_waiting(commit, 60_000).await
    }

    async fn land_waiting(&self, commit: &str, wait_ms: u32) -> Result<v1::Landing, ClientError> {
        self.propose(commit).await;
        self.client()
            .land_change(LandChangeRequest {
                repo: self.repo.id.clone(),
                revision: commit.to_owned(),
                wait_ms,
                ..LandChangeRequest::default()
            })
            .await
            .map(|response| response.landing.unwrap())
    }

    async fn operation_count(&self) -> usize {
        self.client()
            .list_operations_page(ListOperationsRequest {
                repo: self.repo.id.clone(),
                page_size: 1_000,
                ..ListOperationsRequest::default()
            })
            .await
            .unwrap()
            .operations
            .len()
    }

    async fn main(&self) -> v1::Change {
        self.client()
            .get_change(GetChangeRequest {
                repo: self.repo.id.clone(),
                revision: "main".to_owned(),
                skip_snapshot: true,
                ..GetChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
    }

    async fn push_state(&self) -> v1::DestinationPush {
        let pushes = self.client().list_pushes(&self.repo.id).await.unwrap();
        assert_eq!(pushes.len(), 1, "{pushes:#?}");
        pushes.into_iter().next().unwrap()
    }

    /// Waits until main's push state is `state`.
    async fn wait_for(&self, state: PushState) -> v1::DestinationPush {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let pushes = self.client().list_pushes(&self.repo.id).await.unwrap();
            if let Some(push) = pushes.into_iter().find(|push| push.state() == state) {
                return push;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "main never reached {state:?}: {:#?}",
                self.client().list_pushes(&self.repo.id).await.unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn push_now(&self) -> v1::PushLandedResponse {
        self.client()
            .push_landed(PushLandedRequest {
                repo: self.repo.id.clone(),
                destination: "main".to_owned(),
                wait: true,
            })
            .await
            .unwrap()
    }

    async fn publish_operations(&self) -> usize {
        self.client()
            .list_operations_page(ListOperationsRequest {
                repo: self.repo.id.clone(),
                filter: Some(OperationFilter {
                    kinds: vec![OperationKind::Publish.into()],
                    ..OperationFilter::default()
                }),
                ..ListOperationsRequest::default()
            })
            .await
            .unwrap()
            .operations
            .len()
    }
}

fn reason(error: &ClientError) -> String {
    match error {
        ClientError::Rpc { reason, .. } => reason.clone(),
        other => panic!("{other:?}"),
    }
}

// 5. A landing reaches the bare remote fast forward; `reef land`'s
// response carries the push; the push is one `publish` operation; the
// remote-tracking bookmark keeps its tracking state.
#[tokio::test(flavor = "multi_thread")]
async fn land_then_push() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let change = fx.change(&fx.base, "a.txt").await;
    let landing = fx.land(&change.commit_id).await.unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let push = landing.push.clone().unwrap();
    assert_eq!(push.state(), PushState::Pushed, "{landing:#?}");
    assert_ne!(push.operation_id, "");
    let landed = fx.main().await.commit_id;
    assert_eq!(fx.remote_main(), landed);
    let state = fx.push_state().await;
    assert_eq!(state.state(), PushState::Pushed);
    assert_eq!(state.git_remote, "origin");
    assert_eq!(state.local_commit_id, landed);
    assert_eq!(state.remote_commit_id, landed);
    assert_eq!(fx.publish_operations().await, 1);
    // A second landing waits for its own push too (the row already said
    // `pushed` before it).
    let second = fx.change(&landed, "b.txt").await;
    let landing = fx.land(&second.commit_id).await.unwrap();
    assert_eq!(
        landing.push.as_ref().map(v1::LandingPush::state),
        Some(PushState::Pushed),
        "{landing:#?}"
    );
    let landed = fx.main().await.commit_id;
    assert_eq!(fx.remote_main(), landed);
    assert_eq!(fx.publish_operations().await, 2);
    // Pushing again is up to date and writes nothing.
    let again = fx.push_now().await;
    assert_eq!(again.push.unwrap().state(), PushState::Pushed);
    assert!(again.operation.is_none());
    assert_eq!(fx.publish_operations().await, 2);
}

// 7. Someone else pushes on top of what Reef pushed: the next batch's fetch
// follows it, lands on top, and pushes fast forward; `main` is never
// conflicted.
#[tokio::test(flavor = "multi_thread")]
async fn remote_ahead_is_followed() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    // Every batch fetches: no window after the last fetch or push.
    fx.daemon
        .handle
        .state()
        .pushes
        .set_fetch_window(Duration::ZERO);
    let first = fx.change(&fx.base, "a.txt").await;
    fx.land(&first.commit_id).await.unwrap();
    let theirs = fx.foreign_push("theirs.txt", "theirs\n");
    let landed = fx.main().await.commit_id;
    let second = fx.change(&landed, "b.txt").await;
    let landing = fx.land(&second.commit_id).await.unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let main = fx.main().await;
    assert_eq!(
        main.parent_commit_ids,
        std::slice::from_ref(&theirs),
        "landed on theirs"
    );
    let pushed = fx.wait_for(PushState::Pushed).await;
    assert_eq!(fx.remote_main(), main.commit_id);
    assert_eq!(pushed.remote_commit_id, main.commit_id);
}

// 8. A remote ahead whose policy file changed is not followed: refused
// with REMOTE_POLICY_CHANGED, `main` unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn remote_policy_change_is_not_followed() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    // A first push records the destination as pushing, so fetches follow it.
    assert_eq!(fx.push_now().await.push.unwrap().state(), PushState::Pushed);
    fx.foreign_push("reef-policy.toml", &format!("{POLICY}# changed\n"));
    // `reef fetch` sees the remote ahead and does not follow it.
    let fetched = fx
        .client()
        .fetch_git_remote(v1::FetchGitRemoteRequest {
            repo: fx.repo.id.clone(),
            branches: vec!["main".to_owned()],
            ..v1::FetchGitRemoteRequest::default()
        })
        .await
        .unwrap()
        .fetched
        .unwrap();
    assert!(!fetched.branches[0].local_moved);
    assert!(
        fetched
            .notes
            .iter()
            .any(|note| note.contains("reef-policy.toml")),
        "{fetched:#?}"
    );
    assert_eq!(fx.main().await.commit_id, fx.base);
    let response = fx.push_now().await;
    let push = response.push.unwrap();
    assert_eq!(push.state(), PushState::Refused, "{push:#?}");
    assert_eq!(push.reason, "REMOTE_POLICY_CHANGED");
    assert_ne!(push.hint, "");
    assert_eq!(fx.main().await.commit_id, fx.base);
}

// 9 and 10. Offline: the landing lands and `reef land` succeeds; the push
// retries with backoff. Back online after someone else pushed on the old
// base: the push is rejected, the fetch leaves `main` unconflicted, and the
// state is `diverged` with the remote's head; another landing still lands.
#[tokio::test(flavor = "multi_thread")]
async fn offline_then_diverged() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let away = fx.offline();
    let change = fx.change(&fx.base, "a.txt").await;
    let landing = fx.land(&change.commit_id).await.unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed);
    let retrying = fx.wait_for(PushState::Retrying).await;
    assert_eq!(retrying.attempts, 1);
    assert!(retrying.next_attempt_at.is_some());
    assert_eq!(retrying.unpushed_count, 1);
    assert_ne!(retrying.reason, "");
    fx.online(&away);
    let theirs = fx.foreign_push("theirs.txt", "theirs\n");
    let response = fx.push_now().await;
    let push = response.push.unwrap();
    assert_eq!(push.state(), PushState::Diverged, "{push:#?}");
    assert_eq!(push.reason, "BRANCH_DIVERGED");
    assert!(push.remote_head.starts_with(&theirs[..12]), "{push:#?}");
    assert_eq!(push.unpushed_count, 1);
    let main = fx.main().await;
    assert!(!main.conflict);
    assert_eq!(fx.remote_main(), theirs);
    // Landings go on locally.
    let more = fx.change(&main.commit_id, "b.txt").await;
    assert_eq!(
        fx.land(&more.commit_id).await.unwrap().outcome(),
        Outcome::Landed
    );
}

// 10. Offline, then back with nothing else pushed: `PushLanded` pushes and
// marks the landing.
#[tokio::test(flavor = "multi_thread")]
async fn offline_then_back() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let away = fx.offline();
    let change = fx.change(&fx.base, "a.txt").await;
    fx.land(&change.commit_id).await.unwrap();
    fx.wait_for(PushState::Retrying).await;
    fx.online(&away);
    let response = fx.push_now().await;
    assert_eq!(response.push.unwrap().state(), PushState::Pushed);
    assert!(response.operation.is_some());
    assert_eq!(fx.remote_main(), fx.main().await.commit_id);
}

// 11. A hook on the remote that declines: refused, REMOTE_REJECTED, no
// retry without a trigger.
#[tokio::test(flavor = "multi_thread")]
async fn refused_by_remote() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let hook = fx.remote.join("hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho 'main is protected' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let change = fx.change(&fx.base, "a.txt").await;
    fx.land(&change.commit_id).await.unwrap();
    let refused = fx.wait_for(PushState::Refused).await;
    assert_eq!(refused.reason, "REMOTE_REJECTED");
    assert!(refused.next_attempt_at.is_none());
}

// 12. A push whose record was lost (the remote already at the head, Reef's
// view stale): pushed again, up to date at git, and recorded.
#[tokio::test(flavor = "multi_thread")]
async fn lost_record_is_repaired() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let away = fx.offline();
    let change = fx.change(&fx.base, "a.txt").await;
    fx.land(&change.commit_id).await.unwrap();
    fx.wait_for(PushState::Retrying).await;
    fx.online(&away);
    let landed = fx.main().await.commit_id;
    // As if a push reached the remote and the daemon stopped before its
    // record.
    git(
        &fx.store,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("{landed}:refs/heads/main"),
        ],
    );
    let response = fx.push_now().await;
    let push = response.push.unwrap();
    assert_eq!(push.state(), PushState::Pushed, "{push:#?}");
    assert_eq!(push.remote_commit_id, landed);
}

// 14. The default identity refuses a landing onto a pushing destination;
// `[publish] landings = false` lands and reads `disabled`; a destination
// that does not push is DESTINATION_NOT_PUSHED for PushLanded.
#[tokio::test(flavor = "multi_thread")]
async fn identity_and_switches() {
    if !has_git() {
        return;
    }
    let fx = Fixture::with(no_automatic_snapshots).await;
    let change = fx.change(&fx.base, "a.txt").await;
    let error = fx.land(&change.commit_id).await.unwrap_err();
    assert_eq!(reason(&error), "IDENTITY_UNSET");

    let fx = Fixture::with(|config| {
        with_identity(config);
        config.file.publish.landings = false;
    })
    .await;
    let change = fx.change(&fx.base, "a.txt").await;
    let landing = fx.land(&change.commit_id).await.unwrap();
    assert_eq!(landing.outcome(), Outcome::Landed);
    let disabled = fx.wait_for(PushState::Disabled).await;
    assert_eq!(disabled.reason, "PUBLISH_DISABLED");
    assert_eq!(fx.remote_main(), fx.base);
    // Off means no git at all: no fetch before the batch either.
    let fetches = fx
        .client()
        .list_operations_page(ListOperationsRequest {
            repo: fx.repo.id.clone(),
            filter: Some(OperationFilter {
                kinds: vec![OperationKind::Fetch.into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    assert!(fetches.is_empty(), "{fetches:#?}");

    let error = fx
        .client()
        .push_landed(PushLandedRequest {
            repo: fx.repo.id.clone(),
            destination: "nope".to_owned(),
            wait: true,
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "DESTINATION_NOT_FOUND");
}

// The review's bug 2: a fetch of another remote never moves a destination
// that pushes to `origin`.
#[tokio::test(flavor = "multi_thread")]
async fn other_remote_is_not_followed() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    assert_eq!(fx.push_now().await.push.unwrap().state(), PushState::Pushed);
    let upstream = fx.daemon.tmp.path().join("upstream.git");
    let status = Command::new("git")
        .args(["clone", "--bare", "--quiet"])
        .arg(&fx.remote)
        .arg(&upstream)
        .status()
        .unwrap();
    assert!(status.success());
    git(
        &fx.store,
        &["remote", "add", "upstream", upstream.to_str().unwrap()],
    );
    // Someone pushes to upstream's main.
    let index = fx.daemon.tmp.path().join("upstream-index");
    git_at(&upstream, Some(&index), &["read-tree", &fx.base], None);
    let tree = git_at(&upstream, Some(&index), &["write-tree"], None);
    let theirs = git_at(
        &upstream,
        None,
        &["commit-tree", &tree, "-p", &fx.base, "-m", "Upstream"],
        None,
    );
    git(&upstream, &["update-ref", "refs/heads/main", &theirs]);
    let fetched = fx
        .client()
        .fetch_git_remote(v1::FetchGitRemoteRequest {
            repo: fx.repo.id.clone(),
            git_remote: "upstream".to_owned(),
            branches: vec!["main".to_owned()],
            ..v1::FetchGitRemoteRequest::default()
        })
        .await
        .unwrap()
        .fetched
        .unwrap();
    assert!(!fetched.branches[0].local_moved, "{fetched:#?}");
    assert_eq!(fx.main().await.commit_id, fx.base);
}

// 6. Three changes in one batch: one land operation, one push.
#[tokio::test(flavor = "multi_thread")]
async fn batch_is_one_push() {
    if !has_git() {
        return;
    }
    let policy = POLICY.replace(
        "checks = \"proposal\"",
        "checks = \"proposal\"\nbatch_max = 3\nbatch_wait = \"3s\"",
    );
    let fx = Fixture::with_policy(&policy, with_identity).await;
    let mut changes = Vec::new();
    for file in ["a.txt", "b.txt", "c.txt"] {
        let change = fx.change(&fx.base, file).await;
        fx.land_waiting(&change.commit_id, 0).await.unwrap();
        changes.push(change);
    }
    let pushed = fx.wait_for(PushState::Pushed).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let landings = fx
            .client()
            .list_landings(
                v1::ListLandingsRequest {
                    repo: fx.repo.id.clone(),
                    ..v1::ListLandingsRequest::default()
                },
                0,
            )
            .await
            .unwrap()
            .items;
        let carried = landings
            .iter()
            .filter(|landing| {
                landing
                    .push
                    .as_ref()
                    .is_some_and(|push| push.state() == PushState::Pushed)
            })
            .count();
        if carried == 3 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{landings:#?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(fx.publish_operations().await, 1);
    assert_eq!(fx.remote_main(), pushed.local_commit_id);
}

// 13. No loop: after a landing and its push, nothing else is written.
#[tokio::test(flavor = "multi_thread")]
async fn no_loop() {
    if !has_git() {
        return;
    }
    let fx = Fixture::new().await;
    let change = fx.change(&fx.base, "a.txt").await;
    fx.land(&change.commit_id).await.unwrap();
    fx.wait_for(PushState::Pushed).await;
    let before = fx.operation_count().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(fx.operation_count().await, before);
    assert_eq!(fx.publish_operations().await, 1);
}
