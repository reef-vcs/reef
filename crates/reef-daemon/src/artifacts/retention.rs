//! GC's seventh step, **evidence and artifacts** (R3.02, ADR 0056): after
//! history and before the reindex, under the create lock `run_repo` holds,
//! reading the head after the jj step. It follows the symbols step's mark,
//! sweep and cap.
//!
//! 1. **Mark**: [`files::live_set`] (`trunk() | bookmarks() |
//!    working_copies() | mutable()`) on the read pool, once, into the
//!    connection's temporary tables. Evidence is pinned ([`pinned`]) when
//!    its tree is a live commit's, or a link ties it to a live change.
//! 2. **Evidence sweep**: evidence that is not pinned and was added before
//!    `min(now - [evidence] retention, keep_newer)` goes, with its links and
//!    artifact index rows, 1,000 objects per transaction. Evidence keys stay.
//! 3. **Artifact sweep**, with the repository's [`ArtifactLock`] held:
//!    nothing used at or after `keep_newer` is touched. An artifact named
//!    by no evidence goes, row and file, once used before `min(keep_newer,
//!    now -` [`UPLOAD_GRACE`]`)`: an upload waiting for the `AddEvidence`
//!    that names it keeps a day's grace even with `[gc] undo_window = "0s"`.
//!    One named only by evidence that is not pinned becomes a tombstone once
//!    used before `min(now - [artifacts] retention, keep_newer)`; one named
//!    by pinned evidence stays.
//! 4. **Cap**: while the stored bytes pass `[artifacts] max_size`, what may
//!    be collected goes, least recently used first; what is pinned, or used
//!    within the upload grace, never does.
//! 5. **Tidy**: tombstones no evidence names lose their rows, partial
//!    uploads last written more than [`INCOMING_MAX_AGE`] ago go, and so do
//!    stored files with no stored row whose mtime is more than
//!    [`ORPHAN_MIN_AGE`] old. A stored row whose file is missing becomes a
//!    tombstone, logged: readers see it collected, and an upload of the
//!    genuine bytes stores them again.
//!
//! A row change commits before its file is removed, so a crash in between
//! leaves a file with no row, which the next tidy removes; the database
//! never names stored bytes that GC removed. A dry run decides the same and
//! changes nothing.
//!
//! [`ArtifactLock`]: super::ArtifactLock

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::files::{self, LiveSet};
use reef_lib::jj::revset::RevisionError;
use reef_lib::telemetry::elapsed_ms;

use super::{ArtifactBytes, Digest, Holder};
use crate::daemon::AppState;
use crate::db::{Db, DbError, PlannedArtifact};
use crate::model::{RepoId, Timestamp};
use crate::read::ReadError;

/// How old a partial upload's last write must be for the tidy to delete
/// it. Partial uploads are not in backups, so the backup floor does not
/// apply.
pub const INCOMING_MAX_AGE: Duration = Duration::from_hours(24);

/// The grace an artifact no pinned evidence names has, whatever
/// `keep_newer` is: the unnamed sweep and the cap leave alone what was used
/// (uploaded, or found stored by a probe) within it, so an upload is not
/// collected before the `AddEvidence` that names it. As long as a partial
/// upload is kept.
pub const UPLOAD_GRACE: Duration = INCOMING_MAX_AGE;

/// How old a stored file with no stored row (a crash between a commit and
/// its row) must be for the tidy to delete it.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_hours(1);

/// Artifacts whose rows change per transaction.
const APPLY_BATCH: usize = 1000;

/// What the step did, or would do in a dry run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    /// Evidence objects deleted.
    pub evidence_removed: u32,
    /// Artifacts whose bytes were deleted, tombstoned or not.
    pub artifacts_collected: u32,
    /// What their files had allocated; in a dry run, their sizes.
    pub artifact_bytes_freed: u64,
    /// The sizes of the stored artifacts after the step.
    pub artifact_bytes: u64,
}

