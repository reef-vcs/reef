//! The workspace service end to end (R1.05): `Repositories` and
//! `Workspaces` over the socket, against a registered synthetic repository.
//! Numbered as the brief numbers them; the last two are the acceptance
//! criterion's failed creates.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; timings are reported on stderr"
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use reef_api::client::ClientError;
use reef_api::error::detail_of;
use reef_api::proto::v1::register_request::IfNotJj;
use reef_api::proto::v1::{
    CreateRequest, ListWorkspacesRequest, MoveRequest, Workspace, create_request, move_request,
};
use reef_daemon::model::{ReclaimReason, WorkspaceState};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::{ChangeSummary, CommitIdHex, Repo, RepoPath};
use reef_testing::synth::{self, Preset};

const OWNER: &str = "tester@host";

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo(Preset::Small).await
}

fn request(repo: &SyntheticRepo, start: Option<create_request::Start>) -> CreateRequest {
    CreateRequest {
        repo: repo.id.clone(),
        start,
        owner: OWNER.to_owned(),
        ..CreateRequest::default()
    }
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(request(repo, None))
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Opens the registered root's own jj workspace, at head.
async fn open(root: &Path) -> Repo {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(Repo::open(&root, &test_settings())).unwrap()
    })
    .await
    .unwrap()
}

/// The `default` workspace's working-copy commit.
async fn default_wc(root: &Path) -> ChangeSummary {
    open(root).await.wc_change().unwrap()
}

/// The kind an RPC failed with, checking the status carries an
/// `ErrorDetail` that agrees.
fn kind_of(error: &ClientError) -> ErrorKind {
    let ClientError::Rpc { kind, status, .. } = error else {
        panic!("not an RPC error: {error:?}");
    };
    let detail = detail_of(status).expect("an ErrorDetail in the status");
    assert_eq!(detail.kind, reef_api::error::kind_name(*kind));
    assert_eq!(error.kind(), *kind);
    *kind
}

fn millis(time: &prost_types::Timestamp) -> i64 {
    time.seconds * 1000 + i64::from(time.nanos) / 1_000_000
}

