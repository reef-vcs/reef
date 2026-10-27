//! [`Manager::rewrite`]: describe, abandon and undo (R2.03, ADR 0031), and
//! file edits (R2.04, ADR 0032), with the live workspaces they move held
//! still around the write.
//!
//! 0. Check what needs no repository: an edit's paths and shape, a
//!    description.
//! 1. Plan, on the repository thread: resolve and check the request, and
//!    list the jj workspaces whose working copy the write would move (for
//!    an edit that creates a change, those on its parent). An edit is
//!    applied as a dry run against the tree as it is now, so a request
//!    wrong against it records nothing, unless a workspace sits on the
//!    commit itself: its snapshot at step 3 may change that tree, and step
//!    4 decides. Nothing is written (a colocated repository's ref import
//!    aside).
//! 2. Lock: the repository's create lock shared (so GC and a forget wait,
//!    as for a move), then, for each active Reef workspace among them (and
//!    an undo's named workspace), automatic snapshots paused and its scan
//!    lock taken, in workspace id order, as GC does.
//! 3. Snapshot (abandon, undo and edit): each locked workspace, so no
//!    unsnapshotted edit is overwritten. A failure stops the write before
//!    anything is written. Describe changes no tree and takes none.
//! 4. Write, on the repository thread: resolve and check again, then one
//!    operation with the request's [`OpMeta`].
//! 5. Follow: every Reef workspace the write moved to another commit of its
//!    change with another tree is handed a [`Carry`] for its next snapshot
//!    (issue #80, ADR 0039). Then each locked workspace whose working-copy
//!    commit moved takes the new commit, descendants first: a row update
//!    when its tree is the same, else a snapshot, which finds the working
//!    copy stale and checks the new tree out, carrying any edits made since
//!    step 3 onto it in the same change. Then the rows' divergent marks
//!    are reconciled with the new head.
//!
//! Without the locks, a describe committed between a snapshot's scan and
//! its commit would make the change divergent (ADR 0018).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::files;
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::revset::{RevisionError, RevsetScope};
use reef_lib::jj::rewrite::{self, RewriteError, WorkingCopy};
use reef_lib::jj::{Carry, CommitIdHex, OpKind, OpMeta, OperationIdHex, ReadonlyRepo, oplog, undo};
use reef_lib::telemetry::elapsed_ms;
use tracing::Instrument as _;

use super::create::{import_meta, refresh_for_resolve};
use super::{Manager, ManagerError, WorkspaceEntry};
use crate::model::{Attention, RepoId, WorkspaceId, WorkspaceState};
use crate::repo::{RepoHandle, RepoHead};
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
    /// Edit files without a workspace (R2.04).
    Edit {
        /// The commit to rewrite, or to build a new change on.
        target: EditTarget,
        /// As for `Describe`.
        expected: String,
        /// The edits, in order; shared, since a write is cloned into each
        /// step.
        edits: Arc<[files::Edit]>,
        /// The new description; `None` keeps the target's (empty for a new
        /// change).
        description: Option<String>,
    },
}

/// What an edit writes to.
#[derive(Clone, Debug)]
pub enum EditTarget {
    /// Rewrite the commit this revision names, keeping its change id.
    Rewrite(String),
    /// Create a new change on the commit this revision names.
    NewOn(String),
}

impl EditTarget {
    fn revision(&self) -> &str {
        match self {
            EditTarget::Rewrite(revision) | EditTarget::NewOn(revision) => revision,
        }
    }
}

