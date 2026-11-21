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
    default_backend_factories, default_working_copy_factories, default_workspace_loader_factory,
};
use jj_lib::object_id::{HexPrefix, ObjectId as _, PrefixResolution};
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::diff::conflicted_paths;
use reef_lib::jj::files::{self, Edit, EditAction};
use reef_lib::jj::land::LandTarget;
use reef_lib::jj::rebase::{self, RebaseMode, RebaseRequest};
use reef_lib::jj::revset::{RevisionError, RevsetScope};
use reef_lib::jj::settings::user_settings_with_max_new_file_size;
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::workspace::CommitLookupError;
use reef_lib::jj::{
    ChangeIdHex, CommitIdHex, CreateWorkspaceError, ForgetWorkspaceError, IgnoreSources, OpenError,
    Repo, StartPoint, commit_snapshot,
};
use reef_lib::jj::{OpKind, OpMeta, Unfinished};
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
        Self::with_settings(&settings()).await
    }

    async fn with_settings(settings: &UserSettings) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main");
        std::fs::create_dir(&root).expect("create main");
        let main = Repo::init_internal_git(&root, settings)
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
        let workspace = default_workspace_loader_factory()
            .create(self.main.workspace_root())
            .unwrap()
            .load(
                &settings(),
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .expect("load workspace");
        workspace
            .repo_loader()
            .load_at_head(None)
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
    // Skipped unless `jj` at jj-lib's minor or the one before is on `PATH`:
    // its workspace store format and `workspace list` output are what Reef
    // writes for; another CLI may differ.
    let Ok(version) = std::process::Command::new("jj").arg("--version").output() else {
        println!("skipped: jj not on PATH");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    if !jj_cli_is_supported(&version) {
        println!(
            "skipped: jj CLI is {:?}, not jj-lib {}'s minor or the one before",
            version.trim(),
            reef_lib::jj::JJ_LIB_VERSION
        );
        return;
    }
    println!("using jj CLI {:?}", version.trim());
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
        .begin_snapshot(Some(&shared), None, &IgnoreSources::default(), None)
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
        .begin_snapshot(
            Some(&shared.commit_id),
            None,
            &IgnoreSources::default(),
            None,
        )
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

/// One Reef workspace, `reef-y`, on a new change with `files` snapshotted
/// at commit C; the fixture's `main` is loaded at head to write on it.
/// The busy workspace's id: its snapshots name it, as the daemon's do.
const Y: &str = "00000000000000bb";

fn y_snapshot() -> OpMeta {
    OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace y").workspace(Y)
}

async fn busy_workspace(fx: &mut Fixture, files: &[(&str, &str)]) -> (Repo, PathBuf, CommitIdHex) {
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::NewOn(base))
        .await;
    for (path, contents) in files {
        write(&y_root, path, contents);
    }
    let snapshot = y.snapshot("reef: snapshot workspace y").await.unwrap();
    assert!(snapshot.changed);
    fx.main.reload_at_head().await.unwrap();
    (y, y_root, snapshot.wc_commit)
}

fn content(path: &str, contents: &str) -> Edit {
    Edit {
        path: path.into(),
        action: Some(EditAction::Content(contents.as_bytes().to_vec())),
        executable: None,
    }
}

/// The write: `main` rewrites `commit` with `edits`, moving the working
/// copy on it; returns the new commit.
async fn write_on(fx: &mut Fixture, commit: &CommitIdHex, edits: &[Edit]) -> CommitIdHex {
    fx.main
        .edit_commit(
            commit,
            edits,
            None,
            &OpMeta::new(OpKind::Edit, "reef: edit"),
        )
        .await
        .unwrap()
        .expect("the edit changed the tree")
        .commit_id
}

async fn file_in(repo: &ReadonlyRepo, commit: &CommitIdHex, path: &str) -> Option<String> {
    let path = RepoPathBuf::from_internal_string(path).unwrap();
    files::read_file(repo, commit, &path, 0, 0)
        .await
        .ok()
        .map(|read| String::from_utf8(read.content).unwrap())
}

/// The commits `change` resolves to in `main`, reloaded at head by the
/// caller.
async fn visible_commits(fx: &Fixture, change: &ChangeIdHex) -> BTreeSet<CommitIdHex> {
    match fx.main.resolve_revision(change.as_str()).await {
        Ok(resolved) => BTreeSet::from([resolved.commit_id]),
        Err(RevisionError::Ambiguous { commit_ids, .. }) => commit_ids.into_iter().collect(),
        Err(error) => panic!("cannot resolve {change}: {error:?}"),
    }
}

/// Snapshots `y`, which `main`'s head moved from `expected` to `to`,
/// against that head, commits and checks the carried commit out, and
/// saves; returns the commit and the files checked out.
async fn carry_snapshot(
    fx: &Fixture,
    y: &mut Repo,
    (expected, to): (&CommitIdHex, &CommitIdHex),
    unfinished: Option<&Unfinished>,
) -> (reef_lib::jj::SnapshotCommit, u32) {
    y.set_head(Arc::clone(fx.main.head()));
    let mut pending = y
        .begin_snapshot(Some(expected), unfinished, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.changed());
    assert!(pending.carried());
    assert!(!pending.adopted());
    let plan = pending.plan().clone();
    assert_eq!(&plan.rewrite_of, to);
    assert!(!plan.divergent);
    let committed = commit_snapshot(fx.main.head(), &plan, &y_snapshot())
        .await
        .unwrap();
    assert!(!committed.divergent);
    let files = pending
        .check_out_carried(&committed.repo, &committed.commit_id)
        .await
        .unwrap();
    pending.finish(&committed.op_id).await.unwrap();
    y.set_head(Arc::clone(&committed.repo));
    (committed, files)
}

// Issue #80: a stale working copy with edits, moved by a write of Reef's,
// records the merge of the write and its edits as a rewrite of the write's
// commit, in the same change, and checks it out. Nothing tells it of the
// write: it reads the operation log (ADR 0084).
#[tokio::test]
async fn stale_carries_onto_the_rewrite() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n"), ("b.txt", "b\n")]).await;
    let change = y.wc_change().unwrap().change_id;
    let c2 = write_on(
        &mut fx,
        &c,
        &[content("a.txt", "api a\n"), content("api.txt", "api\n")],
    )
    .await;
    write(&y_root, "late.txt", "late\n");
    write(&y_root, "b.txt", "late b\n");

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    assert_eq!(committed.change_id, change);
    assert_eq!(files, 2, "the write's a.txt and api.txt");
    for (path, contents) in [
        ("a.txt", "api a\n"),
        ("api.txt", "api\n"),
        ("late.txt", "late\n"),
        ("b.txt", "late b\n"),
    ] {
        assert_eq!(
            std::fs::read_to_string(y_root.join(path)).unwrap(),
            contents,
            "{path} on disk"
        );
        assert_eq!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .as_deref(),
            Some(contents),
            "{path} in the commit"
        );
    }
    fx.main.reload_at_head().await.unwrap();
    assert_eq!(
        visible_commits(&fx, &change).await,
        BTreeSet::from([committed.commit_id.clone()])
    );

    // The working copy is fresh again: the next edit rewrites the carried
    // commit.
    write(&y_root, "after.txt", "after\n");
    let pending = y
        .begin_snapshot(
            Some(&committed.commit_id),
            None,
            &IgnoreSources::default(),
            None,
        )
        .await
        .unwrap();
    assert!(pending.changed());
    assert!(!pending.carried());
    assert_eq!(pending.plan().rewrite_of, committed.commit_id);
    assert!(!pending.plan().divergent);
}

// Issue #80: where the write and the late edit change the same line, the
// carried commit holds a conflict, materialized on disk, in one commit.
#[tokio::test]
async fn carry_leaves_overlap_as_a_conflict() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "one\n"), ("b.txt", "b\n")]).await;
    let change = y.wc_change().unwrap().change_id;
    let c2 = write_on(&mut fx, &c, &[content("a.txt", "two\n")]).await;
    write(&y_root, "a.txt", "three\n");

    let (committed, _) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    assert_eq!(
        conflicted_paths(&committed.repo, &committed.commit_id, 10).unwrap(),
        (vec!["a.txt".to_owned()], 1)
    );
    let on_disk = std::fs::read_to_string(y_root.join("a.txt")).unwrap();
    assert!(
        on_disk.contains("two") && on_disk.contains("three"),
        "markers: {on_disk}"
    );
    fx.main.reload_at_head().await.unwrap();
    assert_eq!(
        visible_commits(&fx, &change).await,
        BTreeSet::from([committed.commit_id.clone()])
    );

    write(&y_root, "a.txt", "resolved\n");
    let resolved = y.snapshot("reef: snapshot workspace y").await.unwrap();
    assert!(resolved.changed);
    assert_eq!(
        conflicted_paths(y.head(), &resolved.wc_commit, 10).unwrap(),
        (Vec::new(), 0)
    );
}

