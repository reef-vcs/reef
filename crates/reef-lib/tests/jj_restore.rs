//! Working copies after a restore from backup (R1.18): the `default`
//! working copy recreated at a new root from a copy of `.jj/repo`, and a
//! Reef workspace given a new working copy when its state directory is gone.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;

use jj_lib::settings::UserSettings;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CreateWorkspaceError, RecreateWorkingCopyError, Repo, StartPoint};

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

/// Copies `from` to `to` (which must not exist) as a backup copies
/// `.jj/repo`: everything but the index's contents, whose `type` file
/// stays beside empty directories for jj to rebuild into.
fn copy_repo(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() == "index" {
                std::fs::create_dir(&dst).unwrap();
                std::fs::copy(src.join("type"), dst.join("type")).unwrap();
                for dir in ["op_links", "segments", "changed_paths"] {
                    std::fs::create_dir(dst.join(dir)).unwrap();
                }
            } else {
                copy_repo(&src, &dst);
            }
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A repository at `main/` with two files snapshotted.
async fn repo_with_files(dir: &Path) -> Repo {
    let root = dir.join("main");
    std::fs::create_dir(&root).unwrap();
    let mut repo = Repo::init_internal_git(&root, &settings()).await.unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "dir/b.txt", "b\n");
    assert!(repo.snapshot("snapshot").await.unwrap().changed);
    repo
}

#[tokio::test]
async fn default_working_copy_is_recreated_at_a_new_root() {
    let dir = tempfile::tempdir().unwrap();
    let main = repo_with_files(dir.path()).await;
    let wc = main.wc_change().unwrap();
    let other = dir.path().join("other");
    copy_repo(main.repo_path(), &other.join(".jj/repo"));
    drop(main);

    let (restored, summary) = Repo::recreate_default_working_copy(&other, &settings())
        .await
        .unwrap();
    assert_eq!(summary.added, 2);
    assert_eq!(std::fs::read_to_string(other.join("a.txt")).unwrap(), "a\n");
    assert_eq!(
        std::fs::read_to_string(other.join("dir/b.txt")).unwrap(),
        "b\n"
    );
    assert_eq!(
        std::fs::read_to_string(other.join(".jj/working_copy/type")).unwrap(),
        "local"
    );
    assert_eq!(restored.wc_change().unwrap().commit_id, wc.commit_id);
    drop(restored);

    // It opens as any jj workspace does, and a snapshot finds nothing new.
    let mut reopened = Repo::open(&other, &settings()).await.unwrap();
    assert_eq!(reopened.workspace_name(), "default");
    assert!(!reopened.snapshot("snapshot").await.unwrap().changed);

    let error = Repo::recreate_default_working_copy(&other, &settings())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, RecreateWorkingCopyError::Exists(_)),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
}

#[tokio::test]
async fn a_workspace_whose_state_is_gone_gets_a_new_working_copy() {
    let dir = tempfile::tempdir().unwrap();
    let mut main = repo_with_files(dir.path()).await;
    let base = main.wc_change().unwrap().commit_id;
    let (root, state) = (dir.path().join("ws"), dir.path().join("wc"));
    std::fs::create_dir(&root).unwrap();
    let (ws, _) = main
        .create_workspace("reef-1", &root, &state, StartPoint::NewOn(base), "create")
        .await
        .unwrap();
    let commit = ws.wc_change().unwrap().commit_id;
    drop(ws);
    std::fs::remove_dir_all(&state).unwrap();
    let op_before = main.op_id();

    let mut attached = main
        .attach_workspace_with("reef-1", &root, &state, &settings())
        .unwrap();
    assert!(state.join("checkout").is_file());
    attached.reset_to(&commit).await.unwrap();
    assert_eq!(attached.wc_change().unwrap().commit_id, commit);
    assert_eq!(main.op_id(), op_before, "no operation is written");
    drop(attached);
    assert!(main.open_workspace(&root, &state).is_ok());

    let error = main
        .attach_workspace_with("reef-2", &root, &dir.path().join("wc2"), &settings())
        .err()
        .unwrap();
    assert!(
        matches!(error, CreateWorkspaceError::NoSuchWorkspace(_)),
        "{error:?}"
    );
    assert!(!dir.path().join("wc2").exists());
    let error = main
        .attach_workspace_with("reef-1", &root, &state, &settings())
        .err()
        .unwrap();
    assert!(
        matches!(error, CreateWorkspaceError::StateDirExists(_)),
        "{error:?}"
    );
}
