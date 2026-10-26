//! Reef workspaces as jj workspaces (R1.01, ADR 0014): creating, reopening,
//! and forgetting them, seeding their working-copy state, and resolving
//! revisions. Each test works in its own temporary directory, with the
//! user's workspace at `main/` and Reef workspaces under `ws/` and `wc/`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::revset::RevisionError;
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::workspace::CommitLookupError;
use reef_lib::jj::{
    ChangeIdHex, CommitIdHex, CreateWorkspaceError, ForgetWorkspaceError, IgnoreSources, OpenError,
    Repo, StartPoint, commit_snapshot,
};
use reef_lib::jj::{OpKind, OpMeta};
use tempfile::TempDir;

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

/// A temporary directory holding the user's repository at `main/`.
struct Fixture {
    dir: TempDir,
    main: Repo,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main");
        std::fs::create_dir(&root).expect("create main");
        let main = Repo::init_internal_git(&root, &settings())
            .await
            .expect("init repo");
        Self { dir, main }
    }

    /// `main/` with `a.txt` and `dir/b.txt` snapshotted; returns the
    /// working-copy commit holding them.
    async fn with_files() -> (Self, CommitIdHex) {
        let mut fixture = Self::new().await;
        let root = fixture.main.workspace_root().to_owned();
        write(&root, "a.txt", "a\n");
        write(&root, "dir/b.txt", "b\n");
        let outcome = fixture
            .main
            .snapshot("snapshot working copy")
            .await
            .unwrap();
        assert!(outcome.changed);
        (fixture, outcome.wc_commit)
    }

    /// A new, empty workspace root, and a state directory path that does not
    /// exist yet, for the workspace `name`.
    fn paths(&self, name: &str) -> (PathBuf, PathBuf) {
        let root = self.dir.path().join("ws").join(name);
        std::fs::create_dir_all(&root).expect("create workspace root");
        (root, self.dir.path().join("wc").join(name))
    }

    async fn create(&mut self, name: &str, start: StartPoint) -> (Repo, PathBuf, PathBuf) {
        let (root, state) = self.paths(name);
        let (repo, op_id) = self
            .main
            .create_workspace(
                name,
                &root,
                &state,
                start,
                &OpMeta::new(
                    OpKind::CreateWorkspace,
                    format!("reef: create workspace {name}"),
                ),
            )
            .await
            .expect("create workspace");
        assert_eq!(self.main.op_id(), op_id, "the caller adopts the operation");
        assert_eq!(repo.op_id(), op_id);
        (repo, root, state)
    }

    /// The repository as jj itself loads it at head from `main/`.
    async fn jj_head(&self) -> Arc<ReadonlyRepo> {
        let workspace = Workspace::load(
            &settings(),
            self.main.workspace_root(),
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .expect("load workspace");
        workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load at head")
    }

    /// Workspace names with a working-copy commit in the head view, the
    /// `jj workspace list` equivalent.
    async fn wc_names(&self) -> BTreeSet<String> {
        self.jj_head()
            .await
            .view()
            .wc_commit_ids()
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect()
    }

    /// The path jj's workspace store records for `name`.
    async fn store_path(&self, name: &str) -> Option<PathBuf> {
        self.jj_head()
            .await
            .loader()
            .workspace_store()
            .get_workspace_path(WorkspaceName::new(name))
            .expect("read workspace store")
    }

    /// Commit ids of the heads of `main`'s loaded view.
    fn head_ids(&self) -> BTreeSet<String> {
        self.main
            .heads()
            .unwrap()
            .into_iter()
            .map(|head| head.commit_id.to_string())
            .collect()
    }

    /// The change of `commit`, as `main` sees it.
    async fn change_of(&self, commit: &CommitIdHex) -> ChangeIdHex {
        let resolved = self.main.resolve_revision(commit.as_str()).await.unwrap();
        assert_eq!(&resolved.commit_id, commit);
        resolved.change_id
    }

    /// Two workspaces edit `commit`, each writes a different file, and both
    /// snapshot from the operation they were created at, so jj sees two
    /// concurrent rewrites of one change. Returns the two new commits;
    /// `main` is reloaded at head, which merges the operations.
    async fn diverge(&mut self, commit: &CommitIdHex) -> [CommitIdHex; 2] {
        let (mut first, first_root, _) = self
            .create("reef-a", StartPoint::Edit(commit.clone()))
            .await;
        let (mut second, second_root, _) = self
            .create("reef-b", StartPoint::Edit(commit.clone()))
            .await;
        write(&first_root, "from-a.txt", "a\n");
        write(&second_root, "from-b.txt", "b\n");
        let a = first.snapshot("reef: snapshot workspace a").await.unwrap();
        let b = second.snapshot("reef: snapshot workspace b").await.unwrap();
        assert!(a.changed && b.changed);
        self.main.reload_at_head().await.unwrap();
        [a.wc_commit, b.wc_commit]
    }
}

