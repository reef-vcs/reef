//! GC's eighth step (R3.04, ADR 0058): provenance compaction and expiry,
//! after evidence and artifacts and before the reindex. Nothing recorded at
//! or after `keep_newer` is touched.
//!
//! 1. **Compact**: of the records recorded before `min(now -
//!    [provenance] compact_after, keep_newer)`, in each run of one session's
//!    consecutive records of one change under one context digest, keep the
//!    first and the last, and any of a commit visible in the head's view;
//!    delete the rest.
//! 2. **Expire**: of the records recorded before `min(now - [provenance]
//!    retention, keep_newer)`, delete those of changes with no visible
//!    commit in the head's view (abandoned work, or work undone away).
//!    Records of visible changes are kept for good once compacted.
//! 3. **Sessions**: delete ended sessions with no record left.
//!
//! Each change is resolved with an index lookup, not a revset walk. Held
//! keys that remaining records name stay pinned in R3.03's sweep
//! ([`crate::identity::retention::pinned`]).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use reef_lib::jj::files;

use reef_lib::telemetry::elapsed_ms;

use crate::daemon::AppState;
use crate::model::{RepoId, Timestamp};

/// What the step did, or would have in a dry run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Records compacted away.
    pub compacted: u64,
    /// Records expired.
    pub removed: u64,
    /// Ended sessions with no record left, deleted.
    pub sessions: u64,
}

/// Why the step stopped short.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProvenanceRetentionError {
    /// The repository is not open.
    #[error("repository {0} is not open")]
    NotOpen(RepoId),
    /// Reading the head failed.
    #[error(transparent)]
    Read(#[from] crate::read::ReadError),
    /// Resolving the changes failed.
    #[error(transparent)]
    Files(#[from] files::FileError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    /// A blocking task did not finish.
    #[error("the provenance step's task failed: {0}")]
    Task(String),
}

/// `min(now - age, keep_newer)`; `None` when `age` is zero (never).
fn cutoff(age: std::time::Duration, keep_newer: SystemTime) -> Option<Timestamp> {
    if age.is_zero() {
        return None;
    }
    let by_age = SystemTime::now()
        .checked_sub(age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Some(Timestamp::from(by_age.min(keep_newer)))
}

/// Runs the step on `repo`.
#[tracing::instrument(skip_all, fields(repo = %repo, dry_run = dry_run))]
pub async fn run(
    state: &AppState,
    repo: RepoId,
    keep_newer: SystemTime,
    dry_run: bool,
) -> Result<Swept, ProvenanceRetentionError> {
    let begun = Instant::now();
    let settings = &state.config.file.provenance;
    let compact_before = cutoff(settings.compact_after, keep_newer);
    let expire_before = cutoff(settings.retention, keep_newer);
    let earliest = match (compact_before, expire_before) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let db = Arc::clone(&state.db);
    let mut swept = Swept::default();
    if let Some(before) = earliest {
        let changes = {
            let db = Arc::clone(&db);
            tokio::task::spawn_blocking(move || db.provenance_changes_before(repo, before))
                .await
                .map_err(|error| ProvenanceRetentionError::Task(error.to_string()))??
        };
        let resolved: HashSet<String> = changes.iter().cloned().collect();
        let visible_commits = if changes.is_empty() {
            std::collections::BTreeMap::default()
        } else {
            let handle = state
                .repos
                .get(repo)
                .ok_or(ProvenanceRetentionError::NotOpen(repo))?;
            let head = state.reads.current(&handle).await?;
            state
                .reads
                .run(&handle, move || async move {
                    files::visible_commits_of_changes(&head, &changes).await
                })
                .await??
        };
        let visible_changes: HashSet<String> = visible_commits
            .iter()
            .filter(|(_, commits)| !commits.is_empty())
            .map(|(change, _)| change.clone())
            .collect();
        let visible: HashSet<String> = visible_commits.into_values().flatten().collect();
        let db = Arc::clone(&db);
        let (compacted, removed) = tokio::task::spawn_blocking(move || {
            let compact = match compact_before {
                Some(before) => db.provenance_to_compact(repo, before, &visible)?,
                None => Vec::new(),
            };
            let mut expire = match expire_before {
                Some(before) => {
                    db.provenance_to_expire(repo, before, &resolved, &visible_changes)?
                }
                None => Vec::new(),
            };
            // A record both rules take counts once, as a compaction, in a dry
            // run as in a real one.
            let compacted_ids: HashSet<&String> = compact.iter().collect();
            expire.retain(|id| !compacted_ids.contains(id));
            let count = |ids: &[String]| u64::try_from(ids.len()).unwrap_or(u64::MAX);
            if dry_run {
                return Ok::<_, crate::db::DbError>((count(&compact), count(&expire)));
            }
            Ok((
                db.delete_provenance(repo, &compact)?,
                db.delete_provenance(repo, &expire)?,
            ))
        })
        .await
        .map_err(|error| ProvenanceRetentionError::Task(error.to_string()))??;
        swept.compacted = compacted;
        swept.removed = removed;
    }
    let keep = Timestamp::from(keep_newer);
    swept.sessions =
        tokio::task::spawn_blocking(move || db.sweep_provenance_sessions(repo, keep, dry_run))
            .await
            .map_err(|error| ProvenanceRetentionError::Task(error.to_string()))??;
    tracing::info!(
        compacted_count = swept.compacted,
        removed_count = swept.removed,
        session_count = swept.sessions,
        elapsed_ms = elapsed_ms(begun),
        "provenance compacted and expired"
    );
    Ok(swept)
}
