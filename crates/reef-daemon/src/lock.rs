//! The single-instance lock on `<state>/daemon.lock`.
//!
//! An advisory `flock(2)` through [`File::try_lock`], which is what `fd-lock`
//! wraps on Unix; the standard library's is used because the lock must live
//! as long as the daemon, and `fd-lock`'s guard borrows its lock. The holder
//! writes its pid into the file so a refused daemon can say who holds it.
//! The kernel releases the lock when the file is closed, including when the
//! process dies, so a crashed daemon never leaves a stale lock; the file
//! itself is left in place.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use reef_lib::error::{Classify, ErrorKind};

/// A held lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

/// Failure to take the lock.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    /// Another process holds it.
    #[error("{} is held by {}", path.display(), describe(*pid))]
    Held {
        /// The lock file.
        path: PathBuf,
        /// The pid the holder wrote, if it could be read.
        pid: Option<u32>,
    },
    /// The lock file could not be opened, locked, or written.
    #[error("lock {}", path.display())]
    Io {
        /// The lock file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

fn describe(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("another reef-daemon (pid {pid})"),
        None => "another reef-daemon".to_owned(),
    }
}

impl Classify for LockError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Held { .. } => ErrorKind::Unavailable,
            Self::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
                ErrorKind::PermissionDenied
            }
            Self::Io { .. } => ErrorKind::Io,
        }
    }
}

impl InstanceLock {
    /// Takes the lock without waiting, creating the file (mode `0600`) if
    /// needed, and writes this process's pid into it.
    pub fn acquire(path: &Path) -> Result<InstanceLock, LockError> {
        let io_error = |source| LockError::Io {
            path: path.to_owned(),
            source,
        };
        // Not truncated on open: a refused daemon reads the holder's pid.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LockError::Held {
                    path: path.to_owned(),
                    pid: read_pid(&mut file),
                });
            }
            Err(TryLockError::Error(source)) => return Err(io_error(source)),
        }
        file.set_len(0).map_err(io_error)?;
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        writeln!(file, "{}", std::process::id()).map_err(io_error)?;
        Ok(InstanceLock {
            file,
            path: path.to_owned(),
        })
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the file releases the lock too; unlocking first makes the
        // release independent of other descriptors of the same file.
        let _ = self.file.unlock();
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_refused_with_the_pid_then_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let lock = InstanceLock::acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        let err = InstanceLock::acquire(&path).unwrap_err();
        assert!(
            matches!(err, LockError::Held { pid: Some(pid), .. } if pid == std::process::id()),
            "{err:?}"
        );
        assert_eq!(err.kind(), ErrorKind::Unavailable);
        drop(lock);
        let _again = InstanceLock::acquire(&path).unwrap();
        assert!(path.exists(), "the lock file stays");
    }
}
