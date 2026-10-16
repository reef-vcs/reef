//! Materializing a commit's tree through jj's working copy for the base
//! cache (R1.02, ADR 0015), and reading back the `tree_state` jj writes.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::UNIX_EPOCH;

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::materialize::{TREE_STATE_FILE, is_tree_key, tree_key, tree_state_entries};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CommitIdHex, MaterializeError, Repo};

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(path, contents).expect("write file");
}

/// A repository at `<tmp>/main` with a few files, an executable, and a
/// symlink snapshotted; returns it and the snapshot's commit.
async fn repo_with_files(tmp: &Path) -> (Repo, CommitIdHex) {
    let root = tmp.join("main");
    std::fs::create_dir(&root).unwrap();
    let mut repo = Repo::init_internal_git(&root, &settings()).await.unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "dir/b.txt", "bb\n");
    write(&root, "dir/deeper/c.sh", "#!/bin/sh\n");
    let script = root.join("dir/deeper/c.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    let outcome = repo.snapshot("snapshot").await.unwrap();
    assert!(outcome.changed);
    (repo, outcome.wc_commit)
}

fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    let since = metadata
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_millis()).unwrap()
}

#[tokio::test]
async fn materialize_writes_the_tree_and_its_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_files(tmp.path()).await;
    let dir = tmp.path().join("bases/t.tmp");
    let state_dir = tmp.path().join("bases/t.tmp.state");

    let stats = repo
        .materialize_tree(&commit, &dir, &state_dir)
        .await
        .unwrap();
    assert_eq!(stats.files, 4);
    assert_eq!(stats.checkout.added_files, 4);
    assert_eq!(stats.checkout.skipped_files, 0);
    assert_eq!(stats.bytes, 2 + 3 + 10 + "a.txt".len() as u64);

    assert_eq!(
        std::fs::read_to_string(dir.join("dir/b.txt")).unwrap(),
        "bb\n"
    );
    assert_eq!(
        std::fs::read_link(dir.join("link")).unwrap(),
        Path::new("a.txt")
    );
    let mode = std::fs::metadata(dir.join("dir/deeper/c.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "executable bit kept");
    assert!(!dir.join(".jj").exists(), "only the tree is written");
    assert!(state_dir.join(TREE_STATE_FILE).is_file());
    assert!(state_dir.join("checkout").is_file());

    let entries = tree_state_entries(&state_dir).unwrap();
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "dir/b.txt", "dir/deeper/c.sh", "link"]);
    for entry in &entries {
        let metadata = std::fs::symlink_metadata(dir.join(&entry.path)).unwrap();
        assert_eq!(entry.size, metadata.len(), "{}", entry.path);
        assert_eq!(entry.mtime_ms, mtime_ms(&metadata), "{}", entry.path);
    }

    let key = repo.tree_key(&commit).await.unwrap();
    assert!(is_tree_key(&key), "{key}");
    assert!(!key.contains('+'), "a resolved tree has one id: {key}");
}

#[tokio::test]
async fn materialize_refuses_a_non_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_files(tmp.path()).await;
    let dir = tmp.path().join("occupied");
    write(&dir, "stray", "x");

    let error = repo
        .materialize_tree(&commit, &dir, &tmp.path().join("state"))
        .await
        .unwrap_err();
    assert!(matches!(error, MaterializeError::NotEmpty(_)), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(dir.join("stray")).unwrap(), "x");

    let missing: CommitIdHex = "1".repeat(40).parse().unwrap();
    let error = repo
        .materialize_tree(&missing, &tmp.path().join("d2"), &tmp.path().join("s2"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error:?}");
}

#[tokio::test]
async fn materialize_conflicted_tree_has_joined_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("main");
    std::fs::create_dir(&root).unwrap();
    let mut reef_repo = Repo::init_internal_git(&root, &settings()).await.unwrap();

    // Two sides that both add f.txt with different contents, over a base
    // without it: a three-term conflict.
    let workspace = Workspace::load(
        &settings(),
        &root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    let repo = workspace.repo_loader().load_at_head().await.unwrap();
    let store = repo.store().clone();
    let path = RepoPathBuf::from_internal_string("f.txt").unwrap();
    let file = |id| {
        Some(TreeValue::File {
            id,
            executable: false,
            copy_id: CopyId::placeholder(),
        })
    };
    let left = store.write_file(&path, &mut &b"left\n"[..]).await.unwrap();
    let right = store.write_file(&path, &mut &b"right\n"[..]).await.unwrap();
    let mut builder = MergedTreeBuilder::new(store.empty_merged_tree());
    builder.set_or_remove(path, Merge::from_vec(vec![file(left), None, file(right)]));
    let tree = builder.write_tree().await.unwrap();
    assert!(!tree.tree_ids().is_resolved());
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![store.root_commit_id().clone()], tree)
        .write()
        .await
        .unwrap();
    tx.commit("conflicted commit").await.unwrap();
    reef_repo.reload_at_head().await.unwrap();

    let key = tree_key(&commit);
    assert_eq!(key.split('+').count(), 3, "{key}");
    assert!(is_tree_key(&key), "{key}");
    let commit_hex: CommitIdHex = commit.id().hex().parse().unwrap();
    assert_eq!(reef_repo.tree_key(&commit_hex).await.unwrap(), key);

    let dir = tmp.path().join("base");
    let state_dir = tmp.path().join("base.state");
    let stats = reef_repo
        .materialize_tree(&commit_hex, &dir, &state_dir)
        .await
        .unwrap();
    assert_eq!(stats.files, 1);
    let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
    assert!(text.contains("<<<<<<<"), "{text}");
    assert!(text.contains("left") && text.contains("right"), "{text}");
    let entries = tree_state_entries(&state_dir).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].size, text.len() as u64);
}
