//! Garbage collection of a repository (R1.17): abandoning old operations,
//! pruning the hidden commits only they named, and rebinding a working copy
//! to the rewritten head. The daemon's four-step GC is tested in
//! `reef-daemon/tests/gc.rs`; this covers the jj half on its own.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, SystemTime};

use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use reef_lib::jj::settings::{
    Fsmonitor, Identity, SettingsSpec, user_settings, user_settings_from_spec,
};
use reef_lib::jj::{CommitIdHex, Repo};
use tempfile::TempDir;

fn write(root: &Path, path: &str, contents: &str) {
    std::fs::write(root.join(path), contents).expect("write file");
}

async fn init() -> (TempDir, Repo) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo::init_internal_git(dir.path(), &settings())
        .await
        .expect("init repo");
    (dir, repo)
}

/// Whether the store on disk has the commit. A fresh open, because a
/// loaded store caches the commits it has read.
async fn has_commit(repo: &Repo, id: &CommitIdHex) -> bool {
    let fresh = Repo::open(repo.workspace_root(), &settings())
        .await
        .unwrap();
    let id = CommitId::try_from_hex(id.as_str()).unwrap();
    fresh.head().store().get_commit(&id).is_ok()
}

fn settings() -> jj_lib::settings::UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

/// Two snapshots, so the first one's commit is hidden; returns both
/// commits.
async fn two_snapshots(repo: &mut Repo) -> (CommitIdHex, CommitIdHex) {
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", &"first version\n".repeat(1000));
    let c1 = repo.snapshot("one").await.unwrap().wc_commit;
    write(&root, "a.txt", &"second version\n".repeat(1000));
    let c2 = repo.snapshot("two").await.unwrap().wc_commit;
    (c1, c2)
}

/// `git gc --prune=@<seconds>` keeps what was written in the current
/// second; wait it out so "now" covers everything written so far.
async fn past_this_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

#[tokio::test]
async fn gc_now_prunes_hidden_commits_and_keeps_the_view() {
    let (_dir, mut repo) = init().await;
    let (c1, c2) = two_snapshots(&mut repo).await;
    // The head operation records its rewrite's predecessor (c2), which the
    // rebuilt index keeps; c1 is named only by the operations abandoned.
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "third version\n");
    let c3 = repo.snapshot("three").await.unwrap().wc_commit;
    let heads_before = repo.heads().unwrap();
    past_this_second().await;

    let outcome = repo.gc(SystemTime::now()).await.unwrap();
    // The workspace's creation and the first two snapshots; the root stays.
    assert_eq!(outcome.operations_abandoned, 3, "{outcome:?}");
    assert_eq!(repo.op_id(), outcome.new_head);
    assert!(
        !has_commit(&repo, &c1).await,
        "the hidden snapshot is pruned"
    );
    assert!(
        has_commit(&repo, &c2).await,
        "the head's predecessor record keeps it"
    );
    assert!(
        has_commit(&repo, &c3).await,
        "the working-copy commit stays"
    );
    assert_eq!(repo.heads().unwrap(), heads_before, "the view is unchanged");
    let log = repo.op_log(100).await.unwrap();
    assert_eq!(log.len(), 2, "the head and the root: {log:?}");

    // The working copy still names an abandoned operation until rebound.
    assert_ne!(repo.working_copy_op_id(), outcome.new_head);
    assert!(repo.working_copy_matches_view().unwrap());
    repo.rebind_working_copy(&outcome.new_head).await.unwrap();
    assert_eq!(repo.working_copy_op_id(), outcome.new_head);
    write(&root, "b.txt", "after gc\n");
    let snapshot = repo.snapshot("three").await.unwrap();
    assert!(snapshot.changed);
}

#[tokio::test]
async fn gc_keeps_what_is_newer_than_keep_newer() {
    let (_dir, mut repo) = init().await;
    let (c1, c2) = two_snapshots(&mut repo).await;
    let ops_before = repo.op_log(100).await.unwrap().len();

    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    let outcome = repo.gc(hour_ago).await.unwrap();
    assert_eq!(outcome.operations_abandoned, 0);
    assert!(has_commit(&repo, &c1).await, "inside the window");
    assert!(has_commit(&repo, &c2).await);
    assert_eq!(repo.op_log(100).await.unwrap().len(), ops_before);
    assert_eq!(
        repo.working_copy_op_id(),
        outcome.new_head,
        "nothing to rebind"
    );
}

#[tokio::test]
async fn missing_git_fails_after_the_operation_store() {
    let (dir, repo) = init().await;
    drop(repo);
    let identity = Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    };
    let settings = user_settings_from_spec(&SettingsSpec {
        identity: &identity,
        max_new_file_size: 1 << 20,
        operation_hostname: "",
        operation_username: "",
        fsmonitor: Fsmonitor::None,
        git_executable: "/nonexistent/git",
    })
    .unwrap();
    let mut repo = Repo::open(dir.path(), &settings).await.unwrap();
    let (_c1, c2) = two_snapshots(&mut repo).await;
    past_this_second().await;

    let error = repo.gc(SystemTime::now()).await.unwrap_err();
    assert!(
        matches!(error, reef_lib::jj::GcError::Backend(_)),
        "{error:?}"
    );
    assert_eq!(
        repo.op_log(100).await.unwrap().len(),
        2,
        "the operations were abandoned before git gc ran"
    );
    assert!(has_commit(&repo, &c2).await);
    assert!(repo.working_copy_matches_view().unwrap());
}