fn wc_commit_id(repo: &Repo) -> String {
    repo.wc_commit().unwrap().id().hex()
}

// 1
#[tokio::test]
async fn create_workspace_edit_points_at_commit() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (ws, root, _) = fx.create("reef-x", StartPoint::Edit(commit.clone())).await;

    assert_eq!(wc_commit_id(&ws), commit.as_str());
    assert_eq!(ws.workspace_name(), "reef-x");
    assert_eq!(ws.workspace_root(), std::fs::canonicalize(&root).unwrap());
    assert_eq!(
        fx.main.op_log(1).await.unwrap()[0].description,
        "reef: create workspace reef-x"
    );

    let names = fx.wc_names().await;
    assert_eq!(
        names,
        BTreeSet::from(["default".to_owned(), "reef-x".to_owned()])
    );
    assert_eq!(
        fx.store_path("reef-x").await,
        Some(std::fs::canonicalize(&root).unwrap()),
        "registered for jj workspace list"
    );
}

// 2
#[tokio::test]
async fn create_workspace_new_on_makes_empty_child() {
    let (mut fx, commit) = Fixture::with_files().await;
    let base_change = fx.change_of(&commit).await;
    let (ws, _, _) = fx.create("reef-x", StartPoint::NewOn(commit.clone())).await;

    let wc = ws.wc_commit().unwrap();
    assert_ne!(wc.id().hex(), commit.as_str());
    let parents: Vec<String> = wc.parent_ids().iter().map(CommitId::hex).collect();
    assert_eq!(parents, [commit.as_str()]);
    assert_eq!(wc.description(), "");
    assert_ne!(wc.change_id().hex(), base_change.as_str());
    assert!(ws.changed_paths_in_wc().await.unwrap().is_empty(), "empty");
}

// 3
#[tokio::test]
async fn workspace_state_outside_tree() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (_ws, root, state) = fx.create("reef-x", StartPoint::Edit(commit)).await;

    assert!(!root.join(".jj").exists(), "no .jj in the workspace");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "tree untouched"
    );
    assert!(state.join("checkout").is_file());
    assert!(state.join("tree_state").is_file());
}

