//! Deterministic synthetic source trees for benchmarks and stress tests.
//!
//! [`generate`] writes a tree shaped like a real repository (nested
//! directories, pseudo-source text files with a log-normal size
//! distribution, a few large binaries, ignored build directories, symlinks,
//! and executables) and records it in a [`Manifest`] at
//! `<root>/.reef-synth.json`. [`mutate`] then changes a chosen number of
//! files so a snapshot has something to find.
//!
//! The same [`Spec`] and seed produce byte-identical trees on every
//! platform: all randomness comes from one `ChaCha8` stream drawn in a fixed
//! order, and the arithmetic avoids anything platform-dependent (see
//! `dist`). When the output for a given spec and seed changes, bump
//! [`GENERATOR_VERSION`].
//!
//! No version control is set up here; `reef-testing` cannot depend on
//! jj-lib. Callers initialize the repository around the tree.

mod content;
mod dist;
mod names;
mod tree;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use dist::{SynthRng, below, index, log_normal, sample};

/// Bumped whenever the tree generated for a given `(Spec, seed)` changes, so
/// a stale tree is never mistaken for a current one.
pub const GENERATOR_VERSION: u32 = 1;

/// The manifest's file name, directly under the root.
pub const MANIFEST_FILE: &str = ".reef-synth.json";

/// Present while a tree is being cleared or written; a tree with this file
/// is incomplete and is regenerated even if a manifest exists.
const PARTIAL_FILE: &str = ".reef-synth.partial";

/// Names of the ignored directories, in the order they are placed.
const IGNORED_DIRS: [&str; 2] = ["target", "node_modules"];

/// The root `.gitignore`. Keeps the ignored directories and the manifest out
/// of any snapshot.
const GITIGNORE: &str = "# Written by reef-synth.\ntarget/\nnode_modules/\n.reef-synth.json\n";

/// Shape parameter of the log-normal text file size distribution. With
/// sigma 1 the mean is about 1.65 times the median.
const SIZE_SIGMA: f64 = 1.0;

/// What to generate. Counts are exact; `fan_out` and `files_per_dir` are
/// means.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    /// Text files outside ignored directories. Binaries, ignored files,
    /// symlinks, `.gitignore`, and the manifest are all in addition.
    pub files: u64,
    /// Deepest directory level below the root (the root is depth 0).
    /// Ignored directories may sit one level deeper.
    pub max_depth: u8,
    /// Mean number of subdirectories per directory above `max_depth`.
    pub fan_out: f64,
    /// Mean number of text files per directory. Together with `files` this
    /// sets the number of directories.
    pub files_per_dir: f64,
    /// Median text file size in bytes.
    pub median_size: u64,
    /// Largest text file size in bytes.
    pub max_text_size: u64,
    /// Number of large incompressible files (`.bin`, `.png`, `.wasm`).
    pub binaries: u32,
    /// Size of each binary in bytes.
    pub binary_size: u64,
    /// Files inside ignored directories, as a fraction of `files`.
    pub ignored_fraction: f64,
    /// Relative symlinks to text files. Not written on Windows.
    pub symlinks: u32,
    /// Text files marked executable (mode 0755 on Unix), as a fraction of
    /// `files`.
    pub executable_fraction: f64,
}

/// Named specs, so tickets and benches can say "the large preset".
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Preset {
    /// 1,000 files, depth 4.
    Small,
    /// 10,000 files, depth 5.
    Medium,
    /// 100,000 files, depth 6.
    Large,
    /// 1,000,000 files, depth 7. Opt-in only; not used in CI.
    Xl,
}

impl Spec {
    /// The spec for a preset. Fan-out and files per directory are chosen so
    /// the breadth-first directory plan reaches about `max_depth`.
    pub fn preset(name: Preset) -> Spec {
        let (files, max_depth, fan_out, files_per_dir) = match name {
            Preset::Small => (1_000, 4, 3.0, 8.0),
            Preset::Medium => (10_000, 5, 3.8, 10.0),
            Preset::Large => (100_000, 6, 4.3, 12.0),
            Preset::Xl => (1_000_000, 7, 4.8, 14.0),
        };
        Spec {
            files,
            max_depth,
            fan_out,
            files_per_dir,
            median_size: 2 * 1024,
            max_text_size: 256 * 1024,
            binaries: 3,
            binary_size: 8 * 1024 * 1024,
            ignored_fraction: 0.02,
            symlinks: 10,
            executable_fraction: 0.01,
        }
    }

