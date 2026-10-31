//! Reads at an operation sharing the head's commit index, and finding an
//! operation by time (R2.12). The daemon's reads at an operation are tested
//! in `reef-daemon/tests/replay.rs`; this covers the jj half on its own.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;
use reef_lib::jj::log;
use reef_lib::jj::oplog::{self, Located, OpKind, OpLookupError, OpMeta};
use reef_lib::jj::revset::{self, RevsetScope};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CommitIdHex, OperationIdHex, ReadonlyRepo, Repo};
use tempfile::TempDir;

const DAY: Duration = Duration::from_hours(24);

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

/// `settings()` with every operation stamped `at`.
fn settings_at(at: SystemTime) -> UserSettings {
    let mut config = settings().config().clone();
    let stamp = chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            &format!("debug.operation-timestamp = {stamp:?}"),
        )
        .unwrap(),
    );
    UserSettings::from_config_and_home_dir(config, None).unwrap()
}

/// Sets the mtime of every operation and view file under `root` to `at`,
/// which the operation store's GC prunes by.
fn age_op_store(root: &Path, at: SystemTime) {
    for dir in ["operations", "views"] {
        let dir = root.join(".jj/repo/op_store").join(dir);
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
    }
}

fn write(root: &Path, path: &str, contents: &str) {
    std::fs::write(root.join(path), contents).expect("write file");
}

/// Files under the repository's index directory `dir` (`op_links`,
/// `segments`), by name.
fn index_files(root: &Path, dir: &str) -> Vec<String> {
    let dir = root.join(".jj/repo/index").join(dir);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A repository with three operations ten days old, then recent ones: five
/// snapshots, a describe and an abandon. Returns the recent operations'
/// ids, oldest first.
async fn history() -> (TempDir, Repo, Vec<OperationIdHex>) {
    let dir = tempfile::tempdir().unwrap();
    let old = SystemTime::now() - 10 * DAY;
    let mut repo = Repo::init_internal_git(dir.path(), &settings_at(old))
        .await
        .unwrap();
    for n in 0..2 {
        write(dir.path(), "a.txt", &format!("old {n}\n"));
        repo.snapshot("old").await.unwrap();
    }
    age_op_store(dir.path(), old);
    let mut repo = Repo::open(dir.path(), &settings()).await.unwrap();
    let mut recent = Vec::new();
    let mut commits: Vec<CommitIdHex> = Vec::new();
    for n in 0..5 {
        write(dir.path(), &format!("f{n}.txt"), &format!("{n}\n"));
        commits.push(repo.snapshot("recent").await.unwrap().wc_commit);
        recent.push(repo.op_id());
    }
    let wc = repo.wc_change().unwrap().commit_id.clone();
    repo.describe_commit(&wc, "described", &OpMeta::new(OpKind::Describe, "describe"))
        .await
        .unwrap();
    recent.push(repo.op_id());
    let wc = repo.wc_change().unwrap().commit_id.clone();
    repo.abandon_commit(&wc, &OpMeta::new(OpKind::Abandon, "abandon"))
        .await
        .unwrap();
    recent.push(repo.op_id());
    (dir, repo, recent)
}

fn scope(root: &Path) -> RevsetScope<'_> {
    RevsetScope {
        workspace: Some("default"),
        user_email: "reef-test@example.com",
        root,
    }
}

/// What the daemon's reads see at a repository: the default log, trunk,
/// mutable heads, divergent and conflicted changes, every working copy,
/// and each visible change resolved by its id. With `lookups`, also each
/// change's first two offsets (`<id>/<n>`) and every one-digit prefix,
/// which count the commits an index holds, not only the visible ones.
async fn reads(repo: &ReadonlyRepo, root: &Path, lookups: bool) -> Vec<String> {
    let scope = scope(root);
    let mut seen = Vec::new();
    for text in [
        "present(@) | ancestors(immutable_heads().., 2) | trunk()",
        "trunk()",
        "heads(mutable())",
        "divergent()",
        "mutable() & conflicts()",
        "all()",
        "visible_heads()",
    ] {
        let parsed = revset::parse(repo, text, &scope).unwrap();
        let (ids, total) = log::commits(repo, &parsed, usize::MAX, &|| false)
            .await
            .unwrap();
        seen.push(format!("{text}: {total} {ids:?}"));
    }
    let mut copies: Vec<String> = repo
        .view()
        .wc_commit_ids()
        .iter()
        .map(|(name, id)| format!("{}={}", name.as_str(), id.hex()))
        .collect();
    copies.sort();
    seen.push(format!("working copies: {copies:?}"));
    let parsed = revset::parse(repo, "all()", &scope).unwrap();
    let (changes, _) = log::change_ids(repo, &parsed, usize::MAX).await.unwrap();
    for change in changes {
        let resolved = revset::resolve_revision(repo, change.as_str(), &scope)
            .await
            .map_or_else(
                |error| format!("{error}"),
                |resolved| resolved.commit_id.to_string(),
            );
        seen.push(format!("{change}: {resolved}"));
        if !lookups {
            continue;
        }
        for offset in 0..2 {
            let text = format!("{change}/{offset}");
            seen.push(format!("{text}: {}", resolve(repo, &text, &scope).await));
        }
    }
    if lookups {
        for digit in "0123456789abcdef".chars() {
            let text = digit.to_string();
            seen.push(format!("{text}: {}", resolve(repo, &text, &scope).await));
        }
    }
    seen
}

