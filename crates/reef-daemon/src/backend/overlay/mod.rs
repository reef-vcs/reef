//! The overlayfs backends (Linux, R1.09, ADR 0016): the base cache entry is
//! the read-only lower layer and `<root>.upper` the writable upper one, so a
//! workspace costs one mount and its upper holds exactly what the agent
//! wrote. The merged view shows the lower's files with their own mtimes and
//! sizes, so the base's `tree_state` describes a fresh workspace as is.
//!
//! Two tiers share this module and differ only in their [`Mounter`]:
//! kernel overlayfs through `mount(2)` when the daemon has `CAP_SYS_ADMIN`
//! ([`KernelMounter`]), and the `fuse-overlayfs` binary otherwise
//! ([`FuseMounter`]). [`probe_kernel`] and [`probe_fuse`] decide which can
//! serve a directory with a trial mount.
//!
//! Mounts outlive the daemon: a kernel mount stays in the mount namespace,
//! and `fuse-overlayfs` daemonizes. [`reconcile_mounts`] runs at daemon
//! start, mounting live workspaces again where the mount is gone and
//! removing mounts no workspace owns.
//!
//! The kernel forbids changing a lower layer while it is mounted. Base cache
//! entries are immutable once built (R1.02), and GC never evicts one that an
//! `active` or `releasing` workspace uses (R1.17).

mod fuse;
mod kernel;

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::telemetry::elapsed_ms;
use walkdir::WalkDir;

pub(crate) use fuse::FuseMounter;
pub(crate) use kernel::KernelMounter;

use super::fs_util::{
    PROBE_CLONE_PREFIX, allocated_bytes, remove_all, remove_tree_in_background, walk_error,
};
use super::{
    Availability, Backend, BackendError, BackendKind, BaseEntry, DiskUsage, OverlayConfig,
    SwapOutcome, WorkspacePaths,
};
use crate::model::WorkspaceId;

/// Mode of the upper directory. The merged root takes the upper root's
/// attributes, so this is the workspace root's mode, as for the copy
/// backend's roots.
const UPPER_MODE: u32 = 0o755;
/// Mode of the work directory, which nobody but the overlay uses.
const WORK_MODE: u32 = 0o700;
/// How long a `fuse-overlayfs` mount may take to start and appear.
pub(crate) const MOUNT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often [`FuseMounter`] looks for its mount while waiting.
const MOUNT_POLL: Duration = Duration::from_millis(50);
/// A kernel overlay's type in `/proc/self/mounts`.
pub(crate) const KERNEL_FSTYPE: &str = "overlay";
/// The prefix of a FUSE mount's type in `/proc/self/mounts`. libfuse
/// names the rest after the server binary's file name:
/// `fuse.fuse-overlayfs`, or `fuse.<name>` for a renamed copy or a wrapper.
pub(crate) const FUSE_FSTYPE_PREFIX: &str = "fuse.";
/// The marker `fuse-overlayfs` puts in an opaque upper directory when it
/// cannot set the xattr. It records no change of its own.
const FUSE_OPAQUE_MARKER: &str = ".wh..wh..opq";
/// `CAP_SYS_ADMIN`'s bit in the capability sets of `/proc/self/status`.
const CAP_SYS_ADMIN: u32 = 21;
const PROC_MOUNTS: &str = "/proc/self/mounts";

/// Makes trial mount directory names unique among concurrent probes in one
/// process.
static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Overlay materialization: the base is the lower layer, `ws.upper` the
/// writable one, `ws.work` the overlay's scratch directory, and `ws.root`
/// the mount.
pub struct OverlayBackend {
    tier: Tier,
}

/// How the mounts are made.
pub(crate) enum Tier {
    /// Kernel overlayfs; the daemon has `CAP_SYS_ADMIN`.
    Kernel(KernelMounter),
    /// The `fuse-overlayfs` binary, unprivileged.
    Fuse(FuseMounter),
}

