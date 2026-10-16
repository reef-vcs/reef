//! Planning the tree in memory, then writing it.
//!
//! Every random decision is drawn from one stream in a fixed order: the
//! directory plan, text file names and sizes, executables, ignored
//! directories and their files, symlinks, binary names, then (while writing)
//! text contents in plan order and binary contents last. Reordering any step
//! changes the output and needs a `GENERATOR_VERSION` bump.

use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use super::dist::{SynthRng, index, log_normal, poisson, sample, to_u64};
use super::{GITIGNORE, IGNORED_DIRS, SIZE_SIGMA, Spec, SynthError, content, names};

/// How often progress is reported when stderr is a terminal.
const PROGRESS_EVERY: usize = 10_000;

/// Ignored files per ignored directory, roughly.
const IGNORED_FILES_PER_DIR: u64 = 500;
/// Upper bound on ignored directories, however many ignored files there are.
const MAX_IGNORED_DIRS: u64 = 8;

/// The whole tree, decided before anything touches the disk.
pub(crate) struct Plan {
    /// Index 0 is the root (relative path `""`). Every directory comes after
    /// its parent. Regular directories first, then ignored ones.
    pub(crate) dirs: Vec<Dir>,
    /// Text files first, then files in ignored directories, then binaries.
    pub(crate) files: Vec<File>,
    pub(crate) symlinks: Vec<Symlink>,
}

pub(crate) struct Dir {
    /// Relative path with `/` separators; empty for the root.
    pub(crate) rel: String,
    depth: u8,
    /// Names taken in this directory, files and subdirectories alike.
    names: HashSet<String>,
    next_suffix: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    Ignored,
    Binary,
}

pub(crate) struct File {
    pub(crate) rel: String,
    pub(crate) size: u64,
    pub(crate) kind: Kind,
    executable: bool,
}

pub(crate) struct Symlink {
    rel: String,
    /// Relative to the symlink's directory, `/` separators.
    target: String,
    /// The target's path relative to the root.
    pub(crate) points_to: String,
}

impl Dir {
    fn new(rel: String, depth: u8) -> Dir {
        Dir {
            rel,
            depth,
            names: HashSet::new(),
            next_suffix: 1,
        }
    }

    /// Takes a name in this directory: `stem.ext`, or `stem_<n>.ext` if
    /// that is taken. Words contain no `_`, so suffixed names cannot collide
    /// with plain ones, and the suffix counter never repeats.
    fn claim(&mut self, stem: &str, ext: Option<&str>) -> String {
        let dot_ext = ext.map(|e| format!(".{e}")).unwrap_or_default();
        let plain = format!("{stem}{dot_ext}");
        if self.names.insert(plain.clone()) {
            return plain;
        }
        loop {
            let name = format!("{stem}_{}{dot_ext}", self.next_suffix);
            self.next_suffix += 1;
            if self.names.insert(name.clone()) {
                return name;
            }
        }
    }

    fn child(&self, name: &str) -> String {
        if self.rel.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.rel)
        }
    }
}

/// Number of items for `fraction` of `files`, rounded to nearest.
fn fraction_of(files: u64, fraction: f64) -> u64 {
    #[allow(clippy::cast_precision_loss, reason = "file counts are far below 2^53")]
    let files = files as f64;
    to_u64(files * fraction)
}

fn len_u64(len: usize) -> u64 {
    len as u64
}

/// Decides the whole tree. Draws from `rng` in the documented order.
pub(crate) fn plan(spec: &Spec, rng: &mut SynthRng) -> Plan {
    let mut dirs = vec![Dir::new(String::new(), 0)];

    // Directories: breadth-first, Poisson(fan_out) children per directory
    // above max_depth, until there is one directory per files_per_dir files.
    // If the frontier runs dry first, sweep the eligible directories again.
    // `eligible` lists the directories above max_depth in plan order; the
    // root is always one when max_depth > 0, so the loop terminates.
    #[allow(clippy::cast_precision_loss, reason = "file counts are far below 2^53")]
    let wanted = to_u64(spec.files as f64 / spec.files_per_dir).max(1);
    let wanted = if spec.max_depth == 0 { 1 } else { wanted };
    let mut eligible = vec![0];
    let mut cursor = 0;
    while len_u64(dirs.len()) < wanted {
        if cursor == eligible.len() {
            cursor = 0;
        }
        let parent = eligible[cursor];
        cursor += 1;
        for _ in 0..poisson(rng, spec.fan_out) {
            if len_u64(dirs.len()) >= wanted {
                break;
            }
            let child = add_dir(&mut dirs, parent, names::dir_name(rng));
            if dirs[child].depth < spec.max_depth {
                eligible.push(child);
            }
        }
    }
    let regular_dirs = dirs.len();

    // Text files: Poisson(files_per_dir) per directory in plan order, in as
    // many sweeps as it takes to place exactly spec.files.
    let mut files = Vec::new();
    let mut remaining = spec.files;
    while remaining > 0 {
        for dir in &mut dirs[..regular_dirs] {
            let count = poisson(rng, spec.files_per_dir).min(remaining);
            remaining -= count;
            for _ in 0..count {
                let (stem, ext) = names::file_name(rng);
                let size = log_normal(rng, spec.median_size, SIZE_SIGMA, spec.max_text_size);
                files.push(new_file(dir, stem, ext, size, Kind::Text));
            }
            if remaining == 0 {
                break;
            }
        }
    }
    let text_files = files.len();

    let executables = usize::try_from(fraction_of(spec.files, spec.executable_fraction))
        .unwrap_or(text_files)
        .min(text_files);
    for i in sample(rng, text_files, executables) {
        files[i].executable = true;
    }

    // Ignored directories: `target/` at the root, then alternating
    // `node_modules/` and `target/` under random directories.
    let ignored_files = fraction_of(spec.files, spec.ignored_fraction);
    if ignored_files > 0 {
        let wanted = ignored_files
            .div_ceil(IGNORED_FILES_PER_DIR)
            .clamp(1, MAX_IGNORED_DIRS);
        let mut ignored = vec![add_dir(&mut dirs, 0, IGNORED_DIRS[0])];
        for i in 1..wanted {
            let name = IGNORED_DIRS[usize::from(i % 2 == 1)];
            let parent = index(rng, regular_dirs);
            if !dirs[parent].names.contains(name) {
                ignored.push(add_dir(&mut dirs, parent, name));
            }
        }
        for _ in 0..ignored_files {
            let dir = ignored[index(rng, ignored.len())];
            let (stem, ext) = names::file_name(rng);
            let size = log_normal(rng, spec.median_size, SIZE_SIGMA, spec.max_text_size);
            files.push(new_file(&mut dirs[dir], stem, ext, size, Kind::Ignored));
        }
    }

    // Symlinks to text files, from random regular directories.
    let mut symlinks = Vec::new();
    if text_files > 0 {
        for _ in 0..spec.symlinks {
            let target = &files[index(rng, text_files)].rel;
            let dir = index(rng, regular_dirs);
            let ext = target.rsplit_once('.').map(|(_, ext)| ext);
            let name = dirs[dir].claim(names::word(rng), ext);
            symlinks.push(Symlink {
                rel: dirs[dir].child(&name),
                target: relative(&dirs[dir].rel, target),
                points_to: target.clone(),
            });
        }
    }

    // Binaries last, in random regular directories.
    for _ in 0..spec.binaries {
        let dir = index(rng, regular_dirs);
        let (stem, ext) = names::binary_name(rng);
        files.push(new_file(
            &mut dirs[dir],
            stem,
            ext,
            spec.binary_size,
            Kind::Binary,
        ));
    }

    Plan {
        dirs,
        files,
        symlinks,
    }
}

