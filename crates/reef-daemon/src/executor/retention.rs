//! GC's ninth step (R3.05): finished jobs are deleted `[executor]
//! job_retention` after they finished, never before `[gc] keep_newer`,
//! with the record of the evidence they accepted. The evidence itself
//! stays, under R3.02's retention. Open jobs are never touched.

use std::time::Duration;

use crate::db::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// What the step did, or would do in a dry run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Jobs deleted, or that would be.
    pub removed: u64,
}

/// Deletes `repo`'s jobs that finished before `min(now - retention,
/// keep_newer)`; `retention` zero keeps them all. A dry run counts.
pub fn sweep(
    db: &Db,
    repo: RepoId,
    now: Timestamp,
    retention: Duration,
    keep_newer: Timestamp,
    dry_run: bool,
) -> Result<Swept, DbError> {
    if retention.is_zero() {
        return Ok(Swept::default());
    }
    let by_age = now
        .0
        .saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
    let before = Timestamp(by_age.min(keep_newer.0));
    let removed = if dry_run {
        db.finished_job_count(repo, before)?
    } else {
        db.delete_finished_jobs(repo, before)?
    };
    Ok(Swept { removed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::JobState;

    const DAY: i64 = 86_400_000;

    fn fixture() -> (Db, RepoId) {
        let db = Db::open_in_memory().unwrap();
        let repo: RepoId = "00000000000000bb".parse().unwrap();
        db.with(|conn| {
            conn.execute(
                "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
                 VALUES (?1, '/r', '/r/.jj/repo', 0, 0)",
                [repo],
            )
        })
        .unwrap();
        for (id, finished) in [("job_0000000000000001", 1), ("job_0000000000000002", 40)] {
            let mut job = crate::db::jobs::tests::job(repo, id, 0);
            job.state = JobState::Completed;
            job.finished_at = Some(Timestamp(finished * DAY));
            db.in_transaction(|tx| crate::db::insert_job_in(tx, &job))
                .unwrap();
        }
        (db, repo)
    }

    #[test]
    fn retention_floors_at_keep_newer_and_zero_keeps_all() {
        let (db, repo) = fixture();
        let now = Timestamp(45 * DAY);
        let month = Duration::from_hours(30 * 24);
        // keep_newer before the first job: nothing goes.
        let swept = sweep(&db, repo, now, month, Timestamp(0), false).unwrap();
        assert_eq!(swept.removed, 0);
        // `0s` keeps everything.
        let swept = sweep(&db, repo, now, Duration::ZERO, now, false).unwrap();
        assert_eq!(swept.removed, 0);
        // A dry run counts the job past the retention.
        let swept = sweep(&db, repo, now, month, now, true).unwrap();
        assert_eq!(swept.removed, 1);
        let swept = sweep(&db, repo, now, month, now, false).unwrap();
        assert_eq!(swept.removed, 1);
        assert!(db.job("job_0000000000000001").unwrap().is_none());
        assert!(db.job("job_0000000000000002").unwrap().is_some());
    }
}
