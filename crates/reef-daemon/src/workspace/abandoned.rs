//! [`Manager::sweep_abandoned_creates`]: at daemon start, undoes the
//! creates a crash interrupted (R1.23).
//!
//! A create writes its row last (step 7 in `create.rs`), and undoes its
//! earlier steps itself when one fails; a daemon killed between step 4
//! and the row leaves a directory under the repository's workspace root,
//! maybe store mounts in it, a jj workspace `reef-<id>`, and a
//! working-copy state directory, none of which any row names. Each is
//! found by its id: a name under `ws/` or `wc/` that is a workspace id
//! with no row in the database is an abandoned create.
//!
//! Overlay mounts are mostly not this sweep's: [`crate::daemon`]'s start
//! step 4 unmounts and removes every overlay mount named like a workspace
//! that no active workspace owns, before this runs; the sweep removes the
//! `.upper` and `.work` layers beside a root either way. A create at a
//! path of the caller's (`--path`) is not under `ws/`; its directory is
//! left where it is, and only its jj workspace and state directory are
//! undone.
//!
//! The first start after a restore (`meta.rematerialize_pending`, R1.18)
//! is the exception: a directory with no row then may be a workspace
//! created after the backup was taken, holding edits nothing else has. It
//! is renamed to `<id>.orphan-<ms>` beside the others, out of the sweep's
//! way, and left for a human.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use reef_lib::error::ErrorChain;

use super::Manager;
use super::manager::{forget_jj_workspace, remove_state_dir};
use crate::backend::fs_util::remove_all;
use crate::db::{DbError, META_REMATERIALIZE_PENDING};
use crate::model::{RepoId, Timestamp, WorkspaceId};

impl Manager {
    /// Undoes the creates of `repo` a crash interrupted (see the module
    /// docs): detaches any mount left inside the directory (Linux), removes
    /// the directory, forgets the jj workspace, and removes the
    /// working-copy state directory; and removes every sync directory left
    /// from before (R2.23). Runs at daemon start, after the
    /// repository is open and before active workspaces load, while no
    /// create can be in flight. Returns how many it undid; failures are
    /// logged, and a directory that cannot be cleared of mounts is left.
    /// After a restore, directories are moved aside instead (module docs).
    pub async fn sweep_abandoned_creates(&self, repo: RepoId) -> usize {
        // Sync directories (R2.23) hold only a running daemon's cookies:
        // each active workspace makes its own again when it is tracked.
        let sync = self.config.paths.repo_dir(repo).join("sync");
        if let Err(error) = std::fs::remove_dir_all(&sync)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %sync.display(), %error, "cannot remove the sync directories");
        }
        let ws_root = self.config.workspace_root(repo);
        let state_root = self.config.paths.repo_dir(repo).join("wc");
        let ids: BTreeSet<WorkspaceId> = workspace_ids_in(&ws_root)
            .into_iter()
            .chain(workspace_ids_in(&state_root))
            .collect();
        let Ok(handle) = self.handle(repo) else {
            return 0;
        };
        let restored = match self.db.meta(META_REMATERIALIZE_PENDING) {
            Ok(marker) => marker.is_some(),
            Err(error) => {
                tracing::error!(error = %ErrorChain(&error), "cannot read the restore marker; not sweeping interrupted creates");
                return 0;
            }
        };
        let mut undone = 0;
        for id in ids {
            match self.db.workspace(id) {
                Ok(_) => continue,
                Err(DbError::NotFound { .. }) => {}
                Err(error) => {
                    tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot look up a workspace directory's row");
                    continue;
                }
            }
            let root = ws_root.join(id.as_hex());
            let cleared = if restored {
                keep_aside(id, &root)
            } else {
                match tokio::task::spawn_blocking(move || remove_abandoned(&root)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(error)) => {
                        tracing::error!(workspace_id = %id, %error, "cannot remove the directory of an interrupted create");
                        false
                    }
                    Err(error) => {
                        tracing::error!(workspace_id = %id, %error, "removal task failed");
                        false
                    }
                }
            };
            if let Err(error) = forget_jj_workspace(
                &handle,
                id,
                "reef: release workspace {id} (create interrupted)",
                // No row survived to name an owner.
                "",
            )
            .await
            {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot forget the jj workspace of an interrupted create");
                continue;
            }
            remove_state_dir(&state_root.join(id.as_hex())).await;
            if cleared {
                tracing::warn!(workspace_id = %id, "undid a create the daemon stopped in");
                undone += 1;
            }
        }
        undone
    }
}

