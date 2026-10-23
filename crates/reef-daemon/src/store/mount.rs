//! Putting a store into a workspace and taking it out again (R1.20): a
//! symlink at the mount path everywhere, or on Linux with `CAP_SYS_ADMIN` a
//! read-only bind mount; and sealing a built store read-only.
//!
//! Nothing here follows a symlink inside the workspace. The mount path's
//! ancestors must be real directories or absent, when mounting, unmounting,
//! and finding mounts again at start alike, so a tree or an agent that
//! makes `vendor` a symlink to `/etc` cannot have the daemon create,
//! remove, or unmount anything outside the workspace.

use std::fs;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::{MountMode, StoreError};

/// Write bits for user, group, and other.
const WRITE_BITS: u32 = 0o222;

/// `mount` (repository-root relative, `/`-separated) under `ws_root`.
pub(crate) fn mount_path(ws_root: &Path, mount: &str) -> PathBuf {
    mount
        .split('/')
        .fold(ws_root.to_owned(), |path, part| path.join(part))
}

/// The mount path, after checking that each of `mount`'s ancestors below
/// `ws_root` is a real directory; an absent one is created with `create`,
/// and otherwise ends the walk (nothing can be under it).
/// [`StoreError::MountBlocked`] if an ancestor is a file or a symlink.
fn checked_path(ws_root: &Path, mount: &str, create: bool) -> Result<PathBuf, StoreError> {
    let target = mount_path(ws_root, mount);
    let mut dir = ws_root.to_owned();
    let parts: Vec<&str> = mount.split('/').collect();
    let (_, ancestors) = parts.split_last().unwrap_or((&"", &[]));
    for part in ancestors {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(StoreError::MountBlocked {
                    mount: mount.to_owned(),
                    path: dir,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&dir).map_err(|source| StoreError::Io {
                    op: "create directory",
                    path: dir.clone(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(StoreError::Io {
                    op: "stat",
                    path: dir,
                    source,
                });
            }
        }
    }
    Ok(target)
}

/// Checks that nothing is at `mount` in the workspace and that each of its
/// ancestors below `ws_root` is a directory or absent, then creates the
/// missing ancestors. [`StoreError::MountExists`] if something is there;
/// [`StoreError::MountBlocked`] if an ancestor is a file or a symlink.
pub(crate) fn prepare(ws_root: &Path, mount: &str) -> Result<PathBuf, StoreError> {
    let target = checked_path(ws_root, mount, true)?;
    match fs::symlink_metadata(&target) {
        Ok(_) => Err(StoreError::MountExists {
            mount: mount.to_owned(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target),
        Err(source) => Err(StoreError::Io {
            op: "stat",
            path: target,
            source,
        }),
    }
}

/// A symlink at `mount` in the workspace pointing at `store` (absolute,
/// since a workspace may live anywhere).
pub(crate) fn symlink_mount(store: &Path, ws_root: &Path, mount: &str) -> Result<(), StoreError> {
    let target = prepare(ws_root, mount)?;
    std::os::unix::fs::symlink(store, &target).map_err(|source| StoreError::Io {
        op: "symlink",
        path: target,
        source,
    })
}

/// After a build that exited 0, makes sure what it installed is in
/// `contents`, the directory about to be sealed. The build ran with a
/// symlink at `mount` pointing there ([`symlink_mount`]); a package
/// manager that deletes its output directory first (`npm ci` removes
/// `node_modules`) replaces that symlink with a directory of its own and
/// installs into it. That directory is moved to `contents`, which must
/// still be empty, or copied there when the workspace is on another
/// filesystem. Returns whether one was moved.
///
/// Anything else at `mount` (nothing, a file, a symlink elsewhere, an
/// ancestor that is no longer a directory, or a directory beside a
/// non-empty `contents`) is [`StoreError::LinkReplaced`]: sealing would
/// seal an empty or partial store.
pub(crate) fn adopt_replaced_link(
    contents: &Path,
    ws_root: &Path,
    mount: &str,
    name: &str,
) -> Result<bool, StoreError> {
    let replaced = |found: String| StoreError::LinkReplaced {
        name: name.to_owned(),
        mount: mount.to_owned(),
        found,
    };
    let target = match checked_path(ws_root, mount, false) {
        Ok(target) => target,
        Err(StoreError::MountBlocked { path, .. }) => {
            return Err(replaced(format!(
                "{} is no longer a directory",
                path.display()
            )));
        }
        Err(error) => return Err(error),
    };
    let io = |op, path: &Path, source| StoreError::Io {
        op,
        path: path.to_owned(),
        source,
    };
    let meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(replaced("it removed the link and left nothing".to_owned()));
        }
        Err(source) => return Err(io("stat", &target, source)),
    };
    if meta.file_type().is_symlink() {
        let points_at =
            fs::read_link(&target).map_err(|source| io("read link", &target, source))?;
        if points_at == contents {
            return Ok(false);
        }
        return Err(replaced(format!(
            "it is now a symlink to {}",
            points_at.display()
        )));
    }
    if !meta.is_dir() {
        return Err(replaced("it is now a file".to_owned()));
    }
    let mut entries =
        fs::read_dir(contents).map_err(|source| io("read directory", contents, source))?;
    if entries.next().is_some() {
        return Err(replaced(
            "it is now a directory, and the build also wrote into the store".to_owned(),
        ));
    }
    fs::remove_dir(contents).map_err(|source| io("remove directory", contents, source))?;
    let moved = match crate::backend::fs_util::rename_even_read_only(&target, contents) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            crate::backend::fs_util::copy_tree(
                &target,
                contents,
                &crate::backend::copy::CopyBackend,
            )
            .map(drop)
            .map_err(io::Error::other)
        }
        moved => moved,
    };
    if let Err(source) = moved {
        // Leave an (empty) contents directory, as before, for the failed
        // build's move aside; a partial copy goes aside with it.
        let _ = fs::create_dir(contents);
        return Err(io(
            "move the build's directory into the store at",
            contents,
            source,
        ));
    }
    Ok(true)
}

