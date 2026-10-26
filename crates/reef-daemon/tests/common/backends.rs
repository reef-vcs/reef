//! The backend conformance suite (R1.11): the daemon-facing contract every
//! materialization backend must keep, asserted the same way on every
//! platform. [`run_conformance`] runs every case against one backend and a
//! [`BaseFixture`], and reports what passed, what was skipped and why, and
//! what failed, so one run shows everything wrong with a backend.
//!
//! The suite checks the contract, not the mechanism: it never looks at an
//! upper directory or at clone block sharing (each backend's own tests do).
//! The one exception is base immutability, which every mechanism must
//! guarantee.
//!
//! Included by `tests/backend_conformance.rs` through `#[path]`, so the
//! other test binaries do not compile it; R1.12's benchmarks reuse
//! [`BaseFixture`]. Case 9 (`swap_base`) holds each backend to what a
//! workspace move (R1.06) relies on.

#![allow(
    dead_code,
    unreachable_pub,
    reason = "R1.12's benchmarks use parts the conformance tests do not"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    reason = "test helpers outside #[test] fns; a panic is a case failure, and skips print on stderr"
)]

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::{self, Debug, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use reef_daemon::backend::copy::CopyBackend;
use reef_daemon::backend::fs_util::{free_bytes, is_executable, mtime_millis};
use reef_daemon::backend::{
    Backend, BackendError, BackendKind, BaseCache, BaseEntry, DiskUsage, SwapOutcome,
    WorkspacePaths,
};
use reef_daemon::db::Db;
use reef_daemon::model::{RepoId, WorkspaceId};
use reef_daemon::testing::{short_tmp, test_settings};
use reef_lib::error::{Classify as _, ErrorChain, ErrorKind};
use reef_lib::jj::materialize::{TREE_STATE_FILE, tree_state_entries};
use reef_lib::jj::{CommitIdHex, Repo, StartPoint};
use reef_testing::synth::{self, MANIFEST_FILE, Manifest, MutateOps, Mutation, Preset, Spec};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use walkdir::WalkDir;

/// The seed [`BaseFixture::small`] generates its tree with.
pub const SEED: u64 = 42;
/// A symlink to nothing, which the fixture adds before building the base.
pub const DANGLING: &str = "dangling";
/// What [`DANGLING`] points to.
pub const DANGLING_TARGET: &str = "missing-target";
/// An empty directory the fixture adds to the base.
pub const EMPTY_DIR: &str = "empty";
/// What the modify cases (6 and 7) do to a workspace.
pub const MUTATION: MutateOps = MutateOps {
    modify: 50,
    add: 10,
    delete: 10,
};
/// How the second base, which case 9 swaps to, differs from the first.
pub const SWAP_MUTATION: MutateOps = MutateOps {
    modify: 20,
    add: 5,
    delete: 5,
};
/// How long case 10 waits for a background deletion to finish.
const TRASH_WAIT: Duration = Duration::from_secs(30);
const MIB: u64 = 1 << 20;

/// A synthetic tree, a jj repository around it, and a base cache entry for
/// its first commit, in a temporary state directory laid out as the
/// daemon's (`repos/<repo-id>/bases`, `ws`, `wc`).
pub struct BaseFixture {
    /// The state directory: a short name (`reef-XXXX`) so socket paths fit
    /// if a later ticket starts a daemon on it.
    pub state: TempDir,
    /// The jj repository's root, `<state>/repo`.
    pub repo_root: PathBuf,
    /// The repository. Cases create jj workspaces on it through a shared
    /// reference, and `Repo` is not `Sync`, hence the `Mutex`.
    pub repo: Mutex<Repo>,
    /// The base cache's repository id.
    pub repo_id: RepoId,
    /// The base for [`BaseFixture::commit`], with [`EMPTY_DIR`] added.
    pub base: BaseEntry,
    /// The commit the base is the tree of.
    pub commit: CommitIdHex,
    /// What the generator wrote.
    pub manifest: Manifest,
    /// The preset's name for the timing row, or `custom`.
    pub preset: &'static str,
    cache: BaseCache,
    swap_target: OnceLock<(CommitIdHex, BaseEntry)>,
    case_base: OnceLock<(CommitIdHex, BaseEntry)>,
}

impl BaseFixture {
    /// The `small` preset with seed [`SEED`], under the short temporary
    /// directory.
    pub fn small() -> Self {
        Self::in_dir(&short_tmp(), &Spec::preset(Preset::Small), SEED, "small")
    }

    /// A fixture for `spec` and `seed` under the short temporary directory.
    pub fn with_spec(spec: &Spec, seed: u64) -> Self {
        Self::in_dir(&short_tmp(), spec, seed, "custom")
    }

    /// A fixture whose state directory is a new directory under `parent`,
    /// for backends that need a particular filesystem (reflink).
    pub fn in_dir(parent: &Path, spec: &Spec, seed: u64, preset: &'static str) -> Self {
        let state = tempfile::Builder::new()
            .prefix("reef-")
            .tempdir_in(parent)
            .unwrap();
        let repo_root = state.path().join("repo");
        let manifest = synth::generate(&repo_root, spec, seed, false).unwrap();
        std::os::unix::fs::symlink(DANGLING_TARGET, repo_root.join(DANGLING)).unwrap();
        let mut repo =
            pollster::block_on(Repo::init_internal_git(&repo_root, &test_settings())).unwrap();
        let outcome = pollster::block_on(repo.snapshot("conformance fixture")).unwrap();
        assert!(outcome.untracked.is_empty(), "{:?}", outcome.untracked);
        let commit = outcome.wc_commit;
        let tree_id = pollster::block_on(repo.tree_key(&commit)).unwrap();

        let db = Arc::new(Db::open(&state.path().join("reef.sqlite3")).unwrap());
        let repo_id = RepoId::generate();
        let repo_dir = state.path().join("repos").join(repo_id.as_hex());
        let cache = BaseCache::new(db, &repo_dir, repo_id);
        let base = cache
            .ensure(&tree_id, |dir, state| {
                pollster::block_on(repo.materialize_tree(&commit, dir, state))
            })
            .unwrap();
        // jj checks out files, never empty directories, so a base built
        // through it never has one; the fixture adds one so the suite sees
        // whether a backend reproduces directories it has no file for. This
        // is the one write under a base after `ensure`, which the cache
        // otherwise never allows; it happens before any digest is taken.
        fs::create_dir(base.path.join(EMPTY_DIR)).unwrap();
        Self {
            state,
            repo_root,
            repo: Mutex::new(repo),
            repo_id,
            base,
            commit,
            manifest,
            preset,
            cache,
            swap_target: OnceLock::new(),
            case_base: OnceLock::new(),
        }
    }

    /// `<state>/repos/<repo-id>`.
    pub fn repo_dir(&self) -> PathBuf {
        self.state.path().join("repos").join(self.repo_id.as_hex())
    }

    /// The base cache directory, `<state>/repos/<repo-id>/bases`.
    pub fn bases_dir(&self) -> &Path {
        self.cache.dir()
    }

    /// Where workspace `id` lives, under `<state>/repos/<repo-id>/`: its
    /// root in `ws/`, overlay siblings beside it, and its jj state in `wc/`.
    /// The same for every kind, so the suite is kind-agnostic.
    pub fn workspace_paths(&self, id: WorkspaceId) -> WorkspacePaths {
        WorkspacePaths::under(&self.repo_dir(), id)
    }

    /// A second commit, the first with `ops` applied by `synth::mutate`,
    /// and its base.
    pub fn second_commit(&self, ops: MutateOps) -> (CommitIdHex, BaseEntry) {
        self.derived_base("conformance: second commit", |root| {
            synth::mutate(root, 1, ops).unwrap();
        })
    }

    /// The base case 9 swaps to: [`BaseFixture::second_commit`] with
    /// [`SWAP_MUTATION`], built once.
    pub fn swap_target(&self) -> &(CommitIdHex, BaseEntry) {
        self.swap_target
            .get_or_init(|| self.second_commit(SWAP_MUTATION))
    }

    /// The base case 12 materializes: the first commit plus `A.txt` and
    /// `a.txt`, built once. Only meaningful on a case-sensitive filesystem.
    pub fn case_base(&self) -> &(CommitIdHex, BaseEntry) {
        self.case_base.get_or_init(|| {
            self.derived_base("conformance: case-sensitive names", |root| {
                fs::write(root.join("A.txt"), "upper\n").unwrap();
                fs::write(root.join("a.txt"), "lower\n").unwrap();
            })
        })
    }

    /// A new commit on top of [`BaseFixture::commit`] whose tree is the
    /// first one after `edit`, and its base. The edit happens in a scratch
    /// jj workspace (a copy of the first base), not in the repository root,
    /// so the default workspace stays on the first commit and the fixture
    /// can be shared by tests running in parallel.
    fn derived_base(
        &self,
        description: &str,
        edit: impl FnOnce(&Path),
    ) -> (CommitIdHex, BaseEntry) {
        let mut repo = self.repo.lock().unwrap_or_else(PoisonError::into_inner);
        let id = WorkspaceId::generate();
        let paths = self.workspace_paths(id);
        CopyBackend.materialize(&self.base, &paths).unwrap();
        // Ignored by the tree's `.gitignore`; `synth::mutate` reads it.
        fs::copy(
            self.repo_root.join(MANIFEST_FILE),
            paths.root.join(MANIFEST_FILE),
        )
        .unwrap();
        let (mut scratch, _) = pollster::block_on(repo.create_workspace(
            &id.jj_workspace_name(),
            &paths.root,
            &paths.state_dir,
            StartPoint::NewOn(self.commit.clone()),
            &reef_lib::jj::OpMeta::new(reef_lib::jj::OpKind::CreateWorkspace, description),
        ))
        .unwrap();
        seed_tree_state(&self.base, &paths).unwrap();
        edit(&paths.root);
        let outcome = pollster::block_on(scratch.snapshot(description)).unwrap();
        assert!(outcome.changed, "{description}: the edit changed nothing");
        assert!(outcome.untracked.is_empty(), "{:?}", outcome.untracked);
        let commit = outcome.wc_commit;
        let tree_id = pollster::block_on(scratch.tree_key(&commit)).unwrap();
        let entry = self
            .cache
            .ensure(&tree_id, |dir, state| {
                pollster::block_on(scratch.materialize_tree(&commit, dir, state))
            })
            .unwrap();
        CopyBackend.tear_down(&paths).unwrap();
        pollster::block_on(repo.reload_at_head()).unwrap();
        (commit, entry)
    }
}

/// Copies the base's `tree_state` over the workspace's, as the daemon's
/// create flow does (R1.05).
pub fn seed_tree_state(base: &BaseEntry, ws: &WorkspacePaths) -> io::Result<u64> {
    fs::copy(base.tree_state_file(), ws.state_dir.join(TREE_STATE_FILE))
}

/// Applies `ops` to a workspace with `synth::mutate`, which needs the
/// generator's manifest: it is copied in for the call and removed after, so
/// the workspace holds only the tree and the mutation.
pub fn mutate_workspace(fx: &BaseFixture, root: &Path, ops: MutateOps) -> Mutation {
    let manifest = root.join(MANIFEST_FILE);
    fs::copy(fx.repo_root.join(MANIFEST_FILE), &manifest).unwrap();
    let mutation = synth::mutate(root, 1, ops).unwrap();
    fs::remove_file(&manifest).unwrap();
    mutation
}

/// What the suite runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct SuiteOptions {
    /// Skip case 12 even on a case-sensitive filesystem.
    pub skip_case_sensitivity: bool,
    /// The name in the report and the timing row, normally the kind.
    pub label: &'static str,
}

