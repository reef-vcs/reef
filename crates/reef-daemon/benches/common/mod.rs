//! The workspace benchmarks' fixture and base (R1.12).
//!
//! The fixture is the same synthetic tree with a jj repository around it as
//! `reef-lib`'s snapshot benchmarks build (`crates/reef-lib/benches/common`),
//! under `target/bench-fixtures/<preset>-42/` with the same `.ready` stamp,
//! so whichever bench runs first builds it and the other reuses it. The
//! builder is duplicated rather than shared because it needs jj and the
//! generator, which `reef-testing` cannot link: keep [`SEED`],
//! [`FIXTURE_VERSION`], the stamp, and the reuse rules identical to
//! `reef-lib`'s.
//!
//! On top of it, [`BenchBase`] is a base cache entry for the fixture's
//! working-copy commit in a temporary state directory under
//! `target/bench-state/`, laid out as the daemon's
//! (`repos/<repo-id>/bases`, `ws`, `wc`), and [`available_kinds`] the
//! backends whose probe says they can serve it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use jj_lib::settings::UserSettings;
use reef_daemon::backend::{
    Availability, BackendKind, BaseCache, BaseEntry, OverlayConfig, WorkspacePaths, probe,
};
use reef_daemon::config::WatcherBackend;
use reef_daemon::db::Db;
use reef_daemon::model::{RepoId, WorkspaceId};
use reef_lib::error::ErrorChain;
use reef_lib::jj::Repo;
use reef_lib::jj::settings::{Identity, user_settings_with_max_new_file_size};
use reef_testing::synth::{self, GENERATOR_VERSION, Preset, Spec};
use tempfile::TempDir;

/// The seed every fixture is generated from, as in `just synth`.
const SEED: u64 = 42;

/// Bumped when the way a fixture is built changes, so old fixtures are
/// rebuilt rather than reused. Must equal `reef-lib`'s.
const FIXTURE_VERSION: u32 = 1;

/// Above the generator's 8 MiB binaries, so they are tracked rather than
/// reported as untracked.
const MAX_NEW_FILE_SIZE: u64 = 16 << 20;

/// A synthetic tree with a jj repository whose working copy is snapshotted.
pub(crate) struct Fixture {
    /// The tree's root, which is also the jj workspace root.
    pub(crate) root: PathBuf,
    /// The preset the tree was generated from.
    pub(crate) preset: Preset,
    /// Files and symlinks a snapshot tracks: text files, binaries, symlinks,
    /// and the root `.gitignore`.
    pub(crate) files: u64,
}

impl Fixture {
    /// The preset's name, as `REEF_BENCH_PRESET` spells it.
    pub(crate) fn preset_name(&self) -> &'static str {
        preset_name(self.preset)
    }

    /// The generator's manifest in the tree, which `synth::mutate` reads.
    /// The tree's `.gitignore` ignores it, so workspaces do not have it.
    pub(crate) fn manifest(&self) -> PathBuf {
        self.root.join(synth::MANIFEST_FILE)
    }
}

/// The preset `REEF_BENCH_PRESET` names: `small`, `medium` (the default), or
/// `large`.
pub(crate) fn preset() -> Preset {
    match std::env::var("REEF_BENCH_PRESET").as_deref() {
        Ok("small") => Preset::Small,
        Ok("medium") | Err(std::env::VarError::NotPresent) => Preset::Medium,
        Ok("large") => Preset::Large,
        other => panic!("REEF_BENCH_PRESET must be small, medium, or large, not {other:?}"),
    }
}

/// The watcher `REEF_BENCH_WATCHER` names for `create_api`'s daemon:
/// `notify`, `watchman`, or unset for `auto` (R1.29).
pub(crate) fn watcher() -> WatcherBackend {
    match std::env::var("REEF_BENCH_WATCHER").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("auto") => WatcherBackend::Auto,
        Ok("notify") => WatcherBackend::Notify,
        Ok("watchman") => WatcherBackend::Watchman,
        other => panic!("REEF_BENCH_WATCHER must be notify, watchman, or auto, not {other:?}"),
    }
}

/// The fixture for [`preset`], built on first use in this process.
pub(crate) fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| build(preset()))
}

/// A runtime to drive `reef_lib::jj` futures on. Current-thread is enough:
/// jj-lib's futures do blocking I/O and are not `Send`.
pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build the tokio runtime")
}

/// Settings for the fixture's repository.
pub(crate) fn settings() -> UserSettings {
    user_settings_with_max_new_file_size(
        &Identity {
            name: "Reef Bench".into(),
            email: "bench@reef.invalid".into(),
        },
        MAX_NEW_FILE_SIZE,
    )
    .expect("bench settings")
}