// Issue #80: jj's tree merge decides modes and deletions; a new file over
// the size limit stays untracked and on disk.
#[tokio::test]
async fn carry_merges_modes_and_deletions() {
    let identity = Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    };
    let mut fx =
        Fixture::with_settings(&user_settings_with_max_new_file_size(&identity, 1024).unwrap())
            .await;
    let (mut y, y_root, c) = busy_workspace(
        &mut fx,
        &[
            ("run.sh", "echo 1\n"),
            ("gone.txt", "gone\n"),
            ("keep.txt", "keep\n"),
        ],
    )
    .await;
    let exec = Edit {
        path: "run.sh".into(),
        action: None,
        executable: Some(true),
    };
    let delete = Edit {
        path: "gone.txt".into(),
        action: Some(EditAction::Delete),
        executable: None,
    };
    let c2 = write_on(&mut fx, &c, &[exec, delete]).await;
    write(&y_root, "run.sh", "echo 2\n");
    write(&y_root, "gone.txt", "changed\n");
    std::fs::remove_file(y_root.join("keep.txt")).unwrap();
    write(&y_root, "big.bin", &"x".repeat(2048));

    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert_eq!(
        pending
            .untracked()
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["big.bin"]
    );
    drop(pending);
    let (committed, _) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    let (repo, id) = (&committed.repo, &committed.commit_id);
    assert_eq!(
        file_in(repo, id, "run.sh").await.as_deref(),
        Some("echo 2\n")
    );
    assert_eq!(file_in(repo, id, "keep.txt").await, None);
    assert_eq!(file_in(repo, id, "big.bin").await, None);
    assert_eq!(
        conflicted_paths(repo, id, 10).unwrap(),
        (vec!["gone.txt".to_owned()], 1)
    );
    let run = RepoPathBuf::from_internal_string("run.sh").unwrap();
    assert!(
        files::read_file(repo, id, &run, 0, 0)
            .await
            .unwrap()
            .entry
            .kind
            == reef_lib::jj::diff::EntryKind::Executable,
        "the write's bit"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(y_root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable on disk: {mode:o}");
    }
    assert!(y_root.join("big.bin").is_file());
}

// ADR 0084: two writes of Reef's before the snapshot are followed as one
// move, from the commit the working copy was checked out from to the
// view's.
#[tokio::test]
async fn two_writes_carry_as_one_move() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    let c3 = write_on(&mut fx, &c2, &[content("api2.txt", "api\n")]).await;
    write(&y_root, "late.txt", "late\n");
    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c3), None).await;
    assert_eq!(files, 2, "both writes' files");
    for path in ["a.txt", "api.txt", "api2.txt", "late.txt"] {
        assert!(y_root.join(path).is_file(), "{path} on disk");
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// ADR 0018's fourth arm stands for a move that is not Reef's: another jj
// client's rewrite, a commit the working copy was not checked out from,
// and another workspace's snapshot of the same change. The operation log,
// not the shape of the change, decides.
#[tokio::test]
async fn a_move_that_is_not_reefs_takes_arm_four() {
    // (a) another jj client rewrote the commit.
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    fx.main
        .edit_commit(
            &c,
            &[content("api.txt", "api\n")],
            None,
            &OpMeta::new(OpKind::External, "another client's rewrite"),
        )
        .await
        .unwrap()
        .expect("the edit changed the tree");
    write(&y_root, "late.txt", "late\n");
    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.changed());
    assert!(!pending.carried());
    assert_eq!(pending.plan().rewrite_of, c);
    assert!(pending.plan().divergent);
    drop(pending);

    // (b) Reef's write, but the row names a commit the view never had for
    // this workspace.
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    write(&y_root, "late.txt", "late\n");
    y.set_head(Arc::clone(fx.main.head()));
    let other = fx.main.commit_parents(&c).await.unwrap().remove(0);
    let pending = y
        .begin_snapshot(Some(&other), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(!pending.carried());
    assert_eq!(pending.plan().rewrite_of, other);
    drop(pending);

    // (c) two workspaces on one change: x's snapshot is not y's to follow.
    let x_id = "00000000000000aa";
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create(&format!("reef-{x_id}"), StartPoint::NewOn(base))
        .await;
    let shared = x.wc_change().unwrap().commit_id;
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::Edit(shared.clone()))
        .await;
    write(&x_root, "from-x.txt", "x\n");
    write(&y_root, "from-y.txt", "y\n");
    x.set_head(Arc::clone(fx.main.head()));
    let pending = x
        .begin_snapshot(Some(&shared), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    let meta = OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace x").workspace(x_id);
    let x_snapshot = commit_snapshot(fx.main.head(), pending.plan(), &meta)
        .await
        .unwrap();
    pending.finish(&x_snapshot.op_id).await.unwrap();
    y.set_head(Arc::clone(&x_snapshot.repo));
    let pending = y
        .begin_snapshot(Some(&shared), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(!pending.carried());
    assert_eq!(pending.plan().rewrite_of, shared);
    assert!(pending.plan().divergent);
}

// ADR 0084: a snapshot of the workspace below, which rebases this one's
// commit, is Reef's move: the edits made meanwhile are carried onto the
// rebased commit.
#[tokio::test]
async fn a_snapshot_below_carries() {
    let x_id = "00000000000000aa";
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create(&format!("reef-{x_id}"), StartPoint::NewOn(base))
        .await;
    let below = x.wc_change().unwrap().commit_id;
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::NewOn(below.clone()))
        .await;
    let c = y.wc_change().unwrap().commit_id;
    write(&x_root, "from-x.txt", "x\n");
    write(&y_root, "from-y.txt", "y\n");
    x.set_head(Arc::clone(fx.main.head()));
    let pending = x
        .begin_snapshot(Some(&below), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    let meta = OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace x").workspace(x_id);
    let x_snapshot = commit_snapshot(fx.main.head(), pending.plan(), &meta)
        .await
        .unwrap();
    pending.finish(&x_snapshot.op_id).await.unwrap();
    fx.main.set_head(Arc::clone(&x_snapshot.repo));
    let rebased = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(rebased, c);

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &rebased), None).await;
    assert_eq!(files, 1, "x's file");
    for path in ["from-x.txt", "from-y.txt"] {
        assert!(y_root.join(path).is_file(), "{path} on disk");
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

/// Another jj client's operation on `at`, bookmark `name` set on `commit`,
/// concurrent with whatever `main` wrote since `at`: `main`'s next reload
/// merges the operation heads. Returns the repository at that operation.
async fn concurrent_bookmark(
    at: &Arc<ReadonlyRepo>,
    name: &str,
    commit: &CommitIdHex,
) -> Arc<ReadonlyRepo> {
    let mut tx = at.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit.as_str()).unwrap()),
    );
    tx.commit(format!("another client's bookmark {name}"))
        .await
        .unwrap()
}

/// Another jj client's rewrite of `commit` on `at`, which empties its
/// tree, concurrent with whatever `main` wrote since `at`. It moves every
/// working copy on the commit. Returns the repository at that operation.
async fn concurrent_rewrite(at: &Arc<ReadonlyRepo>, commit: &CommitIdHex) -> Arc<ReadonlyRepo> {
    let id = CommitId::try_from_hex(commit.as_str()).unwrap();
    let old = at.store().get_commit_async(&id).await.unwrap();
    let mut tx = at.start_transaction();
    tx.repo_mut()
        .rewrite_commit(&old)
        .set_tree(at.store().root_commit().tree())
        .write()
        .await
        .unwrap();
    tx.repo_mut().rebase_descendants().await.unwrap();
    tx.commit("another client's rewrite").await.unwrap()
}

/// Another jj client's new description of `commit` on `at`, concurrent
/// with whatever `main` wrote since `at`. It keeps the commit's tree.
/// Returns the repository at that operation.
async fn concurrent_describe(at: &Arc<ReadonlyRepo>, commit: &CommitIdHex) -> Arc<ReadonlyRepo> {
    let id = CommitId::try_from_hex(commit.as_str()).unwrap();
    let old = at.store().get_commit_async(&id).await.unwrap();
    let mut tx = at.start_transaction();
    tx.repo_mut()
        .rewrite_commit(&old)
        .set_description("another client's")
        .write()
        .await
        .unwrap();
    tx.repo_mut().rebase_descendants().await.unwrap();
    tx.commit("another client's describe").await.unwrap()
}

/// Points `main` at the merge of `heads`, as jj merges operation heads:
/// each one in turn into the first.
async fn merge_in_order(fx: &mut Fixture, heads: &[&Arc<ReadonlyRepo>]) {
    let operations = heads.iter().map(|head| head.operation().clone()).collect();
    let attributes = Vec::<(String, String)>::new();
    let (merged, _) = fx
        .main
        .head()
        .loader()
        .merge_operations(operations, None, None, attributes)
        .await
        .unwrap();
    assert_eq!(merged.operation().parent_ids().len(), heads.len());
    fx.main.set_head(merged);
}