/// Measurements the suite records but does not assert (R1.12 compares
/// them).
#[derive(Clone, Debug, Default)]
pub struct Timings {
    /// Case 1's `materialize`.
    pub materialize: Option<Duration>,
    /// Free space before case 1's `materialize` minus free space after.
    pub free_delta_bytes: Option<i64>,
    /// Case 10's `tear_down`.
    pub tear_down: Option<Duration>,
}

/// What [`run_conformance`] found.
#[derive(Clone, Debug, Default)]
pub struct ConformanceReport {
    /// [`SuiteOptions::label`].
    pub label: &'static str,
    /// [`BaseFixture::preset`].
    pub preset: &'static str,
    /// Files and symlinks in the base.
    pub files: u64,
    /// Cases that passed.
    pub passed: Vec<&'static str>,
    /// Cases that could not run here, with the reason.
    pub skipped: Vec<(&'static str, String)>,
    /// Cases that failed, with what was wrong.
    pub failed: Vec<(&'static str, String)>,
    /// What was measured.
    pub timings: Timings,
}

impl ConformanceReport {
    /// The names of the failed cases, in order.
    pub fn failed_cases(&self) -> Vec<&'static str> {
        self.failed.iter().map(|(name, _)| *name).collect()
    }

    /// The timings as a Markdown table with one row, for a PR:
    /// `| kind | preset | files | materialize ms | free delta MiB | tear_down ms |`.
    pub fn timing_table(&self) -> String {
        let ms = |time: Option<Duration>| {
            time.map_or_else(
                || "-".to_owned(),
                |time| format!("{:.1}", time.as_secs_f64() * 1e3),
            )
        };
        #[allow(clippy::cast_precision_loss, reason = "a MiB figure for people")]
        let free = self.timings.free_delta_bytes.map_or_else(
            || "-".to_owned(),
            |bytes| format!("{:.1}", bytes as f64 / MIB as f64),
        );
        format!(
            "| kind | preset | files | materialize ms | free delta MiB | tear_down ms |\n\
             | --- | --- | --- | --- | --- | --- |\n\
             | {} | {} | {} | {} | {} | {} |",
            self.label,
            self.preset,
            self.files,
            ms(self.timings.materialize),
            free,
            ms(self.timings.tear_down),
        )
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{}: {} passed, {} skipped, {} failed",
            self.label,
            self.passed.len(),
            self.skipped.len(),
            self.failed.len()
        )?;
        for (name, reason) in &self.failed {
            writeln!(f, "  failed: {name}: {reason}")?;
        }
        for (name, reason) in &self.skipped {
            writeln!(f, "  skipped: {name}: {reason}")?;
        }
        Ok(())
    }
}

