//! Snapshots narrowed to reported paths (R1.30, ADR 0026): what a narrowed
//! scan covers, and what it leaves behind for the next snapshot. The
//! daemon's half, which paths it passes and when, is tested in
//! `reef-daemon/tests/narrowed.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::object_id::ObjectId as _;
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CommitIdHex, FullScanReason, IgnoreSources, Repo, RepoPath, commit_snapshot};
use reef_lib::jj::{OpKind, OpMeta};
use tempfile::TempDir;

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).expect("write file");
}

fn settings() -> jj_lib::settings::UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

async fn init() -> (TempDir, Repo) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo::init_internal_git(dir.path(), &settings())
        .await
        .expect("init repo");
    (dir, repo)
}

/// A snapshot narrowed to `paths`, recorded as the daemon records one.
/// Returns whether it was narrowed and whether it changed anything.
async fn narrowed_snapshot(repo: &mut Repo, paths: &[&str]) -> (bool, bool) {
    let prefixes: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let head = Arc::clone(repo.head());
    let pending = repo
        .begin_snapshot(None, None, &IgnoreSources::default(), Some(&prefixes))
        .await
        .unwrap();
    let (narrowed, changed) = (pending.narrowed(), pending.changed());
    if !changed {
        let op_id = pending.head_op().clone();
        pending.finish(&op_id).await.unwrap();
        return (narrowed, false);
    }
    let committed = commit_snapshot(
        &head,
        pending.plan(),
        &OpMeta::new(OpKind::Snapshot, "narrowed"),
    )
    .await
    .unwrap();
    pending.finish(&committed.op_id).await.unwrap();
    repo.set_head(committed.repo);
    (narrowed, true)
}

/// `path` in the working-copy commit, as text.
async fn file(repo: &Repo, path: &str) -> Option<String> {
    let commit: CommitIdHex = repo.wc_commit().unwrap().id().hex().parse().unwrap();
    repo.read_file_at(&commit, RepoPath::from_internal_string(path).unwrap())
        .await
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

// 15: the in-memory working copy keeps the tree it was locked with; what
// the daemon reads of it comes from disk, and the narrowed scan's tree state
// was saved.
#[tokio::test]
async fn in_memory_working_copy_follows_a_narrowed_snapshot() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "one\n");
    repo.snapshot("full").await.unwrap();
    write(&root, "a.txt", "two\n");
    assert_eq!(narrowed_snapshot(&mut repo, &["a.txt"]).await, (true, true));
    assert_eq!(file(&repo, "a.txt").await.as_deref(), Some("two\n"));
    assert!(repo.working_copy_matches_view().unwrap());
    let full = repo.snapshot("full again").await.unwrap();
    assert!(!full.changed, "the full scan found the narrowed one's tree");
    assert!(repo.working_copy_matches_view().unwrap());
}

// A write no path names waits for a later scan.
#[tokio::test]
async fn narrowed_scan_covers_only_the_reported_paths() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "one\n");
    write(&root, "b.txt", "one\n");
    repo.snapshot("full").await.unwrap();
    write(&root, "a.txt", "two\n");
    write(&root, "b.txt", "two\n");
    assert_eq!(narrowed_snapshot(&mut repo, &["a.txt"]).await, (true, true));
    assert_eq!(file(&repo, "a.txt").await.as_deref(), Some("two\n"));
    assert_eq!(file(&repo, "b.txt").await.as_deref(), Some("one\n"));
    assert!(repo.snapshot("full").await.unwrap().changed);
    assert_eq!(file(&repo, "b.txt").await.as_deref(), Some("two\n"));
}

// A reported directory covers everything under it, on disk and in the
// recorded tree: files added under a new directory, and files deleted with
// a removed one.
#[tokio::test]
async fn a_reported_directory_covers_everything_under_it() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "old/deep/1.txt", "x\n");
    write(&root, "old/2.txt", "x\n");
    write(&root, "kept.txt", "x\n");
    repo.snapshot("full").await.unwrap();
    std::fs::remove_dir_all(root.join("old")).unwrap();
    write(&root, "new/deep/3.txt", "y\n");
    assert_eq!(
        narrowed_snapshot(&mut repo, &["old", "new"]).await,
        (true, true)
    );
    assert_eq!(file(&repo, "old/deep/1.txt").await, None);
    assert_eq!(file(&repo, "old/2.txt").await, None);
    assert_eq!(file(&repo, "new/deep/3.txt").await.as_deref(), Some("y\n"));
    assert_eq!(file(&repo, "kept.txt").await.as_deref(), Some("x\n"));
    assert!(!repo.snapshot("full").await.unwrap().changed);
}

// The root, or a path outside it, scans everything.
#[tokio::test]
async fn a_path_that_cannot_be_narrowed_to_scans_everything() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "one\n");
    assert_eq!(narrowed_snapshot(&mut repo, &[""]).await, (false, true));
    write(&root, "a.txt", "two\n");
    assert_eq!(
        narrowed_snapshot(&mut repo, &["../elsewhere"]).await,
        (false, true)
    );
    assert_eq!(file(&repo, "a.txt").await.as_deref(), Some("two\n"));
}

// 12, the jj half: on a case-insensitive volume a write through another
// spelling is found under the tracked one.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn another_spelling_is_resolved() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "README.md", "one\n");
    repo.snapshot("full").await.unwrap();
    if !root.join("readme.md").exists() {
        eprintln!("skipped: the temporary directory is case-sensitive");
        return;
    }
    write(&root, "readme.md", "two\n");
    assert_eq!(
        narrowed_snapshot(&mut repo, &["readme.md"]).await,
        (true, true)
    );
    assert_eq!(file(&repo, "README.md").await.as_deref(), Some("two\n"));
}

/// R2.23: why a scan was not narrowed, which the daemon reports as a
/// status's walk reason.
#[tokio::test]
async fn full_reason_says_why_a_scan_was_not_narrowed() {
    let (_dir, mut repo) = init().await;
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "one\n");
    let reason = async |repo: &mut Repo, prefixes: Option<Vec<PathBuf>>| {
        let pending = repo
            .begin_snapshot(None, None, &IgnoreSources::default(), prefixes.as_deref())
            .await
            .unwrap();
        let reason = pending.full_reason();
        assert_eq!(pending.narrowed(), reason.is_none());
        let op_id = pending.head_op().clone();
        if !pending.changed() {
            pending.finish(&op_id).await.unwrap();
        }
        reason
    };
    assert_eq!(
        reason(&mut repo, None).await,
        Some(FullScanReason::NotAsked)
    );
    assert_eq!(
        reason(&mut repo, Some(vec![PathBuf::from("../x")])).await,
        Some(FullScanReason::NotNarrowable)
    );
    assert_eq!(
        reason(&mut repo, Some(vec![PathBuf::from("a.txt")])).await,
        None
    );
}