/// Mounts and unmounts overlays: the one part of the backend that differs
/// between the tiers.
pub(crate) trait Mounter: Send + Sync {
    /// Mounts `lower` (read-only) under `upper` (writable; `work` is on its
    /// filesystem) at `target`, an existing directory. Returns once the
    /// mount is in `/proc/self/mounts`.
    fn mount(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        target: &Path,
    ) -> Result<(), MountError>;

    /// Unmounts `target`. With `lazy`, detaches it at once while processes
    /// using it keep it alive until they let go; without, fails with
    /// [`MountError::Busy`] while it is in use.
    fn unmount(&self, target: &Path, lazy: bool) -> Result<(), MountError>;
}

/// Failure of a [`Mounter`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum MountError {
    /// `mount(2)` failed.
    #[error("mount an overlay at {}", target.display())]
    Mount {
        target: PathBuf,
        #[source]
        source: Errno,
    },
    /// `umount2(2)` failed.
    #[error("unmount {}", target.display())]
    Unmount {
        target: PathBuf,
        #[source]
        source: Errno,
    },
    /// The mount is in use; a lazy unmount would detach it.
    #[error("{} is busy", target.display())]
    Busy { target: PathBuf },
    /// A program could not be started.
    #[error("cannot run {}", binary.display())]
    Spawn {
        binary: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A program failed.
    #[error("{} exited with {status}: {stderr}", binary.display())]
    Exited {
        binary: PathBuf,
        status: String,
        stderr: String,
    },
    /// The mount did not appear within [`MOUNT_TIMEOUT`].
    #[error("{} was not mounted within {MOUNT_TIMEOUT:?}", target.display())]
    Timeout { target: PathBuf },
    /// There is no mount to unmount.
    #[error("{} is not mounted", target.display())]
    NotMounted { target: PathBuf },
    /// A path the mounter cannot pass on.
    #[error("{}: {reason}", path.display())]
    Unsupported { path: PathBuf, reason: &'static str },
    /// `/proc/self/mounts` could not be read.
    #[error("read {PROC_MOUNTS}")]
    Mounts {
        #[source]
        source: io::Error,
    },
}

impl Classify for MountError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Mount { source, .. } | Self::Unmount { source, .. } => match source {
                Errno::EPERM | Errno::EACCES => ErrorKind::PermissionDenied,
                Errno::EBUSY => ErrorKind::Conflict,
                _ => ErrorKind::Io,
            },
            Self::Busy { .. } => ErrorKind::Conflict,
            Self::Unsupported { .. } => ErrorKind::InvalidArgument,
            _ => ErrorKind::Io,
        }
    }
}

impl From<MountError> for BackendError {
    fn from(error: MountError) -> Self {
        let mount = |op, path, source| Self::Mount { op, path, source };
        match error {
            MountError::Mount { target, source } => mount("mount", target, source.into()),
            MountError::Unmount { target, source } => mount("unmount", target, source.into()),
            MountError::Busy { target } => mount("unmount", target, Errno::EBUSY.into()),
            MountError::Timeout { target } => mount(
                "mount",
                target,
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("not mounted within {MOUNT_TIMEOUT:?}"),
                ),
            ),
            MountError::NotMounted { target } => mount(
                "unmount",
                target,
                io::Error::new(io::ErrorKind::InvalidInput, "not mounted"),
            ),
            MountError::Unsupported { path, reason } => mount(
                "mount",
                path,
                io::Error::new(io::ErrorKind::InvalidInput, reason),
            ),
            MountError::Spawn { binary, source } => Self::Io {
                op: "run",
                path: binary,
                source,
            },
            MountError::Exited {
                binary,
                status,
                stderr,
            } => Self::Tool {
                tool: binary.display().to_string(),
                status,
                stderr,
            },
            MountError::Mounts { source } => Self::Io {
                op: "read",
                path: PROC_MOUNTS.into(),
                source,
            },
        }
    }
}

impl OverlayBackend {
    /// The kernel tier, which needs `CAP_SYS_ADMIN`.
    pub fn kernel(config: &OverlayConfig) -> Self {
        Self {
            tier: Tier::Kernel(KernelMounter {
                nosuid: config.nosuid,
            }),
        }
    }