/// Why a case did not pass.
enum Verdict {
    Fail(String),
    Skip(String),
}

type CaseResult = Result<(), Verdict>;

/// One case: the backend, the fixture, a fresh workspace's paths (nothing
/// on disk yet), and the run's shared state.
type Case = fn(&dyn Backend, &BaseFixture, &WorkspacePaths, &mut Run) -> CaseResult;

/// State the cases share within one run.
struct Run {
    opts: SuiteOptions,
    /// The base's digest before case 1.
    digest_before: BaseDigest,
    timings: Timings,
}

/// The cases in the order they run.
const CASES: [(&str, Case); 14] = [
    ("materialize", case_materialize),
    ("mtime_and_size_preserved", case_mtime_and_size_preserved),
    ("symlinks", case_symlinks),
    ("empty_directory", case_empty_directory),
    ("first_snapshot_is_noop", case_first_snapshot_is_noop),
    ("modify_is_private", case_modify_is_private),
    ("disk_usage_tracks_changes", case_disk_usage_tracks_changes),
    ("large_file_modify", case_large_file_modify),
    ("swap_base", case_swap_base),
    ("tear_down", case_tear_down),
    (
        "tear_down_twice_is_not_found",
        case_tear_down_twice_is_not_found,
    ),
    ("case_sensitivity", case_case_sensitivity),
    ("base_immutable_after_all", case_base_immutable_after_all),
    (
        "materialize_into_existing_fails",
        case_materialize_into_existing_fails,
    ),
];

