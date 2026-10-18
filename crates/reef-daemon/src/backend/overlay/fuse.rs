//! Overlays through the `fuse-overlayfs` binary, for an unprivileged
//! daemon.
//!
//! The binary runs without `-f`, so it daemonizes and its mount survives a
//! daemon restart like a kernel mount. Its parent process exits once the
//! mount is made (libfuse's `fuse_daemonize` waits for the child to set up),
//! so after a successful exit the mount is expected at once; it is still
//! polled for, up to [`MOUNT_TIMEOUT`]. Unmounting is `fusermount3 -u`
//! (`fusermount` where there is no `fusermount3`), `-uz` for a lazy one.

use std::ffi::OsStr;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};

use super::{
    FUSE_FSTYPE, MOUNT_POLL, MOUNT_TIMEOUT, MountError, Mounter, find_program, layer_options,
    mount_info,
};

/// How often the spawned `fuse-overlayfs` is checked for having exited.
const EXIT_POLL: Duration = Duration::from_millis(5);
/// At most this much of a failed program's standard error goes into the
/// error.
const STDERR_LIMIT: usize = 4096;

/// Mounts overlays by running `fuse-overlayfs`.
#[derive(Clone, Debug)]
pub(crate) struct FuseMounter {
    /// The `fuse-overlayfs` binary: a path, or a name looked up on `PATH`.
    binary: PathBuf,
}

impl FuseMounter {
    pub(crate) fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Mounter for FuseMounter {
    fn mount(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        target: &Path,
    ) -> Result<(), MountError> {
        let spawn_error = |source| MountError::Spawn {
            binary: self.binary.clone(),
            source,
        };
        let mut child = Command::new(&self.binary)
            .arg("-o")
            .arg(OsStr::from_bytes(&layer_options(lower, upper, work)))
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_error)?;
        let deadline = Instant::now() + MOUNT_TIMEOUT;
        let status = loop {
            match child.try_wait().map_err(spawn_error)? {
                Some(status) => break status,
                None if Instant::now() < deadline => std::thread::sleep(EXIT_POLL),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(MountError::Timeout {
                        target: target.to_owned(),
                    });
                }
            }
        };
        if !status.success() {
            return Err(MountError::Exited {
                binary: self.binary.clone(),
                status: status.to_string(),
                stderr: child.stderr.take().map(read_available).unwrap_or_default(),
            });
        }
        loop {
            let info = mount_info(target).map_err(|source| MountError::Mounts { source })?;
            if info.is_some_and(|info| info.fstype == FUSE_FSTYPE) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(MountError::Timeout {
                    target: target.to_owned(),
                });
            }
            std::thread::sleep(MOUNT_POLL);
        }
    }

    fn unmount(&self, target: &Path, lazy: bool) -> Result<(), MountError> {
        let binary = find_program("fusermount3")
            .or_else(|| find_program("fusermount"))
            .unwrap_or_else(|| PathBuf::from("fusermount3"));
        let output = Command::new(&binary)
            .arg(if lazy { "-uz" } else { "-u" })
            .arg(target)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| MountError::Spawn {
                binary: binary.clone(),
                source,
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let target = target.to_owned();
        if stderr.contains("busy") {
            Err(MountError::Busy { target })
        } else if stderr.contains("not found in") || stderr.contains("not mounted") {
            // fusermount3: "entry for <path> not found in /etc/mtab".
            Err(MountError::NotMounted { target })
        } else {
            Err(MountError::Exited {
                binary,
                status: output.status.to_string(),
                stderr,
            })
        }
    }
}

/// What a failed program wrote to its standard error, without blocking:
/// a process it forked may still hold the pipe open.
fn read_available(mut stderr: ChildStderr) -> String {
    let _ = fcntl(&stderr, FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while bytes.len() < STDERR_LIMIT {
        match stderr.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => bytes.extend_from_slice(chunk.get(..read).unwrap_or_default()),
        }
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}