    /// The `fuse-overlayfs` tier, running `config.fuse_overlayfs`.
    pub fn fuse(config: &OverlayConfig) -> Self {
        Self {
            tier: Tier::Fuse(FuseMounter::new(&config.fuse_overlayfs)),
        }
    }

    /// The tier for `kind`, which must be an overlay kind.
    fn for_kind(kind: BackendKind, config: &OverlayConfig) -> Self {
        if kind == BackendKind::Overlayfs {
            Self::kernel(config)
        } else {
            Self::fuse(config)
        }
    }

    fn mounter(&self) -> &dyn Mounter {
        match &self.tier {
            Tier::Kernel(mounter) => mounter,
            Tier::Fuse(mounter) => mounter,
        }
    }

    /// The type this tier's mounts have in `/proc/self/mounts`.
    fn fstype(&self) -> String {
        match &self.tier {
            Tier::Kernel(_) => KERNEL_FSTYPE.to_owned(),
            Tier::Fuse(mounter) => mounter.fstype(),
        }
    }

    /// Unmounts `root`, lazily if it is busy (an agent's shell has its cwd
    /// inside) or if its FUSE server died. A root that is not mounted is
    /// fine.
    fn unmount_root(&self, root: &Path) -> Result<(), BackendError> {
        let lazily = || match self.mounter().unmount(root, true) {
            Ok(()) | Err(MountError::NotMounted { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        };
        match self.mounter().unmount(root, false) {
            Ok(()) | Err(MountError::NotMounted { .. }) => Ok(()),
            Err(MountError::Busy { .. }) => {
                tracing::warn!(
                    path = %root.display(),
                    "the workspace is in use; unmounting it lazily"
                );
                lazily()
            }
            Err(_) if is_disconnected(root) => lazily(),
            Err(error) => Err(error.into()),
        }
    }
}

/// Creates `ws.upper` and `ws.work`, which must not exist.
fn create_layers(ws: &WorkspacePaths) -> Result<(), BackendError> {
    for (dir, mode, op) in [
        (&ws.upper, UPPER_MODE, "create upper directory"),
        (&ws.work, WORK_MODE, "create work directory"),
    ] {
        DirBuilder::new()
            .mode(mode)
            .create(dir)
            .map_err(|source| BackendError::Io {
                op,
                path: dir.clone(),
                source,
            })?;
    }
    Ok(())
}

/// Moves an upper or work directory to a `.trash-*` sibling and deletes it
/// in the background. A missing one is fine.
fn remove_layer(dir: &Path) -> Result<(), BackendError> {
    match remove_tree_in_background(dir) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BackendError::Io {
            op: "move to trash",
            path: dir.to_owned(),
            source,
        }),
    }
}

/// Removes an unmounted root: an empty directory once its mount is gone,
/// or, if something was written into it while it was not mounted, by the
/// rename-then-delete of the other backends. Renaming a directory that is
/// still a mountpoint fails with `EBUSY`, so this never deletes through a
/// mount.
fn remove_root(root: &Path) -> Result<(), BackendError> {
    match fs::remove_dir(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            super::fs_util::tear_down_root(root)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BackendError::Io {
            op: "remove",
            path: root.to_owned(),
            source,
        }),
    }
}

impl Backend for OverlayBackend {
    fn kind(&self) -> BackendKind {
        match self.tier {
            Tier::Kernel(_) => BackendKind::Overlayfs,
            Tier::Fuse(_) => BackendKind::FuseOverlayfs,
        }
    }