#[tokio::test]
async fn create_workspace_refuses_existing_name_and_state_dir() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (_ws, _, state) = fx.create("reef-x", StartPoint::Edit(commit.clone())).await;

    let (root, other_state) = fx.paths("reef-y");
    let error = fx
        .main
        .create_workspace(
            "reef-x",
            &root,
            &other_state,
            StartPoint::Edit(commit.clone()),
            &OpMeta::new(OpKind::CreateWorkspace, ""),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, CreateWorkspaceError::WorkspaceExists(_)),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);

    let op_before = fx.main.op_id();
    let error = fx
        .main
        .create_workspace(
            "reef-y",
            &root,
            &state,
            StartPoint::Edit(commit.clone()),
            &OpMeta::new(OpKind::CreateWorkspace, ""),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, CreateWorkspaceError::StateDirExists(_)),
        "{error:?}"
    );
    assert_eq!(fx.main.op_id(), op_before, "no operation written");

    let unknown: CommitIdHex = "ab".repeat(20).parse().unwrap();
    let error = fx
        .main
        .create_workspace(
            "reef-y",
            &root,
            &other_state,
            StartPoint::NewOn(unknown),
            &OpMeta::new(OpKind::CreateWorkspace, ""),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            CreateWorkspaceError::Lookup(CommitLookupError::NotFound(_))
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(
        !other_state.exists(),
        "state dir not created for a bad commit"
    );

    let missing_root = fx.dir.path().join("nowhere");
    let error = fx
        .main
        .create_workspace(
            "reef-y",
            &missing_root,
            &other_state,
            StartPoint::Edit(commit),
            &OpMeta::new(OpKind::CreateWorkspace, ""),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

// 4
#[tokio::test]
async fn reset_then_snapshot_is_noop() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (mut ws, root, _) = fx.create("reef-x", StartPoint::Edit(commit.clone())).await;
    // What a backend would materialize from the base (R1.02).
    write(&root, "a.txt", "a\n");
    write(&root, "dir/b.txt", "b\n");

    ws.reset_to(&commit).await.unwrap();
    let outcome = ws.snapshot("reef: snapshot workspace x").await.unwrap();
    assert!(!outcome.changed, "{outcome:?}");
    assert_eq!(outcome.wc_commit, commit);
}

// 5
#[tokio::test]
async fn open_workspace_after_drop() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (ws, root, state) = fx.create("reef-x", StartPoint::NewOn(commit)).await;
    drop(ws);

    let mut ws = fx.main.open_workspace(&root, &state).unwrap();
    assert_eq!(ws.workspace_name(), "reef-x");
    write(&root, "new.txt", "new\n");
    let outcome = ws.snapshot("reef: snapshot workspace x").await.unwrap();
    assert!(outcome.changed);
    let ops = ws.op_log(1).await.unwrap();
    assert_eq!(ops[0].workspace.as_deref(), Some("reef-x"));
    assert!(ops[0].is_snapshot);

    let error = fx
        .main
        .open_workspace(&root, &fx.dir.path().join("wc/missing"))
        .err()
        .unwrap();
    assert!(matches!(error, OpenError::Load(_)), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

// 6
#[tokio::test]
async fn forget_workspace_removes_pointer_and_abandons_empty() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (empty, _, _) = fx.create("reef-x", StartPoint::NewOn(commit.clone())).await;
    let (mut edited, edited_root, _) = fx.create("reef-y", StartPoint::NewOn(commit)).await;
    let empty_commit = wc_commit_id(&empty);
    write(&edited_root, "y.txt", "y\n");
    let edited_commit = edited
        .snapshot("reef: snapshot workspace y")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    assert!(fx.head_ids().contains(&empty_commit));

    fx.main
        .forget_workspace(
            "reef-x",
            &OpMeta::new(OpKind::ReleaseWorkspace, "reef: release workspace x"),
        )
        .await
        .unwrap();
    assert!(!fx.wc_names().await.contains("reef-x"));
    assert_eq!(fx.store_path("reef-x").await, None);
    assert!(
        !fx.head_ids().contains(&empty_commit),
        "empty commit abandoned"
    );

    fx.main
        .forget_workspace(
            "reef-y",
            &OpMeta::new(OpKind::ReleaseWorkspace, "reef: release workspace y"),
        )
        .await
        .unwrap();
    assert_eq!(fx.wc_names().await, BTreeSet::from(["default".to_owned()]));
    assert!(
        fx.head_ids().contains(edited_commit.as_str()),
        "edits stay visible"
    );
    assert_eq!(
        fx.main.op_log(1).await.unwrap()[0].description,
        "reef: release workspace y"
    );

    let again = fx
        .main
        .forget_workspace("reef-y", &OpMeta::new(OpKind::ReleaseWorkspace, ""))
        .await
        .unwrap_err();
    assert!(
        matches!(again, ForgetWorkspaceError::NoSuchWorkspace(_)),
        "{again:?}"
    );
    assert_eq!(again.kind(), ErrorKind::NotFound);
    let own = fx
        .main
        .forget_workspace("default", &OpMeta::new(OpKind::ReleaseWorkspace, ""))
        .await
        .unwrap_err();
    assert!(
        matches!(own, ForgetWorkspaceError::NotReefWorkspace(_)),
        "{own:?}"
    );
    assert_eq!(own.kind(), ErrorKind::InvalidArgument);
}

#[tokio::test]
async fn forget_refuses_default_from_a_reef_workspace() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (mut ws, _, _) = fx.create("reef-x", StartPoint::NewOn(commit)).await;

    let error = ws
        .forget_workspace("default", &OpMeta::new(OpKind::ReleaseWorkspace, ""))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ForgetWorkspaceError::NotReefWorkspace(_)),
        "{error:?}"
    );
    let error = ws
        .forget_workspace("reef-x", &OpMeta::new(OpKind::ReleaseWorkspace, ""))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ForgetWorkspaceError::CurrentWorkspace(_)),
        "{error:?}"
    );
    assert!(fx.wc_names().await.contains("default"));
}

