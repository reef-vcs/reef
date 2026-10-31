//! Garbage collection of a repository (R1.17): abandoning old operations,
//! pruning the hidden commits only they named, rebinding a working copy to
//! the rewritten head, and moving the user's jj workspaces along (issue
//! #94). The daemon's four-step GC is tested in `reef-daemon/tests/gc.rs`;
//! this covers the jj half on its own.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::default_index::DefaultIndexStore;
use jj_lib::lock::FileLock;
use jj_lib::op_walk::{reparent_range, walk_ancestors};
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::jj::files::{Edit, EditAction};
use reef_lib::jj::oplog::{OpKind, OpMeta};
use reef_lib::jj::settings::{
    Fsmonitor, Identity, SettingsSpec, user_settings, user_settings_from_spec,
};
use reef_lib::jj::workspace::StartPoint;
use reef_lib::jj::{
    AbandonBlocked, CHECKOUT_LOCK_WAIT, CheckoutMove, CheckoutOutcome, CommitIdHex, OperationIdHex,
    Repo,
};
use tempfile::TempDir;

const DAY: Duration = Duration::from_hours(24);

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

fn settings() -> UserSettings {
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

/// `settings()` with every operation stamped `at`
/// (`debug.operation-timestamp`).
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

/// The undo window the tests below collect with: a week.
fn keep() -> SystemTime {
    SystemTime::now() - 7 * DAY
}

/// A repository whose operations so far (its creation and two snapshots)
/// ended ten days ago, reopened so that later operations are recent. The
/// root's working copy names the last old snapshot.
async fn old_history() -> (TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let old = SystemTime::now() - 10 * DAY;
    let mut repo = Repo::init_internal_git(dir.path(), &settings_at(old))
        .await
        .unwrap();
    two_snapshots(&mut repo).await;
    age_op_store(dir.path(), old);
    let repo = Repo::open(dir.path(), &settings()).await.unwrap();
    (dir, repo)
}

/// A Reef workspace `reef-x` on a new change on the root's `@`, with one
/// snapshot: two recent operations. Its directories are under `dir`.
async fn reef_workspace(repo: &mut Repo, dir: &Path) -> Repo {
    let root = dir.join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let commit = repo.wc_change().unwrap().commit_id;
    let (mut ws, _) = repo
        .create_workspace(
            "reef-x",
            &root,
            &dir.join("wc"),
            StartPoint::NewOn(commit),
            &OpMeta::new(OpKind::CreateWorkspace, "reef: create workspace reef-x"),
        )
        .await
        .unwrap();
    write(&root, "ws.txt", "in the reef workspace\n");
    assert!(ws.snapshot("ws").await.unwrap().changed);
    ws
}

/// The root after a recent `jj` command there: a snapshot of a new file,
/// whose operation the root's working copy names. Returns it.
async fn recent_root_snapshot(repo: &mut Repo) -> OperationIdHex {
    let root = repo.workspace_root().to_owned();
    write(&root, "root.txt", "recent\n");
    assert!(repo.snapshot("recent").await.unwrap().changed);
    let op = repo.working_copy_op_id();
    assert_eq!(op, repo.op_id());
    op
}

fn op_file(repo: &Repo, op: &OperationIdHex) -> PathBuf {
    repo.repo_path()
        .join("op_store/operations")
        .join(op.as_str())
}

/// What `Repo::gc` did before issue #94, with jj-lib directly: abandons
/// the operations before `keep_newer`, collects them and resets the index,
/// moving no working copy. Leaves `repo` loaded at the new head.
async fn strand_like_0_1(repo: &mut Repo, keep_newer: SystemTime) {
    let loader = repo.head().loader().clone();
    let head = loader.load_at_head().await.unwrap().operation().clone();
    let mut ancestors = std::pin::pin!(walk_ancestors(std::slice::from_ref(&head)));
    let mut cut = None;
    while let Some(op) = ancestors.next().await {
        let op = op.unwrap();
        let end = std::time::UNIX_EPOCH
            + Duration::from_millis(op.metadata().time.end.timestamp.0.unsigned_abs());
        if op.id() != head.id() && end < keep_newer {
            cut = Some(op);
            break;
        }
    }
    let cut = cut.expect("an old operation");
    let root = loader.root_operation().await;
    let stats = reparent_range(
        loader.op_store().as_ref(),
        std::slice::from_ref(&cut),
        std::slice::from_ref(&head),
        &root,
    )
    .await
    .unwrap();
    loader
        .op_heads_store()
        .update_op_heads(&[head.id().clone()], &stats.new_head_ids[0])
        .await
        .unwrap();
    let heads = loader.op_heads_store().get_op_heads().await.unwrap();
    loader.op_store().gc(&heads, keep_newer).await.unwrap();
    repo.head()
        .index_store()
        .downcast_ref::<DefaultIndexStore>()
        .unwrap()
        .reinit()
        .unwrap();
    repo.set_head(loader.load_at_head().await.unwrap());
}

/// Runs the `jj` CLI in `dir` with no user config, or `None` (printing
/// why) unless `jj` 0.46 is on `PATH`: its checkout and operation formats
/// are what Reef writes for.
#[allow(
    clippy::print_stdout,
    reason = "a skipped CLI check says so in the test output"
)]
fn jj(dir: &Path, args: &[&str]) -> Option<Output> {
    let version = std::process::Command::new("jj")
        .arg("--version")
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    if !version.trim().starts_with("jj 0.46.") {
        println!("skipped: jj CLI is {:?}, not 0.46.x", version.trim());
        return None;
    }
    Some(
        std::process::Command::new("jj")
            .args(args)
            .current_dir(dir)
            .env("JJ_CONFIG", dir.join(".jj/no-jj-config.toml"))
            .env("JJ_USER", "Reef Test")
            .env("JJ_EMAIL", "reef-test@example.com")
            .output()
            .expect("run jj"),
    )
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

async fn op_count(root: &Path) -> usize {
    Repo::open(root, &settings())
        .await
        .unwrap()
        .op_log(1000)
        .await
        .unwrap()
        .len()
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
    let op_before = repo.op_id();
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

    // GC moved the working copy itself: it named the head, whose rewrite
    // is the new head.
    assert_eq!(
        outcome.checkouts,
        [CheckoutMove {
            workspace: "default".into(),
            path: root.clone(),
            from: Some(op_before.clone()),
            outcome: CheckoutOutcome::Rewritten {
                to: outcome.new_head.clone()
            },
        }]
    );
    assert_eq!(repo.working_copy_op_id(), outcome.new_head);
    // R2.16: the kept operation, the head, maps to its rewrite.
    assert_eq!(
        outcome.rewritten,
        [(op_before.clone(), outcome.new_head.clone())]
    );
    assert!(repo.working_copy_matches_view().unwrap());
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
    assert!(outcome.rewritten.is_empty(), "nothing rewritten");
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

#[tokio::test]
async fn stranded_working_copy_is_rebound_only_when_stranded() {
    let (_dir, mut repo) = init().await;
    two_snapshots(&mut repo).await;
    assert!(
        !repo.rebind_if_stranded().await.unwrap(),
        "a fresh working copy is left alone"
    );
    let root = repo.workspace_root().to_owned();
    write(&root, "a.txt", "third version\n");
    repo.snapshot("three").await.unwrap();
    past_this_second().await;

    // A 0.1 GC, which moved no working copy: its operation is gone.
    strand_like_0_1(&mut repo, SystemTime::now()).await;
    let new_head = repo.op_id();
    assert_ne!(repo.working_copy_op_id(), new_head);
    assert!(repo.rebind_if_stranded().await.unwrap());
    assert_eq!(repo.working_copy_op_id(), new_head);
    assert!(!repo.rebind_if_stranded().await.unwrap(), "only once");
    write(&root, "b.txt", "after repair\n");
    assert!(repo.snapshot("four").await.unwrap().changed);
}

/// Issue #94: the root ran `jj` within the undo window, in a repository
/// with older operations. GC rewrites the root's operation and prunes its
/// old ancestors; the root names the rewrite and loads.
#[tokio::test]
async fn gc_moves_the_root_to_its_rewritten_operation() {
    let (dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let root = repo.workspace_root().to_owned();
    let x = recent_root_snapshot(&mut repo).await;
    let parent = repo.op_log(1).await.unwrap()[0].parents[0].clone();
    let at = repo.wc_change().unwrap();
    let _ws = reef_workspace(&mut repo, other.path()).await;

    let outcome = repo.gc(keep()).await.unwrap();
    assert!(outcome.operations_abandoned >= 3, "{outcome:?}");
    let [moved] = outcome.checkouts.as_slice() else {
        panic!("one move: {outcome:?}");
    };
    let CheckoutOutcome::Rewritten { to } = &moved.outcome else {
        panic!("rewritten: {moved:?}");
    };
    assert_eq!(
        (moved.workspace.as_str(), &moved.path, &moved.from),
        ("default", &root, &Some(x.clone()))
    );
    assert_ne!(*to, x);
    let log = repo.op_log(100).await.unwrap();
    assert!(log.iter().any(|op| op.id == *to), "{log:?}");
    assert!(
        op_file(&repo, &x).is_file() && !op_file(&repo, &parent).exists(),
        "the #94 precondition: the old operation is young, its parent is not"
    );

    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), *to);
    fresh.reload_at_working_copy_op().await.unwrap();
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert!(!fresh.rebind_if_stranded().await.unwrap());
    assert_eq!(fresh.wc_change().unwrap(), at);

    if let Some(st) = jj(&root, &["st"]) {
        assert!(st.status.success(), "{}", text(&st));
        assert_eq!(op_count(&root).await, log.len(), "jj st recorded nothing");
    }
    write(&root, "after.txt", "after gc\n");
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert!(fresh.snapshot("after").await.unwrap().changed);
    assert_eq!(fresh.wc_change().unwrap().change_id, at.change_id);
}

/// The root's last operation is old, so GC abandons it; its files are its
/// commit's, so it moves to the head.
#[tokio::test]
async fn gc_moves_a_clean_root_from_an_abandoned_operation_to_the_head() {
    let (dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let root = repo.workspace_root().to_owned();
    let x = repo.working_copy_op_id();
    let at = repo.wc_change().unwrap();
    let _ws = reef_workspace(&mut repo, other.path()).await;

    let outcome = repo.gc(keep()).await.unwrap();
    assert_eq!(
        outcome.checkouts,
        [CheckoutMove {
            workspace: "default".into(),
            path: root.clone(),
            from: Some(x),
            outcome: CheckoutOutcome::ToHead {
                to: outcome.new_head.clone()
            },
        }]
    );
    let fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), outcome.new_head);
    if let Some(st) = jj(&root, &["st"]) {
        assert!(st.status.success(), "{}", text(&st));
    }
    let fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.wc_change().unwrap(), at);
}