/// Reloads `main` at head, which merges two operation heads.
async fn merge_heads(fx: &mut Fixture) {
    fx.main.reload_at_head().await.unwrap();
    assert_eq!(
        fx.main.head().operation().parent_ids().len(),
        2,
        "the head is a merge of two operations"
    );
}

/// `y`'s snapshot against `main`'s head, with edits on `expected`, takes
/// ADR 0018's fourth arm.
async fn assert_diverges(fx: &Fixture, y: &mut Repo, expected: &CommitIdHex) {
    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(expected), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.changed());
    assert!(!pending.carried());
    assert_eq!(&pending.plan().rewrite_of, expected);
    assert!(pending.plan().divergent);
}

// Review 7, F1: another jj client's operation concurrent with the snapshot
// of the workspace below leaves a merge of the two operation heads. The
// merge took this workspace's commit from the snapshot's side, and the
// client's side never moved it: the move is still Reef's.
#[tokio::test]
async fn a_merge_beside_a_snapshot_below_carries() {
    let x_id = "00000000000000aa";
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create(&format!("reef-{x_id}"), StartPoint::NewOn(base))
        .await;
    let below = x.wc_change().unwrap().commit_id;
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::NewOn(below.clone()))
        .await;
    let c = y.wc_change().unwrap().commit_id;
    write(&x_root, "from-x.txt", "x\n");
    write(&y_root, "from-y.txt", "y\n");
    let fork = Arc::clone(fx.main.head());
    x.set_head(Arc::clone(&fork));
    let pending = x
        .begin_snapshot(Some(&below), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    let meta = OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace x").workspace(x_id);
    let x_snapshot = commit_snapshot(&fork, pending.plan(), &meta).await.unwrap();
    pending.finish(&x_snapshot.op_id).await.unwrap();
    concurrent_bookmark(&fork, "concurrent", &below).await;
    merge_heads(&mut fx).await;
    let rebased = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(rebased, c);

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &rebased), None).await;
    assert_eq!(files, 1, "x's file");
    for path in ["from-x.txt", "from-y.txt"] {
        assert!(y_root.join(path).is_file(), "{path} on disk");
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 7, F1: the other client loaded the head before this workspace's
// last snapshot, and committed after Reef's write: its side of the merge
// has a commit older than the one the working copy was checked out from.
// It still never moved it.
#[tokio::test]
async fn a_merge_with_a_side_older_than_the_last_snapshot_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c0) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    write(&y_root, "b.txt", "b\n");
    let c = y
        .snapshot("reef: snapshot workspace y")
        .await
        .unwrap()
        .wc_commit;
    assert_ne!(c, c0);
    fx.main.reload_at_head().await.unwrap();
    let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    concurrent_bookmark(&fork, "concurrent", &c0).await;
    merge_heads(&mut fx).await;
    write(&y_root, "late.txt", "late\n");

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    assert_eq!(files, 1, "the write's file");
    for path in ["a.txt", "b.txt", "api.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 7, F1: a merge after Reef's write, of two operations that both
// left this workspace's commit alone, is walked through to the write.
#[tokio::test]
async fn a_merge_after_the_write_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    let fork = Arc::clone(fx.main.head());
    let base = fx.main.wc_change().unwrap().commit_id;
    fx.create("reef-00000000000000cc", StartPoint::NewOn(base.clone()))
        .await;
    concurrent_bookmark(&fork, "concurrent", &base).await;
    merge_heads(&mut fx).await;
    write(&y_root, "late.txt", "late\n");

    let (committed, _) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    for path in ["a.txt", "api.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 7, F1: a merge does not launder a move that is not Reef's. ADR
// 0018's fourth arm stands when the other client's side moved this
// workspace's commit, alone or beside a write of Reef's, and when the
// merge is beside another workspace's snapshot of the same change.
#[tokio::test]
async fn a_merge_keeps_arm_four_for_moves_that_are_not_reefs() {
    // (a) only the other client moved the commit; Reef's side is unrelated.
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    let base = fx.main.wc_change().unwrap().commit_id;
    fx.create("reef-00000000000000cc", StartPoint::NewOn(base))
        .await;
    concurrent_rewrite(&fork, &c).await;
    merge_heads(&mut fx).await;
    let moved = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(moved, c, "the other client's rewrite moved y");
    write(&y_root, "late.txt", "late\n");
    assert_diverges(&fx, &mut y, &c).await;

    // (b) both sides moved it: Reef's write and the other client's rewrite.
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    concurrent_rewrite(&fork, &c).await;
    merge_heads(&mut fx).await;
    write(&y_root, "late.txt", "late\n");
    assert_diverges(&fx, &mut y, &c).await;

    // (c) two workspaces on one change: x's snapshot is not y's to follow,
    // with a merge beside it either.
    let x_id = "00000000000000aa";
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create(&format!("reef-{x_id}"), StartPoint::NewOn(base.clone()))
        .await;
    let shared = x.wc_change().unwrap().commit_id;
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::Edit(shared.clone()))
        .await;
    write(&x_root, "from-x.txt", "x\n");
    write(&y_root, "from-y.txt", "y\n");
    let fork = Arc::clone(fx.main.head());
    x.set_head(Arc::clone(&fork));
    let pending = x
        .begin_snapshot(Some(&shared), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    let meta = OpMeta::new(OpKind::Snapshot, "reef: snapshot workspace x").workspace(x_id);
    let x_snapshot = commit_snapshot(&fork, pending.plan(), &meta).await.unwrap();
    pending.finish(&x_snapshot.op_id).await.unwrap();
    concurrent_bookmark(&fork, "concurrent", &base).await;
    merge_heads(&mut fx).await;
    assert_diverges(&fx, &mut y, &shared).await;
}

// Review 8, R8-1: two other jj clients ran beside Reef's write, and the
// reload merges three operation heads in one operation. Neither client
// moved this workspace's commit: the move is still Reef's.
#[tokio::test]
async fn a_merge_of_three_heads_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    let base = fx.main.wc_change().unwrap().commit_id;
    let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    concurrent_bookmark(&fork, "one", &base).await;
    concurrent_bookmark(&fork, "two", &base).await;
    fx.main.reload_at_head().await.unwrap();
    assert_eq!(fx.main.head().operation().parent_ids().len(), 3);
    write(&y_root, "late.txt", "late\n");

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    assert_eq!(files, 1, "the write's file");
    for path in ["a.txt", "api.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 8, R8-1: a criss-cross. Reef's write and another client's
// operation are merged twice, by the daemon and by another jj process, and
// the next reload merges the two merges, which have two closest common
// ancestors. Only Reef moved this workspace's commit.
#[tokio::test]
async fn a_criss_cross_merge_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    let base = fx.main.wc_change().unwrap().commit_id;
    let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    let written = Arc::clone(fx.main.head());
    let other = concurrent_bookmark(&fork, "one", &base).await;
    merge_heads(&mut fx).await;
    // Another jj process merged the same two heads, and wrote on top.
    let daemons = Arc::clone(fx.main.head());
    merge_in_order(&mut fx, &[&written, &other]).await;
    concurrent_bookmark(fx.main.head(), "two", &base).await;
    fx.main.set_head(daemons);
    merge_heads(&mut fx).await;
    write(&y_root, "late.txt", "late\n");

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c2), None).await;
    assert_eq!(files, 1, "the write's file");
    for path in ["a.txt", "api.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 8, R8-1: no shape of merge launders a move that is not Reef's.
// ADR 0018's fourth arm stands when another client moved this workspace's
// commit on any of the merged heads, whichever commit the merge kept, and
// when the merge itself moved it.
#[tokio::test]
async fn a_merge_of_many_heads_keeps_arm_four() {
    // (a) three heads: Reef's write, a bookmark, and another client's
    // rewrite. jj keeps the commit of the head it merges into, so each
    // order is tried: Reef's commit kept, and the other client's.
    for reefs_first in [true, false] {
        let mut fx = Fixture::new().await;
        let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
        let fork = Arc::clone(fx.main.head());
        let base = fx.main.wc_change().unwrap().commit_id;
        let c2 = write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
        let written = Arc::clone(fx.main.head());
        let bookmark = concurrent_bookmark(&fork, "one", &base).await;
        let rewrite = concurrent_rewrite(&fork, &c).await;
        let heads = if reefs_first {
            [&written, &bookmark, &rewrite]
        } else {
            [&rewrite, &bookmark, &written]
        };
        merge_in_order(&mut fx, &heads).await;
        let kept = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
        assert_eq!(kept == c2, reefs_first, "the merge kept its first head's");
        write(&y_root, "late.txt", "late\n");
        assert_diverges(&fx, &mut y, &c).await;
    }

    // (b) a criss-cross of Reef's write and another client's rewrite.
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let fork = Arc::clone(fx.main.head());
    let base = fx.main.wc_change().unwrap().commit_id;
    write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    let written = Arc::clone(fx.main.head());
    let rewrite = concurrent_rewrite(&fork, &c).await;
    merge_in_order(&mut fx, &[&written, &rewrite]).await;
    let daemons = Arc::clone(fx.main.head());
    merge_in_order(&mut fx, &[&written, &rewrite]).await;
    let others = concurrent_bookmark(fx.main.head(), "two", &base).await;
    merge_in_order(&mut fx, &[&daemons, &others]).await;
    write(&y_root, "late.txt", "late\n");
    assert_diverges(&fx, &mut y, &c).await;

    // (c) the merge moved the commit: Reef rebased y onto x's commit, and
    // another client described x's commit meanwhile, so the merge rebased
    // y's commit again. Neither head has the commit the merge has.
    let x_id = "00000000000000aa";
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create(&format!("reef-{x_id}"), StartPoint::NewOn(base))
        .await;
    write(&x_root, "from-x.txt", "x\n");
    let onto = x
        .snapshot("reef: snapshot workspace x")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let fork = Arc::clone(fx.main.head());
    let request = RebaseRequest {
        revision: c.as_str(),
        onto: Some(onto.as_str()),
        mode: RebaseMode::Source,
        expected: None,
        remote: None,
    };
    let scope = RevsetScope {
        workspace: None,
        user_email: "reef-test@example.com",
        root: fx.main.workspace_root(),
    };
    let plan = rebase::plan(&fork, &request, &scope).await.unwrap();
    let meta = OpMeta::new(OpKind::Rebase, "reef: rebase");
    fx.main.rebase(&plan, &meta).await.unwrap();
    let rebased = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(rebased, c, "Reef's rebase moved y");
    concurrent_describe(&fork, &onto).await;
    merge_heads(&mut fx).await;
    let merged = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(merged, rebased, "the merge moved y again");
    write(&y_root, "late.txt", "late\n");
    assert_diverges(&fx, &mut y, &c).await;
}

/// `y`'s snapshot against `main`'s head, clean, with `expected` its last
/// snapshot: it diverges, planning a rewrite of `expected`, and keeps its
/// files on disk.
async fn assert_keeps(fx: &Fixture, y: &mut Repo, y_root: &Path, expected: &CommitIdHex) {
    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(expected), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(!pending.adopted(), "the older commit is not checked out");
    assert_eq!(&pending.plan().rewrite_of, expected);
    assert!(pending.plan().divergent);
    let committed = commit_snapshot(fx.main.head(), pending.plan(), &y_snapshot())
        .await
        .unwrap();
    assert!(committed.divergent);
    pending.finish(&committed.op_id).await.unwrap();
    assert!(y_root.join("two.txt").is_file(), "two.txt stays on disk");
    assert!(
        file_in(&committed.repo, &committed.commit_id, "two.txt")
            .await
            .is_some(),
        "and in the workspace's commit"
    );
}

// Issue #344: another jj client loaded the head before `y`'s last snapshot,
// which added two.txt, and described the commit before it. jj's merge of
// the two operation heads kept the client's commit, as it does when the
// client's clock is behind. `y` has no edits, but that commit did not come
// from its last snapshot: checking it out would take two.txt off disk.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_an_older_rewrite() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("one.txt", "one\n")]).await;
    let stale = Arc::clone(fx.main.head());
    write(&y_root, "two.txt", "two\n");
    y.set_head(Arc::clone(fx.main.head()));
    let c2 = y
        .snapshot("reef: snapshot workspace y")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let reefs = Arc::clone(fx.main.head());
    let clients = concurrent_describe(&stale, &c).await;
    merge_in_order(&mut fx, &[&clients, &reefs]).await;
    let kept = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(kept, c2, "the merge kept the client's commit");

    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

// Issue #344: a rewrite by another jj client that loaded the head after
// `y`'s last snapshot came from it, and a clean `y` checks it out, though
// the move is not Reef's.
#[tokio::test]
async fn a_clean_workspace_follows_a_clients_rewrite_of_its_snapshot() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("one.txt", "one\n")]).await;
    let emptied = concurrent_rewrite(fx.main.head(), &c).await;
    fx.main.set_head(emptied);

    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.adopted());
    assert!(!pending.changed());
    pending.finish(&fx.main.op_id()).await.unwrap();
    assert!(!y_root.join("one.txt").exists(), "the rewrite emptied it");
}