/// Cargo's target directory, wherever `CARGO_TARGET_DIR` or `--target-dir`
/// put it: the parent of the `tmp` directory cargo names for bench targets.
pub(crate) fn target_dir() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    tmp.parent().unwrap_or(tmp).to_owned()
}

/// Whether `REEF_BENCH_PRIVILEGED=1`: the run is root, for the kernel
/// overlayfs tier, and benchmarks only that backend.
pub(crate) fn privileged() -> bool {
    std::env::var_os("REEF_BENCH_PRIVILEGED").is_some_and(|value| value == "1")
}

/// A base cache entry for the fixture's working-copy commit, built on
/// first use, in a temporary state directory under `target/bench-state/`
/// that is removed when this is dropped. Lazy, so `--list` and filters
/// that select no backend benchmark do not pay for the base.
pub(crate) struct BenchBase {
    fixture: &'static Fixture,
    entry: OnceLock<BaseEntry>,
    /// `<state>/repos/<repo-id>`.
    pub(crate) repo_dir: PathBuf,
    // Dropped after `cache`, which holds the database open inside it.
    cache: BaseCache,
    _state: TempDir,
}

impl BenchBase {
    /// The state directory for `fixture`'s base, laid out as the daemon's,
    /// with nothing materialized yet.
    pub(crate) fn new(fixture: &'static Fixture) -> BenchBase {
        let parent = target_dir().join("bench-state");
        fs::create_dir_all(&parent).expect("create target/bench-state");
        let state = tempfile::Builder::new()
            .prefix(&format!("{}-", fixture.preset_name()))
            .tempdir_in(&parent)
            .expect("create the bench state directory");
        let repo_id = RepoId::generate();
        let repo_dir = state.path().join("repos").join(repo_id.as_hex());
        for dir in ["ws", "wc"] {
            fs::create_dir_all(repo_dir.join(dir)).expect("create the workspace directories");
        }
        let db = Arc::new(
            Db::open(&state.path().join("reef.sqlite3")).expect("open the bench database"),
        );
        let cache = BaseCache::new(db, &repo_dir, repo_id);
        BenchBase {
            fixture,
            entry: OnceLock::new(),
            repo_dir,
            cache,
            _state: state,
        }
    }

    /// The entry every benchmark materializes from, built on the first
    /// call.
    pub(crate) fn entry(&self) -> &BaseEntry {
        self.entry.get_or_init(|| self.materialize())
    }

    /// The directory workspace roots go in, `<repo_dir>/ws`, which the
    /// backends are probed for and free space is measured on.
    pub(crate) fn ws_dir(&self) -> PathBuf {
        self.repo_dir.join("ws")
    }

    /// The base cache directory, `<repo_dir>/bases`.
    pub(crate) fn bases_dir(&self) -> PathBuf {
        self.repo_dir.join("bases")
    }

    /// Paths for workspace number `n`: sequential ids
    /// (`0000000000000001`, ...) under the daemon's layout.
    pub(crate) fn workspace(&self, n: u64) -> WorkspacePaths {
        let id: WorkspaceId = format!("{n:016x}").parse().expect("a workspace id");
        WorkspacePaths::under(&self.repo_dir, id)
    }

    /// Materializes the fixture's working-copy commit into the base cache
    /// and prints how long it took. Not benchmarked: it is the one O(tree)
    /// step every workspace shares.
    #[allow(
        clippy::print_stderr,
        reason = "the base's build time is reported to whoever runs the benches"
    )]
    fn materialize(&self) -> BaseEntry {
        let fixture = self.fixture;
        let rt = runtime();
        let repo = rt
            .block_on(Repo::open(&fixture.root, &settings()))
            .expect("open the fixture repository");
        let commit = repo
            .wc_change()
            .expect("read the fixture's working-copy commit")
            .commit_id;
        let tree_id = rt
            .block_on(repo.tree_key(&commit))
            .expect("the fixture commit's tree");
        let started = Instant::now();
        let entry = self
            .cache
            .ensure(&tree_id, |dir, state_dir| {
                rt.block_on(repo.materialize_tree(&commit, dir, state_dir))
            })
            .expect("materialize the bench base");
        eprintln!(
            "bench base: materialized {} ({} files, {} bytes) in {:.1} s",
            fixture.preset_name(),
            entry.files,
            entry.bytes,
            started.elapsed().as_secs_f64()
        );
        entry
    }
}