    /// Creates the root, `ws.upper`, and `ws.work`, and mounts the overlay
    /// of `base` at the root. Nothing is copied. A failure after the root
    /// was created leaves what was created, which the daemon tears down
    /// (R1.05); a leftover upper or work directory is a failure.
    fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError> {
        if fs::symlink_metadata(&ws.root).is_ok() {
            return Err(BackendError::RootExists(ws.root.clone()));
        }
        if let Some(parent) = ws.root.parent() {
            fs::create_dir_all(parent).map_err(|source| BackendError::Io {
                op: "create directory",
                path: parent.to_owned(),
                source,
            })?;
        }
        match fs::create_dir(&ws.root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(BackendError::RootExists(ws.root.clone()));
            }
            Err(source) => {
                return Err(BackendError::Io {
                    op: "create directory",
                    path: ws.root.clone(),
                    source,
                });
            }
        }
        create_layers(ws)?;
        let begun = Instant::now();
        self.mounter()
            .mount(&base.path, &ws.upper, &ws.work, &ws.root)?;
        tracing::info!(
            workspace_id = %ws.id,
            backend = self.kind().as_str(),
            elapsed_ms = elapsed_ms(begun),
            "workspace materialized"
        );
        Ok(())
    }

    /// Exact: the allocated blocks under `ws.upper` (not its own), and the
    /// files, symlinks, and whiteouts in it. Copied-up directories count
    /// towards the bytes but not the files.
    fn disk_usage(&self, _: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError> {
        let own = fs::symlink_metadata(&ws.upper)
            .map_err(|source| BackendError::Io {
                op: "stat",
                path: ws.upper.clone(),
                source,
            })?
            .blocks()
            * 512;
        let allocated_bytes = allocated_bytes(&ws.upper)?.saturating_sub(own);
        let mut files_changed = 0;
        for entry in WalkDir::new(&ws.upper)
            .min_depth(1)
            .follow_links(false)
            .follow_root_links(false)
        {
            let entry = entry.map_err(|error| walk_error(&ws.upper, error))?;
            if !entry.file_type().is_dir() && entry.file_name() != FUSE_OPAQUE_MARKER {
                files_changed += 1;
            }
        }
        Ok(DiskUsage {
            allocated_bytes,
            files_changed,
        })
    }

    /// Unmounts (lazily if busy), deletes the upper and work directories,
    /// and mounts `to` with fresh ones. The agent's edits were in the store
    /// already: a move takes a final snapshot first (R1.06).
    fn swap_base(
        &self,
        ws: &WorkspacePaths,
        _: &BaseEntry,
        to: &BaseEntry,
    ) -> Result<SwapOutcome, BackendError> {
        self.unmount_root(&ws.root)?;
        remove_layer(&ws.upper)?;
        remove_layer(&ws.work)?;
        create_layers(ws)?;
        self.mounter()
            .mount(&to.path, &ws.upper, &ws.work, &ws.root)?;
        Ok(SwapOutcome::TreeReplaced)
    }

    /// Unmounts the root (lazily if busy: the kernel keeps the mount for
    /// the processes still in it, but it is gone from the namespace), then
    /// removes the root and deletes the upper and work directories in the
    /// background. A failed unmount leaves everything as it was.
    fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        let mounted = mount_info(&ws.root)
            .map_err(|source| MountError::Mounts { source })?
            .is_some();
        if !mounted
            && fs::symlink_metadata(&ws.root)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            // Layers a failed materialize or an interrupted tear-down left.
            remove_layer(&ws.upper)?;
            remove_layer(&ws.work)?;
            return Err(BackendError::RootMissing(ws.root.clone()));
        }
        if mounted {
            self.unmount_root(&ws.root)?;
        }
        remove_root(&ws.root)?;
        remove_layer(&ws.upper)?;
        remove_layer(&ws.work)
    }

    /// Fails unless the root is mounted with this tier's overlay, and
    /// served: a scan of the bare mountpoint would record every file as
    /// deleted, and one of a mount whose FUSE server died fails file by
    /// file.
    fn check_materialized(&self, ws: &WorkspacePaths) -> Result<(), BackendError> {
        let reason = match mount_info(&ws.root).map_err(|source| MountError::Mounts { source })? {
            Some(info) if info.fstype == self.fstype() && is_disconnected(&ws.root) => {
                format!("the fuse-overlayfs server of {} is gone", ws.root.display())
            }
            Some(info) if info.fstype == self.fstype() => return Ok(()),
            Some(info) => format!(
                "{} is a {} mount, not {}",
                ws.root.display(),
                info.fstype,
                self.fstype()
            ),
            None => format!("{} is not mounted", ws.root.display()),
        };
        Err(BackendError::Unavailable {
            kind: self.kind(),
            reason,
        })
    }
}