/// The root's last operation is old and its `@` was rewritten since with
/// a file it does not have: GC cannot move it exactly, so it deletes the
/// operation and jj's recovery for a missing operation applies.
#[tokio::test]
async fn gc_leaves_a_stale_root_to_jj_recovery() {
    let (dir, mut repo) = old_history().await;
    let root = repo.workspace_root().to_owned();
    let x = repo.working_copy_op_id();
    let commit = repo.wc_change().unwrap().commit_id;
    let edit = Edit {
        path: "added.txt".into(),
        action: Some(EditAction::Content(b"added\n".to_vec())),
        executable: None,
    };
    repo.edit_commit(&commit, &[edit], None, &OpMeta::new(OpKind::Edit, "edit"))
        .await
        .unwrap()
        .expect("an edit");
    assert_eq!(repo.working_copy_op_id(), x, "the root is not written");
    // Young enough that the store's GC alone would keep it.
    std::fs::File::open(op_file(&repo, &x))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();

    let outcome = repo.gc(keep()).await.unwrap();
    assert_eq!(
        outcome.checkouts,
        [CheckoutMove {
            workspace: "default".into(),
            path: root.clone(),
            from: Some(x.clone()),
            outcome: CheckoutOutcome::LeftForRecovery,
        }]
    );
    assert!(!op_file(&repo, &x).exists());
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), x);
    let error = fresh.reload_at_working_copy_op().await.unwrap_err();
    let error = format!("{error:?}");
    assert!(
        error.contains("ObjectNotFound") && error.contains(x.as_str()),
        "X itself is missing, jj's recovery path: {error}"
    );

    let before = std::fs::read(root.join("a.txt")).unwrap();
    let Some(st) = jj(&root, &["st"]) else {
        return;
    };
    assert!(
        !st.status.success() && text(&st).contains("update-stale"),
        "{}",
        text(&st)
    );
    let update = jj(&root, &["workspace", "update-stale"]).unwrap();
    assert!(update.status.success(), "{}", text(&update));
    assert!(
        text(&update).to_lowercase().contains("recovery commit"),
        "{}",
        text(&update)
    );
    let st = jj(&root, &["st"]).unwrap();
    assert!(st.status.success(), "{}", text(&st));
    assert!(!root.join("added.txt").exists());
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), before);
}