fn state_dir_of(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> PathBuf {
    daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("wc")
        .join(&ws.id)
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn register_then_create_default() {
    let (daemon, repo) = fixture().await;
    let again = daemon
        .client
        .register_repository(&repo.root.join("src"), IfNotJj::Fail)
        .await
        .unwrap();
    assert!(
        again.already_registered,
        "a path inside the root registers the root"
    );
    assert_eq!(again.repository.unwrap().id, repo.id);
    // A jj repository is never initialized again (R1.25).
    let again = daemon
        .client
        .register_repository(&repo.root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    assert!(again.already_registered);
    assert!(!again.initialized_jj);
    let listed = daemon.client.list_repositories().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].root, repo.root.display().to_string());

    let begun = Instant::now();
    let cold = daemon
        .client
        .create_workspace(request(&repo, None))
        .await
        .unwrap();
    let cold_wall = begun.elapsed();
    assert!(!cold.base_was_cached);
    assert_ne!(cold.op_id, "");
    let ws = cold.workspace.unwrap();
    let path = PathBuf::from(&ws.path);
    assert!(path.is_dir());
    assert!(
        !path.join(".jj").exists(),
        "the workspace holds only the tree"
    );
    let manifest = synth::load_manifest(&repo.root).unwrap().unwrap();
    for file in &manifest.text_files {
        assert!(path.join(file).is_file(), "{}", file.display());
    }
    assert!(path.join(".gitignore").is_file());
    assert_ne!(
        ws.change_id,
        default_wc(&repo.root).await.change_id.as_str()
    );
    assert_eq!(ws.state, "active");
    // The first available in auto's order: apfs on macOS; on Linux an
    // overlay as root or with `fuse-overlayfs` installed, else the copy.
    let status = daemon.client.status().await.unwrap();
    let expected = status
        .backends
        .iter()
        .find(|backend| backend.available)
        .unwrap();
    if cfg!(target_os = "macos") {
        assert_eq!(expected.kind, "apfs");
    }
    assert_eq!(ws.backend, expected.kind, "auto picks the platform's clone");
    let lease = ws.lease.unwrap();
    assert_eq!(
        millis(lease.expires_at.as_ref().unwrap()) - millis(lease.issued_at.as_ref().unwrap()),
        i64::try_from(daemon.config.file.lease.default.as_millis()).unwrap()
    );
    assert_eq!(lease.owner, OWNER);

    // The same tree again: the base is cached, so this is the creation
    // time R1.12 gates (printed here, not asserted).
    let begun = Instant::now();
    let warm = daemon
        .client
        .create_workspace(request(&repo, None))
        .await
        .unwrap();
    let warm_wall = begun.elapsed();
    assert!(warm.base_was_cached);
    eprintln!(
        "create on the small preset: cold {cold_wall:.2?} (materialize {} ms), \
         base cached {warm_wall:.2?} (materialize {} ms)",
        cold.materialize_ms, warm.materialize_ms
    );
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn create_on_change_edits_it() {
    let (daemon, repo) = fixture().await;
    let wc = default_wc(&repo.root).await;
    let prefix = create_request::Start::Change(wc.change_id.as_str()[..12].to_owned());
    let first = daemon
        .client
        .create_workspace(request(&repo, Some(prefix.clone())))
        .await
        .unwrap();
    let one = first.workspace.unwrap();
    assert_eq!(one.commit_id, wc.commit_id.as_str());
    assert_eq!(one.change_id, wc.change_id.as_str());
    assert!(!first.shared_change, "only the user's default is on it");

    let second = daemon
        .client
        .create_workspace(request(&repo, Some(prefix)))
        .await
        .unwrap();
    assert!(second.shared_change);
    let two = second.workspace.unwrap();
    let inspect_one = daemon
        .client
        .inspect_workspace(&repo.id, &one.id, false)
        .await
        .unwrap();
    assert_eq!(
        inspect_one.other_workspaces_on_change,
        std::slice::from_ref(&two.id)
    );
    let inspect_two = daemon
        .client
        .inspect_workspace("", &two.id[..6], false)
        .await
        .unwrap();
    assert_eq!(
        inspect_two.other_workspaces_on_change,
        std::slice::from_ref(&one.id)
    );
    assert!(inspect_two.op_id.is_empty(), "no snapshot was asked for");
    let usage = inspect_two.disk_usage.unwrap();
    assert_eq!(usage.files_changed, 0, "{usage:?}");
}

/// Sets a local bookmark the way `jj bookmark set` does, from outside the
/// daemon, as a user would in their checkout.
fn set_bookmark(root: &Path, name: &str, commit: &CommitIdHex) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    use jj_lib::workspace::Workspace;

    let settings = test_settings();
    let workspace = Workspace::load(
        &settings,
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit.as_str()).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn create_new_on_bookmark() {
    let (daemon, repo) = fixture().await;
    let wc = default_wc(&repo.root).await;
    // Set after registration: the daemon reloads at head before resolving.
    let root = repo.root.clone();
    let target = wc.commit_id.clone();
    tokio::task::spawn_blocking(move || set_bookmark(&root, "main", &target))
        .await
        .unwrap();

    for start in [
        Some(create_request::Start::NewOn("main".into())),
        None, // the default is the `main` bookmark when it exists (R1.31)
    ] {
        let ws = daemon
            .client
            .create_workspace(request(&repo, start))
            .await
            .unwrap()
            .workspace
            .unwrap();
        let commit: CommitIdHex = ws.commit_id.parse().unwrap();
        let parents = open(&repo.root)
            .await
            .commit_parents(&commit)
            .await
            .unwrap();
        assert_eq!(parents, std::slice::from_ref(&wc.commit_id));
    }

    let error = daemon
        .client
        .create_workspace(request(
            &repo,
            Some(create_request::Start::NewOn("no-such-bookmark".into())),
        ))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::NotFound);
    let ClientError::Rpc { status, chain, .. } = &error else {
        unreachable!()
    };
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert!(
        chain.iter().any(|text| text.contains("no-such-bookmark")),
        "{chain:?}"
    );
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn first_snapshot_is_noop() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let begun = Instant::now();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let elapsed = begun.elapsed();
    assert!(!snapshot.changed);
    assert_eq!(snapshot.op_id, "");
    assert_eq!(snapshot.commit_id, ws.commit_id);

    // For comparison, a cold scan: the same tree with every file's state
    // unknown, as an unseeded working copy has it.
    let manifest = synth::load_manifest(&repo.root).unwrap().unwrap();
    let cold = cold_scan(&repo.root, daemon.tmp.path()).await;
    eprintln!(
        "first snapshot of a seeded workspace ({} files): {elapsed:.2?}; cold scan of the same tree: {cold:.2?}",
        manifest.files
    );
}

/// The time to snapshot the registered tree from a fresh jj repository
/// created over a copy of it: every file is hashed.
async fn cold_scan(root: &Path, tmp: &Path) -> Duration {
    let copy = tmp.join("cold");
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        copy_tree_without_jj(&root, &copy);
        let mut repo =
            pollster::block_on(Repo::init_internal_git(&copy, &test_settings())).unwrap();
        let begun = Instant::now();
        pollster::block_on(repo.snapshot("cold")).unwrap();
        begun.elapsed()
    })
    .await
    .unwrap()
}