/// Runs every case against `backend`, each on a fresh workspace id, and
/// reports them all. A case that panics fails with the panic's message.
/// Prints `skipped: <case>: <reason>` for each skip as it happens.
pub fn run_conformance(
    backend: &dyn Backend,
    fx: &BaseFixture,
    opts: SuiteOptions,
) -> ConformanceReport {
    let mut run = Run {
        opts,
        digest_before: BaseDigest::of(&fx.base),
        timings: Timings::default(),
    };
    let mut report = ConformanceReport {
        label: opts.label,
        preset: fx.preset,
        files: fx.base.files,
        ..ConformanceReport::default()
    };
    for (name, case) in CASES {
        let ws = fx.workspace_paths(WorkspaceId::generate());
        let result = panic::catch_unwind(AssertUnwindSafe(|| case(backend, fx, &ws, &mut run)))
            .unwrap_or_else(|payload| Err(fail(format!("panicked: {}", panic_message(&*payload)))));
        clean_up(backend, &ws);
        match result {
            Ok(()) => report.passed.push(name),
            Err(Verdict::Skip(reason)) => {
                eprintln!("skipped: {name}: {reason}");
                report.skipped.push((name, reason));
            }
            Err(Verdict::Fail(reason)) => report.failed.push((name, reason)),
        }
    }
    report.timings = run.timings;
    report
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-text panic".to_owned())
}

/// Removes what a case left, through the backend and then by force. After
/// a failed unmount the root is left alone: removing it would write
/// through the mount into the upper directory.
fn clean_up(backend: &dyn Backend, ws: &WorkspacePaths) {
    if fs::symlink_metadata(&ws.root).is_ok() {
        match backend.tear_down(ws) {
            Ok(()) | Err(BackendError::Mount { .. }) => {}
            Err(_) => {
                let _ = fs::remove_dir_all(&ws.root);
            }
        }
    }
    for dir in [&ws.upper, &ws.work, &ws.state_dir] {
        let _ = fs::remove_dir_all(dir);
    }
}

fn fail(reason: impl Into<String>) -> Verdict {
    Verdict::Fail(reason.into())
}

/// `Ok` if `holds`, else a failure with `reason`.
fn check(holds: bool, reason: impl FnOnce() -> String) -> CaseResult {
    if holds { Ok(()) } else { Err(fail(reason())) }
}

/// A backend call's result, its error a failure naming `what`.
fn call<T>(what: &str, result: Result<T, BackendError>) -> Result<T, Verdict> {
    result.map_err(|error| fail(format!("{what}: {}", ErrorChain(&error))))
}

fn materialize(backend: &dyn Backend, base: &BaseEntry, ws: &WorkspacePaths) -> CaseResult {
    call("materialize", backend.materialize(base, ws))
}

/// One entry of a tree, as the suite compares trees: directories by
/// existence only (no backend keeps their mtimes), files by size, mtime,
/// and executable bit, symlinks by target and mtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A directory.
    Dir,
    /// A regular file.
    File {
        /// Its length.
        size: u64,
        /// Its mtime in milliseconds, truncated as jj truncates it.
        mtime_ms: Option<i64>,
        /// Whether any executable bit is set, as jj decides it.
        executable: bool,
    },
    /// A symlink.
    Symlink {
        /// Where it points.
        target: PathBuf,
        /// Its own mtime in milliseconds.
        mtime_ms: Option<i64>,
    },
}