impl Write {
    fn kind(&self) -> OpKind {
        match self {
            Write::Describe { .. } => OpKind::Describe,
            Write::Abandon { .. } => OpKind::Abandon,
            Write::Undo { .. } => OpKind::Undo,
            Write::Edit { .. } => OpKind::Edit,
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
    /// For a describe or an edit, the change afterwards (the current commit
    /// when nothing changed).
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
        // 0.
        check(&write)?;
        let named = workspace.map(|id| self.active_entry(id)).transpose()?;
        // The write's own operation falls back to the named workspace's
        // owner; each snapshot falls back to its own workspace's.
        let op_agent = agent
            .clone()
            .or_else(|| named.as_ref().map(|entry| entry.record().owner));
        let handle = self.handle(repo)?;

        // 1.
        let planned = {
            let (write, op_agent) = (write.clone(), op_agent.clone());
            handle
                .call_async(move |head| {
                    async move { plan(head, &write, workspace, op_agent.as_deref()).await }
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
        let snapshotted = if matches!(write, Write::Describe { .. }) {
            Snapshotted::default()
        } else {
            self.snapshot_moved(&order, &locked, workspace, agent.as_ref())
                .await?
        };

        // 4.
        let written = {
            let (write, undone) = (write.clone(), planned.undone.clone());
            let rewritten = snapshotted.rewritten;
            handle
                .call_async(move |head| {
                    let target = Target {
                        workspace,
                        agent: op_agent,
                        rewritten,
                        undone,
                    };
                    async move { commit(head, &write, target).await }.boxed_local()
                })
                .await??
        };

        // 5.
        self.scheduler.follow_hold_point().await;
        self.register_carries(repo, &written);
        let moved = self
            .follow(&order, &locked, &written, agent.as_deref())
            .await;
        self.reconcile_divergence(repo, &handle, &moved).await;
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
            snapshot_op_id: snapshotted.named_op,
            change: written.change,
            abandoned: written.abandoned,
            undone: planned.undone,
        })
    }

    /// Step 3: snapshots each locked workspace, one at a time, in `order`
    /// (descendants first): concurrent snapshots, or an ancestor's first,
    /// rebase a workspace's commit under its scan. `agent` is the request's
    /// own, so each snapshot otherwise records its workspace's owner. A
    /// failure stops the write.
    async fn snapshot_moved(
        &self,
        order: &[WorkspaceId],
        locked: &[&Arc<WorkspaceEntry>],
        named: Option<WorkspaceId>,
        agent: Option<&String>,
    ) -> Result<Snapshotted, ManagerError> {
        let kind = SnapshotKind::Explicit;
        let mut snapshotted = Snapshotted::default();
        for id in order {
            let Some(entry) = locked.iter().find(|entry| entry.paths.id == *id) else {
                continue;
            };
            let before = entry.record().commit_id.parse::<CommitIdHex>().ok();
            let outcome = self
                .snapshot_locked(entry, kind, description(kind, *id, None), agent.cloned())
                .await
                .map_err(|error| ManagerError::RewriteSnapshot {
                    id: *id,
                    source: Box::new(error),
                })?;
            if !outcome.changed {
                continue;
            }
            if let Some(before) = before {
                snapshotted
                    .rewritten
                    .insert(before, outcome.commit_id.clone());
            }
            if Some(*id) == named {
                snapshotted.named_op = Some(outcome.op_id.to_string());
            }
        }
        Ok(snapshotted)
    }

    /// Step 5's carries (ADR 0039): every Reef workspace of `repo` the write
    /// moved within its change, locked or not, since one it moved without
    /// locking it carries at its next snapshot.
    fn register_carries(&self, repo: RepoId, written: &Written) {
        for (id, carry) in carries(&written.before, &written.after) {
            if let Some(entry) = self.entry(id)
                && entry.record().repo_id == repo
            {
                entry.carry.set(carry);
            }
        }
    }

    /// Step 5's follows: each locked workspace whose working-copy commit
    /// moved takes the new one, in `order` (descendants first, as step 3):
    /// an ancestor's carried follow rebases its descendants, which, done
    /// first, would move a descendant off its carry's commit. Failures are
    /// logged; the workspace adopts, or carries, at its next snapshot. A
    /// Reef workspace the write moved without locking it (created or moved
    /// meanwhile) is logged; it follows at its next snapshot, by ADR 0018.
    /// Returns the locked workspaces that moved.
    async fn follow(
        &self,
        order: &[WorkspaceId],
        locked: &[&Arc<WorkspaceEntry>],
        written: &Written,
        agent: Option<&str>,
    ) -> BTreeSet<WorkspaceId> {
        let moved: HashMap<&str, (&WorkingCopy, &WorkingCopy)> =
            moved_copies(&written.before, &written.after)
                .into_iter()
                .map(|(before, after)| (after.workspace.as_str(), (before, after)))
                .collect();
        let locked_names: BTreeSet<String> = locked
            .iter()
            .map(|entry| entry.paths.id.jj_workspace_name())
            .collect();
        for name in moved.keys() {
            if name.starts_with("reef-") && !locked_names.contains(*name) {
                tracing::info!(
                    workspace_id = name.trim_start_matches("reef-"),
                    "the write moved a working copy it did not lock; it follows at its next snapshot"
                );
            }
        }
        let mut followed_ids = BTreeSet::new();
        for id in order {
            let Some(entry) = locked.iter().find(|entry| entry.paths.id == *id) else {
                continue;
            };
            let id = *id;
            let name = id.jj_workspace_name();
            let Some((before, after)) = moved.get(name.as_str()) else {
                continue;
            };
            followed_ids.insert(id);
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
        followed_ids
    }

    /// Step 5's marks, read from the head after the follows (whose
    /// snapshots may have diverged): a workspace the write `moved` is
    /// marked divergent exactly when its change now is; any other active
    /// workspace of `repo` marked divergent whose change no longer is has
    /// the mark cleared. Failures are logged.
    async fn reconcile_divergence(
        &self,
        repo: RepoId,
        handle: &RepoHandle,
        moved: &BTreeSet<WorkspaceId>,
    ) {
        let copies = handle
            .call_async(|head| {
                async move {
                    let repo = head.refresh().await?;
                    Ok::<_, ManagerError>(rewrite::working_copies(&repo).await?)
                }
                .boxed_local()
            })
            .await;
        let copies = match copies {
            Ok(Ok(copies)) => copies,
            Ok(Err(error)) => {
                tracing::warn!(error = %ErrorChain(&error), "cannot read the working copies to update divergent marks");
                return;
            }
            Err(error) => {
                tracing::warn!(error = %ErrorChain(&error), "cannot read the working copies to update divergent marks");
                return;
            }
        };
        let divergent: HashMap<&str, bool> = copies
            .iter()
            .map(|copy| (copy.workspace.as_str(), copy.divergent))
            .collect();
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
            let was_moved = moved.contains(&record.id);
            let attention = match (now, record.attention) {
                (true, None) if was_moved => Some(Attention::Divergent),
                (false, Some(Attention::Divergent)) => None,
                (_, attention) => attention,
            };
            let mark = if was_moved {
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

/// What step 3's snapshots did.
#[derive(Default)]
struct Snapshotted {
    /// The named workspace's snapshot operation, if it recorded one.
    named_op: Option<String>,
    /// Each working-copy commit a snapshot rewrote, to its new commit.
    rewritten: HashMap<CommitIdHex, CommitIdHex>,
}

/// What step 4 writes for.
struct Target {
    workspace: Option<WorkspaceId>,
    /// The write's agent: the request's, else the named workspace's owner.
    agent: Option<String>,
    /// Step 3's rewrites: a revision naming a commit step 3 rewrote follows
    /// it.
    rewritten: HashMap<CommitIdHex, CommitIdHex>,
    /// For an undo, the operation in full, as step 1 resolved it.
    undone: Option<OperationIdHex>,
}

/// Step 0: what a write can be refused for before it reads anything.
fn check(write: &Write) -> Result<(), ManagerError> {
    match write {
        Write::Describe { description, .. } => {
            rewrite::normalize_description(description)?;
        }
        Write::Edit {
            edits, description, ..
        } => {
            if let Some(description) = description {
                rewrite::normalize_description(description)?;
            }
            files::check_edits(edits)?;
        }
        Write::Abandon { .. } | Write::Undo { .. } => {}
    }
    Ok(())
}

/// The carry for every Reef workspace that moved to another commit of the
/// same change with another tree (ADR 0039). A move to another change, or
/// one that kept the tree, carries nothing.
fn carries(before: &[WorkingCopy], after: &[WorkingCopy]) -> Vec<(WorkspaceId, Carry)> {
    moved_copies(before, after)
        .into_iter()
        .filter(|(was, now)| was.change_id == now.change_id && was.tree != now.tree)
        .filter_map(|(was, now)| {
            let id = now.workspace.strip_prefix("reef-")?.parse().ok()?;
            let carry = Carry {
                from: was.commit_id.clone(),
                to: now.commit_id.clone(),
                base: None,
            };
            Some((id, carry))
        })
        .collect()
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

/// Step 1, on the repository thread. The request is checked before a
/// colocated repository's ref import writes anything.
async fn plan(
    head: &mut RepoHead,
    write: &Write,
    workspace: Option<WorkspaceId>,
    agent: Option<&str>,
) -> Result<Planned, ManagerError> {
    let jj_name = workspace.map(|id| id.jj_workspace_name());
    let jj_name = jj_name.as_deref();
    match write {
        Write::Describe {
            revision, expected, ..
        }
        | Write::Abandon { revision, expected } => {
            if revision.trim().is_empty() {
                return Err(RevisionError::Required.into());
            }
            let expected = rewrite::parse_expected(&head.repo.readonly(), expected)?;
            refresh_for_resolve(head, &import_meta(workspace, agent)).await?;
            let repo = head.repo.readonly();
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
        Write::Edit {
            target,
            expected,
            edits,
            ..
        } => {
            if target.revision().trim().is_empty() {
                return Err(RevisionError::Required.into());
            }
            let expected = rewrite::parse_expected(&head.repo.readonly(), expected)?;
            refresh_for_resolve(head, &import_meta(workspace, agent)).await?;
            let repo = head.repo.readonly();
            let scope = scope(head, jj_name);
            let found = match target {
                EditTarget::Rewrite(revision) => {
                    rewrite::writable(&repo, revision, expected.as_ref(), &scope).await?
                }
                EditTarget::NewOn(revision) => {
                    rewrite::buildable(&repo, revision, expected.as_ref(), &scope).await?
                }
            };
            if let Err(error) = files::dry_run(&repo, &found.commit_id, edits).await {
                // A Reef workspace on the commit itself is snapshotted at
                // step 3, which can change the tree the edits apply to: an
                // edit of `@` sees the files on disk, as a read of `@`
                // does. Step 4 decides then. Otherwise the refusal stands
                // before anything is recorded.
                let on_commit = rewrite::workspaces_at(&repo, &found.commit_id);
                let snapshotted = on_commit.iter().any(|name| name.starts_with("reef-"));
                if !snapshotted || !matches!(error, files::EditError::Edit { .. }) {
                    return Err(error.into());
                }
            }
            Ok(Planned {
                workspaces: rewrite::descendants_first(&repo, &found.workspaces).await?,
                undone: None,
            })
        }
    }
}

/// How a revision is checked at step 4.
#[derive(Clone, Copy)]
enum Check {
    /// A commit to rewrite: [`rewrite::writable`].
    Writable,
    /// A new change's parent: [`rewrite::buildable`].
    Buildable,
}

/// Resolves a write's revision again at step 4. A commit step 3's snapshot
/// rewrote (the caller named a working-copy commit by id and its workspace
/// had edits) is followed to its new commit; with an expected id, that is a
/// mismatch naming the new commit.
async fn resolve_target(
    head: &RepoHead,
    repo: &ReadonlyRepo,
    (revision, check): (&str, Check),
    expected: &str,
    target: &Target,
) -> Result<rewrite::Writable, ManagerError> {
    let jj_name = target.workspace.map(|id| id.jj_workspace_name());
    let scope = scope(head, jj_name.as_deref());
    let expected = rewrite::parse_expected(repo, expected)?;
    let resolve = async |revision: &str, expected: Option<&CommitIdHex>| match check {
        Check::Writable => rewrite::writable(repo, revision, expected, &scope).await,
        Check::Buildable => rewrite::buildable(repo, revision, expected, &scope).await,
    };
    let found = resolve(revision, expected.as_ref()).await;
    let Err(RewriteError::Hidden(old)) = &found else {
        return Ok(found?);
    };
    let Some(new) = target.rewritten.get(old) else {
        return Ok(found?);
    };
    if let Some(expected) = expected {
        return Err(RewriteError::Mismatch {
            expected,
            actual: new.clone(),
        }
        .into());
    }
    Ok(resolve(new.as_str(), None).await?)
}

/// Step 4 for an edit: resolve the target again, then rewrite it or build
/// a new change on it. Returns the operation (none when nothing changed)
/// and the commit the change is at afterwards.
async fn commit_edit(
    head: &mut RepoHead,
    repo: &ReadonlyRepo,
    edit_target: &EditTarget,
    expected: &str,
    (edits, description): (&[files::Edit], Option<&str>),
    (target, meta): (&Target, &mut OpMeta),
) -> Result<(Option<OperationIdHex>, CommitIdHex), ManagerError> {
    let check = match edit_target {
        EditTarget::Rewrite(_) => Check::Writable,
        EditTarget::NewOn(_) => Check::Buildable,
    };
    let found = resolve_target(
        head,
        repo,
        (edit_target.revision(), check),
        expected,
        target,
    )
    .await?;
    let edited = match edit_target {
        EditTarget::Rewrite(_) => {
            meta.description = format!("{}{}", oplog::EDIT_PREFIX, found.commit_id);
            head.repo
                .edit_commit(&found.commit_id, edits, description, meta)
                .await?
        }
        EditTarget::NewOn(_) => {
            meta.description = format!("{}{}", oplog::NEW_CHANGE_PREFIX, found.commit_id);
            Some(
                head.repo
                    .new_change_on(&found.commit_id, edits, description, meta)
                    .await?,
            )
        }
    };
    Ok(match edited {
        Some(edited) => (Some(edited.op_id), edited.commit_id),
        None => (None, found.commit_id),
    })
}

/// Step 4, on the repository thread: resolve and check again, then write.
/// Reads after the commit only degrade the answer: the write has happened.
async fn commit(
    head: &mut RepoHead,
    write: &Write,
    target: Target,
) -> Result<Written, ManagerError> {
    let mut meta = OpMeta::new(write.kind(), String::new());
    meta.workspace = target.workspace.map(|id| id.as_hex());
    meta.agent.clone_from(&target.agent);
    let repo = head.refresh().await?;
    let before = rewrite::working_copies(&repo).await?;
    let mut written = Written {
        op_id: None,
        change: None,
        abandoned: None,
        before,
        after: Vec::new(),
    };
    // The commit a describe or an edit leaves, for the response's change.
    let mut changed_commit = None;
    match write {
        Write::Describe {
            revision,
            description,
            expected,
        } => {
            let found =
                resolve_target(head, &repo, (revision, Check::Writable), expected, &target).await?;
            meta.description = format!("{}{}", oplog::DESCRIBE_PREFIX, found.commit_id);
            let described = head
                .repo
                .describe_commit(&found.commit_id, description, &meta)
                .await?;
            changed_commit = Some(
                described
                    .as_ref()
                    .map_or(&found.commit_id, |described| &described.commit_id)
                    .clone(),
            );
            written.op_id = described.map(|described| described.op_id);
        }
        Write::Abandon { revision, expected } => {
            let found =
                resolve_target(head, &repo, (revision, Check::Writable), expected, &target).await?;
            meta.description = format!("{}{}", oplog::ABANDON_PREFIX, found.commit_id);
            let abandoned = head.repo.abandon_commit(&found.commit_id, &meta).await?;
            written.op_id = Some(abandoned.op_id);
            written.abandoned = Some(found.commit_id);
        }
        Write::Undo { operation } => {
            let undone = match &target.undone {
                Some(undone) => undone.clone(),
                None => oplog::entry(&oplog::resolve_operation(&repo, operation).await?).id,
            };
            meta.description = format!("{}{undone}", oplog::REVERT_PREFIX);
            written.op_id = head.repo.revert_operation(&undone, &meta).await?;
        }
        Write::Edit {
            target: edit_target,
            expected,
            edits,
            description,
        } => {
            let (op_id, commit) = commit_edit(
                head,
                &repo,
                edit_target,
                expected,
                (edits, description.as_deref()),
                (&target, &mut meta),
            )
            .await?;
            changed_commit = Some(commit);
            written.op_id = op_id;
        }
    }
    let repo = head.repo.readonly();
    if let Some(commit) = changed_commit {
        let jj_name = target.workspace.map(|id| id.jj_workspace_name());
        let described = match Describer::new(&repo, &scope(head, jj_name.as_deref())) {
            Ok(describer) => describer.describe(&commit).await,
            Err(error) => Err(error),
        };
        match described {
            Ok(change) => written.change = Some(change),
            Err(error) => {
                tracing::warn!(error = %ErrorChain(&error), "cannot describe the change just written; answering without it");
            }
        }
    }
    match rewrite::working_copies(&repo).await {
        Ok(after) => written.after = after,
        Err(error) => {
            // No follows: each workspace adopts at its next snapshot.
            tracing::warn!(error = %ErrorChain(&error), "cannot read the working copies after the write; workspaces adopt it at their next snapshot");
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use reef_lib::jj::ChangeIdHex;

    use super::*;

    fn copy(workspace: &str, commit: &str, change: &str, tree: &str) -> WorkingCopy {
        WorkingCopy {
            workspace: workspace.to_owned(),
            commit_id: commit.parse::<CommitIdHex>().unwrap(),
            change_id: change.parse::<ChangeIdHex>().unwrap(),
            tree: tree.to_owned(),
            divergent: false,
        }
    }

    #[test]
    fn carries_only_moves_within_a_change_with_a_new_tree() {
        let a = "reef-00000000000000aa";
        let b = "reef-00000000000000bb";
        let c = "reef-00000000000000cc";
        let d = "reef-00000000000000dd";
        let before = [
            copy(a, "a1", "aa", "t1"),
            copy(b, "b1", "bb", "t1"),
            copy(c, "c1", "cc", "t1"),
            copy(d, "d1", "dd", "t1"),
            copy("default", "e1", "ee", "t1"),
        ];
        let after = [
            // Same change, new tree: carries.
            copy(a, "a2", "aa", "t2"),
            // Another change (a plain abandon): none.
            copy(b, "b2", "ff", "t2"),
            // Same tree (a describe): none.
            copy(c, "c2", "cc", "t1"),
            // Did not move: none.
            copy(d, "d1", "dd", "t1"),
            // Not a Reef workspace: none.
            copy("default", "e2", "ee", "t2"),
        ];
        let carries = carries(&before, &after);
        assert_eq!(carries.len(), 1, "{carries:?}");
        let (id, carry) = &carries[0];
        assert_eq!(id.jj_workspace_name(), a);
        assert_eq!(carry.from.as_str(), "a1");
        assert_eq!(carry.to.as_str(), "a2");
        assert_eq!(carry.base, None);
    }
}
