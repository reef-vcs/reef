//! Hostile repository content (RX.01): trees git itself would refuse to
//! write, symlinks out of the workspace, a store mount under a symlink, a
//! workspace root swapped for a symlink, and ignore patterns that look like
//! paths. Each test checks that Reef fails cleanly or does the safe thing,
//! and that nothing outside the workspace, the base cache, or the state
//! directory is written, deleted, or mounted. Numbered as the brief numbers
//! them; each is a row in `docs/threat-model.md`.
//!
//! The trees are written with `reef_testing::git::raw_tree`, since neither
//! git's porcelain nor `gix`'s tree encoder writes these names, into a plain
//! git checkout registered colocated, so a create imports the ref as a
//! bookmark.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestDaemon, proto_millis};
use reef_api::v1::{Backend, CreateWorkspaceRequest, IfNotJj, Workspace, create_workspace_request};
use reef_daemon::backend::fs_util::TRASH_INFIX;
use reef_daemon::client::ClientError;
use reef_daemon::config::Config;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::git::{blob, commit_tree, init_repo, raw_tree};

const OWNER: &str = "tester@host";
/// The name every hostile tree gives the file it tries to place outside the
/// workspace; found nowhere afterwards.
const CANARY: &str = "reef-escaped-canary";

const FILE: &str = "100644";
const DIR: &str = "40000";
const LINK: &str = "120000";

/// A daemon and a plain git checkout at `<tmp>/repo` with one commit on
/// `main` (`README`), registered colocated.
struct Fixture {
    daemon: TestDaemon,
    root: PathBuf,
    repo: String,
    main: String,
}

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> Fixture {
    let daemon = TestDaemon::start_with(configure).await;
    let root = daemon.tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let main = init_repo(&root, &[("README", "main\n")]);
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    let repo = registered.repository.unwrap().id;
    Fixture {
        root: root.canonicalize().unwrap(),
        daemon,
        repo,
        main,
    }
}

async fn fixture() -> Fixture {
    fixture_with(|_| {}).await
}

impl Fixture {
    /// A commit on `main` whose tree is `entries` (raw) plus `README`, at
    /// `refs/heads/<name>`.
    fn hostile(&self, name: &str, entries: &[(&str, &[u8], &str)]) -> String {
        let readme = blob(&self.root, b"main\n");
        let mut all: Vec<(&str, &[u8], &str)> = vec![(FILE, b"README", &readme)];
        all.extend_from_slice(entries);
        let tree = raw_tree(&self.root, &all);
        commit_tree(
            &self.root,
            &format!("refs/heads/{name}"),
            &tree,
            Some(&self.main),
        )
    }

    /// A tree holding one file named [`CANARY`].
    fn canary_tree(&self) -> String {
        let canary = blob(&self.root, b"escaped\n");
        raw_tree(&self.root, &[(FILE, CANARY.as_bytes(), &canary)])
    }

    async fn create_on(&self, bookmark: &str, backend: Backend) -> Result<Workspace, ClientError> {
        let response = self
            .daemon
            .client
            .create_workspace(CreateWorkspaceRequest {
                repo: self.repo.clone(),
                start: Some(create_workspace_request::Start::NewOn(bookmark.to_owned())),
                owner: OWNER.to_owned(),
                backend: backend.into(),
                ..CreateWorkspaceRequest::default()
            })
            .await?;
        Ok(response.workspace.unwrap())
    }

    /// `<state>/repos/<id>/ws`.
    fn ws_root(&self) -> PathBuf {
        self.daemon
            .state_dir()
            .join("repos")
            .join(&self.repo)
            .join("ws")
    }

    /// The workspace directories under [`Fixture::ws_root`], without the
    /// `.trash-*` ones a failed create or a release is still deleting in
    /// the background.
    fn workspace_dirs(&self) -> BTreeSet<String> {
        std::fs::read_dir(self.ws_root())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.contains(TRASH_INFIX))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fails if [`CANARY`] is anywhere under the daemon's temporary
    /// directory (state, base cache, workspaces, the checkout) but inside
    /// git's object store.
    fn assert_no_canary(&self) {
        let found = find_named(self.daemon.tmp.path(), CANARY);
        assert!(found.is_empty(), "written outside the tree: {found:?}");
    }