fn copy_tree_without_jj(from: &Path, to: &Path) {
    // The walk visits entries in directory order, which may put a file first.
    std::fs::create_dir_all(to).unwrap();
    for entry in walkdir::WalkDir::new(from).min_depth(1) {
        let entry = entry.unwrap();
        let relative = entry.path().strip_prefix(from).unwrap();
        if relative.starts_with(".jj") {
            continue;
        }
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else if entry.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A workspace on a tree the base cache lacks while it holds another: on
/// macOS the new base is cloned from that one and only the diff is checked
/// out (R1.08); elsewhere it is a full check-out. Either way the workspace
/// holds the tree and its first snapshot finds nothing to record.
#[tokio::test(flavor = "multi_thread")]
async fn second_tree_gets_its_own_base() {
    second_tree(false).await;
}

/// As [`second_tree_gets_its_own_base`], with the first base's
/// `tree_state` damaged, so building from it fails and the daemon falls
/// back to the full check-out.
#[tokio::test(flavor = "multi_thread")]
async fn second_tree_falls_back_to_a_full_check_out() {
    second_tree(true).await;
}

async fn second_tree(damage_first_base: bool) {
    let (daemon, repo) = fixture().await;
    let first = create(&daemon, &repo).await;
    let bases = daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("bases");
    let manifest = synth::load_manifest(&repo.root).unwrap().unwrap();
    let edited = &manifest.text_files[0];
    std::fs::write(Path::new(&first.path).join(edited), "edited in the first\n").unwrap();
    std::fs::write(Path::new(&first.path).join("added-in-first.txt"), "added\n").unwrap();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &first.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    if damage_first_base {
        // The cache holds the first workspace's base only.
        let states: Vec<_> = std::fs::read_dir(&bases)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "state"))
            .collect();
        assert_eq!(states.len(), 1, "{states:?}");
        std::fs::write(states[0].join("tree_state"), b"\xff\xff\xff").unwrap();
    }

    let created = daemon
        .client
        .create_workspace(request(
            &repo,
            Some(create_request::Start::Change(first.change_id.clone())),
        ))
        .await
        .unwrap();
    assert!(!created.base_was_cached);
    let second = created.workspace.unwrap();
    assert_eq!(second.commit_id, snapshot.commit_id);
    let root = Path::new(&second.path);
    assert_eq!(
        std::fs::read_to_string(root.join(edited)).unwrap(),
        "edited in the first\n"
    );
    assert!(root.join("added-in-first.txt").is_file());
    let again = daemon
        .client
        .snapshot_workspace(&repo.id, &second.id, "")
        .await
        .unwrap();
    assert!(!again.changed, "the new base's tree_state describes it");

    let mut trees: Vec<_> = std::fs::read_dir(&bases)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.') && !name.contains('.'))
        .collect();
    trees.sort();
    assert_eq!(trees.len(), 2, "{trees:?}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn edit_then_snapshot_records_op() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    std::fs::write(Path::new(&ws.path).join("agent-wrote-this.txt"), "hello\n").unwrap();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    assert_ne!(snapshot.op_id, "");
    assert_ne!(snapshot.commit_id, ws.commit_id);

    let log = open(&repo.root).await.op_log(20).await.unwrap();
    let op = log
        .iter()
        .find(|op| op.id.as_str() == snapshot.op_id)
        .expect("the snapshot's operation is in the log");
    assert_eq!(
        op.workspace.as_deref(),
        Some(format!("reef-{}", ws.id).as_str())
    );
    assert!(op.is_snapshot);
    assert_eq!(
        op.description,
        format!("reef: snapshot workspace {}", ws.id)
    );

    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.root.display().to_string(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(listed[0].commit_id, snapshot.commit_id);
    assert_eq!(listed[0].last_snapshot_op, snapshot.op_id);
    assert!(listed[0].last_snapshot_at.is_some());
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn release_tears_down_and_keeps_row() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let state_dir = state_dir_of(&daemon, &repo, &ws);
    assert!(state_dir.join("tree_state").is_file());

    let released = daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(released.snapshot_taken);
    assert!(released.final_op_id.is_empty(), "nothing had changed");
    assert_eq!(released.workspace_id, ws.id);
    assert!(!Path::new(&ws.path).exists());
    assert!(!state_dir.exists());

    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            states: vec!["released".into()],
            owner: OWNER.into(),
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ws.id);
    assert_eq!(listed[0].state, "released");
    let lease = listed[0].lease.as_ref().expect("the lease it ended with");
    assert_eq!(lease.state(), reef_api::proto::v1::lease::State::Ended);
    assert_eq!(lease.end_reason, "released");
    let active = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            states: vec!["active".into()],
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(active.len(), 0);

    let db = &daemon.handle.state().db;
    let reclaims = db.reclaims(None, 10).unwrap();
    assert_eq!(reclaims.len(), 1);
    assert_eq!(reclaims[0].reason, ReclaimReason::Released);
    assert!(reclaims[0].snapshot_taken);
    let record = db.workspace(ws.id.parse().unwrap()).unwrap();
    assert_eq!(record.state, WorkspaceState::Released);

    let commit: CommitIdHex = ws.commit_id.parse().unwrap();
    assert!(
        open(&repo.root)
            .await
            .workspaces_on_commit(&commit)
            .is_empty(),
        "no reef-<id> pointer is left in the view"
    );

    let error = daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);

    // Inspecting a released workspace reads its row; there is nothing to
    // snapshot, even when asked to.
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, true)
        .await
        .unwrap();
    assert_eq!(inspected.workspace.unwrap().state, "released");
    assert_eq!(inspected.op_id, "");
    assert!(inspected.disk_usage.is_none());
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn release_with_unsnapshotted_edit_snapshots_first() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    std::fs::write(Path::new(&ws.path).join("last-words.txt"), "unsaved\n").unwrap();
    let released = daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(released.snapshot_taken);
    assert_ne!(released.final_op_id, "");

    let record = daemon
        .handle
        .state()
        .db
        .workspace(ws.id.parse().unwrap())
        .unwrap();
    let commit: CommitIdHex = record.commit_id.parse().unwrap();
    let out = daemon.tmp.path().join("final");
    let state = daemon.tmp.path().join("final.state");
    let root = repo.root.clone();
    let (dir, state_dir) = (out.clone(), state.clone());
    tokio::task::spawn_blocking(move || {
        let repo = pollster::block_on(Repo::open(&root, &test_settings())).unwrap();
        pollster::block_on(repo.materialize_tree(&commit, &dir, &state_dir)).unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(out.join("last-words.txt")).unwrap(),
        "unsaved\n"
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn path_collision_is_already_exists() {
    let (daemon, repo) = fixture().await;
    let taken = daemon.tmp.path().join("taken");
    std::fs::create_dir(&taken).unwrap();
    let error = daemon
        .client
        .create_workspace(CreateRequest {
            path: taken.display().to_string(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::AlreadyExists);

    let relative = daemon
        .client
        .create_workspace(CreateRequest {
            path: "relative/ws".into(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&relative), ErrorKind::InvalidArgument);
    let no_owner = daemon
        .client
        .create_workspace(CreateRequest {
            owner: String::new(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&no_owner), ErrorKind::InvalidArgument);
    let too_long = daemon
        .client
        .create_workspace(CreateRequest {
            lease_ms: 25 * 60 * 60 * 1000,
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&too_long), ErrorKind::InvalidArgument);

    // A path of our choosing works, and is recorded canonically.
    let chosen = daemon.tmp.path().join("chosen/ws");
    let ws = daemon
        .client
        .create_workspace(CreateRequest {
            path: chosen.display().to_string(),
            ..request(&repo, None)
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(
        PathBuf::from(&ws.path),
        std::fs::canonicalize(&chosen).unwrap()
    );

    // A released workspace's row keeps its path until GC (R1.17), so the
    // same path is refused before anything is materialized.
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(!chosen.exists());
    let reused = daemon
        .client
        .create_workspace(CreateRequest {
            path: chosen.display().to_string(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&reused), ErrorKind::AlreadyExists);
    assert!(reused.to_string().contains("released"), "{reused}");
    assert!(!chosen.exists(), "nothing was materialized");
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn forget_repository_refuses_with_active_workspace() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let error = daemon.client.forget_repository(&repo.id).await.unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);
    let ClientError::Rpc { status, .. } = &error else {
        unreachable!()
    };
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    let forgotten = daemon
        .client
        .forget_repository(&repo.root.display().to_string())
        .await
        .unwrap();
    assert_eq!(forgotten.repository.unwrap().id, repo.id);
    assert_eq!(daemon.client.list_repositories().await.unwrap().len(), 0);
    let gone = daemon.client.forget_repository(&repo.id).await.unwrap_err();
    assert_eq!(kind_of(&gone), ErrorKind::NotFound);
    // Registering again works and gets a new id.
    let again = daemon
        .client
        .register_repository(&repo.root, IfNotJj::Fail)
        .await
        .unwrap();
    assert!(!again.already_registered);
    assert_ne!(again.repository.unwrap().id, repo.id);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn restart_reloads_active_workspaces() {
    let (daemon, repo) = fixture().await;
    // Two workspaces, so the second's operation is not the first's: the
    // first must still snapshot after a restart.
    let first = create(&daemon, &repo).await;
    let second = create(&daemon, &repo).await;
    std::fs::write(Path::new(&second.path).join("before.txt"), "x\n").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &second.id, "")
        .await
        .unwrap();

    let daemon = daemon.restart().await;
    assert_eq!(daemon.handle.state().manager.loaded(), 2);
    for ws in [&first, &second] {
        let inspected = daemon
            .client
            .inspect_workspace(&repo.id, &ws.id, false)
            .await
            .unwrap();
        assert_eq!(inspected.workspace.unwrap().state, "active");
        std::fs::write(Path::new(&ws.path).join("after-restart.txt"), "y\n").unwrap();
        let snapshot = daemon
            .client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
        assert!(snapshot.changed, "{}", ws.id);
    }
    let released = daemon
        .client
        .release_workspace(&repo.id, &first.id, true, false)
        .await
        .unwrap();
    assert!(released.snapshot_taken);
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_backend_is_named() {
    let (daemon, repo) = fixture().await;
    let status = daemon.client.status().await.unwrap();
    let overlay = status
        .backends
        .iter()
        .find(|probe| probe.kind == "overlayfs")
        .unwrap();
    if overlay.available {
        eprintln!("overlayfs is available here; nothing to check");
        return;
    }
    assert!(cfg!(target_os = "linux") || !overlay.reason.is_empty());
    let error = daemon
        .client
        .create_workspace(CreateRequest {
            backend: "overlayfs".into(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Unavailable);
    let message = error.to_string();
    assert!(message.contains(&overlay.reason), "{message}");
    assert!(message.contains("overlayfs"), "{message}");

    let unknown = daemon
        .client
        .create_workspace(CreateRequest {
            backend: "zfs".into(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&unknown), ErrorKind::InvalidArgument);
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn colocated_repo_imports_refs() {
    if !git_available() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    let daemon = TestDaemon::start().await;
    let root = daemon.tmp.path().join("colocated");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(root.join("README"), "colocated\n").unwrap();
    let at = root.clone();
    let commit = tokio::task::spawn_blocking(move || {
        let mut repo = pollster::block_on(Repo::init_colocated_git(&at, &test_settings())).unwrap();
        assert!(repo.is_colocated());
        pollster::block_on(repo.snapshot("snapshot"))
            .unwrap()
            .wc_commit
    })
    .await
    .unwrap();
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::Fail)
        .await
        .unwrap();
    let repository = registered.repository.unwrap();
    assert!(repository.colocated);

    // A branch made with git, never seen by jj.
    let output = Command::new("git")
        .args(["branch", "feat", commit.as_str()])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let begun = Instant::now();
    let ws = daemon
        .client
        .create_workspace(CreateRequest {
            repo: repository.id.clone(),
            start: Some(create_request::Start::NewOn("feat".into())),
            owner: OWNER.into(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    eprintln!(
        "create in a colocated repository, importing refs first: {:.2?}",
        begun.elapsed()
    );
    let new_commit: CommitIdHex = ws.commit_id.parse().unwrap();
    let parents = open(&root).await.commit_parents(&new_commit).await.unwrap();
    assert_eq!(parents, [commit]);
    assert!(Path::new(&ws.path).join("src/lib.rs").is_file());
}

/// Every workspace row in any state.
async fn all_workspaces(daemon: &TestDaemon) -> Vec<Workspace> {
    daemon
        .client
        .list_workspaces(ListWorkspacesRequest::default())
        .await
        .unwrap()
}

/// Whether any operation in the fixture's log created a workspace.
async fn any_create_op(root: &Path) -> bool {
    open(root)
        .await
        .op_log(100)
        .await
        .unwrap()
        .iter()
        .any(|op| op.description.starts_with("reef: create workspace"))
}

/// Entries of `dir` that are not trash still being deleted.
fn live_entries(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| !name.contains(".trash-"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Acceptance: a backend failure (a read-only parent) leaves no row, no
/// directory, and no jj workspace.
#[tokio::test(flavor = "multi_thread")]
async fn failed_create_leaves_nothing() {
    let (daemon, repo) = fixture().await;
    let parent = daemon.tmp.path().join("read-only");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::create_dir(parent.join("probe")).is_ok() {
        eprintln!("skipped: permissions are not enforced here (running as root?)");
        return;
    }
    let target = parent.join("ws");
    let error = daemon
        .client
        .create_workspace(CreateRequest {
            backend: "copy".into(),
            path: target.display().to_string(),
            ..request(&repo, None)
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::PermissionDenied);
    assert_eq!(all_workspaces(&daemon).await.len(), 0);
    assert!(!target.exists());
    assert!(!any_create_op(&repo.root).await);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A failure after the directory is materialized (the working-copy state
/// directory cannot be created) tears the directory down again.
#[tokio::test(flavor = "multi_thread")]
async fn failed_create_after_materialize_tears_down() {
    let (daemon, repo) = fixture().await;
    let repo_dir = daemon.state_dir().join("repos").join(&repo.id);
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("wc"), "not a directory").unwrap();
    let error = daemon
        .client
        .create_workspace(request(&repo, None))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Io);
    assert_eq!(all_workspaces(&daemon).await.len(), 0);
    assert_eq!(live_entries(&repo_dir.join("ws")), Vec::<String>::new());
    assert!(!any_create_op(&repo.root).await);

    // With the obstacle gone, creating works.
    std::fs::remove_file(repo_dir.join("wc")).unwrap();
    create(&daemon, &repo).await;
}

/// Whether git itself, reading `.git/info/exclude`, ignores `<root>/.jj/`.
fn git_ignores_jj_dir(root: &Path) -> bool {
    let repo = gix::open(root).unwrap();
    let index = repo.index_or_empty().unwrap();
    let mut excludes = repo
        .excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )
        .unwrap();
    excludes
        .at_entry(".jj", Some(gix::index::entry::Mode::DIR))
        .unwrap()
        .is_excluded()
}

/// R1.25: `INIT_ALONGSIDE_GIT` makes a plain git checkout a colocated jj
/// repository, adds the exclude line, and registers it; `FAIL` and the
/// cases that cannot be initialized are refused.
#[tokio::test(flavor = "multi_thread")]
async fn register_initializes_jj_alongside_git() {
    let daemon = TestDaemon::start().await;
    let root = daemon.tmp.path().join("plain-git");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let head = reef_testing::git::init_repo(&root, &[("README", "plain git\n")]);
    assert!(!git_ignores_jj_dir(&root));

    let refused = daemon
        .client
        .register_repository(&root, IfNotJj::Fail)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&refused), ErrorKind::NotFound);
    assert!(!root.join(".jj").exists());

    let registered = daemon
        .client
        .register_repository(&root.join("sub"), IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    assert!(registered.initialized_jj);
    assert!(!registered.already_registered);
    let repository = registered.repository.unwrap();
    assert!(repository.colocated);
    assert_eq!(
        Path::new(&repository.root),
        std::fs::canonicalize(&root).unwrap()
    );
    assert!(root.join(".jj").is_dir());
    assert!(git_ignores_jj_dir(&root));

    let again = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    assert!(again.already_registered);
    assert!(!again.initialized_jj);

    // git's `main` is the default base once the create imports refs.
    let ws = daemon
        .client
        .create_workspace(CreateRequest {
            repo: repository.id.clone(),
            owner: OWNER.into(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&ws.path).join("README")).unwrap(),
        "plain git\n"
    );
    let new_commit: CommitIdHex = ws.commit_id.parse().unwrap();
    let parents = open(&root).await.commit_parents(&new_commit).await.unwrap();
    assert_eq!(parents, [head.parse::<CommitIdHex>().unwrap()]);

    let worktree = daemon.tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
    let error = daemon
        .client
        .register_repository(&worktree, IfNotJj::InitAlongsideGit)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::InvalidArgument);
    assert!(
        error.to_string().contains("worktree or submodule"),
        "{error}"
    );

    let empty = daemon.tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let error = daemon
        .client
        .register_repository(&empty, IfNotJj::InitAlongsideGit)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::NotFound);
    assert!(error.to_string().contains("run git init first"), "{error}");
}

// R1.06: moving a workspace between changes, numbered as its brief numbers
// them.

fn move_request(repo: &SyntheticRepo, ws: &Workspace, target: move_request::Target) -> MoveRequest {
    MoveRequest {
        repo: repo.id.clone(),
        workspace: ws.id.clone(),
        target: Some(target),
        ..MoveRequest::default()
    }
}

/// The contents of `path` in `commit`'s tree, read from the registered
/// root's repository.
async fn file_at(root: &Path, commit: &str, path: &str) -> Option<String> {
    let commit: CommitIdHex = commit.parse().unwrap();
    let blob = open(root)
        .await
        .file_blob_at(&commit, RepoPath::from_internal_string(path).unwrap())
        .await
        .unwrap()?;
    Some(String::from_utf8(blob.bytes).unwrap())
}

/// A second change: a workspace on the default commit that wrote `name`
/// and snapshotted it. Returns the snapshot's commit.
async fn change_with_file(daemon: &TestDaemon, repo: &SyntheticRepo, name: &str) -> String {
    let ws = create(daemon, repo).await;
    std::fs::write(Path::new(&ws.path).join(name), "a second change\n").unwrap();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    snapshot.commit_id
}

// R1.06 1
#[tokio::test(flavor = "multi_thread")]
async fn move_keeps_lease_and_path() {
    let (daemon, repo) = fixture().await;
    let base = default_wc(&repo.root).await;
    let ws = create(&daemon, &repo).await;
    let path = PathBuf::from(&ws.path);
    std::fs::write(path.join("x.txt"), "x\n").unwrap();

    let moved = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::NewOn(base.commit_id.to_string()),
        ))
        .await
        .unwrap();
    let after = moved.workspace.clone().unwrap();
    assert_eq!(after.id, ws.id);
    assert_eq!(after.path, ws.path);
    assert_eq!(after.backend, ws.backend);
    assert_eq!(after.state, "active");
    let (lease, lease_after) = (ws.lease.unwrap(), after.lease.unwrap());
    assert_eq!(lease_after.id, lease.id);
    assert_eq!(
        millis(lease_after.expires_at.as_ref().unwrap()),
        millis(lease.expires_at.as_ref().unwrap()),
        "a move is not a heartbeat"
    );
    assert_ne!(after.change_id, ws.change_id, "a new change");
    let commit: CommitIdHex = after.commit_id.parse().unwrap();
    assert_eq!(
        open(&repo.root)
            .await
            .commit_parents(&commit)
            .await
            .unwrap(),
        std::slice::from_ref(&base.commit_id)
    );

    assert!(!path.join("x.txt").exists(), "the new change's tree");
    let manifest = synth::load_manifest(&repo.root).unwrap().unwrap();
    assert!(path.join(&manifest.text_files[0]).is_file());
    let left = open(&repo.root)
        .await
        .resolve_revision(&ws.change_id)
        .await
        .unwrap();
    assert_eq!(
        file_at(&repo.root, left.commit_id.as_str(), "x.txt").await,
        Some("x\n".to_owned()),
        "the final snapshot holds the edit"
    );

    assert_ne!(moved.final_op_id, "");
    assert_ne!(moved.op_id, "");
    assert_ne!(moved.final_op_id, moved.op_id);
    let log = open(&repo.root).await.op_log(20).await.unwrap();
    let description = format!(
        "reef: move workspace {} to a new change on {}",
        ws.id, base.commit_id
    );
    let final_op = log
        .iter()
        .find(|op| op.id.as_str() == moved.final_op_id)
        .unwrap();
    assert!(final_op.is_snapshot);
    assert_eq!(final_op.description, description);
    let pointer_op = log.iter().find(|op| op.id.as_str() == moved.op_id).unwrap();
    assert!(!pointer_op.is_snapshot);
    assert_eq!(pointer_op.description, description);
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap();
    assert_eq!(inspected.recent_snapshots[0].op_id, moved.final_op_id);
    assert_eq!(inspected.recent_snapshots[0].kind, "move");
    assert_eq!(inspected.workspace.unwrap().change_id, after.change_id);
    assert_eq!(all_workspaces(&daemon).await.len(), 1, "no new row");
}

// R1.06 2
#[tokio::test(flavor = "multi_thread")]
async fn move_to_existing_change_edits_it() {
    let (daemon, repo) = fixture().await;
    let target = change_with_file(&daemon, &repo, "c.txt").await;
    let target_change = open(&repo.root)
        .await
        .resolve_revision(&target)
        .await
        .unwrap()
        .change_id;
    let ws = create(&daemon, &repo).await;

    let moved = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::Change(target_change.as_str()[..12].to_owned()),
        ))
        .await
        .unwrap();
    let after = moved.workspace.unwrap();
    assert_eq!(after.commit_id, target);
    assert_eq!(after.change_id, target_change.as_str());
    assert_eq!(moved.final_op_id, "", "nothing to snapshot");
    assert!(Path::new(&ws.path).join("c.txt").is_file());
    let error = open(&repo.root)
        .await
        .resolve_revision(&ws.change_id)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::NotFound,
        "the empty change it left is abandoned, as `jj edit` does"
    );

    std::fs::write(Path::new(&ws.path).join("d.txt"), "d\n").unwrap();
    // The watcher may snapshot the write first (on Linux the move
    // re-attaches the watch in the background, and a full scan follows),
    // leaving this one nothing to record: what matters is that C now holds
    // the file, whichever snapshot wrote it.
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert_eq!(
        file_at(&repo.root, &snapshot.commit_id, "d.txt").await,
        Some("d\n".to_owned())
    );
    let rewritten = open(&repo.root)
        .await
        .resolve_revision(&snapshot.commit_id)
        .await
        .unwrap();
    assert_eq!(rewritten.change_id, target_change, "the snapshot rewrote C");
    assert_eq!(
        file_at(&repo.root, &snapshot.commit_id, "c.txt").await,
        Some("a second change\n".to_owned())
    );
}

// R1.06 3
#[tokio::test(flavor = "multi_thread")]
async fn move_with_failing_snapshot_is_conflict() {
    let (daemon, repo) = fixture().await;
    let base = default_wc(&repo.root).await;
    let ws = create(&daemon, &repo).await;
    let path = PathBuf::from(&ws.path);
    let blocked = path.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("inside.txt"), "unreadable\n").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&blocked).is_ok() {
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: permissions are not enforced here (running as root?)");
        return;
    }

    let error = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::NewOn(base.commit_id.to_string()),
        ))
        .await
        .unwrap_err();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);
    let ClientError::Rpc { status, .. } = &error else {
        unreachable!()
    };
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("unsnapshotted write in flight"),
        "{}",
        status.message()
    );
    let advice = error.advice().expect("advice");
    assert_eq!(advice.next, ["reef", "ws", "inspect", ws.id.as_str()]);

    let row = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(row.change_id, ws.change_id);
    assert_eq!(row.commit_id, ws.commit_id);
    assert_eq!(row.path, ws.path);
    assert!(
        blocked.join("inside.txt").is_file(),
        "the directory is intact"
    );
    let manifest = synth::load_manifest(&repo.root).unwrap().unwrap();
    assert!(path.join(&manifest.text_files[0]).is_file());
}

// R1.06 4
#[tokio::test(flavor = "multi_thread")]
async fn move_is_cheap_on_shared_tree() {
    let (daemon, repo) = fixture().await;
    let target = change_with_file(&daemon, &repo, "one-more.txt").await;
    let ws = create(&daemon, &repo).await;

    let moved = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::NewOn(target),
        ))
        .await
        .unwrap();
    eprintln!(
        "backend {}: tree_replaced {}, files {} updated, {} added, {} removed",
        ws.backend,
        moved.tree_replaced,
        moved.files_updated,
        moved.files_added,
        moved.files_removed
    );
    if moved.tree_replaced {
        assert_eq!(
            moved.files_updated + moved.files_added + moved.files_removed,
            0
        );
    } else {
        assert_eq!(
            moved.files_updated + moved.files_added + moved.files_removed,
            1,
            "{moved:?}"
        );
        assert_eq!(moved.files_added, 1);
    }
    assert!(Path::new(&ws.path).join("one-more.txt").is_file());
}

// R1.06 5
#[tokio::test(flavor = "multi_thread")]
async fn move_then_first_snapshot_is_noop() {
    let (daemon, repo) = fixture().await;
    let target = change_with_file(&daemon, &repo, "theirs.txt").await;
    let ws = create(&daemon, &repo).await;
    std::fs::write(Path::new(&ws.path).join("mine.txt"), "mine\n").unwrap();

    let moved = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::NewOn(target.clone()),
        ))
        .await
        .unwrap();
    let after = moved.workspace.unwrap();
    assert!(Path::new(&ws.path).join("theirs.txt").is_file());
    assert!(!Path::new(&ws.path).join("mine.txt").exists());
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(!snapshot.changed, "{snapshot:?}");
    assert_eq!(snapshot.commit_id, after.commit_id);

    // Moving to the change it is on records no pointer operation.
    let again = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::Change(after.change_id.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(again.op_id, "");
    assert_eq!(again.final_op_id, "");
    assert_eq!(again.workspace.unwrap().commit_id, after.commit_id);
}