impl Node {
    fn kind(&self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File { .. } => "file",
            Self::Symlink { .. } => "symlink",
        }
    }
}

/// Every entry under `dir` by `/`-separated relative path, without
/// following symlinks.
pub fn listing(dir: &Path) -> BTreeMap<String, Node> {
    WalkDir::new(dir)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let node = if entry.file_type().is_dir() {
                Node::Dir
            } else if entry.file_type().is_symlink() {
                Node::Symlink {
                    target: fs::read_link(entry.path()).unwrap(),
                    mtime_ms: mtime_millis(&metadata),
                }
            } else {
                Node::File {
                    size: metadata.len(),
                    mtime_ms: mtime_millis(&metadata),
                    executable: is_executable(metadata.mode()),
                }
            };
            (relative(dir, entry.path()), node)
        })
        .collect()
}

fn relative(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir).unwrap().to_str().unwrap().to_owned()
}

/// Up to five keys where `got` differs from `want`, and how many in all,
/// or `None` if they are equal.
fn differences<V: Debug + PartialEq>(
    want: &BTreeMap<String, V>,
    got: &BTreeMap<String, V>,
) -> Option<String> {
    let mut lines = Vec::new();
    for (path, value) in want {
        match got.get(path) {
            None => lines.push(format!("{path}: missing (want {value:?})")),
            Some(other) if other != value => {
                lines.push(format!("{path}: {other:?}, want {value:?}"));
            }
            Some(_) => {}
        }
    }
    for (path, value) in got {
        if !want.contains_key(path) {
            lines.push(format!("{path}: unexpected {value:?}"));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut text = format!("{} differences", lines.len());
    for line in lines.iter().take(5) {
        let _ = write!(text, "; {line}");
    }
    Some(text)
}

/// `Ok` if the maps are equal, else a failure naming `what` and the
/// differences.
fn same<V: Debug + PartialEq>(
    what: &str,
    want: &BTreeMap<String, V>,
    got: &BTreeMap<String, V>,
) -> CaseResult {
    differences(want, got).map_or(Ok(()), |text| Err(fail(format!("{what}: {text}"))))
}

/// SHA-256 over a base's tree and its state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BaseDigest {
    tree: Vec<u8>,
    state: Vec<u8>,
}

impl BaseDigest {
    fn of(base: &BaseEntry) -> Self {
        Self {
            tree: tree_digest(&base.path),
            state: tree_digest(&base.state_dir),
        }
    }

    /// `Ok` if `self` (taken earlier) still describes `base`.
    fn still_holds(&self, base: &BaseEntry, when: &str) -> CaseResult {
        let now = Self::of(base);
        check(now.tree == self.tree, || {
            format!("{when}, the base's tree changed")
        })?;
        check(now.state == self.state, || {
            format!("{when}, the base's state directory changed")
        })
    }
}

/// SHA-256 over every entry under `dir` in path order: each entry's path
/// and type, then for files their size, mtime, mode, and contents, for
/// symlinks their size, mtime, and target. Directory mtimes are left out
/// (backends do not keep them).
fn tree_digest(dir: &Path) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for entry in WalkDir::new(dir)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.unwrap();
        let metadata = entry.metadata().unwrap();
        hasher.update(relative(dir, entry.path()).as_bytes());
        hasher.update(b"\0");
        if entry.file_type().is_dir() {
            hasher.update(b"dir\0");
            continue;
        }
        let mtime = mtime_millis(&metadata).unwrap_or(i64::MIN);
        let header = |kind: &str| format!("{kind}\0{}\0{mtime}\0", metadata.len());
        if entry.file_type().is_symlink() {
            hasher.update(header("symlink").as_bytes());
            hasher.update(
                fs::read_link(entry.path())
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
            );
        } else {
            hasher.update(header("file").as_bytes());
            hasher.update(format!("{:o}\0", metadata.mode() & 0o7777).as_bytes());
            hasher.update(fs::read(entry.path()).unwrap());
        }
    }
    hasher.finalize().to_vec()
}

// 1
fn case_materialize(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    run: &mut Run,
) -> CaseResult {
    let free_before = free_bytes(fx.state.path()).ok();
    let begun = Instant::now();
    materialize(backend, &fx.base, ws)?;
    run.timings.materialize = Some(begun.elapsed());
    run.timings.free_delta_bytes = free_before
        .zip(free_bytes(fx.state.path()).ok())
        .map(|(before, after)| i64::try_from(before).unwrap() - i64::try_from(after).unwrap());

    let manifest = &fx.manifest;
    let missing: Vec<_> = manifest
        .text_files
        .iter()
        .chain(&manifest.binaries)
        .chain(&manifest.symlink_targets)
        .map(PathBuf::as_path)
        .chain([Path::new(".gitignore")])
        .filter(|path| fs::symlink_metadata(ws.root.join(path)).is_err())
        .collect();
    check(missing.is_empty(), || {
        format!(
            "{} manifest paths missing, such as {:?}",
            missing.len(),
            &missing[..1]
        )
    })?;
    let kinds = |dir: &Path| -> BTreeMap<String, &'static str> {
        listing(dir)
            .into_iter()
            .map(|(path, node)| (path, node.kind()))
            .collect()
    };
    same("paths and types", &kinds(&fx.base.path), &kinds(&ws.root))
}