/// `y` on the change of workspace `reef-aa` below it, with one.txt and
/// then two.txt snapshotted, at C2. Another jj client loaded the head
/// between the two snapshots and emptied the change below, which rebased
/// `y`'s commit as it was before two.txt. With `base_described`, a second
/// client loaded at the same head describes the commit both changes are
/// on. `main` is left at the merge of the operation heads, Reef's and the
/// clients', rotated left by `order` and each merged into the first.
async fn below_rewritten_beside_a_snapshot(
    order: usize,
    base_described: bool,
) -> (Fixture, Repo, PathBuf, CommitIdHex) {
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create("reef-00000000000000aa", StartPoint::NewOn(base.clone()))
        .await;
    write(&x_root, "below.txt", "below\n");
    let below = x
        .snapshot("reef: snapshot workspace x")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let (mut y, y_root, _) = fx
        .create(&format!("reef-{Y}"), StartPoint::NewOn(below.clone()))
        .await;
    // A new workspace's directory starts empty; it holds the change below.
    write(&y_root, "below.txt", "below\n");
    write(&y_root, "one.txt", "one\n");
    y.snapshot("reef: snapshot workspace y").await.unwrap();
    fx.main.reload_at_head().await.unwrap();
    let stale = Arc::clone(fx.main.head());
    write(&y_root, "two.txt", "two\n");
    y.set_head(Arc::clone(fx.main.head()));
    let c2 = y
        .snapshot("reef: snapshot workspace y")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let reefs = Arc::clone(fx.main.head());
    let mut heads = vec![reefs, concurrent_rewrite(&stale, &below).await];
    if base_described {
        heads.push(concurrent_describe(&stale, &base).await);
    }
    heads.rotate_left(order);
    let heads: Vec<_> = heads.iter().collect();
    merge_in_order(&mut fx, &heads).await;
    (fx, y, y_root, c2)
}

// Issue #344: the merge kept `y`'s last snapshot and rebased it onto the
// emptied change below. The merge made that commit from the snapshot, so
// a clean `y` checks it out: two.txt stays, below.txt goes with the change
// below.
#[tokio::test]
async fn a_clean_workspace_follows_a_merge_that_rebased_its_snapshot() {
    let (fx, mut y, y_root, c2) = below_rewritten_beside_a_snapshot(0, false).await;
    assert_follows_the_rebase(&fx, &mut y, &y_root, &c2).await;
}

// #344 review, F1: two other clients rewrote the commits below `y`, so the
// merge of three heads rebased `y`'s last snapshot after merging each in.
// jj records the second rebase's predecessor as the first rebase, a commit
// the merge wrote itself; the move is still from the snapshot.
#[tokio::test]
async fn a_clean_workspace_follows_a_merge_of_three_heads_that_rebased_its_snapshot_twice() {
    let (fx, mut y, y_root, c2) = below_rewritten_beside_a_snapshot(0, true).await;
    assert_follows_the_rebase(&fx, &mut y, &y_root, &c2).await;
}

/// `y`, clean with C2 its last snapshot, checks out the commit a merge of
/// operation heads rebased C2 to.
async fn assert_follows_the_rebase(fx: &Fixture, y: &mut Repo, y_root: &Path, c2: &CommitIdHex) {
    let rebased = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(&rebased, c2, "the merge rebased the snapshot");

    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(c2), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.adopted());
    assert_eq!(pending.wc_commit(), Some(&rebased));
    pending.finish(&fx.main.op_id()).await.unwrap();
    assert!(y_root.join("two.txt").is_file());
    assert!(!y_root.join("below.txt").exists());
}

// Issue #344: the same scene with the client's head merged into: the merge
// kept the client's rebase of the commit before two.txt.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_an_older_rebase() {
    let (fx, mut y, y_root, c2) = below_rewritten_beside_a_snapshot(1, false).await;
    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

/// `y` with one.txt snapshotted at C and then two.txt at C2, with `main`
/// at C2's operation. Returns `y`, its root, C, C2, and the repository at
/// C's operation.
async fn snapshotted_twice(
    fx: &mut Fixture,
) -> (Repo, PathBuf, CommitIdHex, CommitIdHex, Arc<ReadonlyRepo>) {
    let (mut y, y_root, c) = busy_workspace(fx, &[("one.txt", "one\n")]).await;
    let before = Arc::clone(fx.main.head());
    write(&y_root, "two.txt", "two\n");
    y.set_head(Arc::clone(fx.main.head()));
    let c2 = y
        .snapshot("reef: snapshot workspace y")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    (y, y_root, c, c2, before)
}