// 7 is `revisions_follow_jj_order` and `revisions_are_exactly_one` in
// jj_reads.rs: since R2.02 a revision is a revset.

// 8
#[tokio::test]
async fn two_workspaces_same_change_both_snapshot() {
    let (mut fx, commit) = Fixture::with_files().await;
    let change = fx.change_of(&commit).await;
    let [a, b] = fx.diverge(&commit).await;
    assert_ne!(a, b);

    // jj's view of the outcome R1.13 builds on: one change, two visible commits.
    let head = fx.jj_head().await;
    let prefix = HexPrefix::try_from_hex(change.as_str()).unwrap();
    let PrefixResolution::SingleMatch(targets) =
        head.resolve_change_id_prefix(&prefix).await.unwrap()
    else {
        panic!("change not found");
    };
    let visible: BTreeSet<String> = targets
        .into_visible()
        .unwrap()
        .iter()
        .map(CommitId::hex)
        .collect();
    assert_eq!(visible, BTreeSet::from([a.to_string(), b.to_string()]));
}

/// The ADR 0014 consequences as the `jj` CLI sees them: `jj workspace list`
/// in the user's checkout shows the Reef workspace, and the Reef workspace
/// directory is not a jj repository.
#[tokio::test]
async fn jj_cli_lists_reef_workspace() {
    // The workspace store format and `workspace list` output are checked
    // against the jj-lib version Reef embeds; another CLI may differ.
    let Ok(version) = std::process::Command::new("jj").arg("--version").output() else {
        println!("skipped: jj not on PATH");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    if !version.trim().starts_with("jj 0.46.") {
        println!("skipped: jj CLI is {:?}, not 0.46.x", version.trim());
        return;
    }
    let (mut fx, commit) = Fixture::with_files().await;
    let (_ws, root, _) = fx.create("reef-x", StartPoint::Edit(commit)).await;
    let jj = |dir: &Path, args: &[&str]| {
        std::process::Command::new("jj")
            .args(args)
            .arg("--ignore-working-copy")
            .current_dir(dir)
            // Keep the user's jj config out of the test.
            .env("JJ_CONFIG", fx.dir.path().join("no-jj-config.toml"))
            .output()
            .expect("run jj")
    };

    let list = jj(fx.main.workspace_root(), &["workspace", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(stdout.contains("reef-x:"), "{stdout}");
    assert!(stdout.contains("default:"), "{stdout}");

    let status = jj(&root, &["status"]);
    assert!(
        !status.status.success(),
        "a Reef workspace is not a jj repo"
    );
}

// R1.05, R1.13: a reopened workspace snapshots even after other workspaces
// moved the head; its working copy is behind head but its commit was not
// rewritten, so it is fresh.
#[tokio::test]
async fn reopened_workspace_snapshots_after_others_moved_head() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (ws, root, state) = fx.create("reef-x", StartPoint::NewOn(commit.clone())).await;
    let created_at = ws.op_id();
    drop(ws);
    // Another workspace's creation moves the head past reef-x's operation.
    fx.create("reef-y", StartPoint::NewOn(commit)).await;
    assert_ne!(fx.main.op_id(), created_at);

    let mut ws = fx.main.open_workspace(&root, &state).unwrap();
    assert_eq!(ws.op_id(), fx.main.op_id(), "loaded at head");
    // The working copy starts empty and the commit holds the files; seed it.
    ws.reset_to(&ws.wc_change().unwrap().commit_id)
        .await
        .unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "dir/b.txt", "b\n");
    write(&root, "x.txt", "x\n");
    assert!(
        ws.snapshot("reef: snapshot workspace x")
            .await
            .unwrap()
            .changed
    );
    assert_eq!(ws.reload_at_working_copy_op().await.unwrap(), ws.op_id());
}

/// Two Reef workspaces on one empty change: `reef-x` creates it on the
/// user's working-copy commit and `reef-y` edits it.
async fn two_on_one_change() -> (Fixture, Repo, PathBuf, Repo, PathBuf) {
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx.create("reef-x", StartPoint::NewOn(base)).await;
    let change = x.wc_change().unwrap().commit_id;
    let (y, y_root, _) = fx.create("reef-y", StartPoint::Edit(change)).await;
    // As the daemon does before every snapshot: load at the current head.
    x.set_head(Arc::clone(fx.main.head()));
    (fx, x, x_root, y, y_root)
}

// R1.13: a stale working copy with no edits of its own follows the rewrite
// of its commit; no operation is written.
#[tokio::test]
async fn stale_adopts_when_clean() {
    let (_fx, mut x, x_root, mut y, y_root) = two_on_one_change().await;
    let shared = y.wc_change().unwrap().commit_id;
    write(&x_root, "from-x.txt", "x\n");
    let snapshot = x.snapshot("reef: snapshot workspace x").await.unwrap();
    assert!(snapshot.changed);

    y.set_head(Arc::clone(x.head()));
    let pending = y
        .begin_snapshot(Some(&shared), &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.adopted());
    assert!(!pending.changed());
    assert_eq!(pending.updated_files(), 1);
    assert_eq!(pending.wc_commit(), Some(&snapshot.wc_commit));
    pending.finish(&snapshot.op_id).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(y_root.join("from-x.txt")).unwrap(),
        "x\n"
    );
    assert_eq!(y.op_id(), snapshot.op_id, "no operation was written");
    // The working copy is fresh again.
    write(&y_root, "from-y.txt", "y\n");
    assert!(
        y.snapshot("reef: snapshot workspace y")
            .await
            .unwrap()
            .changed
    );
}