    /// Checks the parameters that would otherwise loop forever, exhaust
    /// memory, or produce nonsense. The upper bounds are far beyond any
    /// preset; they exist so a typo is an error rather than a hang.
    pub fn validate(&self) -> Result<(), SynthError> {
        const GIB: u64 = 1024 * 1024 * 1024;
        let invalid = |reason: &str| Err(SynthError::InvalidSpec(reason.to_owned()));
        if self.files > 100_000_000 {
            return invalid("files must be at most 100,000,000");
        }
        if !(0.1..=1e6).contains(&self.fan_out) {
            return invalid("fan_out must be between 0.1 and 1e6");
        }
        if !(0.1..=1e6).contains(&self.files_per_dir) {
            return invalid("files_per_dir must be between 0.1 and 1e6");
        }
        if self.median_size == 0 {
            return invalid("median_size must be at least 1");
        }
        if self.max_text_size < self.median_size {
            return invalid("max_text_size must be at least median_size");
        }
        if self.max_text_size > GIB {
            return invalid("max_text_size must be at most 1 GiB");
        }
        if self.binaries > 10_000 {
            return invalid("binaries must be at most 10,000");
        }
        if self.binary_size > 4 * GIB {
            return invalid("binary_size must be at most 4 GiB");
        }
        if self.symlinks > 1_000_000 {
            return invalid("symlinks must be at most 1,000,000");
        }
        if !(0.0..=1.0).contains(&self.ignored_fraction) {
            return invalid("ignored_fraction must be between 0 and 1");
        }
        if !(0.0..=1.0).contains(&self.executable_fraction) {
            return invalid("executable_fraction must be between 0 and 1");
        }
        if self.symlinks > 0 && self.files == 0 {
            return invalid("symlinks need at least one text file to point to");
        }
        Ok(())
    }

    /// The spec as it reads back from the manifest. JSON float parsing may
    /// be off by an ulp, so generation and the skip check both use this form.
    fn canonical(&self) -> Result<Spec, SynthError> {
        let json = serde_json::to_string(self).map_err(SynthError::Encode)?;
        serde_json::from_str(&json).map_err(SynthError::Encode)
    }
}

/// What [`generate`] wrote, stored at `<root>/.reef-synth.json`. The spec
/// and seed are the tree's identity; nothing is hashed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// The spec the tree was generated from.
    pub spec: Spec,
    /// The seed the tree was generated from.
    pub seed: u64,
    /// Number of text files outside ignored directories; `text_files.len()`.
    pub files: u64,
    /// Directories below the root, ignored ones included.
    pub dirs: u64,
    /// Files inside ignored directories (`target/`, `node_modules/`).
    #[serde(default)]
    pub ignored_files: u64,
    /// Symlinks to text files. Planned on every platform, written only on
    /// Unix.
    #[serde(default)]
    pub symlinks: u64,
    /// Total size of every regular file except the manifest.
    pub bytes: u64,
    /// Binary files, relative to the root with `/` separators, sorted by
    /// path component.
    pub binaries: Vec<PathBuf>,
    /// Text files outside ignored directories, relative to the root with `/`
    /// separators, sorted by path component. These are the only files
    /// [`mutate`] touches. Component order differs from byte order:
    /// `a/b.rs` sorts before `a.rs`.
    pub text_files: Vec<PathBuf>,
    /// Text files that symlinks point to, sorted by path component, without
    /// duplicates. [`mutate`] modifies these but never deletes them, so no
    /// symlink dangles.
    #[serde(default)]
    pub symlink_targets: Vec<PathBuf>,
    /// [`GENERATOR_VERSION`] at generation time.
    pub generator_version: u32,
    /// How many times [`mutate`] has changed the tree. A mutated tree no
    /// longer matches its spec and seed, so [`generate`] rewrites it.
    #[serde(default)]
    pub mutations: u32,
}

impl Manifest {
    fn matches(&self, spec: &Spec, seed: u64) -> bool {
        self.generator_version == GENERATOR_VERSION
            && self.seed == seed
            && &self.spec == spec
            && self.mutations == 0
    }
}

/// How many text files [`mutate`] modifies, adds, and deletes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MutateOps {
    /// Existing text files that get one line appended or replaced.
    pub modify: u32,
    /// New text files in existing directories.
    pub add: u32,
    /// Existing text files removed.
    pub delete: u32,
}

