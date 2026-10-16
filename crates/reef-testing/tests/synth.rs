//! Tests for the synthetic repository generator (R0.05).

#![allow(
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use reef_testing::synth::{
    self, MANIFEST_FILE, Manifest, MutateOps, Preset, Spec, SynthError, generate, mutate,
};
use tempfile::TempDir;

/// One entry of a walked tree.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File(Vec<u8>),
    Symlink(PathBuf),
}

/// Every entry under `root`, keyed by relative path with `/` separators.
fn walk(root: &Path) -> BTreeMap<String, Entry> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Entry>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let kind = fs::symlink_metadata(&path).expect("metadata").file_type();
            if kind.is_symlink() {
                out.insert(
                    rel,
                    Entry::Symlink(fs::read_link(&path).expect("read_link")),
                );
            } else if kind.is_dir() {
                out.insert(rel, Entry::Dir);
                visit(root, &path, out);
            } else {
                out.insert(rel, Entry::File(fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn is_ignored(rel: &str) -> bool {
    rel.split('/').any(|c| c == "target" || c == "node_modules")
}

fn is_binary(rel: &str) -> bool {
    [".bin", ".png", ".wasm"]
        .iter()
        .any(|ext| rel.ends_with(ext))
}

fn gen_into(dir: &TempDir, preset: Preset, seed: u64) -> Manifest {
    generate(dir.path(), &Spec::preset(preset), seed, false).expect("generate")
}

fn tempdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn mtimes(root: &Path) -> BTreeMap<String, SystemTime> {
    walk(root)
        .into_keys()
        .map(|rel| {
            let mtime = fs::symlink_metadata(root.join(&rel))
                .and_then(|m| m.modified())
                .expect("mtime");
            (rel, mtime)
        })
        .collect()
}

#[test]
fn deterministic() {
    let (a, b) = (tempdir(), tempdir());
    let manifest_a = gen_into(&a, Preset::Small, 1);
    let manifest_b = gen_into(&b, Preset::Small, 1);
    assert_eq!(manifest_a, manifest_b);
    let (tree_a, tree_b) = (walk(a.path()), walk(b.path()));
    assert_eq!(
        tree_a.keys().collect::<Vec<_>>(),
        tree_b.keys().collect::<Vec<_>>()
    );
    for (rel, entry) in &tree_a {
        assert!(&tree_b[rel] == entry, "{rel} differs");
    }
}

#[test]
fn seed_changes_output() {
    let (a, b) = (tempdir(), tempdir());
    gen_into(&a, Preset::Small, 1);
    gen_into(&b, Preset::Small, 2);
    let paths_a: BTreeSet<String> = walk(a.path()).into_keys().collect();
    let paths_b: BTreeSet<String> = walk(b.path()).into_keys().collect();
    assert_ne!(paths_a, paths_b);
}

#[test]
fn counts_match_spec() {
    let dir = tempdir();
    let manifest = gen_into(&dir, Preset::Small, 1);
    let spec = Spec::preset(Preset::Small);
    let tree = walk(dir.path());

    let mut text = BTreeSet::new();
    let (mut binaries, mut ignored, mut symlinks, mut dirs) = (0, 0, 0, 0);
    for (rel, entry) in &tree {
        match entry {
            Entry::Dir => dirs += 1,
            Entry::Symlink(_) => symlinks += 1,
            Entry::File(_) if rel == ".gitignore" || rel == MANIFEST_FILE => {}
            Entry::File(_) if is_ignored(rel) => ignored += 1,
            Entry::File(_) if is_binary(rel) => binaries += 1,
            Entry::File(_) => {
                text.insert(PathBuf::from(rel));
            }
        }
    }

    assert_eq!(text.len() as u64, spec.files);
    assert_eq!(manifest.files, spec.files);
    assert_eq!(
        manifest.text_files,
        text.into_iter().collect::<Vec<_>>(),
        "manifest lists exactly the text files, sorted"
    );
    assert_eq!(binaries, spec.binaries);
    assert_eq!(manifest.binaries.len() as u64, u64::from(spec.binaries));
    assert_eq!(manifest.dirs, dirs);
    if cfg!(unix) {
        assert_eq!(symlinks, spec.symlinks);
    }
    #[allow(clippy::cast_precision_loss, reason = "small counts")]
    let expected = spec.files as f64 * spec.ignored_fraction;
    assert!(
        (f64::from(ignored) - expected).abs() <= expected * 0.01,
        "{ignored} ignored files, expected about {expected}"
    );
}

#[cfg(unix)]
#[test]
fn symlinks_resolve_and_executables_are_marked() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir();
    let manifest = gen_into(&dir, Preset::Small, 1);
    let text: BTreeSet<&PathBuf> = manifest.text_files.iter().collect();
    let mut executables = 0;
    for (rel, entry) in walk(dir.path()) {
        let path = dir.path().join(&rel);
        match entry {
            Entry::Symlink(target) => {
                assert!(target.is_relative(), "{rel} -> {}", target.display());
                let resolved = fs::canonicalize(&path).expect("symlink resolves");
                let resolved = resolved
                    .strip_prefix(fs::canonicalize(dir.path()).expect("root"))
                    .expect("symlink stays inside the tree");
                assert!(text.contains(&resolved.to_path_buf()), "{rel}");
            }
            Entry::File(_) => {
                let mode = fs::metadata(&path).expect("metadata").permissions().mode();
                if mode & 0o111 != 0 {
                    assert_eq!(mode & 0o777, 0o755, "{rel}");
                    assert!(text.contains(&PathBuf::from(&rel)), "{rel}");
                    executables += 1;
                }
            }
            Entry::Dir => {}
        }
    }
    assert_eq!(executables, 10, "1% of 1,000 text files");
}

#[test]
fn size_distribution() {
    let dir = tempdir();
    let manifest = gen_into(&dir, Preset::Medium, 1);
    let spec = Spec::preset(Preset::Medium);
    let mut sizes: Vec<u64> = manifest
        .text_files
        .iter()
        .map(|p| fs::metadata(dir.path().join(p)).expect("metadata").len())
        .collect();
    sizes.sort_unstable();
    let median = sizes[sizes.len() / 2];
    #[allow(clippy::cast_precision_loss, reason = "small sizes")]
    let ratio = median as f64 / spec.median_size as f64;
    assert!((0.75..=1.25).contains(&ratio), "median {median}");
    assert!(sizes.last().expect("files") <= &spec.max_text_size);
    for binary in &manifest.binaries {
        let len = fs::metadata(dir.path().join(binary))
            .expect("metadata")
            .len();
        assert_eq!(len, spec.binary_size, "{}", binary.display());
    }
}

#[test]
fn mutate_is_deterministic_and_disjoint() {
    let (a, b) = (tempdir(), tempdir());
    let before = gen_into(&a, Preset::Small, 1);
    gen_into(&b, Preset::Small, 1);
    let original = walk(a.path());
    let ops = MutateOps {
        modify: 50,
        add: 5,
        delete: 5,
    };
    let first = mutate(a.path(), 7, ops).expect("mutate a");
    let second = mutate(b.path(), 7, ops).expect("mutate b");
    assert_eq!(first, second);
    assert_eq!(walk(a.path()), walk(b.path()), "same tree after mutating");

    assert_eq!(
        (first.modified.len(), first.added.len(), first.deleted.len()),
        (50, 5, 5)
    );
    let modified: BTreeSet<_> = first.modified.iter().collect();
    let added: BTreeSet<_> = first.added.iter().collect();
    let deleted: BTreeSet<_> = first.deleted.iter().collect();
    assert!(modified.is_disjoint(&added));
    assert!(modified.is_disjoint(&deleted));
    assert!(added.is_disjoint(&deleted));

    for path in &first.modified {
        let rel = path.to_string_lossy();
        let now = fs::read(a.path().join(path)).expect("modified file exists");
        assert!(
            original[rel.as_ref()] != Entry::File(now),
            "{rel} unchanged"
        );
    }
    for path in &first.added {
        assert!(!original.contains_key(path.to_string_lossy().as_ref()));
        assert!(a.path().join(path).is_file());
        assert!(!is_ignored(&path.to_string_lossy()));
    }
    for path in &first.deleted {
        assert!(!a.path().join(path).exists(), "{}", path.display());
    }

    let after = synth::load_manifest(a.path())
        .expect("load")
        .expect("manifest");
    assert_eq!(after.mutations, 1);
    assert_eq!(after.files, before.files);
    let listed: BTreeSet<_> = after.text_files.iter().collect();
    assert!(added.iter().all(|p| listed.contains(p)));
    assert!(deleted.iter().all(|p| !listed.contains(p)));
    let mut sorted = after.text_files.clone();
    sorted.sort();
    assert_eq!(sorted, after.text_files);
    let on_disk: u64 = walk(a.path())
        .iter()
        .filter(|(rel, _)| rel.as_str() != MANIFEST_FILE)
        .map(|(_, entry)| match entry {
            Entry::File(data) => data.len() as u64,
            _ => 0,
        })
        .sum();
    assert_eq!(after.bytes, on_disk);
}

#[test]
fn mutate_refuses_more_files_than_exist() {
    let dir = tempdir();
    gen_into(&dir, Preset::Small, 1);
    let ops = MutateOps {
        modify: 600,
        add: 0,
        delete: 401,
    };
    assert!(matches!(
        mutate(dir.path(), 7, ops),
        Err(SynthError::NotEnoughFiles { .. })
    ));
}

#[test]
fn regenerate_skips_when_manifest_matches() {
    let dir = tempdir();
    let spec = Spec::preset(Preset::Small);
    let first = generate(dir.path(), &spec, 1, false).expect("generate");
    let before = mtimes(dir.path());
    std::thread::sleep(Duration::from_millis(20));

    let start = Instant::now();
    let second = generate(dir.path(), &spec, 1, false).expect("regenerate");
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(50), "took {elapsed:?}");
    assert_eq!(first, second);
    assert_eq!(before, mtimes(dir.path()), "nothing rewritten");

    let path = dir.path().join(&first.text_files[0]);
    let original = fs::read(&path).expect("read");
    fs::write(&path, b"edited\n").expect("edit");
    generate(dir.path(), &spec, 1, false).expect("regenerate");
    assert_eq!(fs::read(&path).expect("read"), b"edited\n", "skipped");
    generate(dir.path(), &spec, 1, true).expect("force");
    assert_eq!(fs::read(&path).expect("read"), original, "force rewrites");
    assert_ne!(before, mtimes(dir.path()));
}

#[test]
fn regenerate_rewrites_after_mutate_or_spec_change() {
    let (dir, fresh) = (tempdir(), tempdir());
    gen_into(&dir, Preset::Small, 1);
    gen_into(&fresh, Preset::Small, 1);
    let ops = MutateOps {
        modify: 5,
        add: 2,
        delete: 2,
    };
    mutate(dir.path(), 7, ops).expect("mutate");
    let manifest = gen_into(&dir, Preset::Small, 1);
    assert_eq!(manifest.mutations, 0);
    assert_eq!(walk(dir.path()), walk(fresh.path()));

    let mut spec = Spec::preset(Preset::Small);
    spec.files = 200;
    let manifest = generate(dir.path(), &spec, 1, false).expect("smaller spec");
    assert_eq!(manifest.files, 200);
    let text_files = walk(dir.path())
        .into_iter()
        .filter(|(rel, e)| {
            matches!(e, Entry::File(_))
                && !is_ignored(rel)
                && !is_binary(rel)
                && rel != ".gitignore"
                && rel != MANIFEST_FILE
        })
        .count();
    assert_eq!(text_files, 200, "old files were cleared");
}

#[test]
fn refuses_to_clear_a_foreign_directory() {
    let dir = tempdir();
    fs::write(dir.path().join("precious.txt"), b"keep me").expect("write");
    let result = generate(dir.path(), &Spec::preset(Preset::Small), 1, true);
    assert!(matches!(result, Err(SynthError::NotEmpty(_))), "{result:?}");
    assert_eq!(
        fs::read(dir.path().join("precious.txt")).expect("still there"),
        b"keep me"
    );
}

#[test]
fn rejects_invalid_specs() {
    let dir = tempdir();
    let mut spec = Spec::preset(Preset::Small);
    spec.fan_out = 0.0;
    assert!(matches!(
        generate(dir.path(), &spec, 1, false),
        Err(SynthError::InvalidSpec(_))
    ));
}

#[test]
fn gitignore_present() {
    let dir = tempdir();
    gen_into(&dir, Preset::Small, 1);
    let gitignore = fs::read_to_string(dir.path().join(".gitignore")).expect("read");
    let lines: Vec<&str> = gitignore.lines().collect();
    for entry in [MANIFEST_FILE, "target/", "node_modules/"] {
        assert!(lines.contains(&entry), "{entry} missing from {gitignore:?}");
    }
    let ignored_dirs: BTreeSet<String> = walk(dir.path())
        .into_iter()
        .filter(|(rel, e)| *e == Entry::Dir && is_ignored(rel))
        .filter_map(|(rel, _)| rel.rsplit('/').next().map(str::to_owned))
        .collect();
    assert!(!ignored_dirs.is_empty());
    for name in ignored_dirs {
        assert!(lines.contains(&format!("{name}/").as_str()), "{name}");
    }
}

#[test]
#[ignore = "generates 100k files; run by hand and record the time in the README"]
fn large_preset_performance() {
    let dir = tempdir();
    let start = Instant::now();
    let manifest = gen_into(&dir, Preset::Large, 42);
    let elapsed = start.elapsed();
    println!(
        "large preset: {} text files, {} dirs, {} MiB in {elapsed:.2?}",
        manifest.files,
        manifest.dirs,
        manifest.bytes / (1024 * 1024)
    );
    assert_eq!(manifest.files, 100_000);
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
}