/// A released workspace cannot move, and the advice is to create another.
#[tokio::test(flavor = "multi_thread")]
async fn move_of_released_workspace_is_conflict() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    let error = daemon
        .client
        .move_workspace(move_request(
            &repo,
            &ws,
            move_request::Target::NewOn("main".into()),
        ))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);
}

/// Everything the daemons in this process logged, for
/// [`create_reports_timings`].
static LOGS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// R1.29: the response and the `workspace created` line time every step,
/// and attaching the watcher is cheap. On Linux the attach is queued for
/// the attach thread (R1.32), so `watch_ms` is only the queueing. On macOS `watch_ms` is mostly
/// `FSEvents` registering the stream, which fseventsd makes wait in
/// proportion to the entries beside the test binary and to its own load
/// (0.7 to 10 s seen in a crowded `target/debug/deps`; `docs/benchmarks.md`,
/// R1.29), so the bound is Linux's; `watcher::notify::tests` bound the
/// attach itself over that registration.
#[tokio::test(flavor = "multi_thread")]
async fn create_reports_timings() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("reef_daemon=info"))
        .with_ansi(false)
        .with_writer(|| Capture)
        .try_init();
    let (daemon, repo) = fixture().await;
    for _ in 0..2 {
        let created = daemon
            .client
            .create_workspace(request(&repo, None))
            .await
            .unwrap();
        let timings = created.timings.expect("timings in the response");
        let steps = [
            timings.resolve_ms,
            timings.base_ms,
            timings.materialize_ms,
            timings.mount_ms,
            timings.jj_ms,
            timings.seed_ms,
            timings.insert_ms,
            timings.watch_ms,
        ];
        assert!(timings.total_ms >= steps.iter().sum::<u64>(), "{timings:?}");
        assert!(timings.total_ms > 0, "{timings:?}");
        // R1.32: on Linux the attach is queued, not waited for (unless
        // it is done by the time the response is built).
        let watch_state = created.workspace.as_ref().unwrap().watch_state.as_str();
        if cfg!(target_os = "linux") {
            assert!(timings.watch_ms < 100, "{timings:?}");
            assert!(matches!(watch_state, "" | "attaching"), "{watch_state:?}");
        } else {
            assert_eq!(watch_state, "");
        }
        assert_eq!(
            created.materialize_ms,
            timings.base_ms + timings.materialize_ms
        );
        eprintln!(
            "create timings (base cached: {}): {timings:?}",
            created.base_was_cached
        );

        let id = created.workspace.unwrap().id;
        let logs = LOGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let logs = String::from_utf8_lossy(&logs);
        let line = logs
            .lines()
            .find(|line| line.contains("workspace created") && line.contains(&id))
            .unwrap_or_else(|| panic!("no `workspace created` line for {id}:\n{logs}"));
        for field in [
            "resolve_ms",
            "base_ms",
            "materialize_ms",
            "mount_ms",
            "jj_ms",
            "seed_ms",
            "insert_ms",
            "watch_ms",
            "total_ms",
        ] {
            assert!(line.contains(&format!(" {field}=")), "{field} in {line}");
        }
    }
}