// R1.13: a stale working copy with edits rewrites the commit it last
// snapshotted, which makes the change divergent; neither workspace's files
// are lost.
#[tokio::test]
async fn stale_diverges_when_dirty() {
    let (fx, mut x, x_root, mut y, y_root) = two_on_one_change().await;
    let shared = y.wc_change().unwrap();
    write(&x_root, "from-x.txt", "x\n");
    write(&y_root, "from-y.txt", "y\n");
    let x_snapshot = x.snapshot("reef: snapshot workspace x").await.unwrap();

    y.set_head(Arc::clone(x.head()));
    let pending = y
        .begin_snapshot(Some(&shared.commit_id), &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.changed());
    assert!(!pending.adopted());
    let plan = pending.plan().clone();
    assert!(plan.divergent);
    assert_eq!(plan.rewrite_of, shared.commit_id);
    let committed = commit_snapshot(
        x.head(),
        &plan,
        &OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace y"),
    )
    .await
    .unwrap();
    assert!(committed.divergent);
    assert_eq!(committed.change_id, shared.change_id);
    pending.finish(&committed.op_id).await.unwrap();
    y.set_head(committed.repo);

    let mut main = fx.main;
    main.reload_at_head().await.unwrap();
    let error = main
        .resolve_revision(shared.change_id.as_str())
        .await
        .unwrap_err();
    let RevisionError::Ambiguous { commit_ids, .. } = error else {
        panic!("expected a divergent change, got {error:?}");
    };
    let commits: BTreeSet<_> = commit_ids.into_iter().collect();
    assert_eq!(
        commits,
        BTreeSet::from([x_snapshot.wc_commit, committed.commit_id.clone()])
    );
    assert_eq!(y.wc_change().unwrap().commit_id, committed.commit_id);
    assert!(y_root.join("from-y.txt").is_file());
    assert!(
        !y_root.join("from-x.txt").exists(),
        "x's edit is not adopted"
    );
}

// R1.13: a workspace jj forgot is recovered onto a new change on its last
// commit's parents.
#[tokio::test]
async fn forgotten_workspace_is_recovered() {
    let (mut fx, mut x, x_root, mut y, _) = two_on_one_change().await;
    let before = x.wc_change().unwrap();
    let parents = fx.main.commit_parents(&before.commit_id).await.unwrap();
    fx.main
        .forget_workspace(
            "reef-x",
            &OpMeta::new(OpKind::ReleaseWorkspace, "forget reef-x"),
        )
        .await
        .unwrap();
    drop(y.reload_at_head().await);
    write(&x_root, "after-forget.txt", "x\n");

    x.set_head(Arc::clone(fx.main.head()));
    let pending = x
        .begin_snapshot(Some(&before.commit_id), &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.changed());
    assert_eq!(pending.wc_commit(), None);
    let plan = pending.plan().clone();
    assert_eq!(plan.recover.as_deref(), Some(parents.as_slice()));
    let committed = commit_snapshot(
        fx.main.head(),
        &plan,
        &OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace x"),
    )
    .await
    .unwrap();
    assert!(!committed.divergent);
    assert_ne!(committed.change_id, before.change_id, "a fresh change id");
    pending.finish(&committed.op_id).await.unwrap();
    x.set_head(committed.repo);
    assert_eq!(x.wc_change().unwrap().commit_id, committed.commit_id);
    assert_eq!(
        fx.main.commit_parents(&committed.commit_id).await.unwrap(),
        parents
    );
}

