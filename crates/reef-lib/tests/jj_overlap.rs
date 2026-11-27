//! Overlaps (#263, ADR 0079) over jj-lib: the path cache's bounds, either
//! side cut at its per-commit limit, a cache cleared at its budget, and
//! commits landed on `main@origin` past a lagging local `main`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::transaction::Transaction;
use reef_lib::jj::Repo;
use reef_lib::jj::overlap::{self, PathCache, Policy};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::settings::{Identity, user_settings};

const EMAIL: &str = "reef-test@example.com";

/// A commit on `parent` writing each of `files` with `text`.
async fn write(tx: &mut Transaction, parent: &Commit, files: &[&str], text: &str) -> Commit {
    let files: Vec<(&str, &str)> = files.iter().map(|file| (*file, text)).collect();
    write_files(tx, parent, &files).await
}

/// A commit on `parent` writing each `(path, text)`.
async fn write_files(tx: &mut Transaction, parent: &Commit, files: &[(&str, &str)]) -> Commit {
    let store = tx.repo().store().clone();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(parent.tree());
    for (file, text) in files {
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
    repo.set_head(Arc::clone(&head));

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

/// A base, a lower mutable change, a tip whose tree is trunk's, and a `me`
/// working copy on the base sharing the lower change's path.
struct Case {
    dir: tempfile::TempDir,
    repo: Repo,
    head: Arc<ReadonlyRepo>,
    base: Commit,
    lower: Commit,
    tip: Commit,
}

async fn case() -> Case {
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
    let lower = write(&mut tx, &base, &["a-only.txt"], "a\n").await;
    let tip_files = [
        ("a-only.txt", "x\n"),
        ("other.txt", "o\n"),
        ("tip.txt", "t\n"),
    ];
    let tip = write_files(&mut tx, &lower, &tip_files).await;
    let trunk = write_files(&mut tx, &base, &tip_files).await;
    let me = write(&mut tx, &base, &["a-only.txt"], "mine\n").await;
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("main"), RefTarget::normal(trunk.id().clone()));
    tx.repo_mut()
        .set_wc_commit("me".into(), me.id().clone())
        .unwrap();
    let head = tx.commit("case").await.unwrap();
    repo.set_head(Arc::clone(&head));
    Case {
        dir,
        repo,
        head,
        base,
        lower,
        tip,
    }
}

async fn find_ids(
    head: &ReadonlyRepo,
    root: &Path,
    cache: &PathCache,
    should_stop: &dyn Fn() -> bool,
) -> Vec<String> {
    let always = |_: &str| false;
    let policy = Policy {
        since: UNIX_EPOCH,
        always: &always,
    };
    let scope = RevsetScope {
        workspace: Some("me"),
        user_email: EMAIL,
        root,
    };
    let found = overlap::find(head, &scope, &policy, cache, should_stop)
        .await
        .unwrap();
    let mut ids: Vec<String> = found
        .overlaps
        .iter()
        .map(|overlap| overlap.commit_id.to_string())
        .collect();
    ids.sort();
    ids
}

async fn abandon(repo: &mut Repo, commit: &Commit) -> Arc<ReadonlyRepo> {
    let mut tx = repo.head().start_transaction();
    tx.repo_mut().record_abandoned_commit(commit);
    tx.repo_mut().rebase_descendants().await.unwrap();
    let head = tx.commit("abandon").await.unwrap();
    repo.set_head(Arc::clone(&head));
    head
}

async fn move_main_to(repo: &mut Repo, commit: &Commit) -> Arc<ReadonlyRepo> {
    let mut tx = repo.head().start_transaction();
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("main"), RefTarget::normal(commit.id().clone()));
    let head = tx.commit("move main").await.unwrap();
    repo.set_head(Arc::clone(&head));
    head
}

async fn tip_on(repo: &mut Repo, lower: &Commit) -> (Arc<ReadonlyRepo>, Commit) {
    let mut tx = repo.head().start_transaction();
    let tip = write_files(
        &mut tx,
        lower,
        &[
            ("a-only.txt", "x\n"),
            ("other.txt", "o\n"),
            ("tip.txt", "t\n"),
        ],
    )
    .await;
    let head = tx.commit("tip").await.unwrap();
    repo.set_head(Arc::clone(&head));
    (head, tip)
}

