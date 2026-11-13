//! The jobserver FIFO (#271, ADR 0074): one named pipe at a stable path,
//! `~/.reef/.build/jobserver` (else `<state>/build/jobserver`), holding the pool's tokens in GNU make's POSIX
//! protocol. A token is one `+` byte; a client reads one to start a job and
//! writes it back when the job ends, and every client also has one
//! implicit token it never reads.
//!
//! The daemon holds the FIFO open read-write for its whole life, since a
//! FIFO's buffer exists only while a descriptor is open, and counts what is
//! in it through a second, read-only descriptor: macOS answers `FIONREAD`
//! with 0 on the read-write one. It never reads tokens for itself except
//! to drain a surplus.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    PermissionsExt as _,
};
use std::path::{Path, PathBuf};

/// The FIFO's file name in the build directory.
pub(crate) const FIFO_NAME: &str = "jobserver";

/// The byte every token is.
pub(crate) const TOKEN: u8 = b'+';

/// The open jobserver.
#[derive(Debug)]
pub(crate) struct Jobserver {
    path: PathBuf,
    /// Held for the daemon's life; tokens are written and drained here.
    rw: File,
    /// Only for `FIONREAD`.
    ro: File,
    dev: u64,
    ino: u64,
    tokens: u32,
}

/// What [`Jobserver::open`] did.
#[derive(Debug)]
pub(crate) struct Opened {
    /// The jobserver, holding its pool.
    pub jobserver: Jobserver,
    /// A new FIFO was made (none was there, or what was there failed the
    /// checks and was moved aside).
    pub created: bool,
    /// Tokens written to fill the pool.
    pub restored: u32,
    /// Surplus tokens drained.
    pub drained: u32,
}

impl Jobserver {
    /// Opens the jobserver in `dir` with a pool of `tokens`: reuses a FIFO
    /// already there if it passes the checks (the daemon's uid, type FIFO,
    /// no group or other bits), else moves what is there aside and makes a
    /// new one; then fills the pool and drains any surplus. A build that
    /// opened the FIFO while the daemon was down, and starves on it, is
    /// rescued this way.
    pub(crate) fn open(dir: &Path, tokens: u32) -> io::Result<Opened> {
        private_dir(dir)?;
        let path = dir.join(FIFO_NAME);
        let created = if reusable(&path)? {
            false
        } else {
            replace(dir, &path)?;
            true
        };
        let rw = open_fifo(&path, true)?;
        let ro = open_fifo(&path, false)?;
        // What was opened is what was checked: both descriptors on one
        // private FIFO of the daemon's uid.
        let meta = rw.metadata()?;
        let other = ro.metadata()?;
        if !meta.file_type().is_fifo()
            || meta.uid() != euid()
            || !private(meta.mode())
            || (meta.dev(), meta.ino()) != (other.dev(), other.ino())
        {
            return Err(io::Error::other(format!(
                "{} changed while it was opened",
                path.display()
            )));
        }
        let jobserver = Jobserver {
            path,
            rw,
            ro,
            dev: meta.dev(),
            ino: meta.ino(),
            tokens,
        };
        let (restored, drained) = jobserver.settle()?;
        Ok(Opened {
            jobserver,
            created,
            restored,
            drained,
        })
    }

    /// The FIFO's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The pool when no build runs.
    pub(crate) fn tokens(&self) -> u32 {
        self.tokens
    }

    /// The FIFO's device and inode, which its holders' descriptors name.
    pub(crate) fn id(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    /// Whether the path still names this FIFO: false once it was removed
    /// or replaced, and commands told the path would open something else.
    pub(crate) fn at_path(&self) -> bool {
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| (meta.dev(), meta.ino()) == (self.dev, self.ino))
    }

