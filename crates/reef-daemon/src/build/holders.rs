//! Which processes hold the jobserver FIFO open (#271, ADR 0074).
//!
//! A build killed mid-job loses every token it held: nothing returns them.
//! When the pool has been short for a while and no process but the daemon
//! holds the FIFO, every missing token is lost, and the daemon writes them
//! back. Same-uid processes only: the kernel refuses to list another
//! user's descriptors, and one uid is one trust domain (ADR 0027).

use std::io;

/// The pids, other than the daemon's own, with a descriptor open on the
/// file at `dev` and `ino`.
pub(crate) fn holders(dev: u64, ino: u64) -> io::Result<Vec<u32>> {
    imp::holders(dev, ino)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;

    /// `PROC_PIDFDVNODEINFO` in `<sys/proc_info.h>`.
    const PROC_PIDFDVNODEINFO: libc::c_int = 1;

    /// `struct proc_fileinfo` in `<sys/proc_info.h>`, not in `libc`.
    #[repr(C)]
    #[allow(clippy::struct_field_names, reason = "the kernel's field names")]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }

    /// `struct vnode_fdinfo` in `<sys/proc_info.h>`, not in `libc`.
    #[repr(C)]
    struct VnodeFdInfo {
        pfi: ProcFileInfo,
        pvi: libc::vnode_info,
    }

    pub(super) fn holders(dev: u64, ino: u64) -> io::Result<Vec<u32>> {
        let me = std::process::id();
        let mut found = Vec::new();
        for pid in all_pids()? {
            let Ok(pid_u32) = u32::try_from(pid) else {
                continue;
            };
            if pid_u32 == me || pid_u32 == 0 {
                continue;
            }
            if holds(pid, dev, ino) {
                found.push(pid_u32);
            }
        }
        Ok(found)
    }

    fn all_pids() -> io::Result<Vec<libc::c_int>> {
        // SAFETY: a null buffer of size 0 asks for the count only.
        #[allow(unsafe_code, reason = "proc_listallpids(3) sizing call")]
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pids = vec![0 as libc::c_int; usize::try_from(count).unwrap_or(0) + 64];
        let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::c_int>())
            .unwrap_or(libc::c_int::MAX);
        // SAFETY: the buffer holds `bytes` bytes; the call writes at most that.
        #[allow(
            unsafe_code,
            reason = "proc_listallpids(3) writes at most `bytes` into the buffer"
        )]
        let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if listed < 0 {
            return Err(io::Error::last_os_error());
        }
        pids.truncate(usize::try_from(listed).unwrap_or(0));
        Ok(pids)
    }

    /// Whether `pid` has a vnode descriptor on `dev`/`ino`; false when its
    /// descriptors cannot be listed (another user's, or gone).
    fn holds(pid: libc::c_int, dev: u64, ino: u64) -> bool {
        // SAFETY: a null buffer of size 0 asks for the size only.
        #[allow(unsafe_code, reason = "proc_pidinfo(PROC_PIDLISTFDS) sizing call")]
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        let Ok(needed) = usize::try_from(needed) else {
            return false;
        };
        if needed == 0 {
            return false;
        }
        let entry = size_of::<libc::proc_fdinfo>();
        // Room for descriptors opened since the sizing call.
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(needed / entry + 16);
        let bytes = libc::c_int::try_from(fds.capacity() * entry).unwrap_or(libc::c_int::MAX);
        // SAFETY: the buffer has room for `bytes` bytes; the call writes at
        // most that and returns how many it wrote.
        #[allow(
            unsafe_code,
            reason = "proc_pidinfo writes at most `bytes` into the buffer"
        )]
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast::<c_void>(),
                bytes,
            )
        };
        let Ok(written) = usize::try_from(written) else {
            return false;
        };
        // SAFETY: proc_pidinfo initialized `written` bytes, whole entries.
        #[allow(unsafe_code, reason = "the entries proc_pidinfo wrote")]
        unsafe {
            fds.set_len((written / entry).min(fds.capacity()));
        }
        let device = u32::try_from(dev & u64::from(u32::MAX)).unwrap_or(0);
        fds.iter()
            .filter(|fd| i64::from(fd.proc_fdtype) == i64::from(libc::PROX_FDTYPE_VNODE))
            .any(|fd| {
                let mut buffer = std::mem::MaybeUninit::<VnodeFdInfo>::zeroed();
                let size = libc::c_int::try_from(size_of::<VnodeFdInfo>()).unwrap_or(0);
                // SAFETY: the buffer is a zeroed VnodeFdInfo of `size` bytes;
                // the call writes at most that.
                #[allow(unsafe_code, reason = "proc_pidfdinfo writes at most `size` bytes")]
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDVNODEINFO,
                        buffer.as_mut_ptr().cast::<c_void>(),
                        size,
                    )
                };
                if got != size {
                    return false;
                }
                // SAFETY: zeroed, then filled by the call; every field is
                // plain data valid at any bit pattern.
                #[allow(unsafe_code, reason = "a plain-data struct the kernel filled")]
                let filled = unsafe { buffer.assume_init() };
                filled.pvi.vi_stat.vst_dev == device && filled.pvi.vi_stat.vst_ino == ino
            })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::os::unix::fs::MetadataExt as _;

    pub(super) fn holders(dev: u64, ino: u64) -> io::Result<Vec<u32>> {
        let me = std::process::id();
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == me {
                continue;
            }
            // Another user's descriptors cannot be listed: skipped.
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let held = fds.filter_map(Result::ok).any(|fd| {
                std::fs::metadata(fd.path())
                    .is_ok_and(|meta| meta.dev() == dev && meta.ino() == ino)
            });
            if held {
                found.push(pid);
            }
        }
        Ok(found)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;

    pub(super) fn holders(_dev: u64, _ino: u64) -> io::Result<Vec<u32>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no holder scan on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn finds_children_not_self() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("held");
        std::fs::write(&path, "").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        // The test process itself holds it: not listed.
        let mine = std::fs::File::open(&path).unwrap();
        let held = holders(meta.dev(), meta.ino()).unwrap();
        assert!(held.is_empty(), "{held:?}");
        drop(mine);
        // A child holding it on its stdin is listed until it exits.
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(std::fs::File::open(&path).unwrap())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let found = holders(meta.dev(), meta.ino()).unwrap();
        assert_eq!(found, [child.id()]);
        child.kill().unwrap();
        child.wait().unwrap();
        let held = holders(meta.dev(), meta.ino()).unwrap();
        assert!(held.is_empty(), "{held:?}");
    }
}