/// `lowerdir=<lower>,upperdir=<upper>,workdir=<work>`, each path escaped
/// for the option parser.
fn layer_options(lower: &Path, upper: &Path, work: &Path) -> Vec<u8> {
    let mut options = Vec::new();
    for (key, path) in [
        (&b"lowerdir="[..], lower),
        (b",upperdir=", upper),
        (b",workdir=", work),
    ] {
        options.extend_from_slice(key);
        escape_option(path, &mut options);
    }
    options
}

/// Appends `path` with `\`, `,`, and `:` escaped by a backslash, as
/// overlayfs's option parser (and `fuse-overlayfs`'s) expects: `,`
/// separates options and `:` lower layers.
fn escape_option(path: &Path, out: &mut Vec<u8>) {
    for &byte in path.as_os_str().as_bytes() {
        if matches!(byte, b'\\' | b',' | b':') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}

/// One line of `/proc/self/mounts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MountInfo {
    /// What is mounted: a device, or a name such as `overlay`.
    pub(crate) source: String,
    /// Where.
    pub(crate) mountpoint: PathBuf,
    /// The filesystem type, such as `overlay` or `fuse.fuse-overlayfs`.
    pub(crate) fstype: String,
    /// The mount options.
    pub(crate) options: String,
}

/// Parses the text of `/proc/self/mounts`: space-separated fields with
/// space, tab, newline, and backslash written as octal escapes (`\040`).
pub(crate) fn parse_mounts(text: &[u8]) -> Vec<MountInfo> {
    let lossy = |field: &[u8]| String::from_utf8_lossy(&unescape(field)).into_owned();
    text.split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|&byte| byte == b' ');
            let (source, mountpoint, fstype, options) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            Some(MountInfo {
                source: lossy(source),
                mountpoint: PathBuf::from(OsString::from_vec(unescape(mountpoint))),
                fstype: lossy(fstype),
                options: lossy(options),
            })
        })
        .collect()
}

/// Undoes the kernel's `\ooo` escapes.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'\\'
            && let Some(digits) = tail.get(..3)
            && let Ok(digits) = std::str::from_utf8(digits)
            && let Ok(value) = u8::from_str_radix(digits, 8)
        {
            out.push(value);
            rest = tail.get(3..).unwrap_or_default();
        } else {
            out.push(byte);
            rest = tail;
        }
    }
    out
}

/// Every mount in this process's namespace, in mount order.
pub(crate) fn mounts() -> io::Result<Vec<MountInfo>> {
    Ok(parse_mounts(&fs::read(PROC_MOUNTS)?))
}

/// Reads `/proc/self/mounts` and reports whether `target` is a mountpoint
/// and, if so, its topmost mount. `None` if its parent does not exist.
pub(crate) fn mount_info(target: &Path) -> io::Result<Option<MountInfo>> {
    let Some(target) = canonical(target)? else {
        return Ok(None);
    };
    Ok(mounts()?
        .into_iter()
        .rev()
        .find(|mount| mount.mountpoint == target))
}

