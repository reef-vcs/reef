//! The `/.jj/` line in `.git/info/exclude` (R1.25). jj-lib writes nothing
//! there; the `jj` CLI adds the line itself when it colocates, and so does
//! Reef, or `git status` in the user's checkout lists `.jj/` as untracked.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// The line appended.
const LINE: &str = "/.jj/";
/// Lines that already exclude `.jj` at the root, as written by hand or by
/// another tool.
const SPELLINGS: [&str; 3] = ["/.jj/", ".jj/", ".jj"];

/// The exclude file of the git repository whose `.git` directory is under
/// `root`.
pub(crate) fn exclude_path(root: &Path) -> PathBuf {
    root.join(".git").join("info").join("exclude")
}

/// Appends `/.jj/` to `<root>/.git/info/exclude` unless a line equal to
/// `/.jj/`, `.jj/`, or `.jj` is already there, creating `info/` and the
/// file if missing. Returns whether it wrote. Idempotent.
pub(crate) fn write_jj_exclude(root: &Path) -> io::Result<bool> {
    let path = exclude_path(root);
    // Bytes: git allows any encoding in the file.
    let existing = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if existing.split(|byte| *byte == b'\n').any(|line| {
        SPELLINGS
            .iter()
            .any(|s| line.trim_ascii_end() == s.as_bytes())
    }) {
        return Ok(false);
    }
    if let Some(info) = path.parent() {
        std::fs::create_dir_all(info)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let separator = if existing.is_empty() || existing.ends_with(b"\n") {
        ""
    } else {
        "\n"
    };
    file.write_all(format!("{separator}{LINE}\n").as_bytes())?;
    file.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        dir
    }

    fn exclude(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(exclude_path(dir.path())).unwrap()
    }

    #[test]
    fn creates_info_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(write_jj_exclude(dir.path()).unwrap());
        assert_eq!(exclude(&dir), "/.jj/\n");
        assert!(!write_jj_exclude(dir.path()).unwrap());
        assert_eq!(exclude(&dir), "/.jj/\n");
    }

    #[test]
    fn appends_after_existing_lines() {
        let dir = git_dir();
        std::fs::write(exclude_path(dir.path()), "# comment\n*.log").unwrap();
        assert!(write_jj_exclude(dir.path()).unwrap());
        assert_eq!(exclude(&dir), "# comment\n*.log\n/.jj/\n");
    }

    #[test]
    fn every_existing_spelling_is_kept() {
        for spelling in SPELLINGS {
            let dir = git_dir();
            let text = format!("target/\n{spelling}\r\n");
            std::fs::write(exclude_path(dir.path()), &text).unwrap();
            assert!(!write_jj_exclude(dir.path()).unwrap(), "{spelling}");
            assert_eq!(exclude(&dir), text);
        }
        // Not UTF-8, which git allows.
        let dir = git_dir();
        std::fs::write(exclude_path(dir.path()), b"caf\xe9/\n").unwrap();
        assert!(write_jj_exclude(dir.path()).unwrap());
        assert_eq!(
            std::fs::read(exclude_path(dir.path())).unwrap(),
            b"caf\xe9/\n/.jj/\n"
        );
        // Not the same pattern: `.jj` below the root only.
        let dir = git_dir();
        std::fs::write(exclude_path(dir.path()), "sub/.jj\n").unwrap();
        assert!(write_jj_exclude(dir.path()).unwrap());
    }
}