/// While a `jj` command holds the root's working-copy lock, GC abandons
/// nothing; it does once the lock is free.
#[tokio::test]
async fn gc_abandons_nothing_while_the_root_is_locked() {
    let (_dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let root = repo.workspace_root().to_owned();
    let x = recent_root_snapshot(&mut repo).await;
    let _ws = reef_workspace(&mut repo, other.path()).await;
    let ops = op_count(&root).await;
    let lock = FileLock::lock(root.join(".jj/working_copy/working_copy.lock")).unwrap();

    let started = Instant::now();
    let outcome = repo.gc(keep()).await.unwrap();
    let took = started.elapsed();
    assert_eq!(outcome.operations_abandoned, 0);
    assert_eq!(
        outcome.abandon_blocked,
        Some(AbandonBlocked::Locked {
            workspace: "default".into(),
            path: root.clone(),
        })
    );
    assert_eq!(outcome.checkouts, []);
    assert_eq!(op_count(&root).await, ops);
    let fresh = Repo::open(&root, &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), x, "the checkout on disk");
    assert!(
        took >= CHECKOUT_LOCK_WAIT && took < Duration::from_secs(10),
        "{took:?}"
    );

    drop(lock);
    let outcome = repo.gc(keep()).await.unwrap();
    assert!(outcome.operations_abandoned > 0);
    assert!(matches!(
        outcome.checkouts[0].outcome,
        CheckoutOutcome::Rewritten { .. }
    ));
}

