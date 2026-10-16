//! The benchmarks' fixture: a synthetic tree from `reef_testing::synth` with
//! a jj repository around it, built under `target/bench-fixtures/` and reused
//! across benchmark functions and across runs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use jj_lib::settings::UserSettings;
use reef_lib::error::ErrorChain;
use reef_lib::jj::Repo;
use reef_lib::jj::settings::{Identity, user_settings_with_max_new_file_size};
use reef_testing::synth::{self, GENERATOR_VERSION, Preset, Spec};

/// The seed every fixture is generated from, as in `just synth`.
const SEED: u64 = 42;

/// Bumped when the way a fixture is built changes, so old fixtures are
/// rebuilt rather than reused.
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
    /// The seed the tree was generated from.
    pub(crate) seed: u64,
    /// Files and symlinks a snapshot tracks: text files, binaries, symlinks,
    /// and the root `.gitignore`.
    pub(crate) files: u64,
    /// Next seed for [`Fixture::mutate`]. Starts at the tree's mutation
    /// count, so seeds do not repeat across runs on a reused fixture.
    next_seed: AtomicU64,
}

impl Fixture {
    /// The preset's name, as `REEF_BENCH_PRESET` spells it.
    pub(crate) fn preset_name(&self) -> &'static str {
        preset_name(self.preset)
    }

    /// Applies `ops` to the tree with a seed not used before on it.
    pub(crate) fn mutate(&self, ops: synth::MutateOps) {
        let seed = self.next_seed.fetch_add(1, Ordering::Relaxed);
        synth::mutate(&self.root, seed, ops).expect("mutate the fixture");
    }

    /// Opens the fixture's repository at its head operation.
    pub(crate) fn open(&self, rt: &tokio::runtime::Runtime) -> Repo {
        open_root(&self.root, rt)
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
        seed: SEED,
        files,
        next_seed: AtomicU64::new(u64::from(manifest.mutations)),
    }
}

fn open_root(root: &Path, rt: &tokio::runtime::Runtime) -> Repo {
    rt.block_on(Repo::open(root, &settings()))
        .expect("open the fixture repository")
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

/// Cargo's target directory, wherever `CARGO_TARGET_DIR` or `--target-dir`
/// put it: the parent of the `tmp` directory cargo names for bench targets.
fn target_dir() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    tmp.parent().unwrap_or(tmp).to_owned()
}

fn remove(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            panic!("remove {}: {error}", path.display())
        }
        _ => {}
    }
}