/// Another jj client's restore of `main`'s head to `to`'s view, described
/// `description`, as `jj undo` and `jj op restore` write it: no `reef.*`
/// attributes and no commit rewritten.
async fn restore_as_client(fx: &mut Fixture, to: &ReadonlyRepo, description: &str) {
    let mut tx = fx.main.head().start_transaction();
    tx.repo_mut().set_view(to.view().store_view().clone());
    let restored = tx.commit(description).await.unwrap();
    fx.main.set_head(restored);
}

/// The jj CLI's description of `jj undo` back to `to`.
fn jj_undo_description(to: &ReadonlyRepo) -> String {
    format!("undo: restore to operation {}", to.op_id().hex())
}

// Issue #357: `jj undo` at the root undid `y`'s last snapshot. The jj CLI
// describes its undo as Reef's spike once did, but it is another client's,
// and it rewrote nothing: a clean `y` keeps two.txt and diverges.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_a_jj_undo() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c, c2, before) = snapshotted_twice(&mut fx).await;
    restore_as_client(&mut fx, &before, &jj_undo_description(&before)).await;
    let now = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_eq!(now, c, "the undo put y back on C");

    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

// Issue #357: the same with `jj op restore` to the operation before C2.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_an_op_restore() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _, c2, before) = snapshotted_twice(&mut fx).await;
    let description = format!("restore to operation {}", before.op_id().hex());
    restore_as_client(&mut fx, &before, &description).await;

    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

// Issue #357: another client abandoned `y`'s last snapshot, which put `y`
// on a new commit made from nothing of C2's.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_a_clients_abandon() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
    let id = CommitId::try_from_hex(c2.as_str()).unwrap();
    let head = Arc::clone(fx.main.head());
    let abandoned = head.store().get_commit_async(&id).await.unwrap();
    let mut tx = head.start_transaction();
    tx.repo_mut().record_abandoned_commit(&abandoned);
    tx.repo_mut().rebase_descendants().await.unwrap();
    fx.main.set_head(tx.commit("abandon commit").await.unwrap());
    let now = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(now, c2, "the abandon moved y");

    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

// Issue #357: `y` has an edit since its last snapshot when `jj undo` undoes
// that snapshot. The undo is not Reef's, so nothing is carried onto C: `y`
// diverges with both files on disk.
#[tokio::test]
async fn a_busy_workspace_keeps_its_snapshot_over_a_jj_undo() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _, c2, before) = snapshotted_twice(&mut fx).await;
    write(&y_root, "three.txt", "three\n");
    restore_as_client(&mut fx, &before, &jj_undo_description(&before)).await;

    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(&c2), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(
        !pending.carried(),
        "nothing is carried onto the undone commit"
    );
    assert_eq!(pending.plan().rewrite_of, c2);
    assert!(pending.plan().divergent);
    let committed = commit_snapshot(fx.main.head(), pending.plan(), &y_snapshot())
        .await
        .unwrap();
    pending.finish(&committed.op_id).await.unwrap();
    assert!(y_root.join("two.txt").is_file(), "two.txt stays on disk");
    assert!(y_root.join("three.txt").is_file(), "and so does the edit");
    for path in ["two.txt", "three.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} is in the workspace's commit"
        );
    }
}

/// Issue #357, from the #344 review (R4-1): Reef undid `y`'s last snapshot
/// C2 on one operation head, back to C, while another jj client that
/// loaded the head before C2 described C on the other. `main` is left at
/// the merge of the two heads: the client's merged into Reef's, or with
/// `clients_first` Reef's into the client's.
async fn undone_beside_a_stale_describe(
    clients_first: bool,
) -> (Fixture, Repo, PathBuf, CommitIdHex) {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("one.txt", "one\n")]).await;
    let stale = Arc::clone(fx.main.head());
    write(&y_root, "two.txt", "two\n");
    y.set_head(Arc::clone(fx.main.head()));
    let snapshot = y.snapshot("reef: snapshot workspace y").await.unwrap();
    fx.main.reload_at_head().await.unwrap();
    reefs_undo(&mut fx, &snapshot.op_id).await;
    let undone = Arc::clone(fx.main.head());
    let clients = concurrent_describe(&stale, &c).await;
    let heads = if clients_first {
        [&clients, &undone]
    } else {
        [&undone, &clients]
    };
    merge_in_order(&mut fx, &heads).await;
    (fx, y, y_root, snapshot.wc_commit)
}

/// The daemon's undo of `op` on `main`.
async fn reefs_undo(fx: &mut Fixture, op: &reef_lib::jj::OperationIdHex) {
    let undo = OpMeta::new(OpKind::Undo, format!("reef: undo operation {op}"));
    fx.main
        .revert_operation(op, &undo)
        .await
        .unwrap()
        .expect("the undo changed the view");
}

// The client's describe started at C, which only the undo beside it
// reached: the client never saw C2 or the undo. In either merge order a
// clean `y` keeps two.txt.
#[tokio::test]
async fn a_stale_describe_beside_an_undo_keeps_the_snapshot_in_either_merge_order() {
    for clients_first in [false, true] {
        let (fx, mut y, y_root, c2) = undone_beside_a_stale_describe(clients_first).await;
        assert_keeps(&fx, &mut y, &y_root, &c2).await;
    }
}

// Reef's own undo of `y`'s last snapshot is still followed: someone asked
// Reef to leave C2.
#[tokio::test]
async fn a_clean_workspace_follows_reefs_undo_of_its_snapshot() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("one.txt", "one\n")]).await;
    write(&y_root, "two.txt", "two\n");
    y.set_head(Arc::clone(fx.main.head()));
    let snapshot = y.snapshot("reef: snapshot workspace y").await.unwrap();
    fx.main.reload_at_head().await.unwrap();
    reefs_undo(&mut fx, &snapshot.op_id).await;
    let now = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_eq!(now, c, "the undo put y back on C");

    assert_follows(&fx, &mut y, &snapshot.wc_commit).await;
    assert!(
        !y_root.join("two.txt").exists(),
        "the undo took two.txt away"
    );
}

/// `y`, clean with `last` its last snapshot, checks out the commit `main`'s
/// view has for it.
async fn assert_follows(fx: &Fixture, y: &mut Repo, last: &CommitIdHex) {
    let now = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_ne!(&now, last, "something moved y");
    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(last), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.adopted(), "y checks out the view's commit");
    assert_eq!(pending.wc_commit(), Some(&now));
    pending.finish(&fx.main.op_id()).await.unwrap();
}

// #357 review, F7: one client emptied `y`'s last snapshot while another,
// loaded at the same head, set a bookmark. The merge of the two heads has
// both in its history, so the rewrite counts whichever head comes first.
#[tokio::test]
async fn a_clean_workspace_follows_a_rewrite_on_either_side_of_a_merge() {
    for rewrite_first in [true, false] {
        let mut fx = Fixture::new().await;
        let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
        let at = Arc::clone(fx.main.head());
        let emptied = concurrent_rewrite(&at, &c2).await;
        let base = fx.main.wc_change().unwrap().commit_id;
        let bookmarked = concurrent_bookmark(&at, "other", &base).await;
        let heads = if rewrite_first {
            [&emptied, &bookmarked]
        } else {
            [&bookmarked, &emptied]
        };
        merge_in_order(&mut fx, &heads).await;

        assert_follows(&fx, &mut y, &c2).await;
        assert!(!y_root.join("two.txt").exists(), "the rewrite emptied it");
    }
}

// #357 review round 2: a fork inside the operations since `y`'s last
// snapshot. One client described C2; on its operation, another emptied the
// described commit and a third set a bookmark. The rewrite starts at a
// commit only the fork's set holds, so it counts in either merge order.
#[tokio::test]
async fn a_clean_workspace_follows_a_rewrite_after_a_fork_since_its_snapshot() {
    for rewrite_first in [true, false] {
        let mut fx = Fixture::new().await;
        let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
        let described = concurrent_describe(fx.main.head(), &c2).await;
        let c2_described = reef_lib::jj::view_wc_commit(&described, &format!("reef-{Y}")).unwrap();
        let emptied = concurrent_rewrite(&described, &c2_described).await;
        let base = fx.main.wc_change().unwrap().commit_id;
        let bookmarked = concurrent_bookmark(&described, "other", &base).await;
        let heads = if rewrite_first {
            [&emptied, &bookmarked]
        } else {
            [&bookmarked, &emptied]
        };
        merge_in_order(&mut fx, &heads).await;

        assert_follows(&fx, &mut y, &c2).await;
        assert!(!y_root.join("two.txt").exists(), "the rewrite emptied it");
    }
}

// #357 review, F8: `jj op revert` of `y`'s last snapshot, as the jj CLI
// writes it: the snapshot's parent merged over it, with no commit
// rewritten. A clean `y` keeps two.txt.
#[tokio::test]
async fn a_clean_workspace_keeps_its_snapshot_over_an_op_revert() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c, c2, before) = snapshotted_twice(&mut fx).await;
    let snapshot = Arc::clone(fx.main.head());
    let mut tx = snapshot.start_transaction();
    tx.repo_mut().merge(&snapshot, &before).await.unwrap();
    tx.repo_mut().rebase_descendants().await.unwrap();
    let description = format!("revert operation {}", snapshot.op_id().hex());
    fx.main.set_head(tx.commit(description).await.unwrap());
    let now = reef_lib::jj::view_wc_commit(fx.main.head(), &format!("reef-{Y}")).unwrap();
    assert_eq!(now, c, "the revert put y back on C");

    assert_keeps(&fx, &mut y, &y_root, &c2).await;
}

