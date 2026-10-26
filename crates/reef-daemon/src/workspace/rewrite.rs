//! [`Manager::rewrite`]: describe, abandon and undo (R2.03, ADR 0031), with
//! the live workspaces they move held still around the write.
//!
//! 1. Plan, on the repository thread: resolve and check the request, and
//!    list the jj workspaces whose working copy the write would move.
//!    Nothing is written (a colocated repository's ref import aside).
//! 2. Lock: the repository's create lock shared (so GC and a forget wait,
//!    as for a move), then, for each active Reef workspace among them (and
//!    an undo's named workspace), automatic snapshots paused and its scan
//!    lock taken, in workspace id order, as GC does.
//! 3. Snapshot (abandon and undo): each locked workspace, so no
//!    unsnapshotted edit is overwritten. A failure stops the write before
//!    anything is written. Describe changes no tree and takes none.
//! 4. Write, on the repository thread: resolve and check again, then one
//!    operation with the request's [`OpMeta`].
//! 5. Follow: each locked workspace whose working-copy commit moved takes
//!    the new commit: a row update when its tree is the same, else a
//!    snapshot, which finds the working copy stale with no edits and
//!    checks the new tree out. Then the rows' divergent marks are
//!    reconciled with the new head.
//!
//! Without the locks, a describe committed between a snapshot's scan and
//! its commit would make the change divergent (ADR 0018).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::rewrite::{self, WorkingCopy};
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta, OperationIdHex, oplog, undo};
use reef_lib::telemetry::elapsed_ms;
use tracing::Instrument as _;

use super::create::{import_meta, refresh_for_resolve};
use super::{Manager, ManagerError, WorkspaceEntry};
use crate::model::{Attention, RepoId, WorkspaceId, WorkspaceState};
use crate::repo::RepoHead;
use crate::snapshot::{SnapshotKind, description};

/// What a write does.
#[derive(Clone, Debug)]
pub enum Write {
    /// Set a commit's description.
    Describe {
        /// The revision naming the commit.
        revision: String,
        /// The new description, as given.
        description: String,
        /// The commit the caller expects the revision to name; empty for
        /// no check.
        expected: String,
    },
    /// Abandon a commit, or resolve its divergent change.
    Abandon {
        /// The revision naming the commit.
        revision: String,
        /// As for `Describe`.
        expected: String,
    },
    /// Revert one operation.
    Undo {
        /// A full operation id or a unique prefix.
        operation: String,
    },
}

impl Write {
    fn kind(&self) -> OpKind {
        match self {
            Write::Describe { .. } => OpKind::Describe,
            Write::Abandon { .. } => OpKind::Abandon,
            Write::Undo { .. } => OpKind::Undo,
        }
    }
}

/// A write and whom it is for.
#[derive(Clone, Debug)]
pub struct WriteRequest {
    /// The repository.
    pub repo: RepoId,
    /// The workspace the request names: `@`'s, the operation's
    /// `reef.workspace`, and for an undo the one checked out afterwards.
    pub workspace: Option<WorkspaceId>,
    /// The `reef-agent`; `None` for the workspace's owner (none without a
    /// workspace).
    pub agent: Option<String>,
    /// What to do.
    pub write: Write,
}

/// What [`Manager::rewrite`] did.
#[derive(Clone, Debug, Default)]
pub struct Rewritten {
    /// The operation it recorded; `None` when there was nothing to write.
    pub op_id: Option<String>,
    /// For an undo, the snapshot of the named workspace taken first, if
    /// it recorded an operation.
    pub snapshot_op_id: Option<String>,
    /// For a describe, the change afterwards (the current commit when the
    /// description was already that).
    pub change: Option<ChangeInfo>,
    /// For an abandon, the commit abandoned.
    pub abandoned: Option<CommitIdHex>,
    /// For an undo, the operation reverted, in full.
    pub undone: Option<OperationIdHex>,
}

/// What step 1 found.
struct Planned {
    /// jj workspaces the write would move, descendants first
    /// ([`rewrite::descendants_first`]), the undo's named one among them.
    workspaces: Vec<String>,
    /// For an undo, the operation in full.
    undone: Option<OperationIdHex>,
}

/// What step 4 wrote.
struct Written {
    op_id: Option<OperationIdHex>,
    change: Option<ChangeInfo>,
    abandoned: Option<CommitIdHex>,
    /// Working copies before and after.
    before: Vec<WorkingCopy>,
    after: Vec<WorkingCopy>,
}

