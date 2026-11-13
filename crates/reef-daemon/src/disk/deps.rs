//! #195: a workspace whose repository declares dependencies that are not
//! installed in it. Read from the tracked paths the walk already has, plus
//! one `stat` per candidate directory.

use std::collections::HashSet;
use std::path::Path;

/// How deep a manifest's directory may be: `.` and three levels below.
const MAX_DEPTH: usize = 3;

/// At most this many directories are reported.
pub const MAX_REPORTED: usize = 8;

/// One kind of dependency layout: a manifest, the lockfiles any of which
/// pins it, and the directory an install makes.
struct Layout {
    manifest: &'static str,
    lockfiles: &'static [&'static str],
    installed: &'static str,
}

const LAYOUTS: &[Layout] = &[
    Layout {
        manifest: "package.json",
        lockfiles: &[
            "package-lock.json",
            "npm-shrinkwrap.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lock",
            "bun.lockb",
        ],
        installed: "node_modules",
    },
    Layout {
        manifest: "pyproject.toml",
        // Not poetry.lock: Poetry keeps its environment outside the
        // project unless configured otherwise.
        lockfiles: &["uv.lock"],
        installed: ".venv",
    },
];

/// The directories (repository-relative, `.` for the root, sorted) that
/// track a manifest and a lockfile of one layout but have no installed
/// directory under `root`. Not `Cargo.lock` or `go.sum`: their build
/// directories appear at the first build, with no install step.
pub fn missing<S: std::hash::BuildHasher>(
    root: &Path,
    tracked: &HashSet<String, S>,
) -> Vec<String> {
    let mut found = Vec::new();
    for path in tracked {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        if dir.split('/').filter(|part| !part.is_empty()).count() > MAX_DEPTH {
            continue;
        }
        for layout in LAYOUTS {
            if !layout.lockfiles.contains(&name) {
                continue;
            }
            let manifest = if dir.is_empty() {
                layout.manifest.to_owned()
            } else {
                format!("{dir}/{}", layout.manifest)
            };
            if !tracked.contains(&manifest) {
                continue;
            }
            let at = root.join(dir);
            // A symlink (a store mount) counts as installed, and so does
            // Yarn's Plug'n'Play, which makes no node_modules.
            let installed = std::fs::symlink_metadata(at.join(layout.installed)).is_ok()
                || (layout.installed == "node_modules" && at.join(".pnp.cjs").exists());
            if !installed {
                found.push(if dir.is_empty() {
                    ".".to_owned()
                } else {
                    dir.to_owned()
                });
            }
        }
    }
    found.sort();
    found.dedup();
    found.truncate(MAX_REPORTED);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn detect() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("app/node_modules")).unwrap();
        std::fs::create_dir_all(root.join("pnp")).unwrap();
        std::fs::write(root.join("pnp/.pnp.cjs"), "").unwrap();
        let tracked = set(&[
            "package.json",
            "package-lock.json",
            "app/package.json",
            "app/pnpm-lock.yaml",
            "py/pyproject.toml",
            "py/uv.lock",
            "poetry/pyproject.toml",
            "poetry/poetry.lock",
            "pnp/package.json",
            "pnp/yarn.lock",
            "lonely/yarn.lock",
            "a/b/c/d/package.json",
            "a/b/c/d/package-lock.json",
            "Cargo.lock",
            "Cargo.toml",
        ]);
        // The root and py lack theirs; app has node_modules and pnp Yarn's
        // Plug'n'Play; lonely has no manifest; a/b/c/d is too deep; Poetry
        // and Cargo are never reported.
        assert_eq!(missing(root, &tracked), [".", "py"]);
        // A store mount (a symlink) counts as installed.
        std::os::unix::fs::symlink("/nonexistent", root.join("node_modules")).unwrap();
        assert_eq!(missing(root, &tracked), ["py"]);
    }

    #[test]
    fn at_most_eight() {
        let temp = tempfile::tempdir().unwrap();
        let mut tracked = HashSet::new();
        for i in 0..12 {
            tracked.insert(format!("p{i:02}/package.json"));
            tracked.insert(format!("p{i:02}/yarn.lock"));
        }
        assert_eq!(missing(temp.path(), &tracked).len(), MAX_REPORTED);
    }
}