// #357 review, F8: Reef's own moves count though they rewrite nothing.
// Its abandon of `y`'s last snapshot puts `y` on a new empty commit.
#[tokio::test]
async fn a_clean_workspace_follows_reefs_abandon_of_its_snapshot() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
    let meta = OpMeta::new(OpKind::Abandon, format!("reef: abandon commit {c2}"));
    fx.main.abandon_commit(&c2, &meta).await.unwrap();

    assert_follows(&fx, &mut y, &c2).await;
    assert!(
        !y_root.join("two.txt").exists(),
        "the abandon took two.txt away"
    );
}

// #357 review, F8: `reef move` puts a new change on `main`'s commit in
// `y`'s place.
#[tokio::test]
async fn a_clean_workspace_follows_reefs_move_off_its_snapshot() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let meta = OpMeta::new(OpKind::MoveWorkspace, format!("reef: move workspace {Y}")).workspace(Y);
    fx.main
        .move_pointer(&format!("reef-{Y}"), StartPoint::NewOn(base), &meta)
        .await
        .unwrap();

    assert_follows(&fx, &mut y, &c2).await;
    assert!(
        !y_root.join("two.txt").exists(),
        "the move took two.txt away"
    );
}

// #357 review, F8: a landing of `y`'s last snapshot onto `main`, which
// another workspace moved on, gives `y` a new empty commit on the rebased
// snapshot.
#[tokio::test]
async fn a_clean_workspace_follows_a_landing_of_its_snapshot() {
    let mut fx = Fixture::new().await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let (mut x, x_root, _) = fx
        .create("reef-00000000000000aa", StartPoint::NewOn(base))
        .await;
    write(&x_root, "trunk.txt", "trunk\n");
    let trunk = x
        .snapshot("reef: snapshot workspace x")
        .await
        .unwrap()
        .wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let mut tx = fx.main.head().start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("main"),
        RefTarget::normal(CommitId::try_from_hex(trunk.as_str()).unwrap()),
    );
    fx.main.set_head(tx.commit("bookmark main").await.unwrap());
    let (mut y, y_root, _, c2, _) = snapshotted_twice(&mut fx).await;
    let target = LandTarget {
        change: y.wc_change().unwrap().change_id,
        commit: c2.clone(),
        bookmark: "main".to_owned(),
    };
    let meta = OpMeta::new(OpKind::Land, format!("reef: land change {}", target.change));
    let landed = fx.main.land(&target, &trunk, &meta).await.unwrap();
    assert!(landed.rebased);
    assert_eq!(landed.new_working_copies, [format!("reef-{Y}")]);

    assert_follows(&fx, &mut y, &c2).await;
    assert!(
        y_root.join("two.txt").is_file(),
        "the landed commit has two.txt"
    );
    assert!(y_root.join("trunk.txt").is_file(), "and main's file");
}

/// `settings()`, with every operation stamped `stamp`: a jj client whose
/// clock reads that, whatever the time.
fn settings_at(stamp: &str) -> UserSettings {
    use jj_lib::config::{ConfigLayer, ConfigSource};
    let mut config = settings().config().clone();
    let layer = format!("debug.operation-timestamp = {stamp:?}");
    config.add_layer(ConfigLayer::parse(ConfigSource::User, &layer).unwrap());
    UserSettings::from_config_and_home_dir(config, None).unwrap()
}

/// Another jj client with `settings`, loaded at the head of `main/`.
async fn other_client(fx: &Fixture, settings: &UserSettings) -> Arc<ReadonlyRepo> {
    let factories = (
        default_backend_factories(),
        default_working_copy_factories(),
    );
    let workspace = default_workspace_loader_factory()
        .create(fx.main.workspace_root())
        .unwrap()
        .load(settings, &factories.0, &factories.1)
        .unwrap();
    workspace.repo_loader().load_at_head(None).await.unwrap()
}

/// `count` operations of another jj client on `at`, one after the other,
/// that move no working copy; `main` is left at the last.
async fn operations_on(fx: &mut Fixture, at: &Arc<ReadonlyRepo>, count: usize) {
    let targets = [
        fx.main.wc_change().unwrap().commit_id,
        reef_lib::jj::view_wc_commit(at, &format!("reef-{Y}")).unwrap(),
    ];
    let mut at = Arc::clone(at);
    for nth in 0..count {
        at = concurrent_bookmark(&at, "elsewhere", &targets[nth % 2]).await;
    }
    fx.main.set_head(at);
}

/// The scene of a client that loaded the head too early: `stale` loaded it
/// before `y` snapshots C; Reef's write then moves `y` to C2; `stale`
/// commits a bookmark; the reload merges the two heads; `y`'s agent writes.
/// Returns C and C2.
async fn beside_a_stale_client(
    fx: &mut Fixture,
    (y, y_root): (&mut Repo, &Path),
    stale: &Arc<ReadonlyRepo>,
) -> (CommitIdHex, CommitIdHex) {
    let base = fx.main.wc_change().unwrap().commit_id;
    y.set_head(Arc::clone(fx.main.head()));
    write(y_root, "b.txt", "b\n");
    let snapshot = y.snapshot("reef: snapshot workspace y").await.unwrap();
    let c = snapshot.wc_commit;
    fx.main.reload_at_head().await.unwrap();
    let c2 = write_on(fx, &c, &[content("api.txt", "api\n")]).await;
    concurrent_bookmark(stale, "stale", &base).await;
    merge_heads(fx).await;
    write(y_root, "late.txt", "late\n");
    (c, c2)
}

/// `y` carries its edits from C onto C2, the write's commit.
async fn assert_carries(fx: &Fixture, y: &mut Repo, moved: (&CommitIdHex, &CommitIdHex)) {
    let (committed, files) = carry_snapshot(fx, y, moved, None).await;
    assert_eq!(files, 1, "the write's file");
    for path in ["a.txt", "b.txt", "api.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

/// More operations than the walk judges (`FOLLOW_LIMIT`, 1,024).
const PAST_THE_LIMIT: usize = 1100;

// Review 9, R9-1: another jj client loaded the head, and more operations
// than the walk judges were written before this workspace's last snapshot.
// The client then commits beside Reef's write. Its operation is the only
// one of those since the snapshot: the ones before count for nothing.
#[tokio::test]
async fn a_client_that_loaded_the_head_long_before_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let stale = Arc::clone(fx.main.head());
    operations_on(&mut fx, &stale, PAST_THE_LIMIT).await;

    let (c, c2) = beside_a_stale_client(&mut fx, (&mut y, &y_root), &stale).await;
    assert_carries(&fx, &mut y, (&c, &c2)).await;
}

// ADR 0084: a follow that comes more operations after Reef's move than the
// walk judges takes ADR 0018's fourth arm.
#[tokio::test]
async fn a_move_further_back_than_the_limit_diverges() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
    let written = Arc::clone(fx.main.head());
    operations_on(&mut fx, &written, PAST_THE_LIMIT).await;
    write(&y_root, "late.txt", "late\n");

    assert_diverges(&fx, &mut y, &c).await;
}

// #344 review, F2: another client rewrote a clean `y`'s commit, and more
// operations than a carry judges followed before `y` snapshots (no follow
// is queued for a move outside Reef). The rewrite came from `y`'s last
// snapshot all the same, so `y` checks it out.
#[tokio::test]
async fn a_clean_workspace_follows_a_rewrite_past_the_carry_limit() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("one.txt", "one\n")]).await;
    let emptied = concurrent_rewrite(fx.main.head(), &c).await;
    fx.main.set_head(emptied);
    operations_stamped_ahead(&mut fx, PAST_THE_LIMIT, 1).await;

    y.set_head(Arc::clone(fx.main.head()));
    let pending = y
        .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.adopted());
    pending.finish(&fx.main.op_id()).await.unwrap();
    assert!(!y_root.join("one.txt").exists(), "the rewrite emptied it");
}