/// A workspace the user added with `jj workspace add` moves like the root;
/// one whose directory is gone is skipped and does not stop GC.
#[tokio::test]
async fn gc_moves_a_users_jj_workspace() {
    let (_dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let factory = jj_lib::default_backend_factories::default_working_copy_factory();
    for name in ["mine", "gone"] {
        let dir = other.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        Workspace::init_workspace_with_existing_repo(
            &dir,
            repo.repo_path(),
            &repo.head().loader().load_at_head().await.unwrap(),
            &*factory,
            WorkspaceNameBuf::from(name),
        )
        .await
        .unwrap();
        write(&dir, "mine.txt", "a user's workspace\n");
        let mut theirs = Repo::open(&dir, &settings()).await.unwrap();
        assert!(theirs.snapshot("mine").await.unwrap().changed);
    }
    let mine = std::fs::canonicalize(other.path().join("mine")).unwrap();
    std::fs::remove_dir_all(other.path().join("gone")).unwrap();

    let outcome = repo.gc(keep()).await.unwrap();
    assert!(outcome.operations_abandoned > 0);
    let outcomes: Vec<(&str, &CheckoutOutcome)> = outcome
        .checkouts
        .iter()
        .map(|moved| (moved.workspace.as_str(), &moved.outcome))
        .collect();
    let [
        ("default", CheckoutOutcome::ToHead { .. }),
        ("gone", CheckoutOutcome::Skipped { reason }),
        ("mine", CheckoutOutcome::Rewritten { .. }),
    ] = outcomes.as_slice()
    else {
        panic!("{outcomes:?}");
    };
    assert!(reason.contains("no jj checkout"), "{reason}");
    assert_eq!(outcome.checkouts[2].path, mine);
    let mut theirs = Repo::open(&mine, &settings()).await.unwrap();
    theirs.reload_at_working_copy_op().await.unwrap();
}

/// A root a 0.1 GC stranded (#94's state) is moved to the head by the
/// next GC, which has nothing left to abandon.
#[tokio::test]
async fn gc_repairs_a_root_an_earlier_gc_stranded() {
    let (dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let root = repo.workspace_root().to_owned();
    recent_root_snapshot(&mut repo).await;
    let _ws = reef_workspace(&mut repo, other.path()).await;
    strand_like_0_1(&mut repo, keep()).await;
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert!(
        fresh.reload_at_working_copy_op().await.is_err(),
        "the #94 state"
    );

    let mut repo = Repo::open(dir.path(), &settings()).await.unwrap();
    let head = repo.op_id();
    let outcome = repo.gc(keep()).await.unwrap();
    assert_eq!(outcome.operations_abandoned, 0);
    assert_eq!(outcome.new_head, head);
    assert!(matches!(
        &outcome.checkouts[..],
        [CheckoutMove { workspace, outcome: CheckoutOutcome::ToHead { to }, .. }]
            if workspace == "default" && *to == head
    ));
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), head);
    fresh.reload_at_working_copy_op().await.unwrap();
    if let Some(st) = jj(&root, &["st"]) {
        assert!(st.status.success(), "{}", text(&st));
    }
}

