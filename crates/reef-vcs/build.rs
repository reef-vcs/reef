//! Names the exact binary in `reef --version` (R1.28): the commit it was
//! built from and its target, as `REEF_BUILD_COMMIT` and
//! `REEF_BUILD_TARGET`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let commit = commit().unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env=REEF_BUILD_TARGET={target}");
    println!("cargo::rustc-env=REEF_BUILD_COMMIT={commit}");
}

/// The checkout's short commit, or `None` outside a git checkout of this
/// repository (a source tarball, a Reef workspace, a crate unpacked inside
/// someone else's repository).
fn commit() -> Option<String> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let root = manifest.parent()?.parent()?;
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&toplevel).canonicalize().ok()? != root.canonicalize().ok()? {
        return None;
    }
    let commit = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    // Rebuild when HEAD moves: HEAD itself, the branch it names, packed-refs,
    // where that branch may live instead, and HEAD's reflog, the one file a
    // commit on a packed branch is sure to touch. Only files that exist:
    // cargo reruns the script on every build for one that does not. Before
    // git 2.31 (no `--path-format`) nothing is watched and the commit goes
    // stale until something else rebuilds the crate.
    let mut watched = vec![
        "HEAD".to_owned(),
        "packed-refs".to_owned(),
        "logs/HEAD".to_owned(),
    ];
    if let Some(branch) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for name in watched {
        if let Some(path) = git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", &name],
        ) && Path::new(&path).exists()
        {
            println!("cargo::rerun-if-changed={path}");
        }
    }
    Some(commit)
}

/// `git <args>`'s trimmed stdout in `dir`, or `None` if it failed.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !stdout.trim().is_empty()).then(|| stdout.trim().to_owned())
}
