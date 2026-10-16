//! Integration tests for the jj-lib embedding spike (R0.04). Each test works
//! in its own temporary directory.

#![allow(
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::oplog::OpEntry;
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::snapshot::{ChangeKind, ChangedPath, SnapshotError, UntrackedReasonKind};
use reef_lib::jj::undo::{UNDO_OP_DESC_PREFIX, UndoError};
use reef_lib::jj::{OpenError, Repo};

const MIB: usize = 1 << 20;

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

async fn init_internal_git(root: &Path) {
    Workspace::init_internal_git(&settings(), root, gix::hash::Kind::Sha1)
        .await
        .expect("init internal git repo");
}

async fn init_colocated_git(root: &Path) {
    Workspace::init_colocated_git(&settings(), root, gix::hash::Kind::Sha1)
        .await
        .expect("init colocated git repo");
}

async fn open(root: &Path) -> Repo {
    Repo::open(root, &settings()).await.expect("open repo")
}

fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(path, contents).expect("write file");
}

fn changed(path: &str, kind: ChangeKind) -> ChangedPath {
    ChangedPath {
        path: path.into(),
        kind,
    }
}

/// Descriptions of the operations a fresh repository starts with, newest
/// first. jj-lib writes "add workspace 'default'" on top of the root operation.
fn assert_fresh_op_log(ops: &[OpEntry]) {
    let descriptions: Vec<_> = ops.iter().map(|op| op.description.as_str()).collect();
    assert_eq!(descriptions, ["add workspace 'default'", ""]);
    assert!(
        ops[1].parents.is_empty(),
        "the last entry is the root operation"
    );
}

#[tokio::test]
async fn open_internal_git_repo() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;

    let repo = open(dir.path()).await;
    assert_fresh_op_log(&repo.op_log(10).await.unwrap());
    let heads = repo.heads().unwrap();
    assert_eq!(heads.len(), 1);
    assert!(heads[0].is_wc);
    assert_eq!(heads[0].description, "");
    assert!(repo.repo_path().ends_with(".jj/repo"));
    assert!(!dir.path().join(".git").exists());
}

#[tokio::test]
async fn init_internal_git_creates_an_openable_repo() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "existing.txt", "here before init\n");

    let created = Repo::init_internal_git(dir.path(), &settings())
        .await
        .unwrap();
    assert_fresh_op_log(&created.op_log(10).await.unwrap());
    assert!(!dir.path().join(".git").exists());

    let mut repo = open(dir.path()).await;
    assert_eq!(repo.op_id(), created.op_id());
    assert_eq!(repo.changed_paths_in_wc().await.unwrap(), []);
    assert!(repo.snapshot("test").await.unwrap().changed);
    assert_eq!(
        repo.changed_paths_in_wc().await.unwrap(),
        [changed("existing.txt", ChangeKind::Added)]
    );

    let err = Repo::init_internal_git(dir.path(), &settings())
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{err}");
}

#[tokio::test]
async fn open_colocated_repo() {
    let dir = tempfile::tempdir().unwrap();
    init_colocated_git(dir.path()).await;

    let repo = open(dir.path()).await;
    assert_fresh_op_log(&repo.op_log(10).await.unwrap());
    let heads = repo.heads().unwrap();
    assert_eq!(heads.len(), 1);
    assert!(heads[0].is_wc);
    assert!(dir.path().join(".git").exists());
}

#[tokio::test]
async fn open_from_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let sub = dir.path().join("sub").join("dir");
    std::fs::create_dir_all(&sub).unwrap();

    let repo = open(&sub).await;
    assert_eq!(
        repo.workspace_root().canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[tokio::test]
async fn open_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    // A path relative to the test's current directory, without changing it
    // (the current directory is shared by every test in the process).
    let cwd = std::env::current_dir().unwrap();
    let mut relative = std::path::PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(sub.strip_prefix("/").unwrap());
    assert!(relative.is_relative());

    let repo = open(&relative).await;
    assert!(repo.workspace_root().is_absolute());
    assert_eq!(
        repo.workspace_root().canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[test]
fn repo_is_send() {
    // The daemon moves a `Repo` onto a blocking thread (see `reef_lib::jj`).
    // The futures are not `Send` and `Repo` is not `Sync`; ADR 0008.
    fn assert_send<T: Send>() {}
    assert_send::<Repo>();
}

#[tokio::test]
async fn open_non_repo_fails() {
    let dir = tempfile::tempdir().unwrap();
    let err = Repo::open(dir.path(), &settings()).await.err().unwrap();
    assert!(matches!(err, OpenError::NotAJjRepo(path) if path == dir.path()));
}

#[tokio::test]
async fn snapshot_records_new_file() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    write(dir.path(), "a.txt", "hello\n");

    let first = repo.snapshot("test").await.unwrap();
    assert!(first.changed);
    assert_eq!(first.untracked, []);
    let head = &repo.op_log(1).await.unwrap()[0];
    assert_eq!(head.id, first.op_id);
    assert_eq!(head.description, "test");
    assert!(head.is_snapshot);
    assert_eq!(head.workspace.as_deref(), Some("default"));
    assert_eq!(
        repo.changed_paths_in_wc().await.unwrap(),
        [changed("a.txt", ChangeKind::Added)]
    );

    let second = repo.snapshot("test").await.unwrap();
    assert!(!second.changed);
    assert_eq!(second.op_id, first.op_id);
    assert_eq!(second.wc_commit, first.wc_commit);
}

#[tokio::test]
async fn snapshot_respects_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    write(dir.path(), ".gitignore", "ignored/\n");
    write(dir.path(), "ignored/secret.txt", "no\n");
    write(dir.path(), "kept.txt", "yes\n");

    repo.snapshot("test").await.unwrap();
    assert_eq!(
        repo.changed_paths_in_wc().await.unwrap(),
        [
            changed(".gitignore", ChangeKind::Added),
            changed("kept.txt", ChangeKind::Added),
        ]
    );
}