/// The paths [`mutate`] changed, relative to the root, each list sorted.
/// The three lists are disjoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    /// Files whose content changed.
    pub modified: Vec<PathBuf>,
    /// Files created.
    pub added: Vec<PathBuf>,
    /// Files removed.
    pub deleted: Vec<PathBuf>,
}

/// Errors from generating or mutating a tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SynthError {
    /// The spec's parameters are out of range.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    /// The root has files but no manifest, so it is not a tree this
    /// generator wrote and will not be cleared.
    #[error("{0} is not empty and has no {MANIFEST_FILE}; refusing to overwrite it")]
    NotEmpty(PathBuf),
    /// [`mutate`] was pointed at a directory without a manifest.
    #[error("no {MANIFEST_FILE} under {0}")]
    NoManifest(PathBuf),
    /// [`mutate`] was asked to touch more text files than exist.
    #[error(
        "asked to modify {modify} and delete {delete} text files, but only {available} exist \
         and {protected} of the unmodified ones are symlink targets, which are never deleted"
    )]
    NotEnoughFiles {
        /// Files to modify.
        modify: u32,
        /// Files to delete.
        delete: u32,
        /// Text files in the manifest.
        available: u64,
        /// Symlink targets among the files not picked for modification.
        protected: u64,
    },
    /// The tree was left half-written by an interrupted [`generate`];
    /// regenerate it before mutating.
    #[error("{0} holds an incomplete tree; run generate again")]
    Incomplete(PathBuf),
    /// A file system operation failed.
    #[error("I/O error at {path}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The manifest exists but is not valid.
    #[error("{path} is not a valid manifest")]
    Manifest {
        /// The manifest's path.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
    /// Serializing the spec or manifest failed.
    #[error("failed to encode JSON")]
    Encode(#[source] serde_json::Error),
}

impl SynthError {
    fn io(path: &Path, source: io::Error) -> SynthError {
        SynthError::Io {
            path: path.to_owned(),
            source,
        }
    }
}

/// Generates the tree for `spec` and `seed` under `root` and writes its
/// manifest.
///
/// If `root` already holds a complete, unmutated tree for the same spec,
/// seed, and generator version, returns its manifest without touching the
/// disk, unless `force`. Otherwise clears `root` first, but only if it is
/// empty, missing, or was written by this generator (has a manifest);
/// anything else is [`SynthError::NotEmpty`].
pub fn generate(root: &Path, spec: &Spec, seed: u64, force: bool) -> Result<Manifest, SynthError> {
    spec.validate()?;
    let spec = spec.canonical()?;
    if !force
        && !root.join(PARTIAL_FILE).exists()
        && let Ok(Some(manifest)) = load_manifest(root)
        && manifest.matches(&spec, seed)
    {
        return Ok(manifest);
    }

    prepare_root(root)?;
    let mut rng = SynthRng::seed_from_u64(seed);
    let plan = tree::plan(&spec, &mut rng);
    let bytes = tree::write(root, &plan, &mut rng)?;

    let mut text_files = Vec::new();
    let mut binaries = Vec::new();
    let mut ignored_files = 0;
    for file in &plan.files {
        match file.kind {
            tree::Kind::Text => text_files.push(file.rel.clone()),
            tree::Kind::Binary => binaries.push(file.rel.clone()),
            tree::Kind::Ignored => ignored_files += 1,
        }
    }
    let mut symlink_targets =
        sorted_paths(plan.symlinks.iter().map(|l| l.points_to.clone()).collect());
    symlink_targets.dedup();
    let manifest = Manifest {
        spec,
        seed,
        files: text_files.len() as u64,
        dirs: plan.dirs.len() as u64 - 1,
        ignored_files,
        symlinks: plan.symlinks.len() as u64,
        bytes,
        binaries: sorted_paths(binaries),
        text_files: sorted_paths(text_files),
        symlink_targets,
        generator_version: GENERATOR_VERSION,
        mutations: 0,
    };
    write_manifest(root, &manifest)?;
    remove(&root.join(PARTIAL_FILE))?;
    Ok(manifest)
}

/// Reads `<root>/.reef-synth.json`, or `None` if there is none.
pub fn load_manifest(root: &Path) -> Result<Option<Manifest>, SynthError> {
    let path = root.join(MANIFEST_FILE);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SynthError::io(&path, e)),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|source| SynthError::Manifest { path, source })
}

