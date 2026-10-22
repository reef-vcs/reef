//! Which backends can serve a directory, and the filesystem it is on.
//!
//! Each backend has a real check in `probe_kind`: R1.08 clones a probe
//! file on APFS, R1.09 checks `CAP_SYS_ADMIN` or the `fuse-overlayfs`
//! binary and makes a trial mount, R1.10 clones a probe file with
//! `FICLONE`. A kind this platform does not have is unavailable with that
//! as the reason; `Copy` is always available. Clone backends also need the
//! base cache on the workspace's volume, so the probe takes the base cache
//! directory.

use std::io;
use std::path::{Path, PathBuf};

use reef_lib::error::ErrorChain;

use super::{BackendKind, OverlayConfig};

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

/// Reason `apfs` is unavailable off macOS.
#[cfg(not(target_os = "macos"))]
pub(crate) const APFS_MACOS_ONLY: &str = "APFS clones are macOS only";

/// Reason the overlay kinds are unavailable off Linux.
#[cfg(not(target_os = "linux"))]
pub(crate) const OVERLAY_LINUX_ONLY: &str = "overlayfs is Linux only";

/// Reason `reflink` is unavailable off Linux.
#[cfg(not(target_os = "linux"))]
pub(crate) const REFLINK_LINUX_ONLY: &str = "reflink (FICLONE) is Linux only";

/// Probes every backend for workspace roots under `dir` built from bases in
/// `bases_dir`, one entry per kind in [`BackendKind::AUTO_ORDER`] order.
/// `dir` need not exist yet (the default workspace root is created on first
/// use); its nearest existing ancestor is probed instead. Without
/// `bases_dir` (the daemon's status, for no repository in particular),
/// clone backends report whether `dir`'s volume can clone at all.
///
/// The `apfs` and `reflink` probes write, clone, and remove a one-byte file
/// in that directory, and keep a `.probe` file in `bases_dir`; `reflink`
/// remembers a success for the daemon's lifetime. The overlay probes
/// (with `overlay`'s settings) mount and unmount an empty overlay in a
/// `.reef-probe-*` directory there and remove it.
pub fn probe(dir: &Path, bases_dir: Option<&Path>, overlay: &OverlayConfig) -> Vec<Probe> {
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
            availability: probe_kind(kind, &target, bases_dir, overlay),
            filesystem: filesystem.clone(),
        })
        .collect()
}

#[allow(
    unused_variables,
    reason = "each platform's probes read some of the arguments: the clones the base cache, the overlays the config"
)]
fn probe_kind(
    kind: BackendKind,
    target: &Path,
    bases_dir: Option<&Path>,
    overlay: &OverlayConfig,
) -> Availability {
    match kind {
        BackendKind::Copy => Availability::Available,
        #[cfg(target_os = "macos")]
        BackendKind::ApfsClone => super::apfs::probe_apfs(bases_dir, target),
        #[cfg(not(target_os = "macos"))]
        BackendKind::ApfsClone => Availability::Unavailable(APFS_MACOS_ONLY.to_owned()),
        #[cfg(target_os = "linux")]
        BackendKind::Reflink => super::reflink::probe_reflink(bases_dir, target),
        #[cfg(not(target_os = "linux"))]
        BackendKind::Reflink => Availability::Unavailable(REFLINK_LINUX_ONLY.to_owned()),
        #[cfg(target_os = "linux")]
        BackendKind::Overlayfs => super::overlay::probe_kernel(target, overlay),
        #[cfg(target_os = "linux")]
        BackendKind::FuseOverlayfs => super::overlay::probe_fuse(target, overlay),
        #[cfg(not(target_os = "linux"))]
        BackendKind::Overlayfs | BackendKind::FuseOverlayfs => {
            Availability::Unavailable(OVERLAY_LINUX_ONLY.to_owned())
        }
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
        BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType, OVERLAYFS_SUPER_MAGIC, TMPFS_MAGIC,
    };
    // nix leaves XFS's out on musl, which the release binaries use (R1.28).
    const XFS_SUPER_MAGIC: FsType = FsType(0x5846_5342);

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
        let probes = probe(
            &dir.path().join("not/yet/created"),
            Some(&dir.path().join("bases")),
            &OverlayConfig::default(),
        );
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
                #[cfg(target_os = "macos")]
                BackendKind::ApfsClone => assert_eq!(probe.availability, Availability::Available),
                #[cfg(not(target_os = "macos"))]
                BackendKind::ApfsClone => assert_eq!(
                    probe.availability,
                    Availability::Unavailable(APFS_MACOS_ONLY.to_owned())
                ),
                #[cfg(not(target_os = "linux"))]
                BackendKind::Overlayfs | BackendKind::FuseOverlayfs => assert_eq!(
                    probe.availability,
                    Availability::Unavailable(OVERLAY_LINUX_ONLY.to_owned())
                ),
                // Either, depending on privilege and `fuse-overlayfs`; the
                // overlay module's tests check the reasons.
                #[cfg(target_os = "linux")]
                BackendKind::Overlayfs | BackendKind::FuseOverlayfs => {}
                #[cfg(not(target_os = "linux"))]
                BackendKind::Reflink => assert_eq!(
                    probe.availability,
                    Availability::Unavailable(REFLINK_LINUX_ONLY.to_owned())
                ),
                // Available when the temporary directory is on btrfs or
                // XFS; the reflink module's tests check the reasons.
                #[cfg(target_os = "linux")]
                BackendKind::Reflink => {}
            }
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "bases")
            .collect();
        assert!(leftovers.is_empty(), "probes left {leftovers:?}");
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
