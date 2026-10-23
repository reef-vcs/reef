//! Shared stores in workspaces (R1.20): which stores a create mounts and
//! how, unmounting them before tear-down, finding them again at daemon
//! start, and checking each snapshot's lockfiles against the ones the
//! stores were chosen by.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, PoisonError};

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::CommitIdHex;
use reef_lib::spec::StoreSpec;

use super::{CreateSpec, Manager, ManagerError, MissingStores, WorkspaceEntry};
use crate::backend::BackendKind;
use crate::model::{Attention, RepoId, WorkspaceRecord};
use crate::repo::RepoHandle;
use crate::snapshot::SnapshotOutcome;
use crate::store::{MountedStore, StoreError, StoreKey, StoreManager, hash_lockfile};

/// One `[[store]]` of a spec, resolved against a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreTarget {
    /// The store and its lockfile's hash in the tree.
    pub key: StoreKey,
    /// Where it mounts, repository-root relative.
    pub mount: String,
    /// Its lockfile, repository-root relative.
    pub lockfile: String,
    /// The lockfile's blob id in the tree.
    pub lockfile_id: String,
}

/// Where the spec and lockfiles are read from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TreeAt<'a> {
    /// A commit.
    Commit(&'a CommitIdHex),
    /// A base cache key: a workspace's base tree.
    Tree(&'a str),
}

/// Each of `stores` (a spec's `[[store]]`s), with its lockfile read from
/// the tree and hashed. Runs on the repository thread. A lockfile
/// `reef.toml` names is in the tree (`Repo::spec_at` checks), so a missing
/// one here is a tree the spec was not read from.
pub(crate) async fn store_targets(
    repo: &reef_lib::jj::Repo,
    stores: &[StoreSpec],
    at: TreeAt<'_>,
) -> Result<Vec<StoreTarget>, ManagerError> {
    let mut targets = Vec::with_capacity(stores.len());
    for store in stores {
        let path = store.lockfile_path();
        let blob = match at {
            TreeAt::Commit(commit) => repo.file_blob_at(commit, path).await?,
            TreeAt::Tree(key) => repo.file_blob_in_tree(key, path).await?,
        };
        let Some(blob) = blob else {
            return Err(reef_lib::spec::SpecError::LockfileMissing {
                name: store.name.clone(),
                lockfile: store.lockfile.clone(),
            }
            .into());
        };
        targets.push(StoreTarget {
            key: StoreKey {
                name: store.name.clone(),
                lockfile_hash: hash_lockfile(&blob.bytes),
            },
            mount: store.mount.clone(),
            lockfile: store.lockfile.clone(),
            lockfile_id: blob.id,
        });
    }
    Ok(targets)
}

/// What a create or a move does about stores: the ones to mount, and
/// whether one was skipped as missing.
#[derive(Debug, Default)]
pub(super) struct StorePlan {
    pub mount: Vec<StoreTarget>,
    pub missing: Vec<StoreKey>,
}

/// How [`Manager::plan_stores`] treats a store with no sealed build.
#[derive(Clone, Copy, Debug)]
pub(super) struct StoreRules<'a> {
    /// The repository, for the error's advice.
    pub repo: RepoId,
    /// Fail, or leave the store out.
    pub missing: MissingStores,
    /// A store left out without counting as missing.
    pub skip: Option<&'a str>,
}

impl CreateSpec {
    /// This create's [`StoreRules`].
    pub(super) fn store_rules(&self) -> StoreRules<'_> {
        StoreRules {
            repo: self.repo,
            missing: self.missing_stores,
            skip: self.skip_store.as_deref(),
        }
    }
}

impl Manager {
    /// The daemon's stores.
    pub fn stores(&self) -> &Arc<StoreManager> {
        &self.stores
    }

    /// How many loaded workspaces mount each store.
    pub fn stores_in_use(&self) -> HashMap<StoreKey, u32> {
        let mut counts: HashMap<StoreKey, u32> = HashMap::new();
        for entry in self.read_entries().values() {
            for store in entry
                .stores
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
            {
                *counts.entry(store.key.clone()).or_default() += 1;
            }
        }
        counts
    }