/// A read-only bind mount of `store` at `mount` in the workspace: an empty
/// directory, bound, then remounted read-only (a bind mount cannot be made
/// read-only in one call).
#[cfg(target_os = "linux")]
pub(crate) fn bind_mount(store: &Path, ws_root: &Path, mount: &str) -> Result<(), StoreError> {
    let target = prepare(ws_root, mount)?;
    fs::create_dir(&target).map_err(|source| StoreError::Io {
        op: "create directory",
        path: target.clone(),
        source,
    })?;
    if let Err(error) = bind(store, &target) {
        // Nothing is mounted there if the first call failed; a failed
        // remount leaves a writable bind mount, which must not stay.
        let _ = nix::mount::umount2(&target, nix::mount::MntFlags::MNT_DETACH);
        let _ = fs::remove_dir(&target);
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind(store: &Path, target: &Path) -> Result<(), StoreError> {
    use nix::mount::{MsFlags, mount};

    let failed = |source: nix::errno::Errno| StoreError::Io {
        op: "bind mount",
        path: target.to_owned(),
        source: source.into(),
    };
    mount(
        Some(store),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(failed)?;
    mount(
        None::<&str>,
        target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID,
        None::<&str>,
    )
    .map_err(failed)
}

/// Takes the store out of the workspace: removes the symlink, or detaches
/// the bind mount and removes its empty mount point. Something else at the
/// path (an agent replaced the symlink, or made an ancestor a symlink) is
/// left for the backend's tear-down, which does not follow symlinks;
/// nothing there is not an error.
pub(crate) fn unmount(ws_root: &Path, mount: &str, mode: MountMode) -> Result<(), StoreError> {
    let target = match checked_path(ws_root, mount, false) {
        Ok(target) => target,
        Err(StoreError::MountBlocked { path, .. }) => {
            tracing::warn!(
                mount,
                path = %path.display(),
                "an ancestor of the store's mount path is no longer a directory; leaving it for the tear-down"
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match mode {
        MountMode::Symlink => match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::remove_file(&target).map_err(|source| StoreError::Io {
                    op: "remove symlink",
                    path: target,
                    source,
                })
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                op: "stat",
                path: target,
                source,
            }),
        },
        MountMode::Bind => unbind(&target),
    }
}

#[cfg(target_os = "linux")]
fn unbind(target: &Path) -> Result<(), StoreError> {
    use nix::errno::Errno;
    use nix::mount::{MntFlags, umount2};

    // An agent may have swapped the mount point for a symlink; never unmount
    // or remove what one points at.
    if fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
        tracing::warn!(path = %target.display(), "a symlink replaced the store's mount point; leaving it for the tear-down");
        return Ok(());
    }
    match umount2(target, MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW) {
        // Not a mount point (a reboot dropped it), or gone.
        Ok(()) | Err(Errno::EINVAL | Errno::ENOENT) => {}
        Err(source) => {
            return Err(StoreError::Io {
                op: "unmount",
                path: target.to_owned(),
                source: source.into(),
            });
        }
    }
    match fs::remove_dir(target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // Not empty: an agent wrote into the mount point after a reboot
        // dropped the mount. The tear-down removes it with the tree.
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(source) => Err(StoreError::Io {
            op: "remove mount point",
            path: target.to_owned(),
            source,
        }),
    }
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Linux one"
)]
fn unbind(_: &Path) -> Result<(), StoreError> {
    // Never chosen off Linux.
    Ok(())
}

/// What is at `mount` in the workspace, compared with `store`: how a
/// restarted daemon tells which stores a workspace has mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// A symlink to the store.
    Symlink,
    /// The store, bind-mounted.
    #[cfg_attr(
        not(target_os = "linux"),
        expect(dead_code, reason = "bind mounts are Linux only")
    )]
    Bound,
    /// An empty directory: a bind mount point whose mount a reboot dropped.
    EmptyDir,
    /// Nothing.
    Nothing,
    /// Something else: a symlink elsewhere, a file, a non-empty directory.
    Other,
}

