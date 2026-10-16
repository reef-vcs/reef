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
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::workspace::CommitLookupError;
use reef_lib::jj::{
    ChangeIdHex, CommitIdHex, CreateWorkspaceError, ForgetWorkspaceError, OpenError, Repo,
    ResolveError, ResolvedVia, StartPoint,
};
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
                &format!("reef: create workspace {name}"),
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
            "",
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
            "",
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
            "",
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
            "",
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
        .forget_workspace("reef-x", "reef: release workspace x")
        .await
        .unwrap();
    assert!(!fx.wc_names().await.contains("reef-x"));
    assert_eq!(fx.store_path("reef-x").await, None);
    assert!(
        !fx.head_ids().contains(&empty_commit),
        "empty commit abandoned"
    );

    fx.main
        .forget_workspace("reef-y", "reef: release workspace y")
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

    let again = fx.main.forget_workspace("reef-y", "").await.unwrap_err();
    assert!(
        matches!(again, ForgetWorkspaceError::NoSuchWorkspace(_)),
        "{again:?}"
    );
    assert_eq!(again.kind(), ErrorKind::NotFound);
    let own = fx.main.forget_workspace("default", "").await.unwrap_err();
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

    let error = ws.forget_workspace("default", "").await.unwrap_err();
    assert!(
        matches!(error, ForgetWorkspaceError::NotReefWorkspace(_)),
        "{error:?}"
    );
    let error = ws.forget_workspace("reef-x", "").await.unwrap_err();
    assert!(
        matches!(error, ForgetWorkspaceError::CurrentWorkspace(_)),
        "{error:?}"
    );
    assert!(fx.wc_names().await.contains("default"));
}

// 7
#[tokio::test]
async fn resolve_revision_orders_change_commit_bookmark() {
    let (mut fx, commit) = Fixture::with_files().await;
    let change = fx.change_of(&commit).await;

    let by_change = fx
        .main
        .resolve_revision(&change.as_str()[..8])
        .await
        .unwrap();
    assert_eq!(
        (by_change.via, &by_change.commit_id),
        (ResolvedVia::ChangeId, &commit)
    );
    let by_reverse = fx
        .main
        .resolve_revision(&change.reverse_hex()[..8])
        .await
        .unwrap();
    assert_eq!(
        (by_reverse.via, &by_reverse.commit_id),
        (ResolvedVia::ChangeId, &commit)
    );
    let by_commit = fx
        .main
        .resolve_revision(&commit.as_str()[..12])
        .await
        .unwrap();
    assert_eq!(
        (by_commit.via, &by_commit.change_id),
        (ResolvedVia::CommitId, &change)
    );

    // A bookmark named like neither id alphabet, set the way `jj bookmark set` does.
    let head = fx.jj_head().await;
    let mut tx = head.start_transaction();
    let target = CommitId::try_from_hex(commit.as_str()).unwrap();
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("feature"), RefTarget::normal(target));
    tx.commit("bookmark set feature").await.unwrap();
    fx.main.reload_at_head().await.unwrap();
    let by_bookmark = fx.main.resolve_revision("feature").await.unwrap();
    assert_eq!(
        (by_bookmark.via, &by_bookmark.commit_id),
        (ResolvedVia::Bookmark, &commit)
    );

    let none = fx.main.resolve_revision("nope").await.unwrap_err();
    assert!(matches!(none, ResolveError::NoMatch(_)), "{none:?}");
    assert_eq!(none.kind(), ErrorKind::NotFound);

    // Seventeen more changes guarantee two share a first hex digit.
    let head = fx.jj_head().await;
    let mut tx = head.start_transaction();
    let root_commit = head.store().root_commit();
    for _ in 0..17 {
        tx.repo_mut()
            .new_commit(vec![root_commit.id().clone()], root_commit.tree())
            .write()
            .await
            .unwrap();
    }
    tx.commit("new commits").await.unwrap();
    fx.main.reload_at_head().await.unwrap();
    let mut seen = BTreeSet::new();
    let shared = fx
        .main
        .heads()
        .unwrap()
        .into_iter()
        .map(|head| head.change_id.as_str()[..1].to_owned())
        .find(|digit| !seen.insert(digit.clone()))
        .expect("pigeonhole");
    let ambiguous = fx.main.resolve_revision(&shared).await.unwrap_err();
    assert!(
        matches!(ambiguous, ResolveError::AmbiguousPrefix(_)),
        "{ambiguous:?}"
    );
    assert_eq!(ambiguous.kind(), ErrorKind::InvalidArgument);

    let [a, b] = fx.diverge(&commit).await;
    let divergent = fx.main.resolve_revision(change.as_str()).await.unwrap_err();
    let ResolveError::DivergentChange { change_id, commits } = &divergent else {
        panic!("expected a divergent change, got {divergent:?}");
    };
    assert_eq!(change_id, &change);
    assert_eq!(
        commits.iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([&a, &b])
    );
    assert_eq!(divergent.kind(), ErrorKind::Conflict);
}

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