#[tokio::test]
async fn snapshot_reports_large_file_as_untracked() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;

    write(dir.path(), "big.bin", vec![b'x'; 2 * MIB]);
    let outcome = repo.snapshot("test").await.unwrap();
    assert!(!outcome.changed);
    assert_eq!(
        outcome.untracked,
        [(
            "big.bin".to_owned(),
            UntrackedReasonKind::FileTooLarge {
                size: 2 << 20,
                max_size: 1 << 20,
            }
        )]
    );

    // A file that is already tracked is snapshotted whatever its size.
    write(dir.path(), "grows.bin", "small\n");
    assert!(repo.snapshot("test").await.unwrap().changed);
    write(dir.path(), "grows.bin", vec![b'y'; 2 * MIB]);
    let outcome = repo.snapshot("test").await.unwrap();
    assert!(outcome.changed);
    assert!(
        outcome
            .untracked
            .iter()
            .all(|(path, _)| path != "grows.bin")
    );
    assert_eq!(
        repo.changed_paths_in_wc().await.unwrap(),
        [changed("grows.bin", ChangeKind::Added)]
    );
}

#[tokio::test]
async fn op_log_walks_parents_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    let mut snapshot_ops = Vec::new();
    for i in 0..3 {
        write(dir.path(), &format!("f{i}.txt"), "x\n");
        snapshot_ops.push(repo.snapshot("test").await.unwrap().op_id);
    }

    let ops = repo.op_log(10).await.unwrap();
    // Three snapshots, "add workspace 'default'", and the root operation.
    assert_eq!(ops.len(), 5);
    let ids: Vec<_> = ops[..3].iter().map(|op| op.id.clone()).collect();
    assert_eq!(ids, snapshot_ops.iter().rev().cloned().collect::<Vec<_>>());
    for pair in ops.windows(2) {
        assert_eq!(pair[0].parents, [pair[1].id.clone()]);
        assert!(pair[0].end >= pair[1].end);
    }
    assert_eq!(repo.op_log(2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn undo_restores_previous_view_and_disk() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    let before = repo.op_id();
    write(dir.path(), "a.txt", "hello\n");
    let snapshot = repo.snapshot("test").await.unwrap();

    let outcome = repo.undo_last().await.unwrap();
    assert_eq!(outcome.snapshotted, None);
    assert_eq!(outcome.undone, snapshot.op_id);
    assert_eq!(outcome.restored_to, before);
    let head = &repo.op_log(1).await.unwrap()[0];
    assert_eq!(head.id, outcome.new_op);
    assert_eq!(
        head.description,
        format!("{UNDO_OP_DESC_PREFIX}{}", outcome.restored_to)
    );
    assert!(!head.is_snapshot);
    assert_eq!(repo.changed_paths_in_wc().await.unwrap(), []);
    assert!(!dir.path().join("a.txt").exists());

    let after = repo.snapshot("test").await.unwrap();
    assert!(!after.changed);
    assert_eq!(after.op_id, outcome.new_op);
}

#[tokio::test]
async fn undo_snapshots_unsaved_edits_first() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    write(dir.path(), "a.txt", "v1\n");
    let first = repo.snapshot("test").await.unwrap();
    write(dir.path(), "a.txt", "v2\n");

    let outcome = repo.undo_last().await.unwrap();
    let snapshotted = outcome
        .snapshotted
        .clone()
        .expect("the v2 edit was snapshotted");
    assert_eq!(outcome.undone, snapshotted);
    assert_eq!(outcome.restored_to, first.op_id);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "v1\n"
    );
    // The edit is not lost: its snapshot is still in the operation log.
    let ops = repo.op_log(10).await.unwrap();
    assert!(ops.iter().any(|op| op.id == snapshotted && op.is_snapshot));
}