    /// Which of `targets` (the stores of the commit `commit`) a create or
    /// a move mounts: each needs a sealed store, else
    /// [`StoreError::Missing`] unless `rules.missing` is `Skip`.
    /// `rules.skip` (a store build's own store) is left out without
    /// counting as missing.
    pub(super) fn plan_stores(
        &self,
        targets: &[StoreTarget],
        rules: &StoreRules<'_>,
        commit: &CommitIdHex,
    ) -> Result<StorePlan, ManagerError> {
        let mut plan = StorePlan::default();
        for target in targets {
            if rules.skip == Some(target.key.name.as_str()) {
                continue;
            }
            if self.stores.sealed(&target.key, &target.mount)?.is_some() {
                plan.mount.push(target.clone());
                continue;
            }
            match rules.missing {
                MissingStores::Fail => {
                    return Err(StoreError::Missing {
                        name: target.key.name.clone(),
                        hash: target.key.lockfile_hash.clone(),
                        repo: rules.repo,
                        commit: commit.to_string(),
                    }
                    .into());
                }
                MissingStores::Skip => {
                    tracing::warn!(
                        store = %target.key.name,
                        lockfile_hash = %target.key.lockfile_hash,
                        "store has no build; creating the workspace without it"
                    );
                    plan.missing.push(target.key.clone());
                }
            }
        }
        Ok(plan)
    }

    /// Mounts the planned stores into the materialized workspace at
    /// `root`, on the blocking pool. On failure the ones already mounted
    /// are unmounted again.
    pub(super) async fn mount_stores(
        &self,
        plan: &[StoreTarget],
        root: &Path,
        backend: BackendKind,
    ) -> Result<Vec<MountedStore>, ManagerError> {
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        let stores = Arc::clone(&self.stores);
        let plan = plan.to_vec();
        let root = root.to_owned();
        // FUSE overlays cannot host bind mounts (ADR 0022).
        let bind_allowed = backend != BackendKind::FuseOverlayfs;
        Ok(tokio::task::spawn_blocking(move || {
            let mut mounted: Vec<MountedStore> = Vec::with_capacity(plan.len());
            for target in plan {
                match stores.mount(&target.key, &root, &target.mount, bind_allowed) {
                    Ok(mode) => mounted.push(MountedStore {
                        key: target.key,
                        mount: target.mount,
                        lockfile: target.lockfile,
                        mode,
                        lockfile_id: target.lockfile_id,
                        verified: true,
                    }),
                    Err(error) => {
                        unmount_all(&stores, &root, &mounted);
                        return Err(error);
                    }
                }
            }
            Ok(mounted)
        })
        .await??)
    }

    /// Unmounts every store mounted in `entry`, before its tear-down.
    /// Failures are logged and added to `details`; the tear-down still
    /// runs (it cannot remove a read-only bind mount's contents, so the
    /// store survives either way).
    pub(super) async fn unmount_stores(
        &self,
        entry: &Arc<WorkspaceEntry>,
        details: &mut Vec<String>,
    ) {
        let mounted =
            std::mem::take(&mut *entry.stores.lock().unwrap_or_else(PoisonError::into_inner));
        if mounted.is_empty() {
            return;
        }
        let stores = Arc::clone(&self.stores);
        let root = entry.paths.root.clone();
        let failures = tokio::task::spawn_blocking(move || unmount_all(&stores, &root, &mounted))
            .await
            .unwrap_or_else(|error| vec![format!("unmount task failed: {error}")]);
        details.extend(failures);
    }