    /// The daemon still answers.
    async fn assert_alive(&self) {
        self.daemon.client.status().await.unwrap();
    }
}

/// Every path under `dir` named `name`, not following symlinks.
fn find_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_name() == name {
            found.push(path.clone());
        }
        if entry.file_type().unwrap().is_dir() {
            found.extend(find_named(&path, name));
        }
    }
    found
}

/// Every path under `dir` with its contents, not following symlinks: a
/// canary directory's whole state.
fn snapshot_dir(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            out.push((path.clone(), None));
            out.extend(snapshot_dir(&path));
        } else {
            out.push((path.clone(), std::fs::read(&path).ok()));
        }
    }
    out
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn tree_entry_named_dotdot_fails_the_create_cleanly() {
    let f = fixture().await;
    let canary = f.canary_tree();
    f.hostile("hostile", &[(DIR, b"..", &canary)]);
    let error = f
        .create_on("hostile", Backend::Unspecified)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("\"..\""), "{error}");
    assert!(f.workspace_dirs().is_empty(), "{:?}", f.workspace_dirs());
    f.assert_no_canary();
    f.assert_alive().await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn tree_entry_with_a_slash_does_not_take_the_repository_down() {
    let f = fixture().await;
    let canary = blob(&f.root, b"escaped\n");
    let name = format!("a/{CANARY}");
    f.hostile("hostile", &[(FILE, name.as_bytes(), &canary)]);
    // jj-lib 0.46 panics reading this tree, poisoning its git backend for
    // every workspace of the repository; Reef refuses the commit first.
    let error = f
        .create_on("hostile", Backend::Unspecified)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    assert!(error.to_string().contains("jj cannot read"), "{error}");
    let error = f
        .create_on("hostile", Backend::Unspecified)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::InvalidArgument,
        "the same again: {error}"
    );
    f.assert_no_canary();
    f.assert_alive().await;
    let ws = f.create_on("main", Backend::Unspecified).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&ws.path).join("README")).unwrap(),
        "main\n"
    );
}