impl Manager {
    /// Runs a describe, abandon or undo; see the module docs. Runs as its
    /// own task, so a caller that stops waiting cannot cut it between the
    /// write and the follow-up.
    #[tracing::instrument(skip_all, fields(repo = %request.repo, kind = request.write.kind().as_str()))]
    pub async fn rewrite(&self, request: WriteRequest) -> Result<Rewritten, ManagerError> {
        let manager = self
            .scheduler
            .manager()
            .ok_or(ManagerError::RepoNotOpen(request.repo))?;
        let task = async move { manager.rewrite_detached(request).await };
        tokio::spawn(task.instrument(tracing::Span::current())).await?
    }

    async fn rewrite_detached(&self, request: WriteRequest) -> Result<Rewritten, ManagerError> {
        let begun = std::time::Instant::now();
        let WriteRequest {
            repo,
            workspace,
            agent,
            write,
        } = request;
        if let Write::Describe { description, .. } = &write {
            rewrite::normalize_description(description)?;
        }
        let named = workspace.map(|id| self.active_entry(id)).transpose()?;
        let agent = agent.or_else(|| named.as_ref().map(|entry| entry.record().owner));
        let handle = self.handle(repo)?;
        let jj_name = workspace.map(|id| id.jj_workspace_name());

        // 1.
        let planned = {
            let (write, jj_name, agent) = (write.clone(), jj_name.clone(), agent.clone());
            handle
                .call_async(move |head| {
                    async move {
                        plan(
                            head,
                            &write,
                            jj_name.as_deref(),
                            workspace,
                            agent.as_deref(),
                        )
                        .await
                    }
                    .boxed_local()
                })
                .await??
        };

        // 2.
        let repo_lock = self.repo_lock(repo);
        let _shared = repo_lock.read().await;
        // Locked in id order, as GC does; snapshotted in the plan's order.
        let order: Vec<WorkspaceId> = planned
            .workspaces
            .iter()
            .filter_map(|name| name.strip_prefix("reef-")?.parse().ok())
            .collect();
        let entries: Vec<Arc<WorkspaceEntry>> = (order.iter().copied().collect::<BTreeSet<_>>())
            .iter()
            .filter_map(|id| self.entry(*id))
            .filter(|entry| entry.record().repo_id == repo)
            .collect();
        let _paused: Vec<_> = entries
            .iter()
            .map(|entry| self.scheduler.pause(entry.paths.id))
            .collect();
        let mut scans = Vec::with_capacity(entries.len());
        for entry in &entries {
            scans.push(entry.scan_lock.lock().await);
        }
        let locked: Vec<&Arc<WorkspaceEntry>> = entries
            .iter()
            .filter(|entry| entry.record().state == WorkspaceState::Active)
            .collect();
        if let Some(id) = workspace
            && !locked.iter().any(|entry| entry.paths.id == id)
            && matches!(write, Write::Undo { .. })
        {
            // Released while this waited for its lock.
            return Err(self
                .active_entry(id)
                .err()
                .unwrap_or(ManagerError::NotLoaded(id)));
        }

        // 3.
        let snapshot_op_id = if matches!(write, Write::Describe { .. }) {
            None
        } else {
            self.snapshot_moved(&order, &locked, workspace, agent.as_ref())
                .await?
        };

        // 4.
        let written = {
            let (write, agent) = (write.clone(), agent.clone());
            handle
                .call_async(move |head| {
                    async move { commit(head, &write, workspace, agent).await }.boxed_local()
                })
                .await??
        };

        // 5.
        self.follow(&locked, &written, agent.as_deref()).await;
        self.reconcile_divergence(repo, &locked, &written.after);
        drop(scans);

        let op_id = written.op_id.as_ref().map(ToString::to_string);
        tracing::info!(
            op_id = op_id.as_deref().unwrap_or(""),
            workspaces_count = locked.len(),
            elapsed_ms = elapsed_ms(begun),
            "{} written",
            write.kind().as_str()
        );
        Ok(Rewritten {
            op_id,
            snapshot_op_id,
            change: written.change,
            abandoned: written.abandoned,
            undone: planned.undone,
        })
    }

