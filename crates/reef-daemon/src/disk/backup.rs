//! macOS: Time Machine's sticky exclusion on Reef's caches (#272, ADR 0076,
//! S-199): the default workspace root, the base cache and the stores.
//!
//! The exclusion is the extended attribute `tmutil addexclusion` writes
//! without `-p` (experiment D5): no privilege, and it travels with the
//! directory. Each path is marked once; the daemon records that it did in
//! `meta`, so a user who removes the exclusion is not overridden.

use std::path::Path;

use crate::db::{Db, DbError};

/// The attribute's name.
pub const XATTR: &str = "com.apple.metadata:com_apple_backup_excludeItem";

/// Its value: a binary property list holding the string
/// `com.apple.backupd`, byte for byte what `tmutil addexclusion` writes.
pub const VALUE: &[u8] = &[
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, 0x5F, 0x10, 0x11, 0x63, 0x6F, 0x6D, 0x2E, 0x61,
    0x70, 0x70, 0x6C, 0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, 0x64, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C,
];

/// The `meta` key recording that `path` was marked.
fn meta_key(path: &Path) -> String {
    format!("disk.backup_excluded:{}", path.display())
}

/// Marks `path` excluded from Time Machine unless this daemon marked it
/// before. `Ok(true)` if it marked it now. A path that does not exist is
/// left for a later call.
pub fn exclude_once(db: &Db, path: &Path) -> Result<bool, DbError> {
    let key = meta_key(path);
    if db.meta(&key)?.is_some() || !path.is_dir() {
        return Ok(false);
    }
    match set(path) {
        Ok(()) => {
            db.set_meta(&key, "1")?;
            tracing::info!(path = %path.display(), "excluded from Time Machine");
            Ok(true)
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot exclude from Time Machine");
            Ok(false)
        }
    }
}

/// Writes the attribute.
#[cfg(target_os = "macos")]
pub fn set(path: &Path) -> std::io::Result<()> {
    xattr_set(path, VALUE)
}

#[cfg(target_os = "macos")]
fn xattr_set(path: &Path, value: &[u8]) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes())?;
    let name = CString::new(XATTR)?;
    #[allow(
        unsafe_code,
        reason = "setxattr(2) with NUL-terminated strings and a borrowed value"
    )]
    // SAFETY: `path` and `name` are valid C strings and `value` is valid for
    // `value.len()` bytes for the whole call; the kernel keeps no pointer.
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// macOS only: there is no Time Machine elsewhere.
#[cfg(not(target_os = "macos"))]
pub fn set(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Whether `path` carries the attribute (tests).
#[cfg(all(test, target_os = "macos"))]
fn is_set(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let (Ok(path), Ok(name)) = (
        CString::new(path.as_os_str().as_bytes()),
        CString::new(XATTR),
    ) else {
        return false;
    };
    let mut buffer = [0_u8; 128];
    #[allow(
        unsafe_code,
        reason = "getxattr(2) into a stack buffer of its real length"
    )]
    // SAFETY: valid C strings and a writable buffer of the length passed.
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    usize::try_from(read).is_ok_and(|read| &buffer[..read] == VALUE)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn once_and_respecting_removal() {
        let db = Db::open_in_memory().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("reef");
        assert!(!exclude_once(&db, &dir).unwrap(), "missing: left for later");
        std::fs::create_dir(&dir).unwrap();
        assert!(exclude_once(&db, &dir).unwrap());
        assert!(is_set(&dir));
        // The user removes it: never set again.
        std::process::Command::new("xattr")
            .args(["-d", XATTR])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(!is_set(&dir));
        assert!(!exclude_once(&db, &dir).unwrap());
        assert!(!is_set(&dir));
    }

    /// `tmutil` agrees (D5); skipped where `tmutil` cannot answer.
    #[test]
    fn tmutil_sees_it() {
        let temp = tempfile::tempdir().unwrap();
        set(temp.path()).unwrap();
        let Ok(out) = std::process::Command::new("tmutil")
            .arg("isexcluded")
            .arg(temp.path())
            .output()
        else {
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        if out.status.success() {
            assert!(text.contains("[Excluded]"), "{text}");
        }
    }
}