/// Changes text files in the tree under `root` deterministically for `seed`
/// and updates the manifest.
///
/// Picks `ops.modify` text files from the manifest and appends or replaces
/// one line in each, creates `ops.add` new text files in directories that
/// already hold text files, and deletes `ops.delete` other text files,
/// never a symlink target. The same tree, seed, and ops always give the
/// same result. With all three counts zero it changes nothing, not even the
/// manifest.
pub fn mutate(root: &Path, seed: u64, ops: MutateOps) -> Result<Mutation, SynthError> {
    let mut manifest = load_manifest(root)?.ok_or_else(|| SynthError::NoManifest(root.into()))?;
    if root.join(PARTIAL_FILE).exists() {
        return Err(SynthError::Incomplete(root.into()));
    }
    if ops == MutateOps::default() {
        return Ok(Mutation::default());
    }

    // Pick everything before touching the disk.
    let mut rng = SynthRng::seed_from_u64(seed);
    let (to_modify, to_delete) = pick(&mut rng, &manifest, ops)?;

    // Mark the tree as mutated before touching it, so an interrupted run is
    // never mistaken for a pristine tree.
    manifest.mutations += 1;
    write_manifest(root, &manifest)?;

    let mut bytes = manifest.bytes;
    let mut mutation = Mutation::default();

    for &i in &to_modify {
        let rel = manifest.text_files[i].clone();
        let (old, new) = modify_file(&mut rng, &root.join(&rel))?;
        bytes = bytes.saturating_sub(old) + new;
        mutation.modified.push(rel);
    }

    let dirs: Vec<String> = manifest
        .text_files
        .iter()
        .map(|p| parent_rel(&p.to_string_lossy()).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let dirs = if dirs.is_empty() {
        vec![String::new()]
    } else {
        dirs
    };
    for _ in 0..ops.add {
        let dir = &dirs[index(&mut rng, dirs.len())];
        let (stem, ext) = names::file_name(&mut rng);
        let size = log_normal(
            &mut rng,
            manifest.spec.median_size,
            SIZE_SIGMA,
            manifest.spec.max_text_size,
        );
        let rel = free_name(root, dir, stem, ext)?;
        let data = content::text(&mut rng, size);
        let path = root.join(&rel);
        fs::write(&path, &data).map_err(|e| SynthError::io(&path, e))?;
        bytes += data.len() as u64;
        mutation.added.push(PathBuf::from(rel));
    }

    for &i in &to_delete {
        let rel = manifest.text_files[i].clone();
        let path = root.join(&rel);
        let len = fs::symlink_metadata(&path)
            .map_err(|e| SynthError::io(&path, e))?
            .len();
        remove(&path)?;
        bytes = bytes.saturating_sub(len);
        mutation.deleted.push(rel);
    }

    let deleted: HashSet<&PathBuf> = mutation.deleted.iter().collect();
    let mut text_files: Vec<PathBuf> = manifest
        .text_files
        .iter()
        .filter(|p| !deleted.contains(p))
        .chain(&mutation.added)
        .cloned()
        .collect();
    text_files.sort_unstable();
    manifest.text_files = text_files;
    manifest.files = manifest.text_files.len() as u64;
    manifest.bytes = bytes;
    write_manifest(root, &manifest)?;

    mutation.modified.sort_unstable();
    mutation.added.sort_unstable();
    mutation.deleted.sort_unstable();
    Ok(mutation)
}

/// Indices into `manifest.text_files` of the files to modify and to delete.
/// Modified files come from all text files; deleted ones from the rest,
/// minus symlink targets.
fn pick(
    rng: &mut SynthRng,
    manifest: &Manifest,
    ops: MutateOps,
) -> Result<(Vec<usize>, Vec<usize>), SynthError> {
    let available = manifest.text_files.len();
    let not_enough = |protected: usize| SynthError::NotEnoughFiles {
        modify: ops.modify,
        delete: ops.delete,
        available: available as u64,
        protected: protected as u64,
    };
    let modify = ops.modify as usize;
    if modify > available {
        return Err(not_enough(0));
    }
    let to_modify = sample(rng, available, modify);
    let targets: HashSet<&PathBuf> = manifest.symlink_targets.iter().collect();
    let picked: HashSet<usize> = to_modify.iter().copied().collect();
    let unpicked: Vec<usize> = (0..available).filter(|i| !picked.contains(i)).collect();
    let deletable: Vec<usize> = unpicked
        .iter()
        .copied()
        .filter(|&i| !targets.contains(&manifest.text_files[i]))
        .collect();
    let delete = ops.delete as usize;
    if delete > deletable.len() {
        return Err(not_enough(unpicked.len() - deletable.len()));
    }
    let to_delete: Vec<usize> = sample(rng, deletable.len(), delete)
        .into_iter()
        .map(|i| deletable[i])
        .collect();
    Ok((to_modify, to_delete))
}

/// Appends a line to the file or replaces one of its lines, so its content
/// always changes. Returns the old and new sizes.
fn modify_file(rng: &mut SynthRng, path: &Path) -> Result<(u64, u64), SynthError> {
    let mut data = fs::read(path).map_err(|e| SynthError::io(path, e))?;
    let old = data.len() as u64;
    if data.last().is_some_and(|&b| b != b'\n') {
        data.push(b'\n');
    }
    let line_ends: Vec<usize> = data
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| (b == b'\n').then_some(i))
        .collect();
    let mut line = Vec::new();
    if line_ends.is_empty() || below(rng, 2) == 0 {
        content::push_line(rng, &mut line);
        data.extend_from_slice(&line);
    } else {
        let n = index(rng, line_ends.len());
        let start = if n == 0 { 0 } else { line_ends[n - 1] + 1 };
        let end = line_ends[n] + 1;
        content::push_line(rng, &mut line);
        if data[start..end] == line[..] {
            data.extend_from_slice(&line);
        } else {
            data.splice(start..end, line);
        }
    }
    fs::write(path, &data).map_err(|e| SynthError::io(path, e))?;
    Ok((old, data.len() as u64))
}