// Review 9, R9-2: the operations since the last snapshot do not depend on
// what the clocks said. One client's clock is wrong, and it writes the
// operation that the stale client loads the head at, or the stale client's
// own clock is wrong.
#[tokio::test]
async fn a_wrong_clock_changes_no_carry() {
    let (ahead, behind) = ("2030-01-01T00:00:00+00:00", "2001-01-01T00:00:00+00:00");
    for (loaded_at, stale) in [
        (Some(ahead), None),
        (Some(behind), None),
        (None, Some(ahead)),
        (None, Some(behind)),
        (Some(ahead), Some(behind)),
    ] {
        let mut fx = Fixture::new().await;
        let (mut y, y_root, _) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
        let base = fx.main.wc_change().unwrap().commit_id;
        if let Some(stamp) = loaded_at {
            let client = other_client(&fx, &settings_at(stamp)).await;
            concurrent_bookmark(&client, "wrong-clock", &base).await;
            fx.main.reload_at_head().await.unwrap();
        }
        let stale = match stale {
            Some(stamp) => other_client(&fx, &settings_at(stamp)).await,
            None => Arc::clone(fx.main.head()),
        };

        let (c, c2) = beside_a_stale_client(&mut fx, (&mut y, &y_root), &stale).await;
        assert_carries(&fx, &mut y, (&c, &c2)).await;
    }
}

// Review 9, R9-2: an operation that is read before the one that shows it
// is before the last snapshot is before it all the same, and so is its
// history. Two clients loaded the head too early: one at an operation
// stamped ahead, which is read first, and one at the operation before it.
#[tokio::test]
async fn two_stale_clients_below_a_wrong_clock_carry() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let base = fx.main.wc_change().unwrap().commit_id;
    let below = Arc::clone(fx.main.head());
    let client = other_client(&fx, &settings_at("2030-01-01T00:00:00+00:00")).await;
    concurrent_bookmark(&client, "wrong-clock", &base).await;
    fx.main.reload_at_head().await.unwrap();
    let stale = Arc::clone(fx.main.head());

    let (c, c2) = beside_a_stale_client(&mut fx, (&mut y, &y_root), &stale).await;
    concurrent_bookmark(&below, "below", &base).await;
    merge_heads(&mut fx).await;
    assert_carries(&fx, &mut y, (&c, &c2)).await;
}

/// `count` operations of another client on the head, one after the other,
/// with the head's view, stamped `ahead_ms` after it and a millisecond
/// apart; `main` is left at the last. Written straight to the operation
/// store, to keep thousands cheap.
async fn operations_stamped_ahead(fx: &mut Fixture, count: usize, ahead_ms: i64) {
    let head = Arc::clone(fx.main.head());
    let mut data = head.operation().store_operation().clone();
    "another client's operation".clone_into(&mut data.metadata.description);
    data.metadata.attributes.clear();
    let stamp = data.metadata.time.end.timestamp.0 + ahead_ms;
    let mut last = head.op_id().clone();
    for at in (stamp..).take(count) {
        data.parents = vec![last];
        data.metadata.time.start.timestamp.0 = at;
        data.metadata.time.end.timestamp.0 = at;
        last = head.op_store().write_operation(&data).await.unwrap();
    }
    let heads = head.loader().op_heads_store();
    let replaced = std::slice::from_ref(head.op_id());
    heads.update_op_heads(replaced, &last).await.unwrap();
    fx.main.reload_at_head().await.unwrap();
}

// Tenth R3.11 review, R10-1: a clock that ran an hour fast stamped more
// operations than the walk read views of (4,096 at the time), and a client
// loaded the head at the last of them, before this workspace's last
// snapshot. Each of them is read with its view before the operation that
// shows it is before the snapshot: they cost reads, and the carry is the
// same as with a right clock.
#[tokio::test]
async fn a_stretch_stamped_ahead_below_a_stale_client_carries() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, _) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    operations_stamped_ahead(&mut fx, 4200, 3_600_000).await;
    let stale = Arc::clone(fx.main.head());
    operations_on(&mut fx, &stale, 3).await;

    let (c, c2) = beside_a_stale_client(&mut fx, (&mut y, &y_root), &stale).await;
    assert_carries(&fx, &mut y, (&c, &c2)).await;
}

// Review 9, R9-2: every operation is written in the same millisecond, so
// that jj orders them by id. Each round has other ids.
#[tokio::test]
async fn operations_in_one_millisecond_carry() {
    for _ in 0..20 {
        let mut fx = Fixture::with_settings(&settings_at("2026-01-01T00:00:00+00:00")).await;
        let (mut y, y_root, _) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
        let base = fx.main.wc_change().unwrap().commit_id;
        concurrent_bookmark(fx.main.head(), "first", &base).await;
        fx.main.reload_at_head().await.unwrap();
        let stale = Arc::clone(fx.main.head());

        let (c, c2) = beside_a_stale_client(&mut fx, (&mut y, &y_root), &stale).await;
        assert_carries(&fx, &mut y, (&c, &c2)).await;
    }
}

// Review 9, R9-2: the moves are followed in the order the log gives them,
// not the order of their times. Reef writes twice, and the clock was set
// back in between, so the first write is stamped after the second.
#[tokio::test]
async fn a_clock_set_back_between_two_writes_changes_no_carry() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let ahead = settings_at("2030-01-01T00:00:00+00:00");
    let mut early = Repo::open(fx.main.workspace_root(), &ahead).await.unwrap();
    let meta = OpMeta::new(OpKind::Edit, "reef: edit");
    let edit = [content("api.txt", "api\n")];
    let written = early.edit_commit(&c, &edit, None, &meta).await.unwrap();
    let c2 = written.expect("the edit changed the tree").commit_id;
    fx.main.reload_at_head().await.unwrap();
    let c3 = write_on(&mut fx, &c2, &[content("api2.txt", "api\n")]).await;
    write(&y_root, "late.txt", "late\n");

    let (committed, files) = carry_snapshot(&fx, &mut y, (&c, &c3), None).await;
    assert_eq!(files, 2, "both writes' files");
    for path in ["a.txt", "api.txt", "api2.txt", "late.txt"] {
        assert!(
            file_in(&committed.repo, &committed.commit_id, path)
                .await
                .is_some(),
            "{path} in the commit"
        );
    }
}

// Review 9, R9-2: a wrong clock launders no move either. Another client's
// rewrite of this workspace's commit beside Reef's write takes ADR 0018's
// fourth arm, whatever its clock said and with every operation in one
// millisecond.
#[tokio::test]
async fn a_wrong_clock_keeps_arm_four() {
    let now = settings();
    let stamps = [
        (&now, Some("2030-01-01T00:00:00+00:00")),
        (&now, Some("2001-01-01T00:00:00+00:00")),
    ];
    let one_millisecond = settings_at("2026-01-01T00:00:00+00:00");
    let ties = std::iter::repeat_n((&one_millisecond, None), 8);
    for (daemons, others) in stamps.into_iter().chain(ties) {
        let mut fx = Fixture::with_settings(daemons).await;
        let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
        let other = match others {
            Some(stamp) => other_client(&fx, &settings_at(stamp)).await,
            None => Arc::clone(fx.main.head()),
        };
        write_on(&mut fx, &c, &[content("api.txt", "api\n")]).await;
        concurrent_rewrite(&other, &c).await;
        merge_heads(&mut fx).await;
        write(&y_root, "late.txt", "late\n");

        assert_diverges(&fx, &mut y, &c).await;
    }
}

// Issue #80: a carried snapshot whose check-out failed is merged again over
// the disk tree it took, which gives its own commit back: it adopts with no
// operation, also when the first merge conflicted.
#[tokio::test]
async fn carry_is_idempotent() {
    for conflict in [false, true] {
        let mut fx = Fixture::new().await;
        let (mut y, y_root, c) =
            busy_workspace(&mut fx, &[("a.txt", "one\n"), ("b.txt", "b\n")]).await;
        let change = y.wc_change().unwrap().change_id;
        let c2 = write_on(
            &mut fx,
            &c,
            &[content("a.txt", "two\n"), content("api.txt", "api\n")],
        )
        .await;
        if conflict {
            write(&y_root, "a.txt", "three\n");
        }
        write(&y_root, "late.txt", "late\n");

        y.set_head(Arc::clone(fx.main.head()));
        let pending = y
            .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
            .await
            .unwrap();
        assert!(pending.carried());
        let plan = pending.plan().clone();
        assert_eq!(plan.rewrite_of, c2);
        let committed = commit_snapshot(fx.main.head(), &plan, &y_snapshot())
            .await
            .unwrap();
        // The check-out fails: nothing is checked out or saved.
        drop(pending);
        let conflicts = conflicted_paths(&committed.repo, &committed.commit_id, 10).unwrap();
        assert_eq!(conflicts.1, usize::from(conflict));

        y.set_head(Arc::clone(&committed.repo));
        // Its own commit, unexplained (a crash between commit and row): arm
        // four, as before ADR 0084.
        let pending = y
            .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
            .await
            .unwrap();
        assert!(!pending.carried() && pending.plan().divergent);
        drop(pending);
        let again = Unfinished {
            commit: committed.commit_id.clone(),
            disk: plan.carried.clone().unwrap(),
        };
        let pending = y
            .begin_snapshot(Some(&c), Some(&again), &IgnoreSources::default(), None)
            .await
            .unwrap();
        assert!(pending.adopted(), "conflict: {conflict}");
        assert!(!pending.changed());
        assert_eq!(pending.wc_commit(), Some(&committed.commit_id));
        assert_eq!(pending.updated_files(), 2, "the write's a.txt and api.txt");
        pending.finish(&committed.op_id).await.unwrap();
        assert_eq!(y.op_id(), committed.op_id, "no operation was written");
        assert_eq!(
            std::fs::read_to_string(y_root.join("api.txt")).unwrap(),
            "api\n"
        );
        fx.main.reload_at_head().await.unwrap();
        assert_eq!(
            visible_commits(&fx, &change).await,
            BTreeSet::from([committed.commit_id.clone()])
        );
    }
}