    /// At daemon start: the stores `record`'s workspace has mounted, from
    /// the `reef.toml` and lockfiles of the tree it was created on (or,
    /// failing that, its current commit, as for its snapshot spec), with a
    /// bind mount a reboot dropped mounted again. A store not at its mount
    /// path is not mounted. Failures are logged; they mean no stores.
    pub(super) async fn load_stores(
        &self,
        handle: &RepoHandle,
        record: &WorkspaceRecord,
    ) -> Vec<MountedStore> {
        let key = record.base_tree_id.clone();
        let commit = record.commit_id.parse::<CommitIdHex>().ok();
        let read = handle
            .call_async(move |head| {
                async move {
                    if let Ok(Some(spec)) = head.repo.spec_in_tree(&key).await {
                        return store_targets(&head.repo, &spec.stores, TreeAt::Tree(&key)).await;
                    }
                    let Some(commit) = commit else {
                        return Ok(Vec::new());
                    };
                    match head.repo.spec_at(&commit).await? {
                        Some(spec) => {
                            store_targets(&head.repo, &spec.stores, TreeAt::Commit(&commit)).await
                        }
                        None => Ok(Vec::new()),
                    }
                }
                .boxed_local()
            })
            .await;
        let targets = match read {
            Ok(Ok(targets)) => targets,
            Ok(Err(error)) => {
                tracing::error!(workspace_id = %record.id, error = %ErrorChain(&error), "cannot read the workspace's stores");
                return Vec::new();
            }
            Err(error) => {
                tracing::error!(workspace_id = %record.id, error = %ErrorChain(&error), "cannot read the workspace's stores");
                return Vec::new();
            }
        };
        if targets.is_empty() {
            return Vec::new();
        }
        let stores = Arc::clone(&self.stores);
        let root = record.path.clone();
        let id = record.id;
        let verified = record.attention != Some(Attention::StoreHashMismatch);
        let bind_allowed = record.backend != BackendKind::FuseOverlayfs.as_str();
        let found = tokio::task::spawn_blocking(move || {
            let mut mounted = Vec::new();
            for target in targets {
                match stores.remount(&target.key, &root, &target.mount, bind_allowed) {
                    Ok(Some(mode)) => mounted.push(MountedStore {
                        key: target.key,
                        mount: target.mount,
                        lockfile: target.lockfile,
                        mode,
                        lockfile_id: target.lockfile_id,
                        verified,
                    }),
                    Ok(None) => tracing::info!(
                        workspace_id = %id,
                        store = %target.key.name,
                        "store is not mounted in the workspace"
                    ),
                    Err(error) => tracing::error!(
                        workspace_id = %id,
                        store = %target.key.name,
                        error = %ErrorChain(&error),
                        "cannot mount the store again"
                    ),
                }
            }
            mounted
        })
        .await;
        found.unwrap_or_else(|error| {
            tracing::error!(workspace_id = %record.id, %error, "the store mount task failed");
            Vec::new()
        })
    }

    /// After a snapshot that moved the working-copy commit off `previous`
    /// (a recorded change, an adopted rewrite, or a view that moved it):
    /// compares each mounted store's lockfile in the new commit with the one
    /// the store was chosen by, by blob id (no rehash). A difference marks
    /// the workspace `store-hash-mismatch` and logs a warning; the mount
    /// stays. When every lockfile matches again, the mark is cleared.
    /// Failures are logged.
    pub(super) async fn verify_stores(
        &self,
        entry: &Arc<WorkspaceEntry>,
        outcome: &SnapshotOutcome,
        previous: &str,
    ) {
        if outcome.commit_id.as_str() == previous {
            return;
        }
        let mounted = entry.stores();
        if mounted.is_empty() {
            return;
        }
        let id = entry.paths.id;
        let commit = outcome.commit_id.clone();
        let lockfiles: Vec<String> = mounted.iter().map(|store| store.lockfile.clone()).collect();
        let read = entry
            .repo
            .call_async(move |head| {
                async move {
                    let mut ids = Vec::with_capacity(lockfiles.len());
                    for lockfile in &lockfiles {
                        // Validated when the spec was parsed.
                        let Ok(path) = reef_lib::jj::RepoPath::from_internal_string(lockfile)
                        else {
                            ids.push(None);
                            continue;
                        };
                        ids.push(head.repo.file_id_at(&commit, path).await?);
                    }
                    Ok::<_, ManagerError>(ids)
                }
                .boxed_local()
            })
            .await;
        let ids = match read {
            Ok(Ok(ids)) => ids,
            Ok(Err(error)) => {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot check the workspace's lockfiles");
                return;
            }
            Err(error) => {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot check the workspace's lockfiles");
                return;
            }
        };
        let mut mismatched = Vec::new();
        {
            let mut stores = entry.stores.lock().unwrap_or_else(PoisonError::into_inner);
            for (store, now) in stores.iter_mut().zip(&ids) {
                let verified = now.as_deref() == Some(store.lockfile_id.as_str());
                if !verified {
                    mismatched.push(store.key.name.clone());
                }
                store.verified = verified;
            }
        }
        let current = entry.record().attention;
        if mismatched.is_empty() {
            if current == Some(Attention::StoreHashMismatch) {
                self.set_store_attention(entry, None);
            }
            return;
        }
        for name in &mismatched {
            tracing::warn!(
                workspace_id = %id,
                store = %name,
                commit_id = %outcome.commit_id,
                "the workspace's lockfile no longer matches its mounted store; it needs a store build for the new lockfile and a ws move to remount"
            );
        }
        // A mark that says more (divergent, a failed final snapshot, an
        // overlay problem, a store left out) stays; a watch failure yields.
        let replaceable = matches!(current, None | Some(Attention::WatchFailed));
        if replaceable {
            self.set_store_attention(entry, Some(Attention::StoreHashMismatch));
        }
    }