/// A `jj` command that committed before GC loaded the head and checks out
/// once GC releases the lock writes its operation's old id; GC moves the
/// root onto the rewrite again before collecting. The callback runs between
/// the release and that check, so it stands in for the command.
#[tokio::test]
async fn gc_moves_the_root_again_after_a_jj_command_finishes() {
    let (dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let x = recent_root_snapshot(&mut repo).await;
    let _ws = reef_workspace(&mut repo, other.path()).await;
    let checkout = repo.workspace_root().join(".jj/working_copy/checkout");
    let written_by_jj = std::fs::read(&checkout).unwrap();

    let outcome = repo
        .gc_with(keep(), &mut |_, _| {
            std::fs::write(&checkout, &written_by_jj).unwrap();
        })
        .await
        .unwrap();
    let [moved] = outcome.checkouts.as_slice() else {
        panic!("{outcome:?}");
    };
    let CheckoutOutcome::Rewritten { to } = &moved.outcome else {
        panic!("{moved:?}");
    };
    assert_eq!(moved.from, Some(x));
    assert_eq!(repo.working_copy_op_id(), *to);
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    assert_eq!(fresh.working_copy_op_id(), *to);
    fresh.reload_at_working_copy_op().await.unwrap();
}

/// A directory removed and added again under another name is that name's:
/// the stale name is skipped, not mistaken for a busy lock (GC would hold
/// the directory's lock itself).
#[tokio::test]
async fn gc_skips_a_name_whose_directory_another_name_took() {
    let (_dir, mut repo) = old_history().await;
    let other = tempfile::tempdir().unwrap();
    let factory = jj_lib::default_backend_factories::default_working_copy_factory();
    let dir = other.path().join("w");
    for name in ["b", "a"] {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir(&dir).unwrap();
        Workspace::init_workspace_with_existing_repo(
            &dir,
            repo.repo_path(),
            &repo.head().loader().load_at_head().await.unwrap(),
            &*factory,
            WorkspaceNameBuf::from(name),
        )
        .await
        .unwrap();
    }

    let outcome = repo.gc(keep()).await.unwrap();
    assert_eq!(outcome.abandon_blocked, None, "{outcome:?}");
    assert!(outcome.operations_abandoned > 0);
    let outcomes: Vec<(&str, &CheckoutOutcome)> = outcome
        .checkouts
        .iter()
        .map(|moved| (moved.workspace.as_str(), &moved.outcome))
        .collect();
    let [
        ("a", CheckoutOutcome::Rewritten { .. }),
        ("b", CheckoutOutcome::Skipped { reason }),
        ("default", CheckoutOutcome::ToHead { .. }),
    ] = outcomes.as_slice()
    else {
        panic!("{outcomes:?}");
    };
    assert!(reason.contains("names workspace `a`"), "{reason}");
}

/// A busy lock blocks nothing when nothing is old enough to abandon.
#[tokio::test]
async fn a_busy_root_blocks_nothing_with_nothing_to_abandon() {
    let (_dir, mut repo) = init().await;
    two_snapshots(&mut repo).await;
    let root = repo.workspace_root().to_owned();
    let _lock = FileLock::lock(root.join(".jj/working_copy/working_copy.lock")).unwrap();

    let outcome = repo.gc(keep()).await.unwrap();
    assert_eq!(outcome.abandon_blocked, None);
    assert_eq!(outcome.operations_abandoned, 0);
}
