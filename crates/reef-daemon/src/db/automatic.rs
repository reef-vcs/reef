//! The reads the automatic service makes (R3.45, ADR 0081): each cheap and
//! indexed, so a pass can compare every open proposal's evaluation key
//! without verifying a signature or reading a diff.

use rusqlite::{OptionalExtension as _, params};

use super::jobs::{COLUMNS as JOB_COLUMNS, from_row as job_from_row};
use super::lifecycle::{LIFECYCLE_COLUMNS, lifecycle_from_row};
use super::{Db, DbError, JobRow, LifecycleRow, StoredLanding};
use crate::model::{RepoId, Timestamp};

/// The most proposals a pass reads.
pub(crate) const MAX_OPEN_PROPOSALS: usize = 1_000;

impl Db {
    /// The changes of `repo` in review (`proposed` or `queued`), the
    /// longest waiting first, at most 1,000.
    pub fn open_proposals(&self, repo: RepoId) -> Result<Vec<LifecycleRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {LIFECYCLE_COLUMNS} FROM lifecycle
                 WHERE repo_id = ?1 AND state IN ('proposed', 'queued')
                 ORDER BY updated_at, change_id LIMIT ?2"
            ))?;
            let rows = statement
                .query_map(
                    params![repo, i64::try_from(MAX_OPEN_PROPOSALS).unwrap_or(i64::MAX)],
                    lifecycle_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// The ids of the evidence objects on `tree`, sorted: an index-only read
    /// of `evidence_by_tree`.
    pub fn evidence_ids_on_tree(&self, repo: RepoId, tree: &str) -> Result<Vec<String>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT id FROM evidence WHERE repo_id = ?1 AND tree_id = ?2 ORDER BY id",
            )?;
            let ids = statement
                .query_map(params![repo, tree], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(ids)
        })
    }

    /// How many provenance records `change` has, and the newest one's time.
    pub fn provenance_mark(&self, repo: RepoId, change: &str) -> Result<(u64, i64), DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT count(*), coalesce(max(recorded_at), 0) FROM provenance
                 WHERE repo_id = ?1 AND change_id = ?2",
                params![repo, change],
                |row| {
                    Ok((
                        u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                        row.get::<_, i64>(1)?,
                    ))
                },
            )
        })
    }

    /// Every job of `repo` on `tree`, the newest first.
    pub fn jobs_on_tree(&self, repo: RepoId, tree: &str) -> Result<Vec<JobRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 AND tree_id = ?2
                 ORDER BY submitted_at DESC, id DESC LIMIT 200"
            ))?;
            let rows = statement
                .query_map(params![repo, tree], job_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// When an executor last claimed a job of `repo`, and when a job of
    /// `repo` last expired with nobody claiming it.
    pub fn executor_seen(
        &self,
        repo: RepoId,
    ) -> Result<(Option<Timestamp>, Option<Timestamp>), DbError> {
        self.with(|conn| {
            let claimed = conn
                .query_row(
                    "SELECT max(claimed_at) FROM jobs WHERE repo_id = ?1",
                    params![repo],
                    |row| row.get::<_, Option<Timestamp>>(0),
                )
                .optional()?
                .flatten();
            let unclaimed = conn
                .query_row(
                    "SELECT max(finished_at) FROM jobs
                     WHERE repo_id = ?1 AND state = 'expired' AND end_reason = 'queue_timeout'",
                    params![repo],
                    |row| row.get::<_, Option<Timestamp>>(0),
                )
                .optional()?
                .flatten();
            Ok((claimed, unclaimed))
        })
    }

    /// The landings of `change`, newest first, at most `limit`.
    pub fn landings_of_change(
        &self,
        repo: RepoId,
        change: &str,
        limit: u32,
    ) -> Result<Vec<StoredLanding>, DbError> {
        self.landings_where(
            "WHERE repo_id = ?1 AND id IN (SELECT landing_id FROM landing_changes
               WHERE repo_id = ?1 AND change_id = ?2)
             ORDER BY created_at DESC, id DESC LIMIT ?3",
            &[&repo, &change, &i64::from(limit)],
        )
    }

    /// Whether `change` has an instruction the daemon wrote with text
    /// `text_sha256` since `since` (its latest proposal), and whether it has
    /// one still unfinished (R3.45).
    pub fn daemon_instructions(
        &self,
        repo: RepoId,
        change: &str,
        since: Timestamp,
        text_sha256: &str,
    ) -> Result<(bool, bool), DbError> {
        self.with(|conn| {
            let same = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM comments
                   WHERE repo_id = ?1 AND change_id = ?2 AND actor = 'daemon'
                     AND created_at >= ?3 AND text_sha256 = ?4)",
                params![repo, change, since, text_sha256],
                |row| row.get::<_, bool>(0),
            )?;
            let open = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM comments
                   WHERE repo_id = ?1 AND change_id = ?2 AND actor = 'daemon'
                     AND state IN ('awaiting_confirmation', 'pending', 'delivered', 'running'))",
                params![repo, change],
                |row| row.get::<_, bool>(0),
            )?;
            Ok((same, open))
        })
    }

    /// Whether an open batch's trial asked for job `id`.
    pub fn job_in_open_trial(&self, repo: RepoId, id: &str) -> Result<bool, DbError> {
        let pattern = format!("%\"{id}\"%");
        self.with(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM landing_batch_trials t
                   JOIN landing_batches b ON b.repo_id = t.repo_id AND b.id = t.batch_id
                   WHERE t.repo_id = ?1 AND b.state != 'finished' AND t.job_ids LIKE ?2)",
                params![repo, pattern],
                |row| row.get::<_, bool>(0),
            )
        })
    }

    /// When an executor last claimed a job of `repo` with `kind`, and when
    /// one expired with nobody claiming it.
    pub fn executor_seen_kind(
        &self,
        repo: RepoId,
        kind: &str,
    ) -> Result<(Option<Timestamp>, Option<Timestamp>), DbError> {
        let pattern = format!("%{}%", serde_json::Value::String(kind.to_owned()));
        self.with(|conn| {
            conn.query_row(
                "SELECT
                   (SELECT max(claimed_at) FROM jobs
                      WHERE repo_id = ?1 AND kinds LIKE ?2),
                   (SELECT max(finished_at) FROM jobs
                      WHERE repo_id = ?1 AND kinds LIKE ?2
                        AND state = 'expired' AND end_reason = 'queue_timeout')",
                params![repo, pattern],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
    }

    /// Whether the landing service has an open job of `repo` with `kind`.
    pub fn open_service_job_of_kind(&self, repo: RepoId, kind: &str) -> Result<bool, DbError> {
        let pattern = format!("%{}%", serde_json::Value::String(kind.to_owned()));
        self.with(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM jobs
                   WHERE repo_id = ?1 AND state IN ('queued', 'running')
                     AND requested_by_agent = 'reef-landing' AND kinds LIKE ?2)",
                params![repo, pattern],
                |row| row.get::<_, bool>(0),
            )
        })
    }
}