    /// Tokens in the FIFO now.
    pub(crate) fn available(&self) -> io::Result<u32> {
        let mut count: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int through the pointer, which
        // points at a live c_int; the descriptor is open for the call.
        #[allow(
            unsafe_code,
            reason = "ioctl(FIONREAD) on the FIFO's read-only descriptor"
        )]
        let result = unsafe { libc::ioctl(self.ro.as_raw_fd(), libc::FIONREAD, &raw mut count) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u32::try_from(count).unwrap_or(0))
    }

    /// Fills the pool to [`Jobserver::tokens`] or drains it down to it:
    /// `(written, drained)`.
    pub(crate) fn settle(&self) -> io::Result<(u32, u32)> {
        let available = self.available()?;
        match available.cmp(&self.tokens) {
            std::cmp::Ordering::Less => Ok((self.write_tokens(self.tokens - available)?, 0)),
            std::cmp::Ordering::Greater => Ok((0, self.drain(available - self.tokens)?)),
            std::cmp::Ordering::Equal => Ok((0, 0)),
        }
    }

    /// Writes `count` tokens; how many went in.
    pub(crate) fn write_tokens(&self, count: u32) -> io::Result<u32> {
        let bytes = vec![TOKEN; usize::try_from(count).unwrap_or(0)];
        let mut written = 0;
        while written < bytes.len() {
            match (&self.rw).write(&bytes[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(u32::try_from(written).unwrap_or(u32::MAX))
    }

    /// Reads up to `count` tokens out and drops them; how many came out.
    pub(crate) fn drain(&self, count: u32) -> io::Result<u32> {
        let mut buffer = vec![0; usize::try_from(count).unwrap_or(0)];
        let mut read = 0;
        while read < buffer.len() {
            match (&self.rw).read(&mut buffer[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(u32::try_from(read).unwrap_or(u32::MAX))
    }

    /// Removes the path at a graceful stop, if it is still this FIFO.
    /// Builds already running keep their descriptors and their pool; one
    /// started while the daemon is down finds no jobserver and runs
    /// unlimited with a warning, which beats starving on a FIFO nobody
    /// fills.
    pub(crate) fn unlink(&self) {
        if self.at_path()
            && let Err(error) = std::fs::remove_file(&self.path)
        {
            tracing::warn!(path = %self.path.display(), %error, "cannot remove the jobserver FIFO");
        }
    }
}

/// Removes a FIFO the daemon's uid owns at `dir`'s jobserver path, left
/// by an earlier run: with the limit off, a command still naming it would
/// otherwise open a FIFO nobody fills and build one job at a time.
pub(crate) fn remove_stale(dir: &Path) {
    let path = dir.join(FIFO_NAME);
    let stale = std::fs::symlink_metadata(&path)
        .is_ok_and(|meta| meta.file_type().is_fifo() && meta.uid() == euid());
    if stale && let Err(error) = std::fs::remove_file(&path) {
        tracing::warn!(path = %path.display(), %error, "cannot remove the old jobserver FIFO");
    }
}

/// Whether `mode` has no group or other permission bits.
fn private(mode: u32) -> bool {
    let group_or_other = mode & 0o077;
    group_or_other == 0
}

/// The daemon's effective uid.
pub(crate) fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    #[allow(unsafe_code, reason = "geteuid(2)")]
    unsafe {
        libc::geteuid()
    }
}

/// Makes `dir` a `0700` directory the daemon's uid owns: creates it, or
/// tightens its mode, or moves aside whatever else is at the path.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            if meta.uid() != euid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is owned by uid {}, not the daemon's",
                        dir.display(),
                        meta.uid()
                    ),
                ));
            }
            if !private(meta.mode()) {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            return Ok(());
        }
        Ok(_) => move_aside(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Whether `path` is a FIFO the daemon can reuse: its uid's, with no group
/// or other permission bits. `false` when nothing is there.
fn reusable(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.file_type().is_fifo() && meta.uid() == euid() && private(meta.mode())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Moves whatever is at `path` aside, then makes a new FIFO there: made at
/// a temporary name and renamed over the path, so a client never sees a
/// half-made one.
fn replace(dir: &Path, path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        move_aside(path)?;
    }
    let temporary = dir.join(format!("{FIFO_NAME}.{}.tmp", nanos()));
    make_fifo(&temporary)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Renames `path` to `<path>.trash-<nanos>` and removes it if it is a file
/// or a FIFO; a directory is left for the person to look at.
fn move_aside(path: &Path) -> io::Result<()> {
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".trash-{}", nanos()));
    let aside = PathBuf::from(aside);
    std::fs::rename(path, &aside)?;
    tracing::warn!(
        path = %path.display(),
        moved_to = %aside.display(),
        "moved aside what failed the build limit's checks"
    );
    if !aside.is_dir() {
        let _ = std::fs::remove_file(&aside);
    }
    Ok(())
}