// 2
fn case_mtime_and_size_preserved(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let files = |dir: &Path| -> BTreeMap<String, Node> {
        listing(dir)
            .into_iter()
            .filter(|(_, node)| matches!(node, Node::File { .. }))
            .collect()
    };
    let want = files(&fx.base.path);
    check(
        want.values().any(|node| {
            matches!(
                node,
                Node::File {
                    executable: true,
                    ..
                }
            )
        }),
        || "the base has no executable file, so the mode check proves nothing".to_owned(),
    )?;
    same(
        "regular files (size, mtime, executable)",
        &want,
        &files(&ws.root),
    )
}

// 3
fn case_symlinks(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let links = |dir: &Path| -> BTreeMap<String, PathBuf> {
        listing(dir)
            .into_iter()
            .filter_map(|(path, node)| match node {
                Node::Symlink { target, .. } => Some((path, target)),
                Node::Dir | Node::File { .. } => None,
            })
            .collect()
    };
    let want = links(&fx.base.path);
    check(want.len() as u64 == fx.manifest.symlinks + 1, || {
        format!(
            "the base has {} symlinks, want the manifest's {} and {DANGLING}",
            want.len(),
            fx.manifest.symlinks
        )
    })?;
    same("symlinks", &want, &links(&ws.root))?;
    let dangling = ws.root.join(DANGLING);
    check(
        fs::read_link(&dangling).is_ok_and(|target| target == Path::new(DANGLING_TARGET))
            && !dangling.exists(),
        || format!("{DANGLING} is not a dangling symlink to {DANGLING_TARGET}"),
    )
}

// 4
fn case_empty_directory(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let dir = ws.root.join(EMPTY_DIR);
    check(
        fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()),
        || format!("{EMPTY_DIR}/ is not a directory in the workspace"),
    )?;
    check(fs::read_dir(&dir).unwrap().next().is_none(), || {
        format!("{EMPTY_DIR}/ is not empty")
    })
}

// 5
/// The daemon's create flow (R1.05): a jj workspace editing the base's
/// commit at the materialized root, its `tree_state` seeded from the base,
/// then a snapshot. `changed == false` alone does not prove the scan was a
/// no-op: jj rehashes a file whose mtime or size differs from its state,
/// finds the same content, and still reports no change. So the case also
/// requires the state jj saved after the scan to equal the base's, which
/// holds only if jj had nothing to refresh.
fn case_first_snapshot_is_noop(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let mut repo = fx.repo.lock().unwrap_or_else(PoisonError::into_inner);
    let (mut workspace, _) = pollster::block_on(repo.create_workspace(
        &ws.id.jj_workspace_name(),
        &ws.root,
        &ws.state_dir,
        StartPoint::Edit(fx.commit.clone()),
        &reef_lib::jj::OpMeta::new(
            reef_lib::jj::OpKind::CreateWorkspace,
            "conformance: first snapshot",
        ),
    ))
    .map_err(|error| fail(format!("create the jj workspace: {}", ErrorChain(&error))))?;
    drop(repo);
    seed_tree_state(&fx.base, ws).unwrap();
    let outcome = pollster::block_on(workspace.snapshot("conformance: first snapshot"))
        .map_err(|error| fail(format!("snapshot: {}", ErrorChain(&error))))?;
    check(!outcome.changed, || {
        "the first snapshot recorded a change".to_owned()
    })?;
    check(outcome.untracked.is_empty(), || {
        format!("the snapshot left files untracked: {:?}", outcome.untracked)
    })?;
    let state = |dir: &Path| -> BTreeMap<String, (i64, u64)> {
        tree_state_entries(dir)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.path, (entry.mtime_ms, entry.size)))
            .collect()
    };
    same(
        "the working copy's state after the snapshot (mtime ms, size) against the base's \
         (each difference is a file jj had to rehash)",
        &state(&fx.base.state_dir),
        &state(&ws.state_dir),
    )
}

// 6
fn case_modify_is_private(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let before = BaseDigest::of(&fx.base);
    mutate_workspace(fx, &ws.root, MUTATION);
    before.still_holds(&fx.base, "after modifying the workspace")
}

