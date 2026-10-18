//! The repository's workspace spec, `reef.toml` at the root (ADR 0021).
//!
//! Every key is optional and no others are allowed:
//!
//! ```toml
//! [workspace]
//! lease = "30m"                      # humantime duration; capped at the daemon's lease.max
//!
//! [snapshot]
//! exclude = ["target/", "node_modules/"]   # gitignore syntax, relative to the workspace root
//!
//! [[store]]
//! name = "node_modules"              # [a-z0-9][a-z0-9_-]{0,63}
//! lockfile = "pnpm-lock.yaml"        # path relative to the root, must exist in the tree
//! mount = "node_modules"             # path relative to the root; the store appears here
//! build = ["pnpm", "install", "--frozen-lockfile"]   # argv, no shell
//! [store.env]                        # optional, added to the build command's environment
//! CI = "1"
//! ```
//!
//! [`Spec::parse`] checks everything that can be checked without the tree;
//! `Repo::spec_at` (in [`crate::jj::spec`]) reads the file from a commit's
//! tree and also checks that every declared lockfile is in it.

use std::collections::BTreeMap;
use std::time::Duration;

use jj_lib::repo_path::RepoPath;

use crate::error::{Classify, ErrorKind};
use crate::jj::spec::ReadFileError;

/// The spec's file name, at the repository root.
pub const SPEC_FILE: &str = "reef.toml";

/// The shortest `[workspace] lease`: shorter leases flap on the lease
/// manager's 10 s tick.
pub const MIN_LEASE: Duration = Duration::from_mins(1);

/// The longest store name, the length the name pattern allows.
const MAX_STORE_NAME: usize = 64;

/// A parsed and validated `reef.toml`. [`Spec::default`] is what a
/// repository without one gets: no stores, no excludes, the daemon's lease
/// default.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Spec {
    /// `[workspace]`.
    pub workspace: WorkspaceSpec,
    /// `[snapshot]`.
    pub snapshot: SnapshotSpec,
    /// `[[store]]`, in file order.
    #[serde(rename = "store")]
    pub stores: Vec<StoreSpec>,
}

/// `[workspace]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkspaceSpec {
    /// The lease length for workspaces created without one; at least
    /// [`MIN_LEASE`]. The daemon caps it at its `lease.max`.
    #[serde(with = "humantime_serde")]
    pub lease: Option<Duration>,
}

/// `[snapshot]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SnapshotSpec {
    /// Paths kept out of snapshots, in gitignore syntax, relative to the
    /// workspace root.
    pub exclude: Vec<String>,
}

/// One `[[store]]`: a shared dependency store built from a lockfile and
/// mounted into every workspace (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreSpec {
    /// Unique among the spec's stores; `[a-z0-9][a-z0-9_-]{0,63}`.
    pub name: String,
    /// The lockfile the store is built from, a normalized path relative to
    /// the root.
    pub lockfile: String,
    /// Where the store appears in a workspace, a normalized path relative
    /// to the root. Always excluded from snapshots.
    pub mount: String,
    /// The build command as an argv array; no shell is involved.
    pub build: Vec<String>,
    /// Added to the build command's environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A `reef.toml` that cannot be read, parsed, or validated.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpecError {
    /// Not TOML, not UTF-8, an unknown key, or a value of the wrong type.
    #[error("invalid {SPEC_FILE}")]
    Parse(#[source] toml::de::Error),
    /// A store name outside `[a-z0-9][a-z0-9_-]{0,63}`.
    #[error("store {name:?}: invalid name; use [a-z0-9][a-z0-9_-]{{0,63}}")]
    StoreName {
        /// The rejected name.
        name: String,
    },
    /// Two stores share a name.
    #[error("store {name:?} is declared twice")]
    DuplicateStore {
        /// The name.
        name: String,
    },
    /// A store's `lockfile` or `mount` is not a normalized relative path.
    #[error("store {name:?}: {field} must be a normalized relative path, got {value:?} ({reason})")]
    StorePath {
        /// The store.
        name: String,
        /// `lockfile` or `mount`.
        field: &'static str,
        /// The rejected path.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A store's `mount` is, contains, or is inside another store's mount
    /// or any store's lockfile.
    #[error("store {name:?}: mount {mount:?} overlaps {other:?}")]
    MountOverlap {
        /// The store.
        name: String,
        /// Its mount.
        mount: String,
        /// The mount or lockfile it overlaps.
        other: String,
    },
    /// A store's `build` is empty or names an empty command.
    #[error("store {name:?}: build must be a non-empty argv array starting with the command")]
    StoreBuild {
        /// The store.
        name: String,
    },
    /// A `[store.env]` variable the build's environment cannot hold.
    #[error("store {name:?}: env variable {key:?}: {reason}")]
    StoreEnv {
        /// The store.
        name: String,
        /// The variable.
        key: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A `snapshot.exclude` entry that is blank or contains NUL or a line
    /// break (which would add lines to the ignore rules).
    #[error("snapshot.exclude entry {index} is blank or contains NUL or a line break")]
    Exclude {
        /// Its index in the array.
        index: usize,
    },
    /// `workspace.lease` below [`MIN_LEASE`].
    #[error("workspace.lease {0:?} is shorter than one minute")]
    LeaseTooShort(Duration),
    /// A store's lockfile is not a file in the tree the spec was read from.
    #[error("store {name:?}: lockfile {lockfile:?} is not in the tree")]
    LockfileMissing {
        /// The store.
        name: String,
        /// Its lockfile.
        lockfile: String,
    },
    /// `reef.toml` or a lockfile could not be read from the tree; the
    /// error names the path.
    #[error(transparent)]
    Read(ReadFileError),
}

impl Classify for SpecError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Parse(_)
            | Self::StoreName { .. }
            | Self::DuplicateStore { .. }
            | Self::StorePath { .. }
            | Self::MountOverlap { .. }
            | Self::StoreBuild { .. }
            | Self::StoreEnv { .. }
            | Self::Exclude { .. }
            | Self::LeaseTooShort(_) => ErrorKind::InvalidArgument,
            Self::LockfileMissing { .. } => ErrorKind::NotFound,
            Self::Read(error) => error.kind(),
        }
    }
}

