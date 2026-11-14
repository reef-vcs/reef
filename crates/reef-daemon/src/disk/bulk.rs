//! macOS: one directory's entries with `getattrlistbulk(2)`, which returns
//! each entry's type, mtime, file id, link count, sizes and APFS private
//! size in one call per buffer, 1.3 to 1.7× faster than a stat and a
//! `getattrlist` per file (#272, experiments D2 and D3).

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use super::walk::{Kind, Seen};

/// `<sys/attr.h>`: a per-entry error, returned right after the returned
/// attribute set when asked for. Not in `libc` 0.2.191.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

/// `<sys/vnode.h>`'s `enum vtype`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// The buffer one call fills: about 900 entries of this layout.
const BUFFER: usize = 128 * 1024;

/// What is asked for, in the kernel's packing order (common attributes by
/// bit, then file attributes by bit, then the extended common ones).
fn request() -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT
            | libc::ATTR_FILE_ALLOCSIZE
            | libc::ATTR_FILE_DATALENGTH,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    }
}

/// Every entry of `dir` (not `.` or `..`), without following a symlink.
/// Entries the kernel reports an error for are left out.
pub(super) fn read_dir(dir: &Path) -> io::Result<Vec<(OsString, Seen)>> {
    let path = CString::new(dir.as_os_str().as_bytes())?;
    #[allow(
        unsafe_code,
        reason = "open(2) of a NUL-terminated path; the descriptor is owned below"
    )]
    // SAFETY: `path` is a valid C string for the call.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(unsafe_code, reason = "taking ownership of the descriptor just opened")]
    // SAFETY: `fd` was just opened here and is closed only by the `OwnedFd`.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let mut buffer = vec![0_u8; BUFFER];
    let mut entries = Vec::new();
    loop {
        let count = bulk(&fd, &mut buffer)?;
        if count == 0 {
            return Ok(entries);
        }
        parse(&buffer, count, &mut entries);
    }
}

/// One `getattrlistbulk` call into `buffer`: how many entries it packed.
fn bulk(fd: &std::os::fd::OwnedFd, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd as _;

    let mut attributes = request();
    #[allow(
        unsafe_code,
        reason = "FFI; the request and buffer are valid, and the buffer's real length is passed"
    )]
    // SAFETY: `attributes` is a valid `attrlist` and `buffer` a writable
    // region of `buffer.len()` bytes for the whole call; the kernel keeps
    // neither pointer.
    let count = unsafe {
        libc::getattrlistbulk(
            fd.as_raw_fd(),
            (&raw mut attributes).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            u64::from(libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED),
        )
    };
    usize::try_from(count).map_err(|_| io::Error::last_os_error())
}

/// Reads `count` packed entries from `buffer` into `out`. A malformed
/// entry ends the parse (the rest of the buffer cannot be trusted).
pub(super) fn parse(buffer: &[u8], count: usize, out: &mut Vec<(OsString, Seen)>) {
    let mut offset = 0;
    for _ in 0..count {
        let Some(length) = read_u32(buffer, offset) else {
            return;
        };
        let length = length as usize;
        let Some(entry) = buffer.get(offset..offset + length) else {
            return;
        };
        if length < 24 {
            return;
        }
        if let Some(parsed) = parse_entry(entry) {
            out.push(parsed);
        }
        offset += length;
    }
}

/// One entry: the length, the returned set (five words: common, volume,
/// directory, file, fork/extended), then the attributes in order.
fn parse_entry(entry: &[u8]) -> Option<(OsString, Seen)> {
    let common = read_u32(entry, 4)?;
    let file = read_u32(entry, 4 + 12)?;
    let extended = read_u32(entry, 4 + 16)?;
    let mut at = 24;
    if common & ATTR_CMN_ERROR != 0 {
        let error = read_u32(entry, at)?;
        at += 4;
        if error != 0 {
            return None;
        }
    }
    let mut name = None;
    if common & libc::ATTR_CMN_NAME != 0 {
        // An `attrreference_t`: an offset from itself and a length with
        // the NUL.
        let relative = i32::from_ne_bytes(entry.get(at..at + 4)?.try_into().ok()?);
        let length = read_u32(entry, at + 4)? as usize;
        let start = usize::try_from(i64::try_from(at).ok()? + i64::from(relative)).ok()?;
        let bytes = entry.get(start..start + length.checked_sub(1)?)?;
        name = Some(OsStr::from_bytes(bytes).to_owned());
        at += 8;
    }
    let mut kind = Kind::Other;
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        kind = match read_u32(entry, at)? {
            VREG => Kind::File,
            VDIR => Kind::Dir,
            VLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        at += 4;
    }
    let mut mtime_ms = None;
    if common & libc::ATTR_CMN_MODTIME != 0 {
        let seconds = read_i64(entry, at)?;
        let nanos = read_i64(entry, at + 8)?;
        mtime_ms = seconds
            .checked_mul(1_000)
            .and_then(|ms| ms.checked_add(nanos / 1_000_000));
        at += 16;
    }
    let mut file_id = None;
    if common & libc::ATTR_CMN_FILEID != 0 {
        file_id = Some(read_i64(entry, at)?.cast_unsigned());
        at += 8;
    }
    let (mut links, mut allocated, mut size) = (1, 0, 0);
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        links = read_u32(entry, at)?;
        at += 4;
    }
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        allocated = read_i64(entry, at)?.max(0).cast_unsigned();
        at += 8;
    }
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        size = read_i64(entry, at)?.max(0).cast_unsigned();
        at += 8;
    }
    let private = (extended & libc::ATTR_CMNEXT_PRIVATESIZE != 0)
        .then(|| read_i64(entry, at))
        .flatten()
        .map(|bytes| bytes.max(0).cast_unsigned());
    let name = name?;
    if name == "." || name == ".." {
        return None;
    }
    Some((
        name,
        Seen {
            kind,
            allocated,
            private,
            size,
            mtime_ms,
            hard_link: (kind != Kind::Dir && links > 1)
                .then_some(file_id)
                .flatten()
                .map(|id| (0, id)),
        },
    ))
}