/// Renames a directory with no row, found on the first start after a
/// restore, to `<id>.orphan-<ms>`. Whether nothing is left under the id.
fn keep_aside(id: WorkspaceId, root: &Path) -> bool {
    if std::fs::symlink_metadata(root).is_err() {
        return true;
    }
    let mut aside = root.as_os_str().to_owned();
    aside.push(format!(".orphan-{}", Timestamp::now().0));
    match std::fs::rename(root, &aside) {
        Ok(()) => {
            tracing::warn!(
                workspace_id = %id,
                path = %Path::new(&aside).display(),
                "a workspace directory has no row after the restore; moved aside, not removed"
            );
            true
        }
        Err(error) => {
            tracing::error!(workspace_id = %id, %error, "cannot move aside a workspace directory with no row");
            false
        }
    }
}

/// The entries of `dir` named exactly like a workspace id; none if `dir`
/// cannot be read.
fn workspace_ids_in(dir: &Path) -> Vec<WorkspaceId> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

/// Detaches every mount under `root` (store bind mounts, an overlay mount
/// made after start step 4 looked; Linux), then removes it and its overlay
/// layers `<root>.upper` and `<root>.work` without following symlinks (a
/// symlinked store is only the link). Gone is fine; a mount that will not
/// detach is an error and nothing is removed.
fn remove_abandoned(root: &Path) -> io::Result<()> {
    let mut paths = vec![root.to_owned()];
    for suffix in [".upper", ".work"] {
        let mut layer = root.as_os_str().to_owned();
        layer.push(suffix);
        paths.push(layer.into());
    }
    for path in paths {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        #[cfg(target_os = "linux")]
        detach_mounts_under(&path)?;
        remove_all(&path)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn detach_mounts_under(root: &Path) -> io::Result<()> {
    use nix::mount::{MntFlags, umount2};

    let root = std::fs::canonicalize(root)?;
    let mut under: Vec<_> = crate::backend::overlay::mounts()?
        .into_iter()
        .filter(|mount| mount.mountpoint.starts_with(&root))
        .map(|mount| (mount.mountpoint, mount.fstype))
        .collect();
    // Deepest first, so a mount on top of another comes off first.
    under.sort_by_key(|(point, _)| std::cmp::Reverse(point.as_os_str().len()));
    under.dedup();
    for (point, fstype) in under {
        match umount2(&point, MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW) {
            Ok(()) | Err(nix::errno::Errno::EINVAL | nix::errno::Errno::ENOENT) => {}
            // Unprivileged, only the setuid helper unmounts FUSE.
            Err(nix::errno::Errno::EPERM)
                if fstype.starts_with("fuse") && fusermount_lazy(&point) => {}
            Err(errno) => {
                return Err(io::Error::other(format!(
                    "detach {}: {errno}",
                    point.display()
                )));
            }
        }
    }
    Ok(())
}

/// `fusermount3 -uz <point>` (`fusermount` where there is no
/// `fusermount3`); whether either succeeded.
#[cfg(target_os = "linux")]
fn fusermount_lazy(point: &Path) -> bool {
    ["fusermount3", "fusermount"].iter().any(|program| {
        std::process::Command::new(program)
            .arg("-uz")
            .arg(point)
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_exact_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "0123456789abcdef",
            "0123456789abcdef.upper",
            "x.trash-1",
            "fedcba9876543210",
        ] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let mut ids: Vec<String> = workspace_ids_in(dir.path())
            .into_iter()
            .map(|id| id.as_hex())
            .collect();
        ids.sort();
        assert_eq!(ids, ["0123456789abcdef", "fedcba9876543210"]);
        assert_eq!(workspace_ids_in(&dir.path().join("missing")), []);
    }

    #[test]
    fn remove_abandoned_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(store.join("kept"), "x").unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::os::unix::fs::symlink(&store, root.join("vendor/deps")).unwrap();
        std::fs::create_dir(dir.path().join("ws.upper")).unwrap();
        std::fs::create_dir_all(dir.path().join("ws.work/work")).unwrap();
        remove_abandoned(&root).unwrap();
        assert!(!root.exists());
        assert!(!dir.path().join("ws.upper").exists());
        assert!(!dir.path().join("ws.work").exists());
        assert!(store.join("kept").exists());
        remove_abandoned(&root).unwrap();
    }

    #[test]
    fn keep_aside_renames_out_of_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let id: WorkspaceId = "0123456789abcdef".parse().unwrap();
        let root = dir.path().join(id.as_hex());
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("edit"), "x").unwrap();
        assert!(keep_aside(id, &root));
        assert!(!root.exists());
        assert_eq!(workspace_ids_in(dir.path()), []);
        let kept: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(kept.len(), 1);
        assert!(keep_aside(id, &root));
    }
}