/// The backends to benchmark on `base`: every kind whose probe reports it
/// available for the workspace directory, in `auto` order, each
/// unavailable one printed with its reason. With [`privileged`], only
/// kernel overlayfs, and a panic if it is unavailable.
#[allow(
    clippy::print_stderr,
    reason = "skipped backends are reported to whoever runs the benches"
)]
pub(crate) fn available_kinds(base: &BenchBase) -> Vec<BackendKind> {
    let probes = probe(
        &base.ws_dir(),
        Some(&base.bases_dir()),
        &OverlayConfig::default(),
    );
    let mut kinds = Vec::new();
    for probe in probes {
        match probe.availability {
            Availability::Available => kinds.push(probe.kind),
            Availability::Unavailable(reason)
                if privileged() && probe.kind == BackendKind::Overlayfs =>
            {
                panic!("REEF_BENCH_PRIVILEGED=1 but overlayfs is unavailable: {reason}")
            }
            Availability::Unavailable(reason) => {
                eprintln!("bench backends: skipped {}: {reason}", probe.kind);
            }
        }
    }
    if privileged() {
        kinds.retain(|kind| *kind == BackendKind::Overlayfs);
    }
    kinds
}

fn preset_name(preset: Preset) -> &'static str {
    match preset {
        Preset::Small => "small",
        Preset::Medium => "medium",
        Preset::Large => "large",
        Preset::Xl => "xl",
    }
}

#[allow(
    clippy::print_stderr,
    reason = "fixture build times are reported to whoever runs the benches"
)]
fn build(preset: Preset) -> Fixture {
    let name = preset_name(preset);
    let spec = Spec::preset(preset);
    let dir = target_dir().join("bench-fixtures");
    let root = dir.join(format!("{name}-{SEED}"));
    // A sibling of the root, so the snapshot does not see it. Written last:
    // a fixture without it is incomplete and is rebuilt from scratch.
    let ready = dir.join(format!("{name}-{SEED}.ready"));
    let stamp = format!(
        "generator {GENERATOR_VERSION}, fixture {FIXTURE_VERSION}, spec {}\n",
        serde_json::to_string(&spec).expect("encode the spec")
    );
    let rt = runtime();

    let reusable =
        fs::read_to_string(&ready).is_ok_and(|text| text == stamp) && root.join(".jj").is_dir();
    // An interrupted run can leave mutations that were never snapshotted;
    // record them now so every benchmark starts from a clean tree. If that
    // fails (a run killed between publishing an operation and saving the
    // working copy leaves it stale), the fixture is rebuilt.
    let reused = reusable
        && clean_up(&root, &rt)
            .inspect_err(|error| eprintln!("bench fixture: rebuilding {name}: {error}"))
            .is_ok();
    if !reused {
        remove(&ready);
        // A mutated or half-built tree is regenerated, and its `.jj` with it.
        match fs::remove_dir_all(&root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                panic!("remove {}: {error}", root.display())
            }
            _ => {}
        }
        let started = Instant::now();
        let manifest = synth::generate(&root, &spec, SEED, false).expect("generate the tree");
        eprintln!(
            "bench fixture: generated {name} ({} text files, {} bytes) in {:.1} s",
            manifest.files,
            manifest.bytes,
            started.elapsed().as_secs_f64()
        );
        let mut repo = rt
            .block_on(Repo::init_internal_git(&root, &settings()))
            .expect("create the fixture repository");
        let started = Instant::now();
        let outcome = rt
            .block_on(repo.snapshot("bench: first snapshot"))
            .expect("first snapshot of the fixture");
        eprintln!(
            "bench fixture: first snapshot of {name} took {:.1} s",
            started.elapsed().as_secs_f64()
        );
        assert!(outcome.changed, "the first snapshot found nothing");
        assert!(
            outcome.untracked.is_empty(),
            "the first snapshot left files untracked: {:?}",
            outcome.untracked
        );
        fs::write(&ready, &stamp).expect("mark the fixture ready");
    }

    let manifest = synth::load_manifest(&root)
        .expect("read the fixture manifest")
        .expect("the fixture has a manifest");
    let files = manifest.files + manifest.binaries.len() as u64 + manifest.symlinks + 1;
    Fixture {
        root,
        preset,
        files,
    }
}

/// Opens a reused fixture and snapshots whatever an earlier run left.
fn clean_up(root: &Path, rt: &tokio::runtime::Runtime) -> Result<(), String> {
    rt.block_on(async {
        let mut repo = Repo::open(root, &settings())
            .await
            .map_err(|error| ErrorChain(&error).to_string())?;
        repo.snapshot("bench: clean up after an earlier run")
            .await
            .map_err(|error| ErrorChain(&error).to_string())?;
        Ok(())
    })
}

fn remove(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            panic!("remove {}: {error}", path.display())
        }
        _ => {}
    }
}