    fn set_store_attention(&self, entry: &WorkspaceEntry, attention: Option<Attention>) {
        let id = entry.paths.id;
        match self.db.set_workspace_attention(id, attention, None) {
            Ok(()) => entry.set_attention(attention),
            Err(error) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot mark the workspace");
            }
        }
    }

    /// The stores the default start revision of `handle`'s repository (the
    /// `main` bookmark, else the `default` workspace's commit) declares,
    /// with its lockfiles' hashes: what GC keeps besides the stores in use.
    pub(crate) async fn default_store_keys(
        &self,
        handle: &RepoHandle,
    ) -> Result<Vec<StoreKey>, ManagerError> {
        handle
            .call_async(move |head| {
                async move {
                    // `main` may have moved on the root since the thread
                    // last reloaded.
                    super::create::refresh_for_resolve(head).await?;
                    let commit = super::create::default_start(&head.repo).await?;
                    let Some(spec) = head.repo.spec_at(&commit).await? else {
                        return Ok(Vec::new());
                    };
                    let targets =
                        store_targets(&head.repo, &spec.stores, TreeAt::Commit(&commit)).await?;
                    Ok::<_, ManagerError>(targets.into_iter().map(|target| target.key).collect())
                }
                .boxed_local()
            })
            .await?
    }
}

impl Manager {
    /// What a store build of `name` in `repo` builds: the commit `rev`
    /// resolves to (default: the `main` bookmark, else the `default`
    /// workspace's commit), the store's `[[store]]` in that commit's
    /// `reef.toml`, and its lockfile hashed there.
    /// [`StoreError::NotDeclared`] if the spec has no such store.
    pub(crate) async fn resolve_store(
        &self,
        repo: RepoId,
        rev: Option<String>,
        name: String,
    ) -> Result<(CommitIdHex, StoreSpec, StoreTarget), ManagerError> {
        let handle = self.handle(repo)?;
        handle
            .call_async(move |head| {
                async move {
                    super::create::refresh_for_resolve(head).await?;
                    let commit = match rev {
                        Some(text) => head.repo.resolve_revision(&text).await?.commit_id,
                        None => super::create::default_start(&head.repo).await?,
                    };
                    let spec = head.repo.spec_at(&commit).await?;
                    let Some(store) = spec.as_ref().and_then(|spec| spec.store(&name)).cloned()
                    else {
                        return Err(StoreError::NotDeclared {
                            name,
                            commit: commit.to_string(),
                        }
                        .into());
                    };
                    let mut targets = store_targets(
                        &head.repo,
                        std::slice::from_ref(&store),
                        TreeAt::Commit(&commit),
                    )
                    .await?;
                    let target = targets.remove(0);
                    Ok::<_, ManagerError>((commit, store, target))
                }
                .boxed_local()
            })
            .await?
    }
}

/// Unmounts `mounted` from the workspace at `root`, logging failures;
/// returns them as `reclaims` detail lines.
fn unmount_all(stores: &StoreManager, root: &Path, mounted: &[MountedStore]) -> Vec<String> {
    let mut failures = Vec::new();
    for store in mounted {
        if let Err(error) = stores.unmount(root, &store.mount, store.mode) {
            let message = ErrorChain(&error).to_string();
            tracing::error!(
                store = %store.key.name,
                mount = %store.mount,
                mode = store.mode.as_str(),
                error = %message,
                "cannot unmount the store"
            );
            failures.push(format!(
                "unmount store {} failed: {message}",
                store.key.name
            ));
        }
    }
    failures
}
