//! Kernel overlayfs through `mount(2)`, for a daemon with `CAP_SYS_ADMIN`.
//!
//! Options: `redirect_dir=on`, so an agent can rename a directory that came
//! from the base (without it the rename fails with `EXDEV`, which build
//! tools do not expect); `index=off` and `metacopy=off`, so the lower layer
//! may be swapped and is never written. A kernel that refuses
//! `redirect_dir=on` with `EINVAL` gets the mount without it, logged once.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags};

use super::{MountError, Mounter, layer_options};

/// Set once a mount without `redirect_dir=on` succeeded after the kernel
/// refused it, so later mounts skip the failing attempt.
static NO_REDIRECT_DIR: AtomicBool = AtomicBool::new(false);

/// Mounts kernel overlays.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KernelMounter {
    /// Mount with `MS_NOSUID` (`workspace.overlay_nosuid`).
    pub(crate) nosuid: bool,
}

impl KernelMounter {
    fn mount_with(self, options: &[u8], redirect_dir: bool, target: &Path) -> Result<(), Errno> {
        let mut data = options.to_vec();
        if redirect_dir {
            data.extend_from_slice(b",redirect_dir=on");
        }
        data.extend_from_slice(b",index=off,metacopy=off");
        let flags = if self.nosuid {
            MsFlags::MS_NOSUID
        } else {
            MsFlags::empty()
        };
        nix::mount::mount(
            Some("overlay"),
            target,
            Some("overlay"),
            flags,
            Some(data.as_slice()),
        )
    }
}

impl Mounter for KernelMounter {
    fn mount(
        &self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        target: &Path,
    ) -> Result<(), MountError> {
        let options = layer_options(lower, upper, work);
        let error = |source| MountError::Mount {
            target: target.to_owned(),
            source,
        };
        if NO_REDIRECT_DIR.load(Ordering::Relaxed) {
            return self.mount_with(&options, false, target).map_err(error);
        }
        match self.mount_with(&options, true, target) {
            Err(Errno::EINVAL) => {
                self.mount_with(&options, false, target).map_err(error)?;
                if !NO_REDIRECT_DIR.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "the kernel refused overlayfs redirect_dir=on; renaming a directory \
                         that came from the base fails with EXDEV in overlay workspaces"
                    );
                }
                Ok(())
            }
            result => result.map_err(error),
        }
    }

    fn unmount(&self, target: &Path, lazy: bool) -> Result<(), MountError> {
        let flags = if lazy {
            MntFlags::MNT_DETACH
        } else {
            MntFlags::empty()
        };
        let target = target.to_owned();
        match nix::mount::umount2(&target, flags) {
            Ok(()) => Ok(()),
            Err(Errno::EBUSY) => Err(MountError::Busy { target }),
            // Not a mountpoint, or no such directory.
            Err(Errno::EINVAL | Errno::ENOENT) => Err(MountError::NotMounted { target }),
            Err(source) => Err(MountError::Unmount { target, source }),
        }
    }
}
