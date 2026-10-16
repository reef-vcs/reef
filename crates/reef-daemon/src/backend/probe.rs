//! Which backends can serve a directory, and the filesystem it is on.
//!
//! Each backend ticket replaces its arm in `probe_kind` with a real check
//! (R1.08 clones a probe file on APFS, R1.09 checks `CAP_SYS_ADMIN` and the
//! `fuse-overlayfs` binary, R1.10 clones a probe file with `FICLONE`). Until
//! then a kind is unavailable with the ticket named as the reason; `Copy`
//! is always available.

use std::io;
use std::path::{Path, PathBuf};

use reef_lib::error::ErrorChain;

use super::BackendKind;

/// Whether a backend can serve a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// It can.
    Available,
    /// It cannot, for this reason (shown to the user).
    Unavailable(String),
}

impl Availability {
    /// Whether this is [`Availability::Available`].
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// One backend's probe result for a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The backend.
    pub kind: BackendKind,
    /// Whether it can serve the directory.
    pub availability: Availability,
    /// The directory's filesystem, as [`filesystem_name`] names it, or
    /// `unknown` if it could not be read.
    pub filesystem: String,
}

/// Probes every backend for workspace roots under `dir`, one entry per
/// kind in [`BackendKind::AUTO_ORDER`] order. `dir` need not exist yet (the
/// default workspace root is created on first use); its nearest existing
/// ancestor is probed instead.
pub fn probe(dir: &Path) -> Vec<Probe> {
    let target = nearest_existing(dir);
    let filesystem = filesystem_name(&target).unwrap_or_else(|error| {
        tracing::debug!(
            path = %target.display(),
            error = %ErrorChain(&error),
            "cannot read the filesystem type"
        );
        "unknown".to_owned()
    });
    BackendKind::AUTO_ORDER
        .into_iter()
        .map(|kind| Probe {
            kind,
            availability: probe_kind(kind),
            filesystem: filesystem.clone(),
        })
        .collect()
}

fn probe_kind(kind: BackendKind) -> Availability {
    let pending =
        |ticket: &str| Availability::Unavailable(format!("not implemented until {ticket}"));
    match kind {
        BackendKind::Copy => Availability::Available,
        BackendKind::ApfsClone => pending("R1.08"),
        BackendKind::Reflink => pending("R1.10"),
        BackendKind::Overlayfs | BackendKind::FuseOverlayfs => pending("R1.09"),
    }
}

/// `dir`, or its nearest ancestor that exists. A relative path with no
/// existing ancestor resolves to the current directory.
fn nearest_existing(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.exists())
        .map_or_else(|| PathBuf::from("."), Path::to_owned)
}

/// The type of the filesystem holding `dir`: `apfs`, `btrfs`, `xfs`,
/// `ext4` (which also covers ext2 and ext3, sharing its magic), `overlay`,
/// `tmpfs`, or otherwise the name macOS reports or Linux's magic number in
/// hex.
#[cfg(target_os = "macos")]
pub fn filesystem_name(dir: &Path) -> io::Result<String> {
    let stat = nix::sys::statfs::statfs(dir).map_err(io::Error::from)?;
    Ok(stat.filesystem_type_name().to_owned())
}

/// The type of the filesystem holding `dir`: `apfs`, `btrfs`, `xfs`,
/// `ext4` (which also covers ext2 and ext3, sharing its magic), `overlay`,
/// `tmpfs`, or otherwise the name macOS reports or Linux's magic number in
/// hex.
#[cfg(target_os = "linux")]
pub fn filesystem_name(dir: &Path) -> io::Result<String> {
    use nix::sys::statfs::{
        BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, OVERLAYFS_SUPER_MAGIC, TMPFS_MAGIC, XFS_SUPER_MAGIC,
    };

    let fs_type = nix::sys::statfs::statfs(dir)
        .map_err(io::Error::from)?
        .filesystem_type();
    let known = [
        (BTRFS_SUPER_MAGIC, "btrfs"),
        (XFS_SUPER_MAGIC, "xfs"),
        (EXT4_SUPER_MAGIC, "ext4"),
        (OVERLAYFS_SUPER_MAGIC, "overlay"),
        (TMPFS_MAGIC, "tmpfs"),
    ];
    Ok(known
        .iter()
        .find(|(magic, _)| *magic == fs_type)
        .map_or_else(
            || format!("{:#x}", fs_type.0),
            |(_, name)| (*name).to_owned(),
        ))
}

/// The type of the filesystem holding `dir`; always `unknown` on this
/// platform.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn filesystem_name(_dir: &Path) -> io::Result<String> {
    Ok("unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_is_always_available_and_every_kind_is_probed() {
        let dir = tempfile::tempdir().unwrap();
        let probes = probe(&dir.path().join("not/yet/created"));
        let kinds: Vec<_> = probes.iter().map(|probe| probe.kind).collect();
        assert_eq!(kinds, BackendKind::AUTO_ORDER);
        for probe in &probes {
            assert_ne!(probe.filesystem, "");
            assert_ne!(
                probe.filesystem, "unknown",
                "the temp dir's filesystem is readable"
            );
            match probe.kind {
                BackendKind::Copy => assert_eq!(probe.availability, Availability::Available),
                _ => assert!(
                    matches!(&probe.availability, Availability::Unavailable(reason) if reason.starts_with("not implemented until R1.")),
                    "{probe:?}"
                ),
            }
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_temp_dir_is_apfs() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(filesystem_name(dir.path()).unwrap(), "apfs");
    }

    #[test]
    fn missing_ancestors_resolve_to_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(nearest_existing(&dir.path().join("a/b")), dir.path());
        assert_eq!(
            nearest_existing(Path::new("no-such-relative-dir")),
            Path::new(".")
        );
    }
}