fn read_u32(buffer: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(buffer.get(at..at + 4)?.try_into().ok()?))
}

fn read_i64(buffer: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_ne_bytes(buffer.get(at..at + 8)?.try_into().ok()?))
}

/// For tests: an entry as the kernel packs it.
#[cfg(test)]
pub(super) fn pack(name: &str, kind: u32, allocated: i64, private: Option<i64>) -> Vec<u8> {
    let mut entry = vec![0_u8; 24];
    let mut common = libc::ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FILEID;
    common &= !libc::ATTR_CMN_RETURNED_ATTRS;
    let file = if kind == VDIR {
        0
    } else {
        libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH
    };
    entry[4..8].copy_from_slice(&common.to_ne_bytes());
    entry[16..20].copy_from_slice(&file.to_ne_bytes());
    if let Some(_private) = private {
        entry[20..24].copy_from_slice(&libc::ATTR_CMNEXT_PRIVATESIZE.to_ne_bytes());
    }
    entry.extend(0_u32.to_ne_bytes()); // error
    let name_at = entry.len();
    entry.extend([0_u8; 8]); // the reference, filled below
    entry.extend(kind.to_ne_bytes());
    entry.extend(1_700_000_000_i64.to_ne_bytes());
    entry.extend(250_000_000_i64.to_ne_bytes());
    entry.extend(42_i64.to_ne_bytes());
    if file != 0 {
        entry.extend(1_u32.to_ne_bytes());
        entry.extend(allocated.to_ne_bytes());
        entry.extend(10_i64.to_ne_bytes());
    }
    if let Some(private) = private {
        entry.extend(private.to_ne_bytes());
    }
    let data_at = entry.len();
    let relative = i32::try_from(data_at - name_at).unwrap();
    let length = u32::try_from(name.len() + 1).unwrap();
    entry[name_at..name_at + 4].copy_from_slice(&relative.to_ne_bytes());
    entry[name_at + 4..name_at + 8].copy_from_slice(&length.to_ne_bytes());
    entry.extend(name.as_bytes());
    entry.push(0);
    while !entry.len().is_multiple_of(4) {
        entry.push(0);
    }
    let total = u32::try_from(entry.len()).unwrap();
    entry[0..4].copy_from_slice(&total.to_ne_bytes());
    entry
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    #[test]
    fn parse_canned_entries() {
        let mut buffer = pack("a file", VREG, 4096, Some(4096));
        buffer.extend(pack("dir", VDIR, 0, None));
        buffer.extend(pack("odd-length-name", VREG, 8192, None));
        let mut out = Vec::new();
        parse(&buffer, 3, &mut out);
        let names: Vec<_> = out.iter().map(|(name, _)| name.to_string_lossy()).collect();
        assert_eq!(names, ["a file", "dir", "odd-length-name"]);
        assert_eq!(out[0].1.kind, Kind::File);
        assert_eq!(out[0].1.allocated, 4096);
        assert_eq!(out[0].1.private, Some(4096));
        assert_eq!(out[0].1.size, 10);
        assert_eq!(out[0].1.mtime_ms, Some(1_700_000_000_250));
        assert_eq!(out[1].1.kind, Kind::Dir);
        assert_eq!(out[2].1.private, None);
        // A truncated buffer stops the parse.
        let mut short = Vec::new();
        parse(&buffer[..buffer.len() - 4], 3, &mut short);
        assert_eq!(short.len(), 2);
    }

    #[test]
    fn errors_and_directories_with_a_private_size() {
        let mut failed = pack("unreadable", VREG, 4096, Some(4096));
        failed[24..28].copy_from_slice(&5_u32.to_ne_bytes());
        let mut buffer = failed;
        buffer.extend(pack("dir", VDIR, 0, Some(0)));
        let mut out = Vec::new();
        parse(&buffer, 2, &mut out);
        // The failed entry is left out; the next still parses.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "dir");
        assert_eq!(out[0].1.kind, Kind::Dir);
        assert_eq!(out[0].1.private, Some(0));
    }

    #[test]
    fn matches_stat() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("one"), vec![1_u8; 10_000]).unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("one", temp.path().join("link")).unwrap();
        std::fs::hard_link(temp.path().join("one"), temp.path().join("two")).unwrap();
        let mut entries = read_dir(temp.path()).unwrap();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<_> = entries
            .iter()
            .map(|(name, _)| name.to_string_lossy())
            .collect();
        assert_eq!(names, ["link", "one", "sub", "two"]);
        let one = &entries[1].1;
        let metadata = std::fs::symlink_metadata(temp.path().join("one")).unwrap();
        assert_eq!(one.kind, Kind::File);
        assert_eq!(one.size, 10_000);
        assert_eq!(one.allocated, metadata.blocks() * 512);
        assert_eq!(
            one.mtime_ms,
            crate::backend::fs_util::mtime_millis(&metadata)
        );
        assert_eq!(one.hard_link, Some((0, metadata.ino())));
        assert_eq!(entries[3].1.hard_link, one.hard_link);
        assert_eq!(entries[0].1.kind, Kind::Symlink);
        assert_eq!(entries[2].1.kind, Kind::Dir);
    }
}