// 7
fn case_disk_usage_tracks_changes(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let usage = |when: &str| -> Result<DiskUsage, Verdict> {
        call(
            &format!("disk_usage {when}"),
            backend.disk_usage(&fx.base, ws),
        )
    };
    let fresh = usage("of a fresh workspace")?;
    check(fresh.files_changed == 0, || {
        format!("a fresh workspace reports {fresh:?}, want files_changed 0")
    })?;

    let mutation = mutate_workspace(fx, &ws.root, MUTATION);
    let added_bytes: u64 = mutation
        .added
        .iter()
        .map(|path| fs::metadata(ws.root.join(path)).unwrap().len())
        .sum();
    let modified = usage("after the mutation")?;
    let at_least = u64::from(MUTATION.modify + MUTATION.add);
    check(modified.files_changed >= at_least, || {
        format!("after modifying and adding {at_least} files: {modified:?}")
    })?;
    check(modified.allocated_bytes >= added_bytes, || {
        format!("after adding {added_bytes} bytes of files: {modified:?}")
    })?;

    let binary = ws.root.join(&fx.manifest.binaries[0]);
    let mut file = fs::OpenOptions::new().append(true).open(&binary).unwrap();
    file.write_all(&vec![0xa5; usize::try_from(4 * MIB).unwrap()])
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    let grown = usage("after appending 4 MiB")?;
    check(
        grown.allocated_bytes >= modified.allocated_bytes + 4 * MIB,
        || format!("appending 4 MiB went from {modified:?} to {grown:?}"),
    )
}

// 8
fn case_large_file_modify(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    let binary = &fx.manifest.binaries[0];
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(ws.root.join(binary))
        .unwrap();
    file.write_all(b"x").unwrap();
    drop(file);
    let size = fx.manifest.spec.binary_size;
    let ws_len = fs::metadata(ws.root.join(binary)).unwrap().len();
    let base_len = fs::metadata(fx.base.path.join(binary)).unwrap().len();
    check(ws_len == size + 1, || {
        format!(
            "the workspace's {} is {ws_len} bytes, want {}",
            binary.display(),
            size + 1
        )
    })?;
    check(base_len == size, || {
        format!(
            "the base's {} is {base_len} bytes, want {size}",
            binary.display()
        )
    })
}

// 9
/// Both outcomes are legal; each is held to its promise. `TreeReplaced`:
/// the root is the second base exactly. `NeedsCheckout`: the directory is
/// untouched (the daemon's check-out is not the backend's job), which is
/// compared against the root as it was just before the swap.
fn case_swap_base(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    let (_, second) = fx.swap_target();
    materialize(backend, &fx.base, ws)?;
    let written = ws.root.join("written-before-swap.txt");
    fs::write(&written, "an agent's edit\n").unwrap();
    let before = listing(&ws.root);
    match call("swap_base", backend.swap_base(ws, &fx.base, second))? {
        SwapOutcome::TreeReplaced => {
            same(
                "the root after TreeReplaced against the second base",
                &listing(&second.path),
                &listing(&ws.root),
            )?;
            check(!written.exists(), || {
                "TreeReplaced kept a file written before the swap".to_owned()
            })
        }
        SwapOutcome::NeedsCheckout => {
            same(
                "the root after NeedsCheckout against the root before it",
                &before,
                &listing(&ws.root),
            )?;
            check(written.is_file(), || {
                "NeedsCheckout lost a file written before the swap".to_owned()
            })
        }
    }
}

// 10
fn case_tear_down(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    run: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    // Under a runtime, as in the daemon, so the deletion goes to the
    // blocking pool and the timing is what a release waits for.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let begun = Instant::now();
    call("tear_down", backend.tear_down(ws))?;
    run.timings.tear_down = Some(begun.elapsed());
    check(fs::symlink_metadata(&ws.root).is_err(), || {
        "the root exists after tear_down".to_owned()
    })?;
    // `<id>.trash-*` while a background deletion runs, and an overlay's
    // `<id>.upper` and `<id>.work`.
    let parent = ws.root.parent().unwrap();
    let prefix = format!("{}.", ws.id);
    let leftovers = || -> Vec<String> {
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&prefix))
            .collect()
    };
    let deadline = Instant::now() + TRASH_WAIT;
    while !leftovers().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = leftovers();
    check(left.is_empty(), || {
        format!("{left:?} still exist {TRASH_WAIT:?} after tear_down")
    })?;
    run.digest_before
        .still_holds(&fx.base, "after a workspace was torn down")
}

// 11
fn case_tear_down_twice_is_not_found(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    materialize(backend, &fx.base, ws)?;
    call("the first tear_down", backend.tear_down(ws))?;
    match backend.tear_down(ws) {
        Err(BackendError::RootMissing(_)) => Ok(()),
        Err(error) => Err(fail(format!(
            "the second tear_down: {} ({:?}), want RootMissing (NotFound)",
            ErrorChain(&error),
            error.kind()
        ))),
        Ok(()) => Err(fail("the second tear_down succeeded, want RootMissing")),
    }
}