#[tokio::test]
async fn undo_refuses_to_remove_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut repo = open(dir.path()).await;
    let before = repo.op_id();

    let err = repo.undo_last().await.err().unwrap();
    assert!(matches!(err, UndoError::WorkspaceNotInRestoredView(name) if name == "default"));
    assert_eq!(repo.op_id(), before, "nothing was written");
    write(dir.path(), "a.txt", "hello\n");
    assert!(repo.snapshot("test").await.unwrap().changed);
}

#[tokio::test]
async fn stale_working_copy_detected() {
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    let mut first = open(dir.path()).await;
    let mut second = open(dir.path()).await;
    write(dir.path(), "a.txt", "hello\n");
    let snapshot = first.snapshot("test").await.unwrap();

    let err = second.snapshot("test").await.err().unwrap();
    match err {
        SnapshotError::Stale { wc_op, repo_op } => {
            assert_eq!(wc_op, snapshot.op_id);
            assert_eq!(repo_op, second.op_id());
        }
        other => panic!("expected Stale, got {other:?}"),
    }

    second.reload_at_head().await.unwrap();
    assert_eq!(second.op_id(), snapshot.op_id);
    write(dir.path(), "b.txt", "world\n");
    assert!(second.snapshot("test").await.unwrap().changed);
}

#[tokio::test]
async fn open_repo_created_by_jj_cli() {
    if Command::new("jj").arg("--version").output().is_err() {
        println!("skipped: jj not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let jj = |args: &[&str]| {
        let output = Command::new("jj")
            .args(args)
            .current_dir(dir.path())
            // Keep the user's jj config out of the test.
            .env("JJ_CONFIG", dir.path().join("no-jj-config.toml"))
            .env("JJ_USER", "Reef Test")
            .env("JJ_EMAIL", "reef-test@example.com")
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    jj(&["git", "init", "--colocate"]);
    write(dir.path(), "from-cli.txt", "hello\n");
    jj(&["status"]);

    let repo = open(dir.path()).await;
    assert_eq!(
        repo.changed_paths_in_wc().await.unwrap(),
        [changed("from-cli.txt", ChangeKind::Added)]
    );
    let ops = repo.op_log(10).await.unwrap();
    assert!(
        ops.iter()
            .any(|op| op.description == "snapshot working copy" && op.is_snapshot),
        "jj's own snapshot operation is in the log: {ops:#?}"
    );
}

/// Measurements for ADR 0008. Run by hand:
/// `cargo test --release -p reef-lib --test jj_spike -- --ignored timing_report --nocapture`
#[tokio::test]
#[ignore = "measurement, run by hand"]
async fn timing_report() {
    const FILES: usize = 10_000;
    const DIRS: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    init_internal_git(dir.path()).await;
    for i in 0..FILES {
        write(
            dir.path(),
            &format!("d{:03}/f{i:05}.txt", i % DIRS),
            format!("file {i}\n").repeat(20),
        );
    }

    let mut rows: Vec<(String, Duration)> = Vec::new();
    let start = Instant::now();
    let mut repo = open(dir.path()).await;
    rows.push(("open".into(), start.elapsed()));

    let start = Instant::now();
    assert!(repo.snapshot("test").await.unwrap().changed);
    rows.push((
        format!("first snapshot ({FILES} new files)"),
        start.elapsed(),
    ));

    let start = Instant::now();
    assert!(!repo.snapshot("test").await.unwrap().changed);
    rows.push(("no-op snapshot".into(), start.elapsed()));

    for i in 0..50 {
        write(
            dir.path(),
            &format!("d{:03}/f{i:05}.txt", i % DIRS),
            "modified\n",
        );
    }
    let start = Instant::now();
    assert!(repo.snapshot("test").await.unwrap().changed);
    rows.push(("snapshot, 50 files modified".into(), start.elapsed()));

    let start = Instant::now();
    let (mut scan, mut write_commit, mut commit_op) =
        (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    for i in 0..100 {
        write(dir.path(), "counter.txt", format!("{i}\n"));
        let outcome = repo.snapshot("test").await.unwrap();
        assert!(outcome.changed);
        scan += outcome.stats.scan;
        write_commit += outcome.stats.write;
        commit_op += outcome.stats.commit;
    }
    rows.push((
        "snapshot, 1 file modified (mean of 100)".into(),
        start.elapsed() / 100,
    ));
    rows.push(("  of which scan".into(), scan / 100));
    rows.push((
        "  of which write commit and rebase".into(),
        write_commit / 100,
    ));
    rows.push((
        "  of which commit operation and save state".into(),
        commit_op / 100,
    ));

    let start = Instant::now();
    assert_eq!(repo.op_log(100).await.unwrap().len(), 100);
    rows.push(("op_log(100)".into(), start.elapsed()));

    let start = Instant::now();
    repo.undo_last().await.unwrap();
    rows.push(("undo_last".into(), start.elapsed()));

    println!("| Step | Time |\n| --- | --- |");
    for (step, time) in rows {
        println!("| {step} | {:.1} ms |", time.as_secs_f64() * 1000.0);
    }
}