/// See [`Found`]. An ancestor that is not a directory is
/// [`Found::Other`].
pub(crate) fn find(store: &Path, ws_root: &Path, mount: &str) -> Result<Found, StoreError> {
    let target = match checked_path(ws_root, mount, false) {
        Ok(target) => target,
        Err(StoreError::MountBlocked { .. }) => return Ok(Found::Other),
        Err(error) => return Err(error),
    };
    let io = |source| StoreError::Io {
        op: "inspect store mount",
        path: target.clone(),
        source,
    };
    let meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Found::Nothing),
        Err(error) => return Err(io(error)),
    };
    if meta.file_type().is_symlink() {
        let points_at = fs::read_link(&target).map_err(io)?;
        return Ok(if points_at == store {
            Found::Symlink
        } else {
            Found::Other
        });
    }
    if !meta.is_dir() {
        return Ok(Found::Other);
    }
    #[cfg(target_os = "linux")]
    {
        // A bind mount has the bound directory's device and inode.
        let bound = fs::metadata(store).map_err(io)?;
        if bound.dev() == meta.dev() && bound.ino() == meta.ino() {
            return Ok(Found::Bound);
        }
    }
    Ok(if fs::read_dir(&target).map_err(io)?.next().is_none() {
        Found::EmptyDir
    } else {
        Found::Other
    })
}