/// A path for a new file in `dir` that does not exist yet: `stem.ext`, or
/// `stem_<n>.ext` for the smallest free `n`.
fn free_name(root: &Path, dir: &str, stem: &str, ext: &str) -> Result<String, SynthError> {
    let join = |name: String| {
        if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        }
    };
    let mut candidate = join(format!("{stem}.{ext}"));
    let mut n = 1u64;
    loop {
        let path = root.join(&candidate);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(e) => return Err(SynthError::io(&path, e)),
            Ok(_) => {}
        }
        candidate = join(format!("{stem}_{n}.{ext}"));
        n += 1;
    }
}

fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Paths sorted component by component, the order `Vec<PathBuf>::sort` gives.
fn sorted_paths(paths: Vec<String>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    paths.sort_unstable();
    paths
}

/// Makes `root` an empty directory with only the partial marker in it.
/// Refuses to clear a non-empty directory this generator did not write.
fn prepare_root(root: &Path) -> Result<(), SynthError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SynthError::io(root, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|e| SynthError::io(root, e))?;
            Vec::new()
        }
        Err(e) => return Err(SynthError::io(root, e)),
    };
    let ours = root.join(MANIFEST_FILE).exists() || root.join(PARTIAL_FILE).exists();
    if !entries.is_empty() && !ours {
        return Err(SynthError::NotEmpty(root.into()));
    }

    // Marker first, manifest next, then the rest: an interruption at any
    // point leaves a tree that is recognized as ours and as incomplete.
    let partial = root.join(PARTIAL_FILE);
    fs::write(&partial, b"").map_err(|e| SynthError::io(&partial, e))?;
    remove(&root.join(MANIFEST_FILE))?;
    for entry in entries {
        let path = entry.path();
        if entry.file_name() == PARTIAL_FILE || entry.file_name() == MANIFEST_FILE {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map_err(|e| SynthError::io(&path, e))?
            .is_dir();
        if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        }
        .map_err(|e| SynthError::io(&path, e))?;
    }
    Ok(())
}

/// Removes a file, treating "already gone" as success.
fn remove(path: &Path) -> Result<(), SynthError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(SynthError::io(path, e)),
        _ => Ok(()),
    }
}

/// Writes the manifest through a temporary file and a rename, so readers
/// never see half of it.
fn write_manifest(root: &Path, manifest: &Manifest) -> Result<(), SynthError> {
    let data = serde_json::to_vec_pretty(manifest).map_err(SynthError::Encode)?;
    let tmp = root.join(format!("{MANIFEST_FILE}.tmp"));
    fs::write(&tmp, data).map_err(|e| SynthError::io(&tmp, e))?;
    let path = root.join(MANIFEST_FILE);
    fs::rename(&tmp, &path).map_err(|e| SynthError::io(&path, e))
}
