//! Free space on the volumes Reef writes to (#272, ADR 0076).
//!
//! `statvfs` answers in microseconds; on macOS the volume's
//! "available for important usage" capacity, which adds what the system
//! purges under pressure (Time Machine's local snapshots, caches), takes 9
//! to 43 ms through CoreFoundation (experiment D1) and is usually larger
//! (not always: the floor takes the larger of the two).
//! So the floor reads `statvfs` first and asks CoreFoundation only for a
//! volume under twice its floor, where the answer can change the level;
//! that answer is reused for 5 seconds.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// A volume's size and free space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// Its size.
    pub total: u64,
    /// Available to an unprivileged user (`statvfs`'s `f_bavail`: on ext4
    /// the root-reserved blocks are not available, D7).
    pub free: u64,
}

/// `statvfs` of the volume holding `path`.
#[allow(
    clippy::useless_conversion,
    reason = "fsblkcnt_t and c_ulong are u64 on Linux but not on every target"
)]
pub fn space(path: &Path) -> io::Result<Space> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    let unit = u64::from(stat.fragment_size());
    Ok(Space {
        total: u64::from(stat.blocks()).saturating_mul(unit),
        free: u64::from(stat.blocks_available()).saturating_mul(unit),
    })
}

/// The device `path` is on.
pub fn device(path: &Path) -> io::Result<u64> {
    std::fs::metadata(path).map(|metadata| metadata.dev())
}

/// `path`, or its nearest ancestor that exists: where a directory Reef
/// has not made yet will be.
pub fn existing_ancestor(path: &Path) -> PathBuf {
    let mut at = path;
    loop {
        if at.exists() {
            return at.to_owned();
        }
        match at.parent() {
            Some(parent) => at = parent,
            None => return PathBuf::from("/"),
        }
    }
}

/// The larger of 20 GiB and 5% of a volume of `total` bytes: the floor's
/// `auto`.
pub fn auto_floor(total: u64) -> u64 {
    (20 << 30_u64).max(total / 20)
}

/// 25% of a volume of `total` bytes: the budget's `auto`.
pub fn auto_budget(total: u64) -> u64 {
    total / 4
}

/// macOS: free space plus what the system would purge for an important
/// write, for the volume holding `path`; `None` elsewhere, if it cannot be
/// read, or when it reads 0 (a process without a login session).
#[cfg(target_os = "macos")]
pub fn purgeable_free(path: &Path) -> Option<u64> {
    cf::important_usage_capacity(path)
}

/// macOS only.
#[cfg(not(target_os = "macos"))]
pub fn purgeable_free(_path: &Path) -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
mod cf {
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use core_foundation_sys::base::{Boolean, CFRelease, CFTypeRef};
    use core_foundation_sys::error::CFErrorRef;
    use core_foundation_sys::number::{CFNumberGetValue, CFNumberRef, kCFNumberSInt64Type};
    use core_foundation_sys::string::CFStringRef;
    use core_foundation_sys::url::{CFURLCreateFromFileSystemRepresentation, CFURLRef};

    // `core-foundation-sys` 0.8.7 has both commented out.
    #[link(name = "CoreFoundation", kind = "framework")]
    #[allow(unsafe_code, reason = "declarations of two CoreFoundation symbols")]
    unsafe extern "C" {
        static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
        fn CFURLCopyResourcePropertyForKey(
            url: CFURLRef,
            key: CFStringRef,
            value: *mut CFTypeRef,
            error: *mut CFErrorRef,
        ) -> Boolean;
    }

    pub(super) fn important_usage_capacity(path: &Path) -> Option<u64> {
        let bytes = path.as_os_str().as_bytes();
        let length = isize::try_from(bytes.len()).ok()?;
        #[allow(
            unsafe_code,
            reason = "FFI; every object created here is released here"
        )]
        // SAFETY: `bytes` is valid for `length` bytes during the call; the
        // URL, the value and the error are each released once, and only when
        // non-null; `CFNumberGetValue` writes one i64 into `value`.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                length,
                Boolean::from(true),
            );
            if url.is_null() {
                return None;
            }
            let mut number: CFTypeRef = std::ptr::null();
            let mut error: CFErrorRef = std::ptr::null_mut();
            let found = CFURLCopyResourcePropertyForKey(
                url,
                kCFURLVolumeAvailableCapacityForImportantUsageKey,
                &raw mut number,
                &raw mut error,
            ) != 0;
            CFRelease(url.cast());
            if !error.is_null() {
                CFRelease(error.cast());
            }
            if !found || number.is_null() {
                return None;
            }
            let mut value: i64 = -1;
            let read = CFNumberGetValue(
                number.cast::<std::ffi::c_void>() as CFNumberRef,
                kCFNumberSInt64Type,
                (&raw mut value).cast(),
            );
            CFRelease(number);
            // A process with no login session (a LaunchDaemon, such as a CI
            // runner's) reads 0: unknown, not a full disk.
            if read && value > 0 {
                u64::try_from(value).ok()
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autos() {
        assert_eq!(auto_floor(100 << 30), 20 << 30);
        assert_eq!(auto_floor(512 << 30), (512 << 30) / 20);
        assert_eq!(auto_budget(400 << 30), 100 << 30);
    }

    #[test]
    fn space_and_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let space = space(temp.path()).unwrap();
        assert!(space.total > 0 && space.free <= space.total);
        assert_eq!(
            existing_ancestor(&temp.path().join("a/b/c")),
            temp.path().to_owned()
        );
        assert_eq!(
            device(temp.path()).unwrap(),
            device(&existing_ancestor(&temp.path().join("x"))).unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn purgeable_is_read() {
        // Usually above `statvfs`'s free space, but not always (a CI
        // runner's was below it): the floor takes the larger of the two.
        // Without a login session it is unknown (`None`), never 0.
        let temp = tempfile::tempdir().unwrap();
        let Some(purgeable) = purgeable_free(temp.path()) else {
            return;
        };
        assert!(purgeable > 0);
        assert!(purgeable <= space(temp.path()).unwrap().total);
    }
}