/// Why the step stopped short.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RetentionError {
    /// The repository has no open thread.
    #[error("repository {0} is not open")]
    NotOpen(RepoId),
    /// The head could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// The live set could not be evaluated.
    #[error("evaluate the live set")]
    LiveSet(#[from] RevisionError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A file could not be read or removed.
    #[error("the artifact store")]
    Io(#[from] std::io::Error),
    /// A blocking task panicked.
    #[error("a retention task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Classify for RetentionError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotOpen(_) => ErrorKind::Unavailable,
            Self::Read(error) => error.kind(),
            Self::LiveSet(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::Io(error) => reef_lib::error::classify_by_source(error),
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

/// When evidence and artifacts are pinned by live work, as SQL over an
/// evidence row `e`; see [`crate::db::PINNED`]. R3.13's landing pin
/// extends that condition.
pub fn pinned() -> &'static str {
    crate::db::PINNED
}

/// `now - retention`, floored at `keep_newer`; `None` for a retention of
/// zero, which keeps everything by age.
fn cutoff(now: SystemTime, retention: Duration, keep_newer: SystemTime) -> Option<Timestamp> {
    if retention.is_zero() {
        return None;
    }
    let by_age = now.checked_sub(retention).unwrap_or(SystemTime::UNIX_EPOCH);
    Some(Timestamp::from(by_age.min(keep_newer)))
}

/// What the artifact sweep, cap and tidy decided.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// Rows and files to delete: `(digest, used_at)`.
    delete: Vec<(String, Timestamp)>,
    /// Files to delete, rows kept as tombstones.
    collect: Vec<(String, Timestamp)>,
    /// Tombstones no evidence names, to delete.
    forget: Vec<String>,
    /// The bytes of what goes.
    freed: u64,
    /// The stored bytes after.
    kept: u64,
}

/// Steps 3 to 5's decisions over `rows`: see the module docs. `floor` is
/// `keep_newer`, `grace` the earlier of it and `now -` [`UPLOAD_GRACE`].
fn plan(
    rows: &[PlannedArtifact],
    floor: Timestamp,
    grace: Timestamp,
    cutoff: Option<Timestamp>,
    max_size: u64,
) -> Plan {
    let mut plan = Plan::default();
    let mut capped = Vec::new();
    for artifact in rows {
        let row = &artifact.row;
        if !row.is_stored() {
            if !artifact.named {
                plan.forget.push(row.sha256.clone());
            }
            continue;
        }
        let past_grace = row.used_at < grace;
        if past_grace && !artifact.named {
            plan.delete.push((row.sha256.clone(), row.used_at));
            plan.freed += row.size;
        } else if artifact.named
            && !artifact.pinned
            && row.used_at < floor
            && cutoff.is_some_and(|cut| row.used_at < cut)
        {
            plan.collect.push((row.sha256.clone(), row.used_at));
            plan.freed += row.size;
        } else {
            plan.kept += row.size;
            if past_grace && !artifact.pinned {
                capped.push(artifact);
            }
        }
    }
    capped.sort_by(|a, b| (a.row.used_at, &a.row.sha256).cmp(&(b.row.used_at, &b.row.sha256)));
    for artifact in capped {
        if plan.kept <= max_size {
            break;
        }
        let row = &artifact.row;
        plan.kept -= row.size;
        plan.freed += row.size;
        // Only named artifacts are left here: the unnamed old ones went.
        plan.collect.push((row.sha256.clone(), row.used_at));
    }
    plan
}

/// Runs the step on `repo`, deleting nothing added or used at or after
/// `keep_newer`.
#[tracing::instrument(skip_all, fields(repo = %repo, dry_run = dry_run))]
pub async fn run(
    state: &AppState,
    repo: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
) -> Result<Retention, RetentionError> {
    let begun = Instant::now();
    let handle = state.repos.get(repo).ok_or(RetentionError::NotOpen(repo))?;
    let head = state.reads.current(&handle).await?;
    let live: LiveSet = state
        .reads
        .run(&handle, move || async move { files::live_set(&head).await })
        .await??;
    let live_ms = elapsed_ms(begun);
    let db = Arc::clone(&state.db);
    let result = sweep(state, repo, keep_newer, dry_run, &live).await;
    if let Err(error) = db.drop_live_tables() {
        tracing::warn!(error = %ErrorChain(&error), "cannot drop GC's temporary tables");
    }
    let report = result?;
    tracing::info!(
        live_commit_count = live.commit_count,
        live_tree_count = live.trees.len(),
        live_ms,
        evidence_removed_count = report.evidence_removed,
        artifacts_collected_count = report.artifacts_collected,
        artifact_freed_bytes = report.artifact_bytes_freed,
        artifact_bytes = report.artifact_bytes,
        elapsed_ms = elapsed_ms(begun),
        "evidence and artifacts collected"
    );
    Ok(report)
}

/// Steps 2 to 5, with the live set marked.
async fn sweep(
    state: &AppState,
    repo: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
    live: &LiveSet,
) -> Result<Retention, RetentionError> {
    let file = &state.config.file;
    let now = SystemTime::now();
    let floor = Timestamp::from(keep_newer);
    let grace = Timestamp::from(
        keep_newer.min(
            now.checked_sub(UPLOAD_GRACE)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        ),
    );
    let evidence_cutoff = cutoff(now, file.evidence.retention, keep_newer);
    let artifact_cutoff = cutoff(now, file.artifacts.retention, keep_newer);
    let max_size = file.artifacts.max_size.0;
    let db = Arc::clone(&state.db);
    let (trees, changes) = (live.trees.clone(), live.changes.clone());

    // 1. and 2.
    let lock = state.artifacts.lock(repo);
    let doomed = {
        let db = Arc::clone(&db);
        tokio::task::spawn_blocking(move || -> Result<u64, DbError> {
            db.create_live_tables(&trees, &changes)?;
            match evidence_cutoff {
                Some(cut) => db.doom_evidence(repo, cut),
                None => Ok(0),
            }
        })
        .await??
    };
    // A test pauses here to add and link evidence between the choice and
    // the deletion, which checks each object again.
    lock.pause(Holder::EvidenceSweep).await;
    let evidence_removed = match evidence_cutoff {
        Some(cut) if !dry_run && doomed > 0 => {
            let db = Arc::clone(&db);
            tokio::task::spawn_blocking(move || db.delete_doomed_evidence(repo, cut)).await??
        }
        _ => doomed,
    };

    // 3. to 5., holding off uploads' commits and stored checks.
    let _held = lock.acquire(Holder::Gc).await;
    let bytes = state.artifacts.bytes(repo);
    let report = tokio::task::spawn_blocking(move || {
        collect(
            &db,
            bytes.as_ref(),
            repo,
            floor,
            grace,
            artifact_cutoff,
            max_size,
            dry_run,
            now,
        )
    })
    .await??;
    Ok(Retention {
        evidence_removed: u32::try_from(evidence_removed).unwrap_or(u32::MAX),
        ..report
    })
}

/// Steps 3 to 5 on the blocking pool, the lock held.
#[expect(
    clippy::too_many_arguments,
    reason = "the step's inputs, each used once"
)]
fn collect(
    db: &Db,
    bytes: &dyn ArtifactBytes,
    repo: RepoId,
    floor: Timestamp,
    grace: Timestamp,
    cutoff: Option<Timestamp>,
    max_size: u64,
    dry_run: bool,
    now: SystemTime,
) -> Result<Retention, RetentionError> {
    let rows = db.plan_artifacts(repo)?;
    let plan = plan(&rows, floor, grace, cutoff, max_size);
    if plan.kept > max_size {
        tracing::warn!(
            max_size_bytes = max_size,
            artifact_bytes = plan.kept,
            "the artifacts live work and the undo window keep pass [artifacts] max_size"
        );
    }
    let collected = plan.delete.len() + plan.collect.len();
    if dry_run {
        return Ok(Retention {
            evidence_removed: 0,
            artifacts_collected: u32::try_from(collected).unwrap_or(u32::MAX),
            artifact_bytes_freed: plan.freed,
            artifact_bytes: plan.kept,
        });
    }
    let (mut collected, mut freed) = (0u32, 0u64);
    let mut deletes = plan.delete.chunks(APPLY_BATCH);
    let mut collects = plan.collect.chunks(APPLY_BATCH);
    let mut forgets = plan.forget.chunks(APPLY_BATCH);
    loop {
        let (delete, collect, forget) = (
            deletes.next().unwrap_or_default(),
            collects.next().unwrap_or_default(),
            forgets.next().unwrap_or_default(),
        );
        if delete.is_empty() && collect.is_empty() && forget.is_empty() {
            break;
        }
        // The rows first: a crash before the files go leaves orphans the
        // next tidy removes.
        let gone = db.apply_artifact_plan(repo, delete, collect, forget, Timestamp::from(now))?;
        for sha256 in gone {
            let Some(digest) = Digest::parse(&sha256) else {
                continue;
            };
            freed += bytes.remove(&digest)?;
            collected += 1;
        }
    }

    // 5. Partial uploads, orphans, and missing files.
    let (incoming_count, incoming_bytes) = bytes.sweep_incoming(
        now.checked_sub(INCOMING_MAX_AGE)
            .unwrap_or(SystemTime::UNIX_EPOCH),
    )?;
    let stored_rows = db.stored_artifact_digests(repo)?;
    let files = bytes.stored()?;
    let on_disk: HashSet<&str> = files.iter().map(|(digest, _, _)| digest.as_str()).collect();
    let orphan_before = now
        .checked_sub(ORPHAN_MIN_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut orphan_count = 0u32;
    for (digest, _, modified) in &files {
        if !stored_rows.contains(digest.as_str()) && *modified < orphan_before {
            bytes.remove(digest)?;
            orphan_count += 1;
        }
    }
    let missing: Vec<&String> = stored_rows
        .iter()
        .filter(|sha256| !on_disk.contains(sha256.as_str()))
        .collect();
    for sha256 in &missing {
        // Under the lock: no upload is storing it meanwhile.
        db.tombstone_artifact(repo, sha256, None, Timestamp::from(now))?;
        tracing::warn!(
            sha256 = %sha256,
            "a stored artifact's file is missing; it reads as collected until the genuine bytes are uploaded again"
        );
    }
    if incoming_count + orphan_count > 0 {
        tracing::info!(
            incoming_count,
            incoming_bytes,
            orphan_count,
            "artifact store tidied"
        );
    }
    let kept = db
        .plan_artifacts(repo)?
        .iter()
        .filter(|artifact| artifact.row.is_stored())
        .map(|artifact| artifact.row.size)
        .sum();
    Ok(Retention {
        evidence_removed: 0,
        artifacts_collected: collected,
        artifact_bytes_freed: freed,
        artifact_bytes: kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ArtifactRow;

    fn artifact(sha: &str, size: u64, used_at: i64, named: bool, pinned: bool) -> PlannedArtifact {
        PlannedArtifact {
            row: ArtifactRow {
                repo_id: "aaaa000000000001".parse().unwrap(),
                sha256: sha.to_owned(),
                size,
                stored_at: Timestamp(used_at),
                used_at: Timestamp(used_at),
                collected_at: None,
                added_by_agent: None,
                remote_token_id: None,
            },
            named,
            pinned,
        }
    }

    #[test]
    fn the_sweep_follows_its_table() {
        let floor = Timestamp(1000);
        let cut = Some(Timestamp(500));
        let mut tombstone = artifact("t", 5, 10, false, false);
        tombstone.row.collected_at = Some(Timestamp(20));
        let mut named_tombstone = artifact("u", 5, 10, true, false);
        named_tombstone.row.collected_at = Some(Timestamp(20));
        let rows = vec![
            artifact("unnamed-old", 1, 900, false, false),
            artifact("unnamed-new", 2, 1000, false, false),
            artifact("unpinned-past", 4, 400, true, false),
            artifact("unpinned-within", 8, 600, true, false),
            artifact("pinned-old", 16, 1, true, true),
            tombstone,
            named_tombstone,
        ];
        let plan = plan(&rows, floor, floor, cut, u64::MAX);
        assert_eq!(plan.delete, [("unnamed-old".to_owned(), Timestamp(900))]);
        assert_eq!(plan.collect, [("unpinned-past".to_owned(), Timestamp(400))]);
        assert_eq!(plan.forget, ["t"]);
        assert_eq!((plan.freed, plan.kept), (5, 2 + 8 + 16));
        // No age retention: only the unnamed go.
        let plan = super::plan(&rows, floor, floor, None, u64::MAX);
        assert_eq!(plan.collect.len(), 0);
    }

    // R3.02 review: with `keep_newer` at now (`undo_window = "0s"`), an
    // upload within the grace is neither swept nor capped.
    #[test]
    fn the_upload_grace_holds_whatever_the_floor() {
        let (floor, grace) = (Timestamp(10_000), Timestamp(1000));
        let rows = vec![
            artifact("unnamed-fresh", 1, 5000, false, false),
            artifact("unnamed-stale", 2, 900, false, false),
            artifact("unpinned-fresh", 4, 5000, true, false),
            artifact("unpinned-stale", 8, 800, true, false),
        ];
        let plan = plan(&rows, floor, grace, None, 0);
        assert_eq!(plan.delete, [("unnamed-stale".to_owned(), Timestamp(900))]);
        assert_eq!(
            plan.collect,
            [("unpinned-stale".to_owned(), Timestamp(800))],
            "the cap takes only what is past the grace"
        );
        assert_eq!(plan.kept, 1 + 4);
    }

    #[test]
    fn the_cap_collects_least_recently_used_first() {
        let floor = Timestamp(1000);
        let rows = vec![
            artifact("c", 10, 300, true, false),
            artifact("a", 10, 100, true, false),
            artifact("b", 10, 200, true, false),
            artifact("pinned", 10, 50, true, true),
            artifact("recent", 10, 2000, true, false),
        ];
        let plan = plan(&rows, floor, floor, None, 15);
        assert_eq!(
            plan.collect,
            [
                ("a".to_owned(), Timestamp(100)),
                ("b".to_owned(), Timestamp(200)),
                ("c".to_owned(), Timestamp(300)),
            ]
        );
        assert_eq!(plan.kept, 20, "pinned and recent stay, past the cap");
    }
}