    /// Step 3: snapshots each locked workspace, one at a time, in `order`
    /// (descendants first): concurrent snapshots, or an ancestor's first,
    /// rebase a workspace's commit under its scan. Returns the operation of
    /// `named`'s snapshot, if it recorded one. A failure stops the write.
    async fn snapshot_moved(
        &self,
        order: &[WorkspaceId],
        locked: &[&Arc<WorkspaceEntry>],
        named: Option<WorkspaceId>,
        agent: Option<&String>,
    ) -> Result<Option<String>, ManagerError> {
        let kind = SnapshotKind::Explicit;
        let mut named_op = None;
        for id in order {
            let Some(entry) = locked.iter().find(|entry| entry.paths.id == *id) else {
                continue;
            };
            let outcome = self
                .snapshot_locked(entry, kind, description(kind, *id, None), agent.cloned())
                .await
                .map_err(|error| ManagerError::RewriteSnapshot {
                    id: *id,
                    source: Box::new(error),
                })?;
            if Some(*id) == named && outcome.changed {
                named_op = Some(outcome.op_id.to_string());
            }
        }
        Ok(named_op)
    }

    /// Step 5's follows: each locked workspace whose working-copy commit
    /// moved takes the new one. Failures are logged; the workspace adopts
    /// at its next snapshot. A Reef workspace the write moved without
    /// locking it (created or moved meanwhile) is logged; it follows at
    /// its next snapshot, by ADR 0018.
    async fn follow(
        &self,
        locked: &[&Arc<WorkspaceEntry>],
        written: &Written,
        agent: Option<&str>,
    ) {
        let locked_names: BTreeSet<String> = locked
            .iter()
            .map(|entry| entry.paths.id.jj_workspace_name())
            .collect();
        for (_, after) in moved_copies(&written.before, &written.after) {
            if after.workspace.starts_with("reef-") && !locked_names.contains(&after.workspace) {
                tracing::info!(
                    workspace_id = after.workspace.trim_start_matches("reef-"),
                    "the write moved a working copy it did not lock; it follows at its next snapshot"
                );
            }
        }
        let moved: HashMap<&str, (&WorkingCopy, &WorkingCopy)> =
            moved_copies(&written.before, &written.after)
                .into_iter()
                .map(|(before, after)| (after.workspace.as_str(), (before, after)))
                .collect();
        for entry in locked {
            let id = entry.paths.id;
            let name = id.jj_workspace_name();
            let Some((before, after)) = moved.get(name.as_str()) else {
                continue;
            };
            let followed = if before.tree == after.tree {
                self.follow_view(entry, &after.commit_id, &after.change_id)
            } else {
                let kind = SnapshotKind::Explicit;
                self.snapshot_locked(
                    entry,
                    kind,
                    description(kind, id, None),
                    agent.map(str::to_owned),
                )
                .await
                .map(|_| ())
            };
            if let Err(error) = followed {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot take the rewritten commit now; the workspace adopts it at its next snapshot");
            }
        }
    }

    /// Step 5's marks: a locked workspace whose working copy moved is
    /// marked divergent exactly when its change now is; any other active
    /// workspace of `repo` marked divergent whose change no longer is has
    /// the mark cleared. Failures are logged.
    fn reconcile_divergence(
        &self,
        repo: RepoId,
        locked: &[&Arc<WorkspaceEntry>],
        after: &[WorkingCopy],
    ) {
        let divergent: HashMap<&str, bool> = after
            .iter()
            .map(|copy| (copy.workspace.as_str(), copy.divergent))
            .collect();
        let locked_ids: BTreeSet<WorkspaceId> = locked.iter().map(|entry| entry.paths.id).collect();
        let entries: Vec<Arc<WorkspaceEntry>> = self
            .read_entries()
            .values()
            .filter(|entry| entry.record().repo_id == repo)
            .cloned()
            .collect();
        for entry in entries {
            let record = entry.record();
            if record.state != WorkspaceState::Active {
                continue;
            }
            let Some(&now) = divergent.get(record.id.jj_workspace_name().as_str()) else {
                continue;
            };
            let attention = match (now, record.attention) {
                (true, None) if locked_ids.contains(&record.id) => Some(Attention::Divergent),
                (false, Some(Attention::Divergent)) => None,
                (_, attention) => attention,
            };
            let mark = if locked_ids.contains(&record.id) {
                now
            } else {
                record.divergent && now
            };
            if mark == record.divergent && attention == record.attention {
                continue;
            }
            match self
                .db
                .set_workspace_attention(record.id, attention, Some(mark))
            {
                Ok(()) => entry.update_record(|record| {
                    record.divergent = mark;
                    record.attention = attention;
                }),
                Err(error) => {
                    tracing::warn!(workspace_id = %record.id, error = %ErrorChain(&error), "cannot update the workspace's divergent mark");
                }
            }
        }
    }
}