// 12a: reads take any revision without the guard (R2.02). jj-lib 0.46
// panics reading the tree; the read fails with `INTERNAL`, its repository
// is reopened, and the next read and create work (the create writes a
// commit, which takes the git backend's lock a poisoned store would fail).
// The `panicked` check fails once a jj-lib bump returns an error instead,
// which is when S-18 changes too.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_a_tree_jj_cannot_read_fails_alone() {
    let f = fixture().await;
    let canary = blob(&f.root, b"escaped\n");
    let name = format!("a/{CANARY}");
    f.hostile("hostile", &[(FILE, name.as_bytes(), &canary)]);
    // A refused create imports the bookmark, so reads can name it.
    f.create_on("hostile", Backend::Unspecified)
        .await
        .unwrap_err();
    for _ in 0..2 {
        let error = f
            .daemon
            .client
            .get_diff(reef_api::v1::GetDiffRequest {
                repo: f.repo.clone(),
                from_revision: "main".to_owned(),
                to_revision: "hostile".to_owned(),
                ..reef_api::v1::GetDiffRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
        assert!(error.to_string().contains("panicked"), "{error}");
    }
    let main = f
        .daemon
        .client
        .get_change(reef_api::v1::GetChangeRequest {
            repo: f.repo.clone(),
            revision: "main".to_owned(),
            ..reef_api::v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(main.commit_id, f.main);
    f.assert_no_canary();
    f.assert_alive().await;
    f.create_on("main", Backend::Unspecified).await.unwrap();
}

// 12b: the guard's backstop. A job that panics anyway (here jj-lib, handed
// the tree directly) leaves no poisoned store behind: the thread reopens
// the repository, so the next job and the next create work, and a
// workspace opened before the panic snapshots on a fresh store too, and
// so does GC after another panic (#63).
#[tokio::test(flavor = "multi_thread")]
async fn a_panicked_job_reopens_the_repository() {
    use futures::FutureExt as _;
    use reef_daemon::model::RepoId;
    use reef_daemon::repo::RepoThreadError;
    use reef_lib::jj::CommitIdHex;

    let f = fixture().await;
    let before = f.create_on("main", Backend::Unspecified).await.unwrap();
    let canary = blob(&f.root, b"escaped\n");
    let name = format!("a/{CANARY}");
    let hostile: CommitIdHex = f
        .hostile("hostile", &[(FILE, name.as_bytes(), &canary)])
        .parse()
        .unwrap();
    let id: RepoId = f.repo.parse().unwrap();
    let handle = f.daemon.handle.state().repos.get(id).unwrap();
    let panic = async || {
        let hostile = hostile.clone();
        let panicked = handle
            .call_async(move |head| {
                async move {
                    let _ = head.repo.spec_at(&hostile).await;
                }
                .boxed_local()
            })
            .await;
        assert!(
            matches!(panicked, Err(RepoThreadError::Panicked { .. })),
            "{panicked:?}"
        );
    };
    panic().await;
    let main: CommitIdHex = f.main.parse().unwrap();
    handle
        .call_async(move |head| {
            async move { head.repo.spec_at(&main).await.map(drop) }.boxed_local()
        })
        .await
        .unwrap()
        .unwrap();
    f.create_on("main", Backend::Unspecified).await.unwrap();
    std::fs::write(Path::new(&before.path).join("after-panic"), "kept\n").unwrap();
    let snapshot = f
        .daemon
        .client
        .snapshot_workspace(&f.repo, &before.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed, "{snapshot:?}");

    panic().await;
    let reports = f
        .daemon
        .client
        .collect_garbage(&f.repo, false)
        .await
        .unwrap()
        .reports;
    assert_eq!(reports.len(), 1);
    assert!(reports[0].jj_error.is_empty(), "{}", reports[0].jj_error);
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn reserved_names_in_a_tree_are_refused() {
    let f = fixture().await;
    let git_before = snapshot_dir(&f.root.join(".git"));
    let canary = f.canary_tree();
    let file = blob(&f.root, b"[core]\n\thooksPath = /tmp\n");
    for (name, entries) in [
        ("git-file", vec![(FILE, b".git".as_slice(), file.as_str())]),
        ("git-dir", vec![(DIR, b".git".as_slice(), canary.as_str())]),
        ("jj-file", vec![(FILE, b".jj".as_slice(), file.as_str())]),
        ("jj-dir", vec![(DIR, b".jj".as_slice(), canary.as_str())]),
    ] {
        f.hostile(name, &entries);
        match f.create_on(name, Backend::Unspecified).await {
            Err(error) => eprintln!("{name}: refused: {error}"),
            Ok(ws) => panic!("{name}: created {}", ws.path),
        }
        f.assert_no_canary();
        f.assert_alive().await;
    }
    let git_after: Vec<_> = snapshot_dir(&f.root.join(".git"))
        .into_iter()
        .filter(|(path, _)| !path.starts_with(f.root.join(".git/objects")))
        .collect();
    let git_before: Vec<_> = git_before
        .into_iter()
        .filter(|(path, _)| !path.starts_with(f.root.join(".git/objects")))
        .collect();
    let changed: Vec<_> = git_after
        .iter()
        .filter(|entry| !git_before.contains(entry))
        .map(|(path, _)| path)
        .collect();
    // Only the refs this test wrote, and their logs.
    let git = f.root.join(".git");
    assert!(
        changed
            .iter()
            .all(|path| path.starts_with(git.join("refs")) || path.starts_with(git.join("logs"))),
        "{changed:?}"
    );
}

/// The available backends, from the daemon's probe.
async fn backends(f: &Fixture) -> Vec<Backend> {
    f.daemon
        .client
        .status()
        .await
        .unwrap()
        .backends
        .into_iter()
        .filter(|probe| probe.available)
        .map(|probe| probe.backend())
        .collect()
}

/// A canary directory beside the state directory, holding one file.
fn canary_dir(f: &Fixture) -> PathBuf {
    let dir = f.daemon.tmp.path().join("canary");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep"), "canary\n").unwrap();
    dir.canonicalize().unwrap()
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn symlink_to_outside_is_recreated_and_never_followed() {
    let f = fixture().await;
    let canary = canary_dir(&f);
    std::fs::write(f.root.join("canary.txt"), "user's\n").unwrap();
    let canary_target = canary.to_str().unwrap().to_owned();
    // `<state>/repos/<id>/ws/<ws>/up` is the temporary directory itself.
    let links: [(&[u8], &str); 3] = [
        (b"escape", "/"),
        (b"up", "../../../../.."),
        (b"canary", &canary_target),
    ];
    let blobs: Vec<(&[u8], String)> = links
        .iter()
        .map(|(name, target)| (*name, blob(&f.root, target.as_bytes())))
        .collect();
    let entries: Vec<(&str, &[u8], &str)> = blobs
        .iter()
        .map(|(name, id)| (LINK, *name, id.as_str()))
        .collect();
    f.hostile("links", &entries);
    let before = (
        snapshot_dir(&canary),
        std::fs::read(f.root.join("canary.txt")).unwrap(),
    );

    for backend in backends(&f).await {
        let ws = f.create_on("links", backend).await.unwrap();
        let root = Path::new(&ws.path);
        for (name, target) in links {
            let path = root.join(std::str::from_utf8(name).unwrap());
            assert!(
                std::fs::symlink_metadata(&path).unwrap().is_symlink(),
                "{backend:?}: {}",
                path.display()
            );
            assert_eq!(
                std::fs::read_link(&path).unwrap(),
                Path::new(target),
                "{backend:?}"
            );
        }
        // Disk usage, a snapshot after a write, and the release all walk
        // the tree.
        let begun = Instant::now();
        let inspected = f
            .daemon
            .client
            .inspect_workspace(&f.repo, &ws.id, false)
            .await
            .unwrap();
        assert!(inspected.disk_usage.is_some(), "{backend:?}");
        assert!(
            begun.elapsed() < Duration::from_secs(30),
            "{backend:?}: followed a link"
        );
        std::fs::write(root.join("new.txt"), "agent\n").unwrap();
        f.daemon
            .client
            .snapshot_workspace(&f.repo, &ws.id, "")
            .await
            .unwrap();
        f.daemon
            .client
            .release_workspace(&f.repo, &ws.id, true, false)
            .await
            .unwrap();
        assert!(!root.exists(), "{backend:?}");
        let after = (
            snapshot_dir(&canary),
            std::fs::read(f.root.join("canary.txt")).unwrap(),
        );
        assert_eq!(before, after, "{backend:?}");
    }
    // The trash sweep at start.
    let stopped = f.daemon.stop_keeping().await;
    let daemon = stopped.start().await;
    let after = (
        snapshot_dir(&canary),
        std::fs::read(f.root.join("canary.txt")).unwrap(),
    );
    assert_eq!(before, after);
    daemon.stop().await;
}

/// A commit on `main` that declares store `deps` mounted at
/// `vendor/deps`, with `extra` entries beside it.
fn with_store(f: &Fixture, name: &str, extra: &[(&str, &[u8], &str)]) -> String {
    let spec = "[[store]]\nname = \"deps\"\nlockfile = \"deps.lock\"\nmount = \"vendor/deps\"\nbuild = [\"sh\", \"-c\", \"mkdir -p \\\"$REEF_STORE_DIR\\\"\"]\n";
    let spec = blob(&f.root, spec.as_bytes());
    let lock = blob(&f.root, b"v1\n");
    let mut entries: Vec<(&str, &[u8], &str)> =
        vec![(FILE, b"deps.lock", &lock), (FILE, b"reef.toml", &spec)];
    entries.extend_from_slice(extra);
    f.hostile(name, &entries)
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn store_mount_under_a_symlinked_ancestor_is_refused_end_to_end() {
    let f = fixture().await;
    let canary = canary_dir(&f);
    let before = snapshot_dir(&canary);
    with_store(&f, "with-store", &[]);
    let mut events = f
        .daemon
        .client
        .build_store(&f.repo, "deps", "with-store", Duration::from_mins(5), false)
        .await
        .unwrap();
    let mut sealed = false;
    while let Some(event) = events.message().await.unwrap() {
        if let Some(reef_api::v1::build_store_response::Event::Result(done)) = event.event {
            sealed = done.sealed;
        }
    }
    assert!(sealed, "the store is built");
    let link = blob(&f.root, canary.to_str().unwrap().as_bytes());
    with_store(&f, "hostile-store", &[(LINK, b"vendor", &link)]);
    let error = f
        .create_on("hostile-store", Backend::Unspecified)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert_eq!(snapshot_dir(&canary), before);
    assert!(f.workspace_dirs().is_empty(), "{:?}", f.workspace_dirs());
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn reclaim_of_a_root_replaced_by_a_symlink_removes_only_the_link() {
    let f = fixture_with(|config| {
        config.file.lease.grace = Duration::from_millis(500);
        config.file.lease.tick = Duration::from_millis(200);
    })
    .await;
    let canary = canary_dir(&f);
    let before = snapshot_dir(&canary);
    let ws = f
        .daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: f.repo.clone(),
            start: Some(create_workspace_request::Start::NewOn("main".into())),
            owner: OWNER.to_owned(),
            // Not an overlay: its root is a mount point, which no rename
            // replaces.
            backend: Backend::Copy.into(),
            lease_length: Some(proto_millis(1000)),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let root = PathBuf::from(&ws.path);
    let aside = root.with_extension("aside");
    std::fs::rename(&root, &aside).unwrap();
    std::os::unix::fs::symlink(&canary, &root).unwrap();

    let id: reef_daemon::model::WorkspaceId = ws.id.parse().unwrap();
    let begun = Instant::now();
    let reclaim = loop {
        let rows = f.daemon.handle.state().db.reclaims(None, 100).unwrap();
        if let Some(row) = rows.into_iter().find(|row| row.workspace_id == id) {
            break row;
        }
        assert!(begun.elapsed() < Duration::from_secs(30), "never reclaimed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // No final snapshot: it would have scanned the canary through the link
    // and recorded its files as the workspace's.
    assert!(!reclaim.snapshot_taken, "{reclaim:?}");
    assert!(
        reclaim
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("workspace root replaced by a symlink")),
        "{reclaim:?}"
    );
    let listed = f
        .daemon
        .client
        .list_workspaces(reef_api::v1::ListWorkspacesRequest {
            repo: f.repo.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let after = listed.iter().find(|w| w.id == ws.id).unwrap();
    assert_eq!(
        common::spawned::file_at(&f.root, &after.commit_id, "keep").await,
        None,
        "nothing from the canary was recorded"
    );
    assert!(
        std::fs::symlink_metadata(&root).is_err(),
        "the link is gone"
    );
    assert_eq!(snapshot_dir(&canary), before, "the canary is intact");
    // What the root was replaced with was not deleted either.
    assert!(aside.join("README").is_file());
}

// 16b: an explicit snapshot of such a root is refused, and a release
// unlinks it without a final snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_of_a_root_replaced_by_a_symlink_is_refused() {
    let f = fixture().await;
    let canary = canary_dir(&f);
    let before = snapshot_dir(&canary);
    // Not an overlay, as in 16.
    let ws = f.create_on("main", Backend::Copy).await.unwrap();
    let root = PathBuf::from(&ws.path);
    std::fs::rename(&root, root.with_extension("aside")).unwrap();
    std::os::unix::fs::symlink(&canary, &root).unwrap();
    let error = f
        .daemon
        .client
        .snapshot_workspace(&f.repo, &ws.id, "")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert!(error.to_string().contains("is a symlink"), "{error}");
    let released = f
        .daemon
        .client
        .release_workspace(&f.repo, &ws.id, true, false)
        .await
        .unwrap();
    assert!(!released.snapshot_taken, "{released:?}");
    assert!(std::fs::symlink_metadata(&root).is_err());
    assert_eq!(snapshot_dir(&canary), before);
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn reefignore_and_excludes_are_patterns_only() {
    let f = fixture().await;
    let canary = canary_dir(&f);
    let before = snapshot_dir(&canary);
    let lines = format!(
        "/etc/passwd\n../x\n{}\n../../../../../canary\n",
        canary.display()
    );
    let spec = format!(
        "[snapshot]\nexclude = [\"/etc/passwd\", \"../x\", {:?}, \"../../../../../canary\"]\n",
        canary.display().to_string()
    );
    let spec = blob(&f.root, spec.as_bytes());
    let ignore = blob(&f.root, lines.as_bytes());
    f.hostile(
        "patterns",
        &[(FILE, b".reefignore", &ignore), (FILE, b"reef.toml", &spec)],
    );
    let ws = f.create_on("patterns", Backend::Unspecified).await.unwrap();
    let root = Path::new(&ws.path);
    std::fs::write(root.join("x"), "x\n").unwrap();
    std::fs::write(root.join("passwd"), "not that one\n").unwrap();
    f.daemon
        .client
        .snapshot_workspace(&f.repo, &ws.id, "")
        .await
        .unwrap();
    f.daemon
        .client
        .release_workspace(&f.repo, &ws.id, true, false)
        .await
        .unwrap();
    assert_eq!(snapshot_dir(&canary), before);
    // `../x`, taken as a path from the workspace root, is here.
    assert!(!f.ws_root().join("x").exists());
}

// 12c: registration checks a plain git checkout's `HEAD` out as it
// initializes jj; a tree jj cannot read is refused first, leaving no `.jj`.
#[tokio::test(flavor = "multi_thread")]
async fn registering_a_checkout_whose_head_jj_cannot_read_fails_cleanly() {
    let daemon = TestDaemon::start().await;
    let root = daemon.tmp.path().join("checkout");
    std::fs::create_dir_all(&root).unwrap();
    let main = init_repo(&root, &[("README", "main\n")]);
    let canary = blob(&root, b"escaped\n");
    let readme = blob(&root, b"main\n");
    let name = format!("a/{CANARY}");
    let tree = raw_tree(
        &root,
        &[(FILE, b"README", &readme), (FILE, name.as_bytes(), &canary)],
    );
    commit_tree(&root, "refs/heads/main", &tree, Some(&main));
    let error = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    assert!(error.to_string().contains("jj cannot read"), "{error}");
    assert!(!root.join(".jj").exists());
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap_or_default();
    assert!(!exclude.lines().any(|line| line == "/.jj/"), "{exclude}");
    daemon.client.status().await.unwrap();
}

// R2.04
#[tokio::test(flavor = "multi_thread")]
async fn edit_files_never_writes_through_a_symlink() {
    use reef_api::v1::{EditFilesRequest, FileEdit, edit_files_request, file_edit};
    let f = fixture().await;
    let canary = canary_dir(&f);
    let target = canary.to_str().unwrap().to_owned();
    let link = blob(&f.root, target.as_bytes());
    f.hostile("editlinks", &[(LINK, b"link", &link)]);
    let before = (
        snapshot_dir(&canary),
        snapshot_dir(&f.root.join(".git").join("hooks")),
    );
    let ws = f
        .create_on("editlinks", Backend::Unspecified)
        .await
        .unwrap();
    let request = |edits: Vec<FileEdit>| EditFilesRequest {
        workspace: ws.id.clone(),
        target: Some(edit_files_request::Target::Revision("@".to_owned())),
        edits,
        ..EditFilesRequest::default()
    };
    let content = |path: &str| FileEdit {
        path: path.to_owned(),
        action: Some(file_edit::Action::Content(CANARY.as_bytes().to_vec())),
        executable: None,
    };
    let reason = |error: ClientError| match error {
        ClientError::Rpc {
            reason, metadata, ..
        } => (reason, metadata.get("path").cloned()),
        other => panic!("{other:?}"),
    };
    let client = &f.daemon.client;
    let through = client
        .edit_files(request(vec![content("link/x")]))
        .await
        .unwrap_err();
    assert_eq!(
        reason(through),
        ("NOT_A_DIRECTORY".to_owned(), Some("link".to_owned()))
    );
    for reserved in [".git/hooks/pre-commit", ".jj/repo/x", "a/.GIT/config"] {
        let error = client
            .edit_files(request(vec![content(reserved)]))
            .await
            .unwrap_err();
        assert_eq!(reason(error).0, "INVALID_PATH", "{reserved}");
    }
    // A link may be copied; it stays a link, never followed.
    let copy = FileEdit {
        path: "inner/link2".to_owned(),
        action: Some(file_edit::Action::CopyFrom("link".to_owned())),
        executable: None,
    };
    client.edit_files(request(vec![copy])).await.unwrap();
    let copied = Path::new(&ws.path).join("inner").join("link2");
    assert!(std::fs::symlink_metadata(&copied).unwrap().is_symlink());
    assert_eq!(std::fs::read_link(&copied).unwrap(), canary);
    let after = (
        snapshot_dir(&canary),
        snapshot_dir(&f.root.join(".git").join("hooks")),
    );
    assert_eq!(before, after);
    f.assert_alive().await;
    f.daemon.stop().await;
}