/// The commit `text` names at `repo`, or why it names none.
async fn resolve(repo: &ReadonlyRepo, text: &str, scope: &RevsetScope<'_>) -> String {
    revset::resolve_revision(repo, text, scope)
        .await
        .map_or_else(
            |error| format!("{error}"),
            |resolved| resolved.commit_id.to_string(),
        )
}

async fn operation(repo: &ReadonlyRepo, id: &OperationIdHex) -> reef_lib::jj::Operation {
    oplog::resolve_operation(repo, id.as_str()).await.unwrap()
}

// 1
#[tokio::test]
async fn load_at_shared_matches_load_at() {
    let (dir, mut repo, recent) = history().await;
    let later_commit = repo.wc_change().unwrap().commit_id.clone();
    let outcome = repo.gc(SystemTime::now() - 7 * DAY).await.unwrap();
    assert!(outcome.operations_abandoned > 0, "{outcome:?}");
    let head = Arc::clone(repo.head());
    // The kept operations' rewrites, and two pre-GC copies GC kept as
    // unreachable files, each read beside its rewrite: jj cannot build an
    // index for a pre-GC copy, whose ancestors are gone.
    let mut points: Vec<(OperationIdHex, OperationIdHex)> = outcome
        .rewritten
        .iter()
        .map(|(_, new)| (new.clone(), new.clone()))
        .collect();
    assert!(points.len() >= 7, "{points:?}");
    for old in [&recent[0], &recent[3]] {
        let (_, new) = outcome.rewritten.iter().find(|(id, _)| id == old).unwrap();
        points.push((old.clone(), new.clone()));
        let op = operation(&head, old).await;
        assert!(oplog::load_at(&head, &op).await.is_err());
    }
    for (id, rewrite) in &points {
        let op = operation(&head, id).await;
        let shared = oplog::load_at_shared(&head, &op).await.unwrap();
        // The head itself is returned as it is.
        assert_eq!(
            oplog::is_shared(&shared),
            op.id() != head.op_id(),
            "at {id}"
        );
        let built = oplog::load_at(&head, &operation(&head, rewrite).await)
            .await
            .unwrap();
        assert!(!oplog::is_shared(&built));
        assert_eq!(shared.op_id().hex(), id.as_str());
        // Offsets and prefixes are resolved against jj's own index for the
        // operation, which a pre-GC copy cannot have.
        let lookups = id == rewrite;
        assert_eq!(
            reads(&shared, dir.path(), lookups).await,
            reads(&built, dir.path(), lookups).await,
            "at {id}"
        );
    }
    // A commit created after an operation is in the borrowed index: by id
    // it resolves at the operation (hidden there), where jj's own index
    // for it does not know it.
    let (_, first) = outcome
        .rewritten
        .iter()
        .find(|(old, _)| *old == recent[0])
        .unwrap();
    let first = operation(&head, first).await;
    let shared = oplog::load_at_shared(&head, &first).await.unwrap();
    let built = oplog::load_at(&head, &first).await.unwrap();
    let scope = scope(dir.path());
    assert!(
        revset::resolve_revision(&shared, later_commit.as_str(), &scope)
            .await
            .is_ok()
    );
    assert!(
        revset::resolve_revision(&built, later_commit.as_str(), &scope)
            .await
            .is_err()
    );
}