/// Clears every write bit under `dir`, `dir` included: `0755` becomes
/// `0555` and `0644` becomes `0444`; execute bits stay. Symlinks are left
/// alone (their mode means nothing). Returns the bytes allocated under it.
pub(crate) fn seal_tree(dir: &Path) -> Result<u64, StoreError> {
    let mut bytes = 0;
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        let entry = entry.map_err(|error| {
            let path = error.path().unwrap_or(dir).to_owned();
            StoreError::Io {
                op: "walk",
                path,
                source: error.into(),
            }
        })?;
        if entry.file_type().is_symlink() {
            continue;
        }
        let meta = entry.metadata().map_err(|error| StoreError::Io {
            op: "stat",
            path: entry.path().to_owned(),
            source: error.into(),
        })?;
        bytes += std::os::unix::fs::MetadataExt::blocks(&meta) * 512;
        let mode = meta.permissions().mode();
        if mode & WRITE_BITS != 0 {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode & !WRITE_BITS))
                .map_err(|source| StoreError::Io {
                    op: "chmod",
                    path: entry.path().to_owned(),
                    source,
                })?;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn seal_clears_write_bits_and_keeps_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        fs::create_dir_all(store.join("bin")).unwrap();
        fs::write(store.join("bin/tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(store.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(store.join("data"), "x").unwrap();
        fs::set_permissions(store.join("data"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("data", store.join("link")).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();

        let bytes = seal_tree(&store).unwrap();
        assert!(bytes > 0);
        assert_eq!(mode(&store), 0o555);
        assert_eq!(mode(&store.join("bin")) & WRITE_BITS, 0);
        assert_eq!(mode(&store.join("bin/tool")), 0o555);
        assert_eq!(mode(&store.join("data")), 0o444);
        assert!(fs::write(store.join("new"), "y").is_err() || nix::unistd::geteuid().is_root());
        crate::backend::fs_util::remove_all(&store).unwrap();
    }

    #[test]
    fn symlink_mount_creates_parents_and_refuses_what_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        fs::create_dir(&store).unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir(&ws).unwrap();

        symlink_mount(&store, &ws, "vendor/deps").unwrap();
        assert_eq!(fs::read_link(ws.join("vendor/deps")).unwrap(), store);
        assert_eq!(find(&store, &ws, "vendor/deps").unwrap(), Found::Symlink);
        let error = symlink_mount(&store, &ws, "vendor/deps").unwrap_err();
        assert!(matches!(error, StoreError::MountExists { .. }), "{error:?}");

        unmount(&ws, "vendor/deps", MountMode::Symlink).unwrap();
        assert!(!ws.join("vendor/deps").exists());
        assert_eq!(find(&store, &ws, "vendor/deps").unwrap(), Found::Nothing);
        assert!(store.is_dir(), "the store is untouched");
        // Twice is fine.
        unmount(&ws, "vendor/deps", MountMode::Symlink).unwrap();
    }

    #[test]
    fn a_symlinked_ancestor_blocks_the_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let outside = tmp.path().join("outside");
        fs::create_dir(&store).unwrap();
        fs::create_dir(&outside).unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir(&ws).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("vendor")).unwrap();

        let error = symlink_mount(&store, &ws, "vendor/deps").unwrap_err();
        assert!(
            matches!(error, StoreError::MountBlocked { .. }),
            "{error:?}"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing created outside the workspace"
        );
        // Unmounting and finding through it touch nothing outside either.
        let decoy = outside.join("deps");
        std::os::unix::fs::symlink(&store, &decoy).unwrap();
        unmount(&ws, "vendor/deps", MountMode::Symlink).unwrap();
        assert!(fs::symlink_metadata(&decoy).is_ok(), "not removed");
        assert_eq!(find(&store, &ws, "vendor/deps").unwrap(), Found::Other);
        fs::remove_file(&decoy).unwrap();
        fs::remove_file(ws.join("vendor")).unwrap();
        fs::write(ws.join("vendor"), "a file").unwrap();
        let error = symlink_mount(&store, &ws, "vendor/deps").unwrap_err();
        assert!(
            matches!(error, StoreError::MountBlocked { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn find_tells_other_things_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        fs::create_dir(&store).unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join("empty")).unwrap();
        fs::create_dir_all(ws.join("full")).unwrap();
        fs::write(ws.join("full/file"), "").unwrap();
        std::os::unix::fs::symlink("/elsewhere", ws.join("link")).unwrap();
        assert_eq!(find(&store, &ws, "empty").unwrap(), Found::EmptyDir);
        assert_eq!(find(&store, &ws, "full").unwrap(), Found::Other);
        assert_eq!(find(&store, &ws, "link").unwrap(), Found::Other);
        // Another tree's leftover symlink is not ours to remove.
        unmount(&ws, "full", MountMode::Symlink).unwrap();
        assert!(ws.join("full/file").exists());
    }
}