// R1.05: the workspaces on a commit, a workspace's own change, and a
// commit's parents.
#[tokio::test]
async fn workspaces_on_commit_wc_change_and_parents() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (edit, _, _) = fx.create("reef-x", StartPoint::Edit(commit.clone())).await;
    let (new, _, _) = fx.create("reef-y", StartPoint::NewOn(commit.clone())).await;
    assert_eq!(
        fx.main.workspaces_on_commit(&commit),
        ["default", "reef-x"],
        "edit shares the commit with the user's own workspace"
    );
    let wc = edit.wc_change().unwrap();
    assert_eq!(wc.commit_id, commit);
    assert!(wc.is_wc);
    let new_wc = new.wc_change().unwrap();
    assert_ne!(new_wc.change_id, wc.change_id);
    assert_eq!(
        fx.main.commit_parents(&new_wc.commit_id).await.unwrap(),
        std::slice::from_ref(&commit)
    );
    assert_eq!(
        fx.main.workspaces_on_commit(&"00".parse().unwrap()).len(),
        0,
        "not a commit id of this repository"
    );
    assert!(
        fx.main
            .workspaces_on_commit(&new_wc.commit_id)
            .contains(&"reef-y".to_owned())
    );
}

// R1.06: moving a workspace's pointer, then checking the new tree out in
// its own working copy, writes only the files that differ.
#[tokio::test]
async fn move_pointer_then_check_out_tree() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (mut ws, root, _) = fx.create("reef-x", StartPoint::NewOn(commit.clone())).await;
    write(&root, "a.txt", "a\n");
    write(&root, "dir/b.txt", "b\n");
    ws.reset_to(&commit).await.unwrap();
    write(&root, "c.txt", "c\n");
    let edited = ws.snapshot("reef: snapshot workspace x").await.unwrap();
    assert!(edited.changed);
    fx.main.reload_at_head().await.unwrap();

    let moved = fx
        .main
        .move_pointer(
            "reef-x",
            StartPoint::NewOn(commit.clone()),
            &OpMeta::new(OpKind::MoveWorkspace, "reef: move workspace x"),
        )
        .await
        .unwrap();
    let op_id = moved.op_id.clone().expect("an operation");
    assert_eq!(fx.main.op_id(), op_id, "the caller adopts the operation");
    assert_eq!(
        fx.main.op_log(1).await.unwrap()[0].description,
        "reef: move workspace x"
    );
    assert_eq!(moved.from, edited.wc_commit);
    assert_ne!(moved.wc.commit_id, edited.wc_commit);
    assert_eq!(
        fx.main.commit_parents(&moved.wc.commit_id).await.unwrap(),
        std::slice::from_ref(&commit)
    );
    assert!(
        fx.head_ids().contains(edited.wc_commit.as_str()),
        "the commit it left holds an edit, so it stays"
    );

    ws.set_head(fx.main.readonly());
    let summary = ws.check_out_tree(&moved.wc.commit_id).await.unwrap();
    assert_eq!(
        (
            summary.updated,
            summary.added,
            summary.removed,
            summary.skipped
        ),
        (0, 0, 1, 0),
        "{summary:?}"
    );
    assert!(!root.join("c.txt").exists());
    assert!(root.join("a.txt").is_file());
    assert_eq!(ws.working_copy_op_id(), op_id);
    let after = ws.snapshot("reef: snapshot workspace x").await.unwrap();
    assert!(!after.changed, "{after:?}");
    assert_eq!(after.wc_commit, moved.wc.commit_id);
}