// 2
#[tokio::test]
async fn load_at_shared_writes_nothing() {
    let (dir, mut repo, recent) = history().await;
    let outcome = repo.gc(SystemTime::now() - 7 * DAY).await.unwrap();
    assert!(outcome.operations_abandoned > 0, "{outcome:?}");
    let head = Arc::clone(repo.head());
    let links = index_files(dir.path(), "op_links");
    let segments = index_files(dir.path(), "segments");
    assert!(!links.is_empty(), "the head's index link");
    for id in recent
        .iter()
        .chain(outcome.rewritten.iter().map(|(_, new)| new))
    {
        let op = operation(&head, id).await;
        oplog::load_at_shared(&head, &op).await.unwrap();
    }
    assert_eq!(index_files(dir.path(), "op_links"), links);
    assert_eq!(index_files(dir.path(), "segments"), segments);
    // jj's own load writes a link for the operation it indexes.
    let (_, rewrite) = outcome
        .rewritten
        .iter()
        .find(|(old, _)| *old == recent[1])
        .unwrap();
    let op = operation(&head, rewrite).await;
    oplog::load_at(&head, &op).await.unwrap();
    assert_ne!(index_files(dir.path(), "op_links"), links);

    // A concurrent operation whose view names a commit the head's index
    // lacks: the shared load falls back, and answers as jj's.
    let at = oplog::load_at(&head, &operation(&head, &repo.op_id()).await)
        .await
        .unwrap();
    let parent = at
        .store()
        .get_commit(at.view().heads().iter().next().unwrap())
        .unwrap();
    let mut tx = at.start_transaction();
    tx.repo_mut()
        .new_commit(vec![parent.id().clone()], parent.tree())
        .set_description("concurrent")
        .write()
        .await
        .unwrap();
    let concurrent = tx.commit("concurrent").await.unwrap();
    let op = concurrent.operation().clone();
    let shared = oplog::load_at_shared(&head, &op).await.unwrap();
    assert!(!oplog::is_shared(&shared), "the fallback");
    let built = oplog::load_at(&head, &op).await.unwrap();
    assert_eq!(
        reads(&shared, dir.path(), true).await,
        reads(&built, dir.path(), true).await
    );
}

// 3
#[tokio::test]
async fn locate_finds_by_time() {
    let (_dir, mut repo, recent) = history().await;
    let head = Arc::clone(repo.head());
    let entries: Vec<oplog::OpEntry> = {
        let mut entries = Vec::new();
        for id in &recent {
            entries.push(oplog::entry(&operation(&head, id).await));
        }
        entries
    };
    let found = |located: Located| match located {
        Located::Found(op) => oplog::entry(&op).id,
        other => panic!("{other:?}"),
    };
    // A time equal to an operation's end finds it; one just before it, the
    // one before; one after the head, the head.
    let third = &entries[3];
    assert_eq!(
        found(oplog::locate(&head, &[], third.end, 1000).await.unwrap()),
        third.id
    );
    let before = third.end - Duration::from_millis(1);
    let older = found(oplog::locate(&head, &[], before, 1000).await.unwrap());
    assert_ne!(older, third.id);
    assert!(oplog::entry(&operation(&head, &older).await).end <= before);
    let future = SystemTime::now() + DAY;
    assert_eq!(
        found(oplog::locate(&head, &[], future, 1000).await.unwrap()),
        repo.op_id()
    );
    // From a given operation the walk starts there.
    assert_eq!(
        found(
            oplog::locate(&head, std::slice::from_ref(&third.id), future, 1000)
                .await
                .unwrap()
        ),
        third.id
    );
    // The budget runs out.
    match oplog::locate(&head, &[], SystemTime::now() - 9 * DAY, 3)
        .await
        .unwrap()
    {
        Located::Exhausted { examined_to } => assert!(examined_to > before - DAY),
        other => panic!("{other:?}"),
    }

    // After GC, a time before the oldest kept operation is before what the
    // log keeps, not the root's empty view.
    let outcome = repo.gc(SystemTime::now() - 7 * DAY).await.unwrap();
    assert!(outcome.operations_abandoned > 0, "{outcome:?}");
    let head = Arc::clone(repo.head());
    match oplog::locate(&head, &[], SystemTime::now() - 9 * DAY, 1000)
        .await
        .unwrap()
    {
        Located::BeforeRetained { oldest_end } => {
            assert!(oldest_end > SystemTime::now() - DAY, "{oldest_end:?}");
        }
        other => panic!("{other:?}"),
    }
    // A walk from a pre-GC copy reaches the collected operations.
    let error = oplog::locate(
        &head,
        &[recent[2].clone()],
        SystemTime::now() - 9 * DAY,
        1000,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, OpLookupError::Collected), "{error:?}");
}
