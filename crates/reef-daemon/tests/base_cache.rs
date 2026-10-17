//! The base cache and backend selection against a real jj repository
//! (R1.02): a synthetic tree from `reef_testing::synth`, snapshotted, then
//! materialized through jj-lib into a base.
//!
//! The brief's third case, a conflicted tree's `+`-joined key, needs jj-lib
//! types to build the conflict, which stay inside `reef-lib`; it is
//! `materialize_conflicted_tree_has_joined_key` in
//! `crates/reef-lib/tests/materialize.rs`, and `base_cache.rs`'s unit tests
//! check that such a key works as a directory name.
//!
//! `REEF_TEST_PRESET=large` runs the first case on the `large` preset to
//! measure the one-time check-out (it prints the time either way).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; timings are reported on stderr"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use reef_daemon::backend::copy::CopyBackend;
use reef_daemon::backend::fs_util::changed_files_vs_base;
use reef_daemon::backend::{
    Availability, Backend as _, BackendChoice, BackendError, BackendKind, BaseCache, Probe,
    WorkspacePaths, select, select_from,
};
use reef_daemon::db::Db;
use reef_daemon::model::RepoId;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::materialize::tree_state_entries;
use reef_lib::jj::settings::{Identity, user_settings_with_max_new_file_size};
use reef_lib::jj::{CommitIdHex, Repo};
use reef_testing::synth::{self, Manifest, Preset, Spec};
use tempfile::TempDir;
use walkdir::WalkDir;

/// Above the generator's 8 MiB binaries, so every file is tracked.
const MAX_NEW_FILE_SIZE: u64 = 16 << 20;

/// A synthetic repository snapshotted once, and a state directory with a
/// database and a base cache for it.
struct Fixture {
    tmp: TempDir,
    repo: Repo,
    commit: CommitIdHex,
    tree_id: String,
    manifest: Manifest,
    cache: BaseCache,
}

impl Fixture {
    fn new(preset: Preset) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let manifest = synth::generate(&root, &Spec::preset(preset), 7, false).unwrap();
        let settings = user_settings_with_max_new_file_size(
            &Identity {
                name: "Reef Test".into(),
                email: "reef-test@example.com".into(),
            },
            MAX_NEW_FILE_SIZE,
        )
        .unwrap();
        let mut repo = pollster::block_on(Repo::init_internal_git(&root, &settings)).unwrap();
        let outcome = pollster::block_on(repo.snapshot("snapshot")).unwrap();
        assert!(outcome.untracked.is_empty(), "{:?}", outcome.untracked);
        let commit = outcome.wc_commit;
        let tree_id = pollster::block_on(repo.tree_key(&commit)).unwrap();

        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let db = Arc::new(Db::open(&state.join("reef.sqlite3")).unwrap());
        let repo_id = RepoId::generate();
        let cache = BaseCache::new(db, &repo_dir(&state, repo_id), repo_id);
        Self {
            tmp,
            repo,
            commit,
            tree_id,
            manifest,
            cache,
        }
    }

    /// Files a snapshot tracks: text files, binaries, symlinks, and the root
    /// `.gitignore` (the manifest and ignored directories are not).
    fn tracked_files(&self) -> u64 {
        self.manifest.files + self.manifest.binaries.len() as u64 + self.manifest.symlinks + 1
    }

    /// `ensure` for the fixture's tree, materializing through jj.
    fn ensure(&self) -> reef_daemon::backend::BaseEntry {
        self.cache
            .ensure(&self.tree_id, |dir, state| {
                pollster::block_on(self.repo.materialize_tree(&self.commit, dir, state))
            })
            .unwrap()
    }

    fn state(&self) -> PathBuf {
        self.tmp.path().join("state")
    }
}

fn repo_dir(state: &Path, repo_id: RepoId) -> PathBuf {
    state.join("repos").join(repo_id.as_hex())
}

fn preset() -> Preset {
    match std::env::var("REEF_TEST_PRESET").as_deref() {
        Ok("large") => Preset::Large,
        Ok("medium") => Preset::Medium,
        _ => Preset::Small,
    }
}

/// Non-directory entries under `dir`, without following symlinks.
fn count_files(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| !entry.file_type().is_dir())
        .count() as u64
}