fn add_dir(dirs: &mut Vec<Dir>, parent: usize, stem: &str) -> usize {
    let name = dirs[parent].claim(stem, None);
    let dir = Dir::new(dirs[parent].child(&name), dirs[parent].depth + 1);
    dirs.push(dir);
    dirs.len() - 1
}

fn new_file(dir: &mut Dir, stem: &str, ext: &str, size: u64, kind: Kind) -> File {
    let name = dir.claim(stem, Some(ext));
    File {
        rel: dir.child(&name),
        size,
        kind,
        executable: false,
    }
}

/// The path of `target` relative to directory `from`, both relative to the
/// root with `/` separators.
fn relative(from: &str, target: &str) -> String {
    let from: Vec<&str> = from.split('/').filter(|c| !c.is_empty()).collect();
    let target: Vec<&str> = target.split('/').collect();
    let common = from
        .iter()
        .zip(&target[..target.len() - 1])
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts = vec![".."; from.len() - common];
    parts.extend(&target[common..]);
    parts.join("/")
}

/// Writes the plan under `root`, which must exist and hold nothing but the
/// partial marker, drawing file contents from `rng`. Returns the total bytes of regular files written.
pub(crate) fn write(root: &Path, plan: &Plan, rng: &mut SynthRng) -> Result<u64, SynthError> {
    for dir in &plan.dirs[1..] {
        let path = root.join(&dir.rel);
        fs::create_dir(&path).map_err(|e| SynthError::io(&path, e))?;
    }

    let gitignore = root.join(".gitignore");
    fs::write(&gitignore, GITIGNORE).map_err(|e| SynthError::io(&gitignore, e))?;
    let mut bytes = len_u64(GITIGNORE.len());

    let progress = std::io::stderr().is_terminal();
    for (i, file) in plan.files.iter().enumerate() {
        let data = match file.kind {
            Kind::Text | Kind::Ignored => content::text(rng, file.size),
            Kind::Binary => content::binary(rng, file.size),
        };
        let path = root.join(&file.rel);
        fs::write(&path, &data).map_err(|e| SynthError::io(&path, e))?;
        if file.executable {
            set_executable(&path)?;
        }
        bytes += len_u64(data.len());
        if progress && (i + 1) % PROGRESS_EVERY == 0 {
            #[allow(clippy::print_stderr, reason = "progress for interactive runs")]
            {
                eprintln!("reef-synth: wrote {} of {} files", i + 1, plan.files.len());
            }
        }
    }

    for link in &plan.symlinks {
        write_symlink(root, link)?;
    }
    Ok(bytes)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), SynthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| SynthError::io(path, e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), SynthError> {
    Ok(())
}

#[cfg(unix)]
fn write_symlink(root: &Path, link: &Symlink) -> Result<(), SynthError> {
    let path = root.join(&link.rel);
    std::os::unix::fs::symlink(&link.target, &path).map_err(|e| SynthError::io(&path, e))
}

/// Symlinks need privileges on Windows; the plan still draws them so the
/// rest of the tree matches other platforms, but none are written.
#[cfg(not(unix))]
fn write_symlink(_root: &Path, _link: &Symlink) -> Result<(), SynthError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::relative;

    #[test]
    fn relative_paths() {
        assert_eq!(relative("", "a/b.rs"), "a/b.rs");
        assert_eq!(relative("a", "a/b.rs"), "b.rs");
        assert_eq!(relative("a/c", "a/b.rs"), "../b.rs");
        assert_eq!(relative("x/y", "a/b/c.rs"), "../../a/b/c.rs");
        assert_eq!(relative("a/b", "c.rs"), "../../c.rs");
    }
}