#[tokio::test]
async fn the_landed_cache_misses_when_the_mutable_set_changes() {
    let mut case = case().await;
    let lower = case.lower.id().hex();
    let cache = PathCache::default();
    let first = find_ids(&case.head, case.dir.path(), &cache, &|| false).await;
    assert!(!first.contains(&lower), "{first:?}");
    // A new operation: the tip is abandoned, trunk does not move.
    let head = abandon(&mut case.repo, &case.tip).await;
    let second = find_ids(&head, case.dir.path(), &cache, &|| false).await;
    assert!(second.contains(&lower), "{second:?}");
}

#[tokio::test]
async fn the_landed_cache_misses_when_trunk_moves() {
    let mut case = case().await;
    let lower = case.lower.id().hex();
    let cache = PathCache::default();
    let first = find_ids(&case.head, case.dir.path(), &cache, &|| false).await;
    assert!(!first.contains(&lower), "{first:?}");
    // The trunk moves back before the matching commit.
    let head = move_main_to(&mut case.repo, &case.base).await;
    let second = find_ids(&head, case.dir.path(), &cache, &|| false).await;
    assert!(second.contains(&lower), "{second:?}");
}

#[tokio::test]
async fn one_landed_cache_serves_two_repositories() {
    let a = case().await;
    let mut b = case().await;
    let b_head = move_main_to(&mut b.repo, &b.base).await;
    let a_lower = a.lower.id().hex();
    let b_lower = b.lower.id().hex();
    let cache = PathCache::default();
    let first = find_ids(&a.head, a.dir.path(), &cache, &|| false).await;
    assert!(!first.contains(&a_lower), "{first:?}");
    let second = find_ids(&b_head, b.dir.path(), &cache, &|| false).await;
    assert!(second.contains(&b_lower), "{second:?}");
    let third = find_ids(&a.head, a.dir.path(), &cache, &|| false).await;
    assert!(!third.contains(&a_lower), "{third:?}");
}

#[tokio::test]
async fn the_landed_cache_is_bounded_across_operations() {
    let mut case = case().await;
    let lower = case.lower.id().hex();
    let cache = PathCache::default();
    let mut head = Arc::clone(&case.head);
    let mut tip = Some(case.tip.clone());
    let mut dropped = true;
    // Seventeen operations, one more than the landed cache keeps, with the
    // tip appearing and disappearing so a stale entry would answer wrongly.
    for round in 0..17 {
        let ids = find_ids(&head, case.dir.path(), &cache, &|| false).await;
        assert_eq!(ids.contains(&lower), !dropped, "round {round}: {ids:?}");
        if dropped {
            head = abandon(&mut case.repo, tip.take().as_ref().unwrap()).await;
            dropped = false;
        } else {
            let (next, new_tip) = tip_on(&mut case.repo, &case.lower).await;
            head = next;
            tip = Some(new_tip);
            dropped = true;
        }
    }
}

#[tokio::test]
async fn a_cut_landed_read_is_not_cached() {
    let case = case().await;
    let lower = case.lower.id().hex();
    // Cut at each call count until one cuts the landed read and leaves the
    // change listed. A complete read at the same head and cache must then
    // drop it: a stored cut result would leave it listed.
    let mut cut_the_read = false;
    for cut_at in 1..=20 {
        let cache = PathCache::default();
        let calls = AtomicUsize::new(0);
        let cut = || calls.fetch_add(1, Ordering::Relaxed) + 1 >= cut_at;
        let first = find_ids(&case.head, case.dir.path(), &cache, &cut).await;
        if !first.contains(&lower) {
            continue;
        }
        let second = find_ids(&case.head, case.dir.path(), &cache, &|| false).await;
        assert!(!second.contains(&lower), "cut at {cut_at}: {second:?}");
        cut_the_read = true;
        break;
    }
    assert!(cut_the_read, "no cut reached the landed read");
}
