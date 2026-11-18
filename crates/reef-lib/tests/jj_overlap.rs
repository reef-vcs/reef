//! Overlaps (#263, ADR 0079) over jj-lib: the path cache's bounds, either
//! side cut at its per-commit limit, a cache cleared at its budget, and
//! commits landed on `main@origin` past a lagging local `main`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::time::UNIX_EPOCH;

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::transaction::Transaction;
use reef_lib::jj::Repo;
use reef_lib::jj::overlap::{self, PathCache, Policy};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::settings::{Identity, user_settings};

const EMAIL: &str = "reef-test@example.com";

/// A commit on `parent` writing each of `files` with `text`.
async fn write(tx: &mut Transaction, parent: &Commit, files: &[&str], text: &str) -> Commit {
    let store = tx.repo().store().clone();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(parent.tree());
    for file in files {
        let path = RepoPathBuf::from_internal_string(*file).unwrap();
        let id = store.write_file(&path, &mut text.as_bytes()).await.unwrap();
        builder.set_or_remove(
            path,
            Some(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
    }
    let tree = builder.write_tree().await.unwrap();
    tx.repo_mut()
        .new_commit(vec![parent.id().clone()], tree)
        .write()
        .await
        .unwrap()
}

#[tokio::test]
async fn cut_lists_and_a_cleared_cache_give_the_same_answer() {
    let dir = tempfile::tempdir().unwrap();
    let settings = user_settings(&Identity {
        name: "Reef Test".into(),
        email: EMAIL.into(),
    })
    .unwrap();
    let mut repo = Repo::init_internal_git(dir.path(), &settings)
        .await
        .unwrap();
    let mut tx = repo.head().start_transaction();
    let root = tx.repo().store().root_commit();
    let base = write(&mut tx, &root, &["base.txt"], "base\n").await;
    // Mine changes five paths; one other change shares only the last, `e`,
    // and another shares only the first, `a`.
    let mine = write(&mut tx, &base, &["a", "b", "c", "d", "e"], "mine\n").await;
    let last = write(&mut tx, &base, &["e", "z"], "last\n").await;
    let first = write(&mut tx, &base, &["a"], "first\n").await;
    // A second workspace changes one path, `y`; `many` changes three, so
    // cut at two its `y` is past the cut and asked of its tree.
    let small = write(&mut tx, &base, &["y"], "small\n").await;
    let many = write(&mut tx, &base, &["0", "1", "y"], "many\n").await;
    // Landed upstream: `main@origin` is ahead of the local `main`.
    let landed = write(&mut tx, &base, &["b"], "landed\n").await;
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("main"), RefTarget::normal(base.id().clone()));
    tx.repo_mut().set_remote_bookmark(
        RefName::new("main").to_remote_symbol(RemoteName::new("origin")),
        RemoteRef {
            target: RefTarget::normal(landed.id().clone()),
            state: RemoteRefState::Tracked,
        },
    );
    tx.repo_mut()
        .set_wc_commit("me".into(), mine.id().clone())
        .unwrap();
    tx.repo_mut()
        .set_wc_commit("small".into(), small.id().clone())
        .unwrap();
    let head = tx.commit("fixture").await.unwrap();
    repo.set_head(std::sync::Arc::clone(&head));

    let root = dir.path();
    let always = |_: &str| false;
    let policy = Policy {
        since: UNIX_EPOCH,
        always: &always,
    };
    let answer = async |workspace: &str, cache: &PathCache| {
        let scope = RevsetScope {
            workspace: Some(workspace),
            user_email: EMAIL,
            root,
        };
        let found = overlap::find(&head, &scope, &policy, cache, &|| false)
            .await
            .unwrap();
        let mut seen: Vec<(String, Vec<String>)> = found
            .overlaps
            .iter()
            .map(|overlap| (overlap.commit_id.to_string(), overlap.paths.clone()))
            .collect();
        seen.sort();
        seen
    };
    // `landed` (on `main@origin`) changes `b` too, and is not named.
    let mut expected = vec![
        (last.id().hex(), vec!["e".to_owned()]),
        (first.id().hex(), vec!["a".to_owned()]),
    ];
    expected.sort();
    assert_eq!(answer("me", &PathCache::default()).await, expected);
    // Mine cut after `a` and `b`: `e` is past the cut and asked of its tree.
    assert_eq!(
        answer("me", &PathCache::with_limits(2, 1_000)).await,
        expected
    );
    // A budget of three paths clears the cache between commits.
    let cleared = PathCache::with_limits(10, 3);
    assert_eq!(answer("me", &cleared).await, expected);
    assert!(cleared.len() <= 5, "{}", cleared.len());
    // Theirs cut: `many` keeps `0` and `1`, and `y` is asked of its tree.
    let theirs = vec![(many.id().hex(), vec!["y".to_owned()])];
    assert_eq!(answer("small", &PathCache::default()).await, theirs);
    assert_eq!(
        answer("small", &PathCache::with_limits(2, 1_000)).await,
        theirs
    );
}
