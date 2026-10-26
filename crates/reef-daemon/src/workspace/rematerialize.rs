//! [`Manager::rematerialize`] (R1.18, ADR 0020): after a restore, every
//! `active` workspace is rebuilt from the restored repository at daemon
//! start, keeping its id, owner, purpose, and lease. Its working-copy state
//! goes whether or not it survived: it may name an operation the restore
//! took back, which no snapshot could get past.
//!
//! 1. (repository thread) Reload at head; the workspace's commit is its
//!    working-copy commit in the restored view, or the row's commit if the
//!    view has none (it was forgotten after the database was copied); its
//!    tree key.
//! 2. (blocking) The working-copy state directory is removed. Without a
//!    directory, the base for the commit's tree is ensured as a create does
//!    it and the backend materializes the directory from it. A directory
//!    that is still there is kept as it is, with its base: an overlay's
//!    lower layer is that base, and edits on disk since the backup are
//!    the next snapshot's.
//! 3. (repository thread) A new working copy for the jj workspace in the
//!    view ([`reef_lib::jj::Repo::attach_workspace_with`], no operation), or
//!    the jj workspace created again on the commit (`Edit`, one operation).
//! 4. (blocking) A new directory's working copy is seeded from the base's
//!    `tree_state`; a kept one is reset to the commit, so the next snapshot
//!    rehashes what is on disk.
//! 5. The row records the commit, its change, and a new directory's base.
//!
//! In a new directory, edits made after the workspace's last snapshot
//! before the backup are gone: directories are caches.

use futures::FutureExt as _;
use reef_lib::jj::{ChangeSummary, CommitIdHex, OpKind, OpMeta, Repo, StartPoint, UserSettings};

use super::create::seed;
use super::{Manager, ManagerError, op_meta};
use crate::backend::{BackendKind, WorkspacePaths, backend_for};
use crate::model::WorkspaceRecord;
use crate::repo::RepoHandle;

impl Manager {
    /// Rebuilds `record`'s directory and working copy; see the module docs.
    /// Updates `record` to what the row now holds.
    #[tracing::instrument(skip_all, fields(workspace_id = %record.id))]
    pub(super) async fn rematerialize(
        &self,
        record: &mut WorkspaceRecord,
        handle: &RepoHandle,
    ) -> Result<(), ManagerError> {
        let id = record.id;
        let name = id.jj_workspace_name();
        let invalid = |what, value: &str| ManagerError::InvalidRecord {
            id,
            what,
            value: value.to_owned(),
        };
        let kind: BackendKind = record
            .backend
            .parse()
            .map_err(|_| invalid("backend", &record.backend))?;
        let backend = backend_for(kind, &self.config.file.workspace.overlay());
        let paths = WorkspacePaths::at(id, record.path.clone(), record.state_dir.clone());
        let settings = self.wc_settings(record.watcher)?;

        // 1.
        let fallback: CommitIdHex = record
            .commit_id
            .parse()
            .map_err(|_| invalid("commit id", &record.commit_id))?;
        let (commit, in_view, tree_key) = target(handle, &name, fallback).await?;

        // 2.
        let fresh = std::fs::symlink_metadata(&paths.root).is_err();
        super::manager::remove_state_dir(&paths.state_dir).await;
        let base = if fresh {
            let (base, _) = self
                .ensure_base_of(handle, &commit, &tree_key, &*backend)
                .await?;
            let (entry, at) = (base.clone(), paths.clone());
            tokio::task::spawn_blocking(move || backend.materialize(&entry, &at)).await??;
            Some(base)
        } else {
            None
        };

        // 3.
        let meta = op_meta(
            OpKind::Other,
            format!("reef: rematerialize workspace {id}"),
            id,
            &record.owner,
        );
        let (ws, wc) = attach(handle, &name, &paths, commit, in_view, settings, meta).await?;

        // 4.
        let ws = if let Some(base) = &base {
            seed(ws, base, &paths, &wc.commit_id).await?
        } else {
            let commit = wc.commit_id.clone();
            let mut ws = ws;
            let (ws, reset) = tokio::task::spawn_blocking(move || {
                let reset = pollster::block_on(ws.reset_to(&commit));
                (ws, reset)
            })
            .await?;
            reset?;
            ws
        };
        drop(ws);

        // 5.
        let base_tree_id = base.as_ref().map(|base| base.tree_id.as_str());
        self.db.set_workspace_commit(
            id,
            wc.change_id.as_str(),
            wc.commit_id.as_str(),
            base_tree_id,
        )?;
        record.change_id = wc.change_id.to_string();
        record.commit_id = wc.commit_id.to_string();
        if let Some(tree_id) = base_tree_id {
            tree_id.clone_into(&mut record.base_tree_id);
        }
        tracing::info!(
            workspace_id = %id,
            commit_id = %record.commit_id,
            path = %record.path.display(),
            directory_kept = !fresh,
            jj_workspace_recreated = !in_view,
            "workspace rematerialized after a restore"
        );
        Ok(())
    }
}

/// Step 1, on the repository thread: the commit to rebuild on, whether the
/// view has a working-copy commit for the jj workspace `name`, and the
/// commit's tree key.
async fn target(
    handle: &RepoHandle,
    name: &str,
    fallback: CommitIdHex,
) -> Result<(CommitIdHex, bool, String), ManagerError> {
    let name = name.to_owned();
    handle
        .call_async(move |head| {
            async move {
                head.reload().await?;
                let in_view = head
                    .repo
                    .workspace_commit(&name)
                    .map(|summary| summary.commit_id);
                let commit = in_view.clone().unwrap_or(fallback);
                let tree_key = head.repo.tree_key(&commit).await?;
                Ok::<_, ManagerError>((commit, in_view.is_some(), tree_key))
            }
            .boxed_local()
        })
        .await?
}

/// Step 4, on the repository thread: the workspace's new jj handle, and its
/// working-copy commit.
async fn attach(
    handle: &RepoHandle,
    name: &str,
    paths: &WorkspacePaths,
    commit: CommitIdHex,
    in_view: bool,
    settings: UserSettings,
    meta: OpMeta,
) -> Result<(Repo, ChangeSummary), ManagerError> {
    let name = name.to_owned();
    let (root, state_dir) = (paths.root.clone(), paths.state_dir.clone());
    handle
        .call_async(move |head| {
            async move {
                let ws = if in_view {
                    head.repo
                        .attach_workspace_with(&name, &root, &state_dir, &settings)?
                } else {
                    let start = StartPoint::Edit(commit);
                    head.repo
                        .create_workspace_with(&name, &root, &state_dir, start, &meta, &settings)
                        .await?
                        .0
                };
                let wc = ws.wc_change()?;
                Ok::<_, ManagerError>((ws, wc))
            }
            .boxed_local()
        })
        .await?
}