fn make_fifo(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in the FIFO's path"))?;
    // SAFETY: `name` is a NUL-terminated path that lives for the call.
    #[allow(unsafe_code, reason = "mkfifo(3)")]
    let result = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // mkfifo applies the umask, which can only remove bits; make sure.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn open_fifo(path: &Path, write: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_reuse_replace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("build");
        let opened = Jobserver::open(&dir, 7).unwrap();
        assert!(opened.created);
        assert_eq!(opened.restored, 7);
        let meta = std::fs::symlink_metadata(dir.join(FIFO_NAME)).unwrap();
        assert!(meta.file_type().is_fifo());
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(opened.jobserver.available().unwrap(), 7);

        // A second open while the first holds it: reused, nothing to add.
        let again = Jobserver::open(&dir, 7).unwrap();
        assert!(!again.created);
        assert_eq!((again.restored, again.drained), (0, 0));
        // A smaller pool drains the surplus.
        let smaller = Jobserver::open(&dir, 3).unwrap();
        assert_eq!(smaller.drained, 4);
        assert_eq!(smaller.jobserver.available().unwrap(), 3);
        drop((opened, again, smaller));

        // Everything closed: a reopen fills the pool to the new size. The
        // kernel usually drops the buffer with the last descriptor, but a
        // process another test forks at that moment holds the descriptors
        // until its exec and keeps the tokens in it (#290's macOS CI), so
        // only the pool is asserted, not how much of it was restored.
        let reopened = Jobserver::open(&dir, 5).unwrap();
        assert!(!reopened.created);
        assert!(reopened.restored <= 5);
        assert_eq!(reopened.jobserver.available().unwrap(), 5);
        drop(reopened);

        // A FIFO with group bits, a regular file and a symlink are replaced.
        let path = dir.join(FIFO_NAME);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
        assert!(Jobserver::open(&dir, 2).unwrap().created);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "x").unwrap();
        assert!(Jobserver::open(&dir, 2).unwrap().created);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path(), &path).unwrap();
        let replaced = Jobserver::open(&dir, 2).unwrap();
        assert!(replaced.created);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        // A directory with other bits is tightened.
        drop(replaced);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        Jobserver::open(&dir, 2).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn top_up_and_drain() {
        let temp = tempfile::tempdir().unwrap();
        let opened = Jobserver::open(temp.path(), 4).unwrap();
        let jobserver = opened.jobserver;
        // A client takes three tokens.
        let mut client = open_fifo(jobserver.path(), true).unwrap();
        let mut taken = [0; 3];
        client.read_exact(&mut taken).unwrap();
        assert_eq!(taken, [TOKEN; 3]);
        assert_eq!(jobserver.available().unwrap(), 1);
        assert_eq!(jobserver.settle().unwrap(), (3, 0));
        assert_eq!(jobserver.available().unwrap(), 4);
        // It returns them late: a surplus, drained.
        client.write_all(&taken).unwrap();
        assert_eq!(jobserver.available().unwrap(), 7);
        assert_eq!(jobserver.settle().unwrap(), (0, 3));
        assert_eq!(jobserver.available().unwrap(), 4);
    }

    #[test]
    fn unlink_at_stop() {
        let temp = tempfile::tempdir().unwrap();
        let jobserver = Jobserver::open(temp.path(), 2).unwrap().jobserver;
        let client = open_fifo(jobserver.path(), true).unwrap();
        jobserver.unlink();
        assert!(!jobserver.path().exists());
        // A descriptor opened before still reads the old pool.
        let mut token = [0; 1];
        (&client).read_exact(&mut token).unwrap();
        assert_eq!(token, [TOKEN]);
    }
}