// 12
fn case_case_sensitivity(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    run: &mut Run,
) -> CaseResult {
    if run.opts.skip_case_sensitivity {
        return Err(Verdict::Skip("disabled by the caller".to_owned()));
    }
    if !is_case_sensitive(fx.state.path()) {
        return Err(Verdict::Skip(
            "the state directory's filesystem is case-insensitive".to_owned(),
        ));
    }
    let (_, base) = fx.case_base();
    materialize(backend, base, ws)?;
    for (name, contents) in [("A.txt", "upper\n"), ("a.txt", "lower\n")] {
        let got = fs::read_to_string(ws.root.join(name)).ok();
        check(got.as_deref() == Some(contents), || {
            format!("{name} holds {got:?}, want {contents:?}")
        })?;
    }
    Ok(())
}

/// Whether names under `dir` are case-sensitive: creates `A` in a scratch
/// directory and looks for `a`.
pub fn is_case_sensitive(dir: &Path) -> bool {
    let scratch = tempfile::tempdir_in(dir).unwrap();
    fs::write(scratch.path().join("A"), "").unwrap();
    fs::symlink_metadata(scratch.path().join("a")).is_err()
}

// 13
fn case_base_immutable_after_all(
    _: &dyn Backend,
    fx: &BaseFixture,
    _: &WorkspacePaths,
    run: &mut Run,
) -> CaseResult {
    run.digest_before.still_holds(&fx.base, "after every case")
}

// 14
fn case_materialize_into_existing_fails(
    backend: &dyn Backend,
    fx: &BaseFixture,
    ws: &WorkspacePaths,
    _: &mut Run,
) -> CaseResult {
    fs::create_dir_all(&ws.root).unwrap();
    let occupant = ws.root.join("occupied.txt");
    fs::write(&occupant, "already here\n").unwrap();
    let result = backend.materialize(&fx.base, ws);
    let contents = fs::read_to_string(&occupant).ok();
    let entries = fs::read_dir(&ws.root).map_or(0, Iterator::count);
    fs::remove_dir_all(&ws.root).unwrap();
    match result {
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(fail(format!(
            "materialize into an existing root: {} ({:?}), want AlreadyExists",
            ErrorChain(&error),
            error.kind()
        ))),
        Ok(()) => Err(fail("materialize into an existing root succeeded")),
    }?;
    check(contents.as_deref() == Some("already here\n"), || {
        format!("the existing file now holds {contents:?}")
    })?;
    check(entries == 1, || {
        format!("the existing root now has {entries} entries, want 1")
    })
}

/// A copy backend that does not restore mtimes, to prove the suite notices
/// (R1.11's sensitivity check): every file and symlink gets the time it was
/// written, everything else is right.
#[cfg(test)]
pub struct BrokenCopy;

#[cfg(test)]
impl Backend for BrokenCopy {
    fn kind(&self) -> BackendKind {
        BackendKind::Copy
    }

    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError> {
        if fs::symlink_metadata(&ws.root).is_ok() {
            return Err(BackendError::RootExists(ws.root.clone()));
        }
        let io_error = |source| BackendError::Io {
            op: "broken copy",
            path: ws.root.clone(),
            source,
        };
        fs::create_dir_all(&ws.root).map_err(io_error)?;
        for entry in WalkDir::new(&base.path).min_depth(1).follow_links(false) {
            let entry = entry.map_err(|error| io_error(error.into()))?;
            let to = ws.root.join(entry.path().strip_prefix(&base.path).unwrap());
            if entry.file_type().is_dir() {
                fs::create_dir_all(&to).map_err(io_error)?;
            } else if entry.file_type().is_symlink() {
                let target = fs::read_link(entry.path()).map_err(io_error)?;
                std::os::unix::fs::symlink(target, &to).map_err(io_error)?;
            } else {
                // A byte copy, not `fs::copy`: on macOS that clones, which
                // keeps the mtime.
                let mut from = fs::File::open(entry.path()).map_err(io_error)?;
                let mut file = fs::File::create(&to).map_err(io_error)?;
                io::copy(&mut from, &mut file).map_err(io_error)?;
                let mode = entry
                    .metadata()
                    .map_err(|error| io_error(error.into()))?
                    .mode();
                fs::set_permissions(&to, fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(io_error)?;
            }
        }
        Ok(())
    }

    fn disk_usage(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        CopyBackend.disk_usage(base, ws)
    }

    fn swap_base(
        &self,
        _: &WorkspacePaths,
        _: &BaseEntry,
        _: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        Ok(SwapOutcome::NeedsCheckout)
    }

    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        CopyBackend.tear_down(ws)
    }
}