#[tokio::test]
async fn move_pointer_edits_abandons_and_refuses() {
    let (mut fx, commit) = Fixture::with_files().await;
    let (ws, _, _) = fx.create("reef-x", StartPoint::NewOn(commit.clone())).await;
    let empty = wc_commit_id(&ws);
    let before = fx.main.op_id();

    // Editing the commit it is on records nothing and abandons nothing.
    let same = fx
        .main
        .move_pointer(
            "reef-x",
            StartPoint::Edit(empty.parse().unwrap()),
            &OpMeta::new(OpKind::MoveWorkspace, ""),
        )
        .await
        .unwrap();
    assert_eq!(same.op_id, None);
    assert_eq!(same.wc.commit_id.as_str(), empty);
    assert_eq!(fx.main.op_id(), before);
    assert!(fx.head_ids().contains(&empty));

    // Leaving an empty commit abandons it, as `jj edit` does.
    let edited = fx
        .main
        .move_pointer(
            "reef-x",
            StartPoint::Edit(commit.clone()),
            &OpMeta::new(OpKind::MoveWorkspace, "reef: move"),
        )
        .await
        .unwrap();
    assert!(edited.op_id.is_some());
    assert_eq!(edited.wc.commit_id, commit);
    assert_eq!(fx.main.workspaces_on_commit(&commit), ["default", "reef-x"]);
    assert!(!fx.head_ids().contains(&empty), "empty commit abandoned");

    // A failed move goes back without abandoning the commit it edited,
    // even an empty head only it is on, and brings back the one it left.
    let (other, _, _) = fx.create("reef-y", StartPoint::NewOn(commit.clone())).await;
    let theirs = wc_commit_id(&other);
    drop(other);
    let theirs_id: CommitIdHex = theirs.parse().unwrap();
    fx.main
        .move_pointer(
            "reef-x",
            StartPoint::Edit(theirs_id.clone()),
            &OpMeta::new(OpKind::MoveWorkspace, "reef: move"),
        )
        .await
        .unwrap();
    fx.main
        .move_pointer(
            "reef-y",
            StartPoint::Edit(commit.clone()),
            &OpMeta::new(OpKind::MoveWorkspace, "reef: move"),
        )
        .await
        .unwrap();
    assert_eq!(fx.main.workspaces_on_commit(&theirs_id), ["reef-x"]);
    let restored = fx
        .main
        .restore_pointer(
            "reef-x",
            &empty.parse().unwrap(),
            false,
            &OpMeta::new(OpKind::MoveWorkspace, "reef: move back"),
        )
        .await
        .unwrap();
    assert_eq!(fx.main.op_id(), restored);
    assert!(fx.head_ids().contains(&empty), "visible again");
    assert!(fx.head_ids().contains(&theirs), "the edited commit stays");

    let error = fx
        .main
        .move_pointer(
            "reef-nope",
            StartPoint::Edit(commit),
            &OpMeta::new(OpKind::MoveWorkspace, ""),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, reef_lib::jj::MovePointerError::NoSuchWorkspace(_)),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

// R1.05: a colocated repository imports git's refs as bookmarks; an
// internal-git one is not colocated.
#[tokio::test]
async fn colocated_repository_imports_git_branches() {
    let (internal, _) = Fixture::with_files().await;
    assert!(!internal.main.is_colocated());
    if !git_available() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("colocated");
    std::fs::create_dir(&root).unwrap();
    write(&root, "a.txt", "a\n");
    let mut repo = Repo::init_colocated_git(&root, &settings()).await.unwrap();
    assert!(repo.is_colocated());
    let commit = repo.snapshot("snapshot").await.unwrap().wc_commit;

    let nothing = repo
        .import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
        .await
        .unwrap();
    assert_eq!(nothing.op_id, None, "nothing moved in git");

    let output = std::process::Command::new("git")
        .args(["branch", "feat", commit.as_str()])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing = repo.resolve_revision("feat").await.unwrap_err();
    assert!(
        matches!(missing, RevisionError::NotFound { .. }),
        "{missing:?}"
    );
    let imported = repo
        .import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
        .await
        .unwrap();
    assert_eq!(imported.op_id.as_ref(), Some(&repo.op_id()));
    assert_eq!(imported.failed_refs.len(), 0, "{:?}", imported.failed_refs);
    let resolved = repo.resolve_revision("feat").await.unwrap();
    assert_eq!(resolved.commit_id, commit);
}