/// `path` with its parent canonicalized, as `/proc/self/mounts` names it.
/// The last component is not resolved: a mountpoint whose FUSE server died
/// cannot be `stat`ed. `None` if the parent does not exist.
fn canonical(path: &Path) -> io::Result<Option<PathBuf>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Some(path.to_owned()));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    match fs::canonicalize(parent) {
        Ok(parent) => Ok(Some(parent.join(name))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether `path` is a FUSE mount whose server has died: `stat` fails with
/// `ENOTCONN` ("Transport endpoint is not connected").
fn is_disconnected(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_err_and(|error| error.raw_os_error() == Some(Errno::ENOTCONN as i32))
}

/// Whether `CAP_SYS_ADMIN` is in this process's effective set: bit 21 of
/// the `CapEff` line of `/proc/self/status`.
pub(crate) fn has_cap_sys_admin() -> io::Result<bool> {
    let status = fs::read_to_string("/proc/self/status")?;
    let caps = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no CapEff line"))?;
    let caps = u64::from_str_radix(caps.trim(), 16)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(caps & (1 << CAP_SYS_ADMIN) != 0)
}

/// Whether the kernel tier can serve workspaces in `dir` (which exists):
/// the daemon has `CAP_SYS_ADMIN`, and a trial mount there succeeds. The
/// trial also catches an upper filesystem overlayfs refuses (NFS, or an
/// overlay itself).
pub(crate) fn probe_kernel(dir: &Path, config: &OverlayConfig) -> Availability {
    match has_cap_sys_admin() {
        Ok(true) => trial_mount(
            dir,
            &KernelMounter {
                nosuid: config.nosuid,
            },
        ),
        Ok(false) => Availability::Unavailable("no CAP_SYS_ADMIN".to_owned()),
        Err(error) => Availability::Unavailable(format!("cannot read the capabilities: {error}")),
    }
}

/// Whether the `fuse-overlayfs` tier can serve workspaces in `dir` (which
/// exists): `config.fuse_overlayfs` is found, `/dev/fuse` opens read-write,
/// and a trial mount there succeeds within [`MOUNT_TIMEOUT`].
pub(crate) fn probe_fuse(dir: &Path, config: &OverlayConfig) -> Availability {
    let Some(binary) = find_program(&config.fuse_overlayfs) else {
        let reason = if config.fuse_overlayfs.contains('/') {
            format!("{} is not an executable file", config.fuse_overlayfs)
        } else {
            format!("{} not on PATH", config.fuse_overlayfs)
        };
        return Availability::Unavailable(reason);
    };
    if let Err(error) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
    {
        return Availability::Unavailable(format!("/dev/fuse: {error}"));
    }
    trial_mount(dir, &FuseMounter::new(binary))
}

/// Mounts and unmounts an empty overlay in a `.reef-probe-*` directory
/// under `dir`, and removes the directory.
fn trial_mount(dir: &Path, mounter: &dyn Mounter) -> Availability {
    let probe = dir.join(format!(
        "{PROBE_CLONE_PREFIX}{}-{}",
        std::process::id(),
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = trial_mount_in(&probe, mounter);
    let merged = probe.join("merged");
    if mount_info(&merged).is_ok_and(|info| info.is_none()) {
        if let Err(error) = remove_all(&probe) {
            tracing::warn!(path = %probe.display(), %error, "cannot remove a probe directory");
        }
    } else {
        tracing::warn!(
            path = %merged.display(),
            "a trial overlay mount is still mounted; the next daemon start removes it"
        );
    }
    match result {
        Ok(()) => Availability::Available,
        Err(reason) => Availability::Unavailable(reason),
    }
}

fn trial_mount_in(probe: &Path, mounter: &dyn Mounter) -> Result<(), String> {
    let [lower, upper, work, merged] = ["lower", "upper", "work", "merged"].map(|n| probe.join(n));
    for dir in [&lower, &upper, &work, &merged] {
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    }
    mounter
        .mount(&lower, &upper, &work, &merged)
        .map_err(|error| match error {
            MountError::Mount {
                source: Errno::ENODEV,
                ..
            } => "trial mount failed: ENODEV (overlay module missing)".to_owned(),
            MountError::Mount { source, .. } => format!("trial mount failed: {source}"),
            MountError::Timeout { .. } => "trial mount timed out".to_owned(),
            error => format!("trial mount failed: {}", ErrorChain(&error)),
        })?;
    mounter
        .unmount(&merged, false)
        .or_else(|_| mounter.unmount(&merged, true))
        .map_err(|error| format!("trial unmount failed: {}", ErrorChain(&error)))
}

/// `name` if it contains a `/` and is an executable file, else the first
/// executable file of that name in a `PATH` directory.
pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let executable = |path: &Path| {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
}

/// An `active` or `releasing` overlay workspace, for [`reconcile_mounts`].
#[derive(Clone, Debug)]
pub struct OverlayWorkspace {
    /// Its paths.
    pub paths: WorkspacePaths,
    /// [`BackendKind::Overlayfs`] or [`BackendKind::FuseOverlayfs`].
    pub kind: BackendKind,
    /// Its base's directory, the lower layer: `Ok(None)` if the base cache
    /// has no entry for its tree, an error if that could not be looked up.
    pub lower: Result<Option<PathBuf>, String>,
}

/// What [`reconcile_mounts`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Workspaces that were still mounted.
    pub mounted: Vec<WorkspaceId>,
    /// Workspaces mounted again over their existing upper directory.
    pub remounted: Vec<WorkspaceId>,
    /// Workspaces whose upper directory is gone: left unmounted, for
    /// `attention = 'upper-missing'`.
    pub upper_missing: Vec<WorkspaceId>,
    /// Workspaces that could not be mounted again, and why: left unmounted,
    /// for `attention = 'mount-failed'`.
    pub failed: Vec<(WorkspaceId, String)>,
    /// Mountpoints with no workspace that were unmounted and removed.
    pub cleaned: Vec<PathBuf>,
}

/// At daemon start: mounts every workspace in `workspaces` again whose
/// mount is gone (or whose FUSE server died), over its existing upper
/// directory, so the agent's edits reappear; a mounted one whose base is
/// known to be gone is unmounted. Then lazily unmounts every overlay mount
/// that looks like Reef's and belongs to no workspace in `workspaces`: one
/// directly in one of `scan_dirs` (the repositories' workspace directories)
/// named like a workspace id, or a trial mount in a `.reef-probe-*`
/// directory there or in the state directory's `repos/`. It removes its
/// root and layers, or its probe directory. Other mounts are left alone. Problems with single mounts are in the
/// report or the log; only failing to read `/proc/self/mounts` is an error.
pub fn reconcile_mounts(
    scan_dirs: &[PathBuf],
    workspaces: &[OverlayWorkspace],
    config: &OverlayConfig,
) -> Result<ReconcileReport, BackendError> {
    let read_mounts = || mounts().map_err(|source| MountError::Mounts { source });
    let mut report = ReconcileReport::default();
    let mut live = HashSet::new();
    let current = read_mounts()?;
    for ws in workspaces {
        let root = canonical(&ws.paths.root)
            .ok()
            .flatten()
            .unwrap_or_else(|| ws.paths.root.clone());
        let mount = current.iter().rev().find(|mount| mount.mountpoint == root);
        live.insert(root);
        let backend = OverlayBackend::for_kind(ws.kind, config);
        let id = ws.paths.id;
        match reconcile_one(&backend, ws, mount.is_some()) {
            Ok(Reconciled::Mounted) => report.mounted.push(id),
            Ok(Reconciled::Remounted) => report.remounted.push(id),
            Ok(Reconciled::UpperMissing) => report.upper_missing.push(id),
            Err(reason) => report.failed.push((id, reason)),
        }
    }

    let scan_dirs: Vec<PathBuf> = scan_dirs
        .iter()
        .filter_map(|dir| fs::canonicalize(dir).ok())
        .collect();
    let mut seen = HashSet::new();
    for mount in read_mounts()?.into_iter().rev() {
        let stale = (mount.fstype == KERNEL_FSTYPE || mount.fstype.starts_with(FUSE_FSTYPE_PREFIX))
            && looks_like_reefs(&mount.mountpoint, &scan_dirs)
            && !live.contains(&mount.mountpoint);
        if stale && seen.insert(mount.mountpoint.clone()) && clean_stale(&mount, config) {
            report.cleaned.push(mount.mountpoint);
        }
    }
    Ok(report)
}

/// Whether `mountpoint` is where Reef mounts: `<dir>/<workspace id>` or
/// `<dir>/.reef-probe-*/merged` for a `dir` in `scan_dirs`.
fn looks_like_reefs(mountpoint: &Path, scan_dirs: &[PathBuf]) -> bool {
    let (Some(parent), Some(name)) = (mountpoint.parent(), mountpoint.file_name()) else {
        return false;
    };
    let in_scan_dir = |dir: &Path| scan_dirs.iter().any(|scan| scan == dir);
    let workspace = in_scan_dir(parent)
        && name
            .to_str()
            .is_some_and(|name| name.parse::<WorkspaceId>().is_ok());
    let probe = name == "merged"
        && parent
            .file_name()
            .is_some_and(|probe| probe.as_bytes().starts_with(PROBE_CLONE_PREFIX.as_bytes()))
        && parent.parent().is_some_and(in_scan_dir);
    workspace || probe
}

/// What [`reconcile_one`] found.
enum Reconciled {
    Mounted,
    Remounted,
    UpperMissing,
}

fn reconcile_one(
    backend: &OverlayBackend,
    ws: &OverlayWorkspace,
    mounted: bool,
) -> Result<Reconciled, String> {
    let paths = &ws.paths;
    let lower = match &ws.lower {
        Ok(Some(lower)) if lower.is_dir() => Ok(lower.as_path()),
        Ok(Some(lower)) => Err(format!("its base {} is missing", lower.display())),
        Ok(None) => Err("its base is not in the base cache".to_owned()),
        Err(error) => Err(format!("cannot look up its base: {error}")),
    };
    // Not a failed lookup, which says nothing about the mount.
    let base_gone = ws.lower.is_ok() && lower.is_err();
    if mounted {
        let healthy = !is_disconnected(&paths.root);
        if healthy && !base_gone {
            return Ok(Reconciled::Mounted);
        }
        // A mount over a base the start-up sweep dropped would snapshot as
        // every base file deleted, so it comes down too.
        if healthy {
            tracing::warn!(workspace_id = %paths.id, path = %paths.root.display(), "the workspace's base is gone; unmounting it");
        } else {
            tracing::warn!(workspace_id = %paths.id, path = %paths.root.display(), "the workspace's fuse-overlayfs server is gone; mounting it again");
        }
        if let Err(error) = backend.mounter().unmount(&paths.root, true) {
            return Err(format!("cannot unmount it: {}", ErrorChain(&error)));
        }
    }
    if !paths.upper.is_dir() {
        return Ok(Reconciled::UpperMissing);
    }
    let lower = lower?;
    for dir in [&paths.root, &paths.work] {
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    }
    backend
        .mounter()
        .mount(lower, &paths.upper, &paths.work, &paths.root)
        .map_err(|error| ErrorChain(&error).to_string())?;
    Ok(Reconciled::Remounted)
}

/// Lazily unmounts a mount no workspace owns, and removes its probe
/// directory or its root and layers. Returns whether it was unmounted.
fn clean_stale(mount: &MountInfo, config: &OverlayConfig) -> bool {
    let root = &mount.mountpoint;
    let unmounted = if mount.fstype == KERNEL_FSTYPE {
        OverlayBackend::kernel(config).mounter().unmount(root, true)
    } else {
        OverlayBackend::fuse(config).mounter().unmount(root, true)
    };
    if let Err(error) = unmounted {
        tracing::warn!(
            path = %root.display(),
            error = %ErrorChain(&error),
            "cannot unmount an overlay that belongs to no workspace"
        );
        return false;
    }
    tracing::warn!(
        path = %root.display(),
        fstype = %mount.fstype,
        "unmounted an overlay that belongs to no workspace"
    );
    let probe = root.parent().filter(|parent| {
        parent
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(PROBE_CLONE_PREFIX.as_bytes()))
    });
    let removed = if let Some(probe) = probe {
        remove_all(probe).map_err(|source| BackendError::Io {
            op: "remove",
            path: probe.to_owned(),
            source,
        })
    } else {
        // `WorkspacePaths::at`'s siblings.
        let sibling = |suffix: &str| {
            let mut name = root.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        remove_root(root)
            .and_then(|()| remove_layer(&sibling(".upper")))
            .and_then(|()| remove_layer(&sibling(".work")))
    };
    if let Err(error) = removed {
        tracing::warn!(
            path = %root.display(),
            error = %ErrorChain(&error),
            "cannot remove what a stale overlay mount left"
        );
    }
    true
}

#[cfg(test)]
mod tests;
