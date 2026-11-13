//! The cargo configuration above the workspaces (#271, ADR 0074):
//! `~/.reef/.cargo/config.toml`, the build limit for cargo commands Reef
//! did not start.
//!
//! Cargo reads every `.cargo/config.toml` from the current directory up,
//! at every invocation; a deeper file wins over a shallower one and over
//! `$CARGO_HOME`'s, and the environment wins over all. So one file at
//! `~/.reef` reaches every repository's workspaces, a repository's own
//! file overrides it, and since it is rewritten as builders come and go,
//! a long-lived session's cargo follows the current share. `[env]` cannot
//! carry a jobserver (cargo ignores `CARGO_MAKEFLAGS` there), so the file
//! holds counts only: `build.jobs`, and `RUST_TEST_THREADS` for test
//! binaries.
//!
//! Written only under the default workspace root Reef owns (S-185, S-190),
//! never over a file without [`HEADER`], and each key is left out when the
//! user's `$CARGO_HOME/config.toml` sets it, since this file would
//! otherwise override theirs.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The file's first line; a file without it is never touched.
pub(crate) const HEADER: &str =
    "# Written by Reef (#271): the build limit for cargo commands Reef did not start.";

/// Which keys the file carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Keys {
    /// `build.jobs`: not when `$CARGO_HOME`'s config sets it.
    pub jobs: bool,
    /// `[env] RUST_TEST_THREADS`: not when `$CARGO_HOME`'s config sets it.
    pub rust_test_threads: bool,
}

/// The file the daemon keeps.
#[derive(Debug)]
pub(crate) struct CargoFile {
    /// `<base>/.cargo/config.toml`.
    path: PathBuf,
    /// The directory every workspace it reaches is below.
    base: PathBuf,
    keys: Keys,
    /// The file on disk is Reef's and says the last share asked for.
    active: AtomicBool,
}

impl CargoFile {
    /// The file for workspaces under `base`, carrying `keys`.
    pub(crate) fn new(base: &Path, keys: Keys) -> CargoFile {
        CargoFile {
            path: base.join(".cargo").join("config.toml"),
            base: base.to_owned(),
            keys,
            active: AtomicBool::new(false),
        }
    }

    /// The file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The keys it carries while it is in effect: Reef's file, current
    /// at the last write. `None` when it is not (a file of the user's is
    /// there, or the write failed), so commands get the variables instead.
    pub(crate) fn active_keys(&self) -> Option<Keys> {
        self.active.load(Ordering::Relaxed).then_some(self.keys)
    }

    /// Whether cargo run in `dir` reads this file.
    pub(crate) fn reaches(&self, dir: &Path) -> bool {
        // Workspace paths are canonical; `~/.reef` may be a symlink (S-187)
        // or under one.
        dir.starts_with(&self.base)
            || std::fs::canonicalize(&self.base).is_ok_and(|base| dir.starts_with(base))
    }

    /// Makes the file say `share`: reads it, and writes it unless it
    /// already says so, so a file removed or changed from outside comes
    /// back. An error when a file without [`HEADER`] is there, which is
    /// left alone; the file is then not in effect.
    pub(crate) fn write(&self, share: u32) -> io::Result<()> {
        let written = self.ensure(share);
        self.active.store(written.is_ok(), Ordering::Relaxed);
        written
    }

    fn ensure(&self, share: u32) -> io::Result<()> {
        let wanted = render(share, self.keys);
        let present = read_if_present(&self.path)?;
        if present.as_deref() == Some(wanted.as_str()) {
            return Ok(());
        }
        if let Some(text) = present
            && !text.starts_with(HEADER)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} was not written by Reef; left as it is",
                    self.path.display()
                ),
            ));
        }
        let dir = self.path.parent().unwrap_or(&self.base);
        super::jobserver::private_dir(dir)?;
        let temporary = dir.join(format!("config.toml.{}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(wanted.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    }
}

/// The file's text for `share`.
pub(crate) fn render(share: u32, keys: Keys) -> String {
    let mut text = format!(
        "{HEADER}\n# Rewritten as workspaces come and go. A repository's own .cargo/config.toml overrides it.\n"
    );
    if keys.jobs {
        let _ = write!(text, "\n[build]\njobs = {share}\n");
    }
    if keys.rust_test_threads {
        let _ = write!(text, "\n[env]\nRUST_TEST_THREADS = \"{share}\"\n");
    }
    text
}

/// Removes the file at `path` if Reef wrote it: when the limit or the file
/// is turned off.
pub(crate) fn remove_if_ours(path: &Path) {
    match read_if_present(path) {
        Ok(Some(text)) if text.starts_with(HEADER) => {
            if let Err(error) = std::fs::remove_file(path) {
                tracing::warn!(path = %path.display(), %error, "cannot remove the build limit's cargo file");
            }
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the build limit's cargo file");
        }
    }
}

fn read_if_present(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn written_rewritten_and_removed() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join(".reef");
        std::fs::create_dir(&base).unwrap();
        let keys = Keys {
            jobs: true,
            rust_test_threads: true,
        };
        let file = CargoFile::new(&base, keys);
        assert_eq!(file.active_keys(), None, "not in effect before a write");
        file.write(4).unwrap();
        assert_eq!(file.active_keys(), Some(keys));
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.starts_with(HEADER));
        assert!(text.contains("[build]\njobs = 4\n"));
        assert!(text.contains("RUST_TEST_THREADS = \"4\""));
        let toml: toml::Table = text.parse().unwrap();
        assert_eq!(toml["build"]["jobs"].as_integer(), Some(4));
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        file.write(2).unwrap();
        assert!(
            std::fs::read_to_string(file.path())
                .unwrap()
                .contains("jobs = 2")
        );
        // Removed from outside: the next write puts it back.
        std::fs::remove_file(file.path()).unwrap();
        file.write(2).unwrap();
        assert!(file.path().exists());
        assert!(file.reaches(&base.join("a3dda866").join("ws")));
        assert!(!file.reaches(temp.path()));
        remove_if_ours(file.path());
        assert!(!file.path().exists());
    }

    #[test]
    fn keys_left_out() {
        let only_jobs = render(
            3,
            Keys {
                jobs: true,
                rust_test_threads: false,
            },
        );
        assert!(only_jobs.contains("jobs = 3"));
        assert!(!only_jobs.contains("RUST_TEST_THREADS"));
        let only_threads = render(
            3,
            Keys {
                jobs: false,
                rust_test_threads: true,
            },
        );
        assert!(!only_threads.contains("[build]"));
        assert!(only_threads.contains("RUST_TEST_THREADS = \"3\""));
    }

    #[test]
    fn a_file_without_the_header_is_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let file = CargoFile::new(temp.path(), Keys::default());
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        std::fs::write(file.path(), "[build]\njobs = 9\n").unwrap();
        assert!(file.write(2).is_err());
        assert_eq!(file.active_keys(), None, "the user's file is not Reef's");
        remove_if_ours(file.path());
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "[build]\njobs = 9\n"
        );
    }
}