#[test]
fn ensure_materializes_once() {
    let preset = preset();
    let started = Instant::now();
    let fixture = Fixture::new(preset);
    eprintln!(
        "base cache: {preset:?} fixture generated and snapshotted in {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let calls = AtomicUsize::new(0);
    let started = Instant::now();
    let entry = fixture
        .cache
        .ensure(&fixture.tree_id, |dir, state| {
            calls.fetch_add(1, Ordering::SeqCst);
            pollster::block_on(fixture.repo.materialize_tree(&fixture.commit, dir, state))
        })
        .unwrap();
    eprintln!(
        "base cache: {preset:?} tree ({} files, {} bytes) materialized through jj in {:.2} s",
        entry.files,
        entry.bytes,
        started.elapsed().as_secs_f64()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(entry.tree_id, fixture.tree_id);
    assert_eq!(entry.files, fixture.tracked_files());
    assert_eq!(count_files(&entry.path), fixture.tracked_files());
    assert!(entry.tree_state_file().is_file());
    assert!(!entry.path.join(".jj").exists());
    assert!(!entry.path.join(synth::MANIFEST_FILE).exists(), "untracked");

    let started = Instant::now();
    let again = fixture
        .cache
        .ensure(&fixture.tree_id, |_, _| panic!("materialized twice"))
        .unwrap();
    eprintln!(
        "base cache: reused in {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(again, entry);
    assert_eq!(fixture.cache.entries().unwrap(), [entry]);
}

#[test]
fn interrupted_materialization_is_swept() {
    let fixture = Fixture::new(Preset::Small);
    let entry = fixture.ensure();
    let bases = fixture.cache.dir().to_owned();
    let tmp = bases.join("x.tmp");
    std::fs::create_dir_all(tmp.join("half/written")).unwrap();
    std::fs::write(tmp.join("half/written/file"), "x").unwrap();
    let rowless = bases.join("abcd");
    std::fs::create_dir(&rowless).unwrap();

    let removed = fixture.cache.sweep_incomplete().unwrap();
    assert_eq!(removed, [rowless.clone(), tmp.clone()]);
    assert!(!tmp.exists() && !rowless.exists());
    assert!(
        entry.path.is_dir() && entry.tree_state_file().is_file(),
        "complete entry kept"
    );
    assert_eq!(
        fixture.cache.sweep_incomplete().unwrap(),
        Vec::<PathBuf>::new()
    );

    // A crash between the two renames leaves `<tree-id>.state` and
    // `<tree-id>.tmp` with no row; the next `ensure` starts over cleanly.
    fixture.cache.remove(&fixture.tree_id).unwrap();
    std::fs::create_dir(bases.join(format!("{}.state", fixture.tree_id))).unwrap();
    std::fs::create_dir(bases.join(format!("{}.tmp", fixture.tree_id))).unwrap();
    let rebuilt = fixture.ensure();
    assert_eq!(count_files(&rebuilt.path), fixture.tracked_files());
}

#[test]
fn tree_state_entries_match_disk() {
    let fixture = Fixture::new(Preset::Small);
    let entry = fixture.ensure();
    let entries = tree_state_entries(&entry.state_dir).unwrap();
    assert_eq!(entries.len() as u64, fixture.tracked_files());
    for recorded in &entries {
        let metadata = std::fs::symlink_metadata(entry.path.join(&recorded.path)).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        assert_eq!(recorded.size, metadata.len(), "{}", recorded.path);
        assert_eq!(
            recorded.mtime_ms,
            i64::try_from(mtime.as_millis()).unwrap(),
            "{}",
            recorded.path
        );
    }
}

#[test]
fn changed_files_vs_base_counts_edits() {
    // The copy backend materializes the workspace, keeping every mtime.
    let fixture = Fixture::new(Preset::Small);
    let base = fixture.ensure();
    let ws = WorkspacePaths::under(
        &repo_dir(&fixture.state(), RepoId::generate()),
        "00000000000000aa".parse().unwrap(),
    );
    CopyBackend.materialize(&base, &ws).unwrap();
    let usage = changed_files_vs_base(&base, &ws).unwrap();
    assert_eq!(usage.files_changed, 0, "an exact copy differs in nothing");
    assert_eq!(usage.allocated_bytes, 0);

    let text = &fixture.manifest.text_files;
    let modified = ws.root.join(&text[0]);
    let mut contents = std::fs::read(&modified).unwrap();
    contents.extend_from_slice(b"\nedited\n");
    std::fs::write(&modified, contents).unwrap();
    std::fs::remove_file(ws.root.join(&text[1])).unwrap();
    std::fs::write(ws.root.join("new-file.txt"), vec![b'x'; 10_000]).unwrap();
    let usage = changed_files_vs_base(&base, &ws).unwrap();
    assert_eq!(
        usage.files_changed, 3,
        "one modified, one deleted, one added"
    );
    assert!(usage.allocated_bytes >= 10_000, "{usage:?}");
}

#[test]
fn select_auto_returns_copy_here() {
    let dir = tempfile::tempdir().unwrap();
    let backend = select(BackendChoice::Auto, &dir.path().join("ws")).unwrap();
    assert_eq!(backend.kind(), BackendKind::Copy);

    // With every other backend stubbed unavailable, auto is the copy; with
    // one available, auto takes it in AUTO_ORDER.
    let stub = |available: &[BackendKind]| -> Vec<Probe> {
        BackendKind::AUTO_ORDER
            .into_iter()
            .map(|kind| Probe {
                kind,
                availability: if available.contains(&kind) {
                    Availability::Available
                } else {
                    Availability::Unavailable("stubbed".into())
                },
                filesystem: "stub".into(),
            })
            .collect()
    };
    let only_copy = stub(&[BackendKind::Copy]);
    assert_eq!(
        select_from(BackendChoice::Auto, &only_copy).unwrap().kind(),
        BackendKind::Copy
    );
    let clones = stub(&[
        BackendKind::Copy,
        BackendKind::Reflink,
        BackendKind::ApfsClone,
    ]);
    assert_eq!(
        select_from(BackendChoice::Auto, &clones).unwrap().kind(),
        BackendKind::ApfsClone
    );
    let error = select_from(BackendChoice::Auto, &stub(&[])).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Unavailable);

    // A named backend that is unavailable is an error with the reason,
    // never a fallback.
    let error = select(
        BackendChoice::Named(BackendKind::Overlayfs),
        &dir.path().join("ws"),
    )
    .err()
    .unwrap();
    assert!(
        matches!(
            &error,
            BackendError::Unavailable { kind: BackendKind::Overlayfs, reason } if !reason.is_empty()
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    let error = select_from(BackendChoice::Named(BackendKind::Reflink), &clones);
    assert_eq!(error.unwrap().kind(), BackendKind::Reflink);
}