// Issue #80 (review): a file the carried check-out would overwrite that
// changed after the scan is not overwritten; merging again over the tree
// the scan took keeps the new edit, as a conflict with the write.
#[tokio::test]
async fn carry_does_not_overwrite_an_edit_after_the_scan() {
    let mut fx = Fixture::new().await;
    let (mut y, y_root, c) = busy_workspace(&mut fx, &[("a.txt", "a\n")]).await;
    let c2 = write_on(&mut fx, &c, &[content("a.txt", "api a\n")]).await;
    write(&y_root, "late.txt", "late\n");

    y.set_head(Arc::clone(fx.main.head()));
    let mut pending = y
        .begin_snapshot(Some(&c), None, &IgnoreSources::default(), None)
        .await
        .unwrap();
    assert!(pending.carried());
    let plan = pending.plan().clone();
    assert_eq!(plan.rewrite_of, c2);
    let committed = commit_snapshot(fx.main.head(), &plan, &y_snapshot())
        .await
        .unwrap();
    // The agent edits the write's path between the scan and the check-out.
    write(&y_root, "a.txt", "agent's a, written later\n");
    let error = pending
        .check_out_carried(&committed.repo, &committed.commit_id)
        .await
        .unwrap_err();
    assert!(
        matches!(error, reef_lib::jj::SnapshotError::ChangedAfterScan(ref path) if path.ends_with("a.txt")),
        "{error:?}"
    );
    drop(pending);
    assert_eq!(
        std::fs::read_to_string(y_root.join("a.txt")).unwrap(),
        "agent's a, written later\n"
    );

    // The daemon's re-merge: the new edit meets the write's.
    fx.main.set_head(Arc::clone(&committed.repo));
    let again = Unfinished {
        commit: committed.commit_id.clone(),
        disk: plan.carried.clone().unwrap(),
    };
    let (merged, _) = carry_snapshot(&fx, &mut y, (&c, &committed.commit_id), Some(&again)).await;
    assert_eq!(
        conflicted_paths(&merged.repo, &merged.commit_id, 10).unwrap(),
        (vec!["a.txt".to_owned()], 1)
    );
    let on_disk = std::fs::read_to_string(y_root.join("a.txt")).unwrap();
    assert!(
        on_disk.contains("agent's a") && on_disk.contains("api a"),
        "{on_disk}"
    );
    assert_eq!(
        file_in(&merged.repo, &merged.commit_id, "late.txt")
            .await
            .as_deref(),
        Some("late\n")
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
        .begin_snapshot(
            Some(&before.commit_id),
            None,
            &IgnoreSources::default(),
            None,
        )
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

fn git_at(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn import_keeps_the_commits_reef_workspaces_use() {
    if !git_available() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    let meta = OpMeta::new(OpKind::ImportRefs, "reef: import git refs");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("colocated");
    std::fs::create_dir(&root).unwrap();
    write(&root, "a.txt", "a\n");
    let mut repo = Repo::init_colocated_git(&root, &settings()).await.unwrap();
    let base = repo.snapshot("snapshot").await.unwrap().wc_commit;
    let tree = git_at(&root, &["rev-parse", &format!("{base}^{{tree}}")]);
    let commit_on_base = |message: &str| {
        git_at(
            &root,
            &["commit-tree", &tree, "-p", base.as_str(), "-m", message],
        )
    };
    let below = commit_on_base("below");
    let unused = commit_on_base("unused");
    git_at(&root, &["branch", "below", &below]);
    git_at(&root, &["branch", "unused", &unused]);
    repo.import_git_refs(&meta).await.unwrap();
    assert!(
        heads_of(&repo).contains(&unused),
        "the import brings in the branch"
    );
    let below: CommitIdHex = below.parse().unwrap();
    let ws_root = dir.path().join("ws");
    std::fs::create_dir(&ws_root).unwrap();
    let (ws, _) = repo
        .create_workspace(
            "reef-0123456789abcdef",
            &ws_root,
            &dir.path().join("wc"),
            StartPoint::NewOn(below.clone()),
            &OpMeta::new(OpKind::CreateWorkspace, "reef: create workspace"),
        )
        .await
        .unwrap();
    let top = wc_commit_id(&ws);

    git_at(&root, &["branch", "-D", "below", "unused"]);
    let imported = repo.import_git_refs(&meta).await.unwrap();
    assert_eq!(imported.abandoned_commits, 1, "only the unused commit");
    assert_eq!(
        heads_of(&repo),
        BTreeSet::from([top.clone()]),
        "the unused commit is abandoned, the Reef workspace's stays"
    );
    assert_eq!(
        repo.resolve_revision(&format!("{top}-"))
            .await
            .unwrap()
            .commit_id,
        below,
        "the commit the workspace builds on stays"
    );
    assert_eq!(
        repo.head()
            .view()
            .get_wc_commit_id(WorkspaceName::new("reef-0123456789abcdef"))
            .map(jj_lib::object_id::ObjectId::hex),
        Some(top),
        "the Reef workspace stays on its commit"
    );
    assert_eq!(
        repo.head().view().local_tags().count(),
        0,
        "the import leaves no tag behind"
    );
}

/// How the user drops a commit with git after `reef init`.
#[derive(Clone, Copy, Debug)]
enum GitDrop {
    /// `git reset --hard HEAD~1` on `main`.
    Reset,
    /// `git checkout main` and `git branch -D feat`.
    DeleteBranch,
}

/// The user's root checkout is not a Reef workspace: Reef never imports
/// git's HEAD after `reef init`, so its commit is often one the user has
/// since dropped with git, and Reef's import abandons it as jj's does.
#[tokio::test]
async fn import_abandons_what_only_the_root_checkout_uses() {
    if !git_available() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    for drop in [GitDrop::Reset, GitDrop::DeleteBranch] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("colocated");
        std::fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            let mut all = vec![
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ];
            all.extend_from_slice(args);
            git_at(&root, &all)
        };
        git(&["init", "-q", "-b", "main"]);
        write(&root, "base.txt", "base\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        if let GitDrop::DeleteBranch = drop {
            git(&["checkout", "-qb", "feat"]);
        }
        write(&root, "x.txt", "x\n");
        git(&["add", "."]);
        git(&["commit", "-qm", "X"]);
        let x = git(&["rev-parse", "HEAD"]);
        let mut repo = Repo::init_in_git_repo(&root, &settings(), None)
            .await
            .unwrap();
        let meta = OpMeta::new(OpKind::ImportRefs, "reef: import git refs");
        repo.import_git_refs(&meta).await.unwrap();
        assert_eq!(
            repo.resolve_revision("@-")
                .await
                .unwrap()
                .commit_id
                .as_str(),
            x,
            "{drop:?}: the root checkout builds on X"
        );

        match drop {
            GitDrop::Reset => git(&["reset", "-q", "--hard", "HEAD~1"]),
            GitDrop::DeleteBranch => {
                git(&["checkout", "-q", "main"]);
                git(&["branch", "-qD", "feat"])
            }
        };
        let imported = repo.import_git_refs(&meta).await.unwrap();
        assert_eq!(imported.abandoned_commits, 1, "{drop:?}: X is abandoned");
        assert!(
            !repo
                .heads()
                .unwrap()
                .iter()
                .any(|head| head.commit_id.as_str() == x),
            "{drop:?}: X is not a head"
        );
        assert_ne!(
            repo.resolve_revision("@-")
                .await
                .unwrap()
                .commit_id
                .as_str(),
            x,
            "{drop:?}: the root checkout no longer builds on X"
        );
    }
}

fn heads_of(repo: &Repo) -> BTreeSet<String> {
    repo.heads()
        .unwrap()
        .into_iter()
        .map(|head| head.commit_id.to_string())
        .collect()
}

/// Whether `version`, `jj --version`'s output, names a `jj` CLI at jj-lib's
/// minor ([`JJ_LIB_VERSION`](reef_lib::jj::JJ_LIB_VERSION)) or the one
/// before it.
fn jj_cli_is_supported(version: &str) -> bool {
    fn minor(version: &str) -> Option<(u64, u64)> {
        let mut parts = version.split('.');
        Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
    }
    let (Some((lib_major, lib_minor)), Some((major, minor))) = (
        minor(reef_lib::jj::JJ_LIB_VERSION),
        version.trim().strip_prefix("jj ").and_then(minor),
    ) else {
        return false;
    };
    major == lib_major && (minor == lib_minor || minor + 1 == lib_minor)
}
