//! The ignore rules a snapshot applies before the repository's own
//! `.gitignore` files, which the working copy reads itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::gitignore::{GitIgnoreError, GitIgnoreFile};
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;

use super::repo::Repo;

/// Ignore sources beyond git's own, each repository-root relative. The
/// daemon fills them from the workspace's spec (R1.19, R1.20); a plain
/// [`Repo::snapshot`] uses none.
#[derive(Clone, Copy, Debug, Default)]
pub struct IgnoreSources<'a> {
    /// The workspace's `.reefignore`, in gitignore syntax; a missing file
    /// is skipped.
    pub reefignore: Option<&'a Path>,
    /// `[snapshot] exclude` entries of `reef.toml`, in gitignore syntax.
    pub excludes: &'a [String],
    /// Shared store mount points (`[[store]] mount`), never snapshotted.
    pub mounts: &'a [String],
}

impl Repo {
    /// git's own rules outside the tree (`core.excludesFile`, or
    /// `$XDG_CONFIG_HOME/git/ignore`, then `info/exclude`) followed by, in
    /// order, `extra.reefignore`, `extra.excludes`, and `extra.mounts`.
    /// Later rules win, as in git.
    ///
    /// Exclude and mount entries are anchored at the repository root: an
    /// entry without a leading `/` gets one; a trailing `/` is kept, so
    /// `target/` matches only the directory `<root>/target`.
    pub fn ignores(&self, extra: &IgnoreSources<'_>) -> Result<Arc<GitIgnoreFile>, GitIgnoreError> {
        let mut ignores = self.base_ignores()?;
        if let Some(path) = extra.reefignore {
            ignores = ignores.chain_with_file(RepoPath::root(), path.to_owned())?;
        }
        if !extra.excludes.is_empty() {
            let lines = gitignore_lines(extra.excludes.iter().map(String::as_str));
            ignores = ignores.chain(RepoPath::root(), Path::new("reef.toml"), lines.as_bytes())?;
        }
        if !extra.mounts.is_empty() {
            let lines = gitignore_lines(extra.mounts.iter().map(String::as_str));
            ignores = ignores.chain(RepoPath::root(), Path::new("reef.toml"), lines.as_bytes())?;
        }
        Ok(ignores)
    }

    /// Ignore rules that apply before the repository's own `.gitignore`
    /// files: git's `core.excludesFile` (or `$XDG_CONFIG_HOME/git/ignore`)
    /// and the git repo's `info/exclude`. Mirrors jj-cli's `base_ignores`.
    pub(crate) fn base_ignores(&self) -> Result<Arc<GitIgnoreFile>, GitIgnoreError> {
        let mut ignores = GitIgnoreFile::empty();
        let Ok(git_backend) = jj_lib::git::get_git_backend(self.repo.store()) else {
            return Ok(ignores);
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let git_repo = git_backend.git_repo();
        let excludes_file = git_repo
            .config_snapshot()
            .string("core.excludesFile")
            .and_then(|value| {
                let value = std::str::from_utf8(&value).ok()?;
                let path = jj_lib::file_util::expand_home_path(value, home.as_deref());
                // A relative path is read from the work tree, as git does.
                Some(self.workspace_root().join(path))
            })
            .or_else(|| xdg_config_home(home.as_deref()).map(|dir| dir.join("git").join("ignore")));
        if let Some(path) = excludes_file {
            ignores = ignores.chain_with_file(RepoPath::root(), path)?;
        }
        ignores.chain_with_file(
            RepoPath::root(),
            git_backend.git_repo_path().join("info").join("exclude"),
        )
    }
}

/// `entries` as gitignore lines, each anchored at the root. A negation
/// keeps its `!` in front of the anchor; empty entries and comments are
/// dropped.
fn gitignore_lines<'a>(entries: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let (negation, pattern) = match entry.strip_prefix('!') {
            Some(pattern) => ("!", pattern),
            None => ("", entry),
        };
        out.push_str(negation);
        if !pattern.starts_with('/') {
            out.push('/');
        }
        out.push_str(pattern);
        out.push('\n');
    }
    out
}

/// The literal directory or file prefixes among `entries`, relative to the
/// root with no leading or trailing `/`: the entries a watcher can skip
/// events under without matching patterns. Entries with glob characters,
/// negations, and comments are left out (the scan still applies them).
pub fn literal_prefixes<'a>(entries: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    entries
        .into_iter()
        .map(str::trim)
        .filter(|entry| !entry.starts_with(['!', '#']) && !entry.contains(['*', '?', '[', '\\']))
        .map(|entry| entry.trim_matches('/').to_owned())
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn xdg_config_home(home: Option<&Path>) -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|home| home.join(".config")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_anchored() {
        let entries = [
            "target/",
            "/node_modules/",
            " build ",
            "",
            "!keep",
            "# note",
        ];
        assert_eq!(
            gitignore_lines(entries.into_iter()),
            "/target/\n/node_modules/\n/build\n!/keep\n"
        );
    }

    #[test]
    fn literal_prefixes_skip_patterns() {
        let entries = [
            "target/",
            "/node_modules/",
            "*.log",
            "!keep",
            "a/b",
            "/",
            "#c",
        ];
        assert_eq!(literal_prefixes(entries), ["target", "node_modules", "a/b"]);
    }
}