impl Spec {
    /// Parses and validates the text of a `reef.toml`.
    pub fn parse(text: &str) -> Result<Spec, SpecError> {
        let spec: Spec = toml::from_str(text).map_err(SpecError::Parse)?;
        spec.validate()?;
        Ok(spec)
    }

    /// [`Spec::parse`] on bytes as read from a tree; text that is not UTF-8
    /// is [`SpecError::Parse`].
    pub fn parse_bytes(bytes: &[u8]) -> Result<Spec, SpecError> {
        let spec: Spec = toml::from_slice(bytes).map_err(SpecError::Parse)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Every path a snapshot must exclude: `snapshot.exclude` plus each
    /// store's `mount` as an anchored pattern (`/node_modules`). The mount
    /// has no trailing `/`, so it also matches a symlink, which jj's scan
    /// matches as a file.
    ///
    /// This is the single-list form, for callers outside the daemon (R1.20,
    /// a future `reef spec check`). The daemon passes the excludes and the
    /// mounts to `Repo::ignores` separately, mounts last, so that a `!`
    /// entry in `exclude` cannot un-exclude a mount;
    /// `jj::ignore::gitignore_lines` anchors mounts the same way.
    pub fn effective_excludes(&self) -> Vec<String> {
        self.snapshot
            .exclude
            .iter()
            .cloned()
            .chain(self.stores.iter().map(|store| format!("/{}", store.mount)))
            .collect()
    }

    /// The store with this name.
    pub fn store(&self, name: &str) -> Option<&StoreSpec> {
        self.stores.iter().find(|store| store.name == name)
    }

    fn validate(&self) -> Result<(), SpecError> {
        if let Some(lease) = self.workspace.lease
            && lease < MIN_LEASE
        {
            return Err(SpecError::LeaseTooShort(lease));
        }
        for (index, entry) in self.snapshot.exclude.iter().enumerate() {
            if entry.trim().is_empty() || entry.contains(['\0', '\n', '\r']) {
                return Err(SpecError::Exclude { index });
            }
        }
        for (i, store) in self.stores.iter().enumerate() {
            store.validate()?;
            if self.stores[..i]
                .iter()
                .any(|other| other.name == store.name)
            {
                return Err(SpecError::DuplicateStore {
                    name: store.name.clone(),
                });
            }
        }
        for (i, store) in self.stores.iter().enumerate() {
            let others = self.stores.iter().enumerate().flat_map(|(j, other)| {
                // A store's own lockfile under its own mount would be
                // shadowed by the mount, so it overlaps too.
                let mount = (i != j).then_some(other.mount.as_str());
                mount.into_iter().chain([other.lockfile.as_str()])
            });
            for other in others {
                if overlaps(&store.mount, other) {
                    return Err(SpecError::MountOverlap {
                        name: store.name.clone(),
                        mount: store.mount.clone(),
                        other: other.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl StoreSpec {
    /// The lockfile as a repository path.
    pub fn lockfile_path(&self) -> &RepoPath {
        repo_path(&self.lockfile)
    }

    /// The mount as a repository path.
    pub fn mount_path(&self) -> &RepoPath {
        repo_path(&self.mount)
    }

    fn validate(&self) -> Result<(), SpecError> {
        if !is_store_name(&self.name) {
            return Err(SpecError::StoreName {
                name: self.name.clone(),
            });
        }
        for (field, value) in [("lockfile", &self.lockfile), ("mount", &self.mount)] {
            if let Err(reason) = check_relative_path(value) {
                return Err(SpecError::StorePath {
                    name: self.name.clone(),
                    field,
                    value: value.clone(),
                    reason,
                });
            }
        }
        if self.build.first().is_none_or(String::is_empty) {
            return Err(SpecError::StoreBuild {
                name: self.name.clone(),
            });
        }
        for (key, value) in &self.env {
            let reason = if key.is_empty() {
                "empty name"
            } else if key.contains(['=', '\0']) {
                "'=' or NUL in the name"
            } else if value.contains('\0') {
                "NUL in the value"
            } else {
                continue;
            };
            return Err(SpecError::StoreEnv {
                name: self.name.clone(),
                key: key.clone(),
                reason,
            });
        }
        Ok(())
    }
}

/// `path` as a [`RepoPath`]. Only called on validated store paths, which
/// [`check_relative_path`] restricts to what `RepoPath` accepts (non-empty
/// `/`-separated components, none `.` or `..`), so it cannot fail.
#[expect(
    clippy::expect_used,
    reason = "validated at parse; see the doc comment"
)]
fn repo_path(path: &str) -> &RepoPath {
    RepoPath::from_internal_string(path).expect("store paths are validated at parse")
}

fn is_store_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    bytes.first().is_some_and(alnum)
        && bytes.len() <= MAX_STORE_NAME
        && bytes[1..]
            .iter()
            .all(|b| alnum(b) || matches!(b, b'_' | b'-'))
}

/// `Ok` if `path` is relative and normalized: non-empty, no leading or
/// trailing `/`, and no empty, `.`, or `..` component. A backslash is a
/// valid file name byte on Unix, but a spec with one is almost certainly a
/// Windows path, so it is rejected too.
///
/// Mounts become gitignore lines (`jj::ignore`), so a path must also read
/// as itself there: no control characters (a line break would add lines),
/// no leading `#` (a comment) or `!` (a negation), no leading or trailing
/// whitespace (trimmed), and no `*`, `?`, or `[` (globs). Lockfiles follow
/// the same rules.
fn check_relative_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty");
    }
    if path.chars().any(char::is_control) {
        return Err("control character");
    }
    if path.starts_with(['#', '!']) {
        return Err("leading '#' or '!', which gitignore reads as a comment or negation");
    }
    if path.trim() != path {
        return Err("leading or trailing whitespace");
    }
    if path.contains(['*', '?', '[']) {
        return Err("glob character");
    }
    if path.starts_with('/') {
        return Err("absolute");
    }
    if path.ends_with('/') {
        return Err("trailing '/'");
    }
    if path.contains('\\') {
        return Err("backslash; separate components with '/'");
    }
    for component in path.split('/') {
        match component {
            "" => return Err("empty component"),
            "." | ".." => return Err("'.' or '..' component"),
            _ => {}
        }
    }
    Ok(())
}

/// Whether one of two normalized paths is the other or an ancestor of it,
/// compared by component so that `vendor` does not overlap `vendors`.
fn overlaps(a: &str, b: &str) -> bool {
    let (a, b): (Vec<_>, Vec<_>) = (a.split('/').collect(), b.split('/').collect());
    let shorter = a.len().min(b.len());
    a[..shorter] == b[..shorter]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_by_component() {
        assert!(overlaps("vendor", "vendor"));
        assert!(overlaps("vendor", "vendor/x"));
        assert!(overlaps("vendor/x", "vendor"));
        assert!(!overlaps("vendor", "vendors"));
        assert!(!overlaps("a/b", "a/c"));
    }

    #[test]
    fn store_names() {
        assert!(is_store_name("node_modules"));
        assert!(is_store_name("0"));
        assert!(is_store_name(&"a".repeat(64)));
        assert!(!is_store_name(&"a".repeat(65)));
        assert!(!is_store_name("_x"));
        assert!(!is_store_name("a.b"));
        assert!(!is_store_name(""));
    }

    #[test]
    fn store_paths_convert() {
        let spec = Spec::parse(
            "[[store]]\nname = \"deps\"\nlockfile = \"a/deps.lock\"\nmount = \"vendor/deps\"\nbuild = [\"true\"]\n",
        )
        .unwrap();
        let store = spec.store("deps").unwrap();
        assert_eq!(
            store.lockfile_path().as_internal_file_string(),
            "a/deps.lock"
        );
        assert_eq!(store.mount_path().as_internal_file_string(), "vendor/deps");
        assert!(spec.store("other").is_none());
    }

    #[test]
    fn build_and_env_rules() {
        let store = |build: &str, env: &str| {
            Spec::parse(&format!(
                "[[store]]\nname = \"s\"\nlockfile = \"l\"\nmount = \"m\"\nbuild = {build}\n{env}"
            ))
        };
        assert!(matches!(store("[]", ""), Err(SpecError::StoreBuild { .. })));
        assert!(matches!(
            store("[\"\"]", ""),
            Err(SpecError::StoreBuild { .. })
        ));
        assert!(matches!(
            store("[\"true\"]", "[store.env]\n\"A=B\" = \"1\"\n"),
            Err(SpecError::StoreEnv { .. })
        ));
        assert!(store("[\"true\"]", "[store.env]\nA = \"1\"\n").is_ok());
    }
}