/// `(before, after)` of every working copy whose commit changed.
fn moved_copies<'a>(
    before: &'a [WorkingCopy],
    after: &'a [WorkingCopy],
) -> Vec<(&'a WorkingCopy, &'a WorkingCopy)> {
    after
        .iter()
        .filter_map(|now| {
            let was = before.iter().find(|was| was.workspace == now.workspace)?;
            (was.commit_id != now.commit_id).then_some((was, now))
        })
        .collect()
}

fn scope<'a>(head: &'a RepoHead, workspace: Option<&'a str>) -> RevsetScope<'a> {
    RevsetScope {
        workspace,
        user_email: head.settings.user_email(),
        root: &head.root,
    }
}

/// Step 1, on the repository thread.
async fn plan(
    head: &mut RepoHead,
    write: &Write,
    jj_name: Option<&str>,
    workspace: Option<WorkspaceId>,
    agent: Option<&str>,
) -> Result<Planned, ManagerError> {
    match write {
        Write::Describe {
            revision, expected, ..
        }
        | Write::Abandon { revision, expected } => {
            refresh_for_resolve(head, &import_meta(workspace, agent)).await?;
            let repo = head.repo.readonly();
            let expected = rewrite::parse_expected(&repo, expected)?;
            let target =
                rewrite::writable(&repo, revision, expected.as_ref(), &scope(head, jj_name))
                    .await?;
            Ok(Planned {
                workspaces: rewrite::descendants_first(&repo, &target.workspaces).await?,
                undone: None,
            })
        }
        Write::Undo { operation } => {
            let repo = head.refresh().await?;
            let op = oplog::resolve_operation(&repo, operation).await?;
            let op_id = oplog::entry(&op).id;
            let mut workspaces = undo::plan_revert(&repo, &op_id).await?.workspaces;
            if let Some(name) = jj_name
                && !workspaces.iter().any(|known| known == name)
            {
                workspaces.push(name.to_owned());
            }
            Ok(Planned {
                workspaces: rewrite::descendants_first(&repo, &workspaces).await?,
                undone: Some(op_id),
            })
        }
    }
}

/// Step 4, on the repository thread: resolve and check again, then write,
/// recorded for `workspace` and `agent`.
async fn commit(
    head: &mut RepoHead,
    write: &Write,
    workspace: Option<WorkspaceId>,
    agent: Option<String>,
) -> Result<Written, ManagerError> {
    let jj_name = workspace.map(|id| id.jj_workspace_name());
    let jj_name = jj_name.as_deref();
    let mut meta = OpMeta::new(write.kind(), String::new());
    meta.workspace = workspace.map(|id| id.as_hex());
    meta.agent = agent;
    let repo = head.refresh().await?;
    let before = rewrite::working_copies(&repo).await?;
    let mut written = Written {
        op_id: None,
        change: None,
        abandoned: None,
        before,
        after: Vec::new(),
    };
    match write {
        Write::Describe {
            revision,
            description,
            expected,
        } => {
            let expected = rewrite::parse_expected(&repo, expected)?;
            let target =
                rewrite::writable(&repo, revision, expected.as_ref(), &scope(head, jj_name))
                    .await?;
            meta.description = format!("{}{}", oplog::DESCRIBE_PREFIX, target.commit_id);
            let described = head
                .repo
                .describe_commit(&target.commit_id, description, &meta)
                .await?;
            let commit = described
                .as_ref()
                .map_or(&target.commit_id, |described| &described.commit_id)
                .clone();
            written.op_id = described.map(|described| described.op_id);
            let repo = head.repo.readonly();
            written.change = Some(
                Describer::new(&repo, &scope(head, jj_name))?
                    .describe(&commit)
                    .await?,
            );
        }
        Write::Abandon { revision, expected } => {
            let expected = rewrite::parse_expected(&repo, expected)?;
            let target =
                rewrite::writable(&repo, revision, expected.as_ref(), &scope(head, jj_name))
                    .await?;
            meta.description = format!("{}{}", oplog::ABANDON_PREFIX, target.commit_id);
            let abandoned = head.repo.abandon_commit(&target.commit_id, &meta).await?;
            written.op_id = Some(abandoned.op_id);
            written.abandoned = Some(target.commit_id);
        }
        Write::Undo { operation } => {
            let undone = oplog::entry(&oplog::resolve_operation(&repo, operation).await?).id;
            meta.description = format!("{}{undone}", oplog::REVERT_PREFIX);
            written.op_id = head.repo.revert_operation(&undone, &meta).await?;
        }
    }
    written.after = rewrite::working_copies(&head.repo.readonly()).await?;
    Ok(written)
}
