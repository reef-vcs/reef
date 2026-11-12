//! Disk figures: a volume's free space, and what a workspace or a cache
//! directory takes, both allocated and (on APFS) unshared.
//!
//! The walk is the harness's own, not the daemon's: the daemon's figure
//! counts every file that differs from the base at full size and does not
//! split build output from the rest (#146, which the follow-on ticket C
//! builds). Walks never follow symlinks, so a store mount counts nothing.

use std::collections::HashSet;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// `st_blocks` is in 512-byte units on macOS and Linux.
const BLOCK_UNIT: u64 = 512;

/// What one directory tree takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footprint {
    /// Every block in full, including blocks a clone shares with its source.
    pub allocated_bytes: u64,
    /// APFS's private bytes: what deleting the tree would free at once.
    /// `None` off APFS.
    pub unshared_bytes: Option<u64>,
    /// Regular files.
    pub files: u64,
}

impl Footprint {
    fn add(&mut self, allocated: u64, private: Option<u64>) {
        self.allocated_bytes += allocated;
        self.unshared_bytes = match (self.unshared_bytes, private) {
            (Some(sum), Some(bytes)) => Some(sum + bytes),
            _ => None,
        };
        self.files += 1;
    }

    /// The figure that frees space: unshared where known, else allocated.
    pub fn freeable_bytes(&self) -> u64 {
        self.unshared_bytes.unwrap_or(self.allocated_bytes)
    }
}

/// A workspace's disk after a round, build output apart from the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDisk {
    /// Under the repository's declared build directories (`build_dirs`).
    pub build: Footprint,
    /// Everything else: the tracked tree (mostly clones of the base) and
    /// other ignored files.
    pub other: Footprint,
    /// How long the walk took.
    pub walk_ms: u64,
}

/// Walks `root`, counting files under any of `build_dirs` (relative paths)
/// apart from the rest. A missing `root` is empty.
pub fn workspace(root: &Path, build_dirs: &[String]) -> io::Result<WorkspaceDisk> {
    let started = std::time::Instant::now();
    let unshared = cfg!(target_os = "macos");
    let empty = Footprint {
        unshared_bytes: unshared.then_some(0),
        ..Footprint::default()
    };
    let mut disk = WorkspaceDisk {
        build: empty,
        other: empty,
        walk_ms: 0,
    };
    let mut seen = HashSet::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.depth() == 0 && not_found(&error) => break,
            Err(error) => return Err(io::Error::other(error.to_string())),
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|e| io::Error::other(e.to_string()))?;
        if metadata.nlink() > 1 && !seen.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        let allocated = metadata.blocks() * BLOCK_UNIT;
        let private = if unshared {
            private_bytes(entry.path()).ok().flatten()
        } else {
            None
        };
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let is_build = build_dirs.iter().any(|dir| relative.starts_with(dir));
        if is_build {
            disk.build.add(allocated, private);
        } else {
            disk.other.add(allocated, private);
        }
    }
    disk.walk_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(disk)
}

/// What `root` takes, as one figure. A missing `root` is empty.
pub fn footprint(root: &Path) -> io::Result<Footprint> {
    Ok(workspace(root, &[String::new()])?.build)
}

fn not_found(error: &walkdir::Error) -> bool {
    error
        .io_error()
        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
}

/// Bytes available to an unprivileged user on the volume holding `path`
/// (`statvfs`).
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stat = statvfs(path)?;
    #[allow(
        clippy::useless_conversion,
        reason = "the field types differ by target"
    )]
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

/// The volume's size in bytes.
pub fn volume_bytes(path: &Path) -> io::Result<u64> {
    let stat = statvfs(path)?;
    #[allow(
        clippy::useless_conversion,
        reason = "the field types differ by target"
    )]
    Ok(u64::from(stat.f_blocks).saturating_mul(u64::from(stat.f_frsize)))
}

/// The volume's device number, to tell whether two paths share a volume.
pub fn device(path: &Path) -> io::Result<u64> {
    std::fs::metadata(path).map(|m| m.dev())
}

fn statvfs(path: &Path) -> io::Result<libc::statvfs> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    #[allow(
        unsafe_code,
        reason = "FFI; the path is a NUL-terminated string and the struct is written by the call"
    )]
    // SAFETY: `path` is a valid C string; on success the kernel fills the
    // whole `statvfs`, which is only read then.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        stat.assume_init()
    };
    Ok(stat)
}

/// macOS's "available for important usage" capacity of the volume holding
/// `path`: free space plus what the system would purge for it (Time
/// Machine's local snapshots, caches). Read through Foundation with
/// `osascript`, about 60 ms; `None` off macOS or if it fails.
pub fn important_free_bytes(path: &Path) -> Option<u64> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let script = format!(
        "ObjC.import('Foundation'); var k = 'NSURLVolumeAvailableCapacityForImportantUsageKey'; \
         var v = $.NSURL.fileURLWithPath({}).resourceValuesForKeysError([k], null); \
         String(ObjC.unwrap(v.objectForKey(k)))",
        serde_json::to_string(&path.to_string_lossy()).ok()?
    );
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// One file's APFS private size, `None` where the filesystem cannot tell.
#[cfg(target_os = "macos")]
pub fn private_bytes(path: &Path) -> io::Result<Option<u64>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    // A u32 length, the returned `attribute_set_t` (five u32s), then the
    // `off_t`, packed at 4-byte alignment (as reef-daemon's `apfs.rs`).
    let mut buffer = [0_u8; 64];
    #[allow(
        unsafe_code,
        reason = "FFI; the path is a NUL-terminated string, and the buffer's real length is passed"
    )]
    // SAFETY: `path` is a valid C string and `request` a valid `attrlist`
    // for the whole call; the kernel writes at most `buffer.len()` bytes.
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut request).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let u32_at = |at: usize| u32::from_ne_bytes(buffer[at..at + 4].try_into().unwrap_or_default());
    if u32_at(4 + 4 * 4) & libc::ATTR_CMNEXT_PRIVATESIZE == 0 {
        return Ok(None);
    }
    let size = i64::from_ne_bytes(buffer[24..32].try_into().unwrap_or_default());
    Ok(Some(u64::try_from(size).unwrap_or(0)))
}

/// One file's private size: unknown off macOS.
#[cfg(not(target_os = "macos"))]
pub fn private_bytes(_path: &Path) -> io::Result<Option<u64>> {
    Ok(None)
}

/// Bytes in GiB, for tables.
#[allow(clippy::cast_precision_loss, reason = "a display figure")]
pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1_u32 << 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_build_output_from_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/big"), vec![1_u8; 64 * 1024]).unwrap();
        std::fs::write(dir.path().join("src.rs"), b"fn main() {}").unwrap();
        std::os::unix::fs::symlink("/usr", dir.path().join("link")).unwrap();
        let disk = workspace(dir.path(), &["target".to_owned()]).unwrap();
        assert_eq!(disk.build.files, 1);
        assert_eq!(disk.other.files, 1);
        assert!(disk.build.allocated_bytes >= 64 * 1024);
        if cfg!(target_os = "macos") {
            assert!(disk.build.unshared_bytes.unwrap() >= 64 * 1024);
        }
        assert_eq!(footprint(&dir.path().join("missing")).unwrap().files, 0);
    }

    #[test]
    fn reads_free_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).unwrap() > 0);
        assert!(volume_bytes(dir.path()).unwrap() >= free_bytes(dir.path()).unwrap());
    }
}
