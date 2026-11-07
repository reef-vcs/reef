//! Queries on `jobs` and `job_evidence` (R3.05, ADR 0059).
//!
//! A job row is one request for checks on a tree and its life: queued,
//! running under a claim, then completed, cancelled or expired. Its JSON
//! columns hold sorted, deduplicated string arrays. A claim token is kept
//! only as its blake3 digest. A `job_evidence` row records one evidence
//! object a job accepted, with its kind and target as the verified payload
//! gave them, for the completeness check; it has no foreign key to
//! `evidence`, whose retention runs on its own schedule.
//!
//! Every transition is one immediate transaction that reads the row again
//! inside it ([`job_in`], [`update_job_in`]); the executor module runs them
//! through [`Db::transact_then`]. Both tables are per repository and deleted
//! with it ([`Db::delete_repository`]); GC's ninth step deletes finished
//! jobs ([`Db::delete_finished_jobs`]).

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::{bytes_at, integer_at, text_at};
use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, unique_match};
use crate::model::{RepoId, Timestamp};

/// The record kind errors name.
const WHAT: &str = "job";

/// Jobs GC deletes per transaction.
const SWEEP_BATCH: usize = 1000;

/// A job's state, as the `state` column holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting for a claim.
    Queued,
    /// Claimed, under a lease.
    Running,
    /// Completed by its executor.
    Completed,
    /// Cancelled by a requester.
    Cancelled,
    /// Ended by the daemon.
    Expired,
}

impl JobState {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    /// The state the column's text names; `None` for anything else.
    pub fn parse(text: &str) -> Option<JobState> {
        Some(match text {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            _ => return None,
        })
    }

    /// Queued or running.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// Who holds, or last held, a job: the executor columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutorColumns {
    /// Its declared name.
    pub name: String,
    /// Its declared version.
    pub version: String,
    /// Its declared labels.
    pub labels: Vec<String>,
    /// Its declared kinds.
    pub kinds: Vec<String>,
    /// The highest protocol version it declared.
    pub protocol: u32,
    /// The claim's `reef-agent`.
    pub agent: Option<String>,
    /// The remote token it claimed through.
    pub token: Option<String>,
    /// That token's linked identity, by id.
    pub identity_id: Option<String>,
}

/// One row of `jobs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `job_` and 16 hex digits.
    pub id: String,
    /// The protocol version an executor needs to claim it.
    pub protocol: u32,
    /// The tree to check.
    pub tree_id: String,
    /// A commit with that tree, when submitted by revision.
    pub commit_id: Option<String>,
    /// The change, when submitted by revision.
    pub change_id: Option<String>,
    /// The kinds requested.
    pub kinds: Vec<String>,
    /// The targets requested.
    pub targets: Vec<String>,
    /// The labels an executor must have.
    pub requirements: Vec<String>,
    /// SHA-256 of the canonical spec, for the dedupe.
    pub spec_digest: String,
    /// One attempt's wall time.
    pub wall_time_ms: i64,
    /// One attempt's CPU time, when limited.
    pub cpu_time_ms: Option<i64>,
    /// Higher is claimed first.
    pub priority: i32,
    /// Where it is.
    pub state: JobState,
    /// `done`, `budget_exhausted` or `failed`, once completed.
    pub outcome: Option<String>,
    /// `queue_timeout`, `attempts` or `deadline`, once expired.
    pub end_reason: Option<String>,
    /// The executor's message.
    pub message: Option<String>,
    /// The current or last attempt; 0 before the first claim.
    pub attempt: u32,
    /// Attempts before a lapse expires it.
    pub max_attempts: u32,
    /// When it was submitted.
    pub submitted_at: Timestamp,
    /// When it expires unless claimed.
    pub queue_expires_at: Timestamp,
    /// The submitter's `reef-agent`.
    pub requested_by_agent: Option<String>,
    /// The submitting remote token.
    pub requested_by_token: Option<String>,
    /// blake3 of the current attempt's claim token.
    pub claim_digest: Option<Vec<u8>>,
    /// When the current or last attempt was claimed.
    pub claimed_at: Option<Timestamp>,
    /// The current claim's lease.
    pub lease_ms: Option<i64>,
    /// When the current claim lapses.
    pub lease_expires_at: Option<Timestamp>,
    /// When the daemon expires the running attempt.
    pub deadline_at: Option<Timestamp>,
    /// The current or last executor; `None` before the first claim.
    pub executor: Option<ExecutorColumns>,
    /// The attempt's progress, as JSON.
    pub progress: Option<String>,
    /// Reported wall time used.
    pub usage_wall_ms: Option<i64>,
    /// Reported CPU time used.
    pub usage_cpu_ms: Option<i64>,
    /// Observed time over attempts.
    pub elapsed_ms: i64,
    /// When it reached a terminal state.
    pub finished_at: Option<Timestamp>,
    /// Who cancelled it.
    pub cancelled_by_agent: Option<String>,
    /// Why.
    pub cancel_reason: Option<String>,
}

/// One row of `job_evidence`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEvidenceRow {
    /// The repository.
    pub repo_id: RepoId,
    /// The job.
    pub job_id: String,
    /// The evidence.
    pub evidence_id: String,
    /// Its place in the order the job accepted evidence, from 1.
    pub seq: i64,
    /// The attempt that sent it.
    pub attempt: u32,
    /// Its kind, from the verified payload.
    pub kind: String,
    /// Its target, from the verified payload.
    pub target: Option<String>,
    /// The call stored it for the first time.
    pub created: bool,
    /// Created, and started no earlier than five minutes before the claim.
    pub fresh: bool,
    /// When the job accepted it.
    pub accepted_at: Timestamp,
}

/// Which jobs a listing returns: the fields AND, and an empty list matches
/// everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobFilter {
    /// Only this repository.
    pub repo: Option<RepoId>,
    /// Only these states.
    pub states: Vec<JobState>,
    /// Only this tree.
    pub tree_id: Option<String>,
    /// Only this change.
    pub change_id: Option<String>,
    /// Only this executor name.
    pub executor: Option<String>,
    /// Only jobs submitted at or after this time.
    pub since: Option<Timestamp>,
}

/// Where a listing goes on from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobCursor {
    /// Newest first: after `(submitted_at, id)`.
    Newest(Timestamp, String),
    /// Queue order: after `(priority, submitted_at, id)`.
    Queue(i32, Timestamp, String),
}

const COLUMNS: &str = "repo_id, id, protocol, tree_id, commit_id, change_id, kinds, targets,
     requirements, spec_digest, wall_time_ms, cpu_time_ms, priority, state, outcome, end_reason,
     message, attempt, max_attempts, submitted_at, queue_expires_at, requested_by_agent,
     requested_by_token, claim_digest, claimed_at, lease_ms, lease_expires_at, deadline_at,
     executor_name, executor_version, executor_labels, executor_kinds, executor_protocol,
     executor_agent, executor_token, executor_identity_id, progress, usage_wall_ms, usage_cpu_ms,
     elapsed_ms, finished_at, cancelled_by_agent, cancel_reason";

/// A JSON array of strings.
fn json_list(list: &[String]) -> String {
    serde_json::to_string(list).unwrap_or_else(|_| "[]".to_owned())
}

/// A JSON array column; empty when it is not one.
fn list_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Vec<String>> {
    Ok(text_at(row, idx)?
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default())
}

fn optional_integer(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<i64>> {
    Ok(match row.get::<_, SqlValue>(idx)? {
        SqlValue::Integer(n) => Some(n),
        _ => None,
    })
}

fn optional_time(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Timestamp>> {
    Ok(optional_integer(row, idx)?.map(Timestamp))
}

fn small(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    let executor = text_at(row, 28)?.map(|name| ExecutorColumns {
        name,
        version: String::new(),
        labels: Vec::new(),
        kinds: Vec::new(),
        protocol: 0,
        agent: None,
        token: None,
        identity_id: None,
    });
    let executor = match executor {
        Some(mut executor) => {
            executor.version = text_at(row, 29)?.unwrap_or_default();
            executor.labels = list_at(row, 30)?;
            executor.kinds = list_at(row, 31)?;
            executor.protocol = small(integer_at(row, 32)?);
            executor.agent = text_at(row, 33)?;
            executor.token = text_at(row, 34)?;
            executor.identity_id = text_at(row, 35)?;
            Some(executor)
        }
        None => None,
    };
    Ok(JobRow {
        repo_id: row.get(0)?,
        id: text_at(row, 1)?.unwrap_or_default(),
        protocol: small(integer_at(row, 2)?),
        tree_id: text_at(row, 3)?.unwrap_or_default(),
        commit_id: text_at(row, 4)?,
        change_id: text_at(row, 5)?,
        kinds: list_at(row, 6)?,
        targets: list_at(row, 7)?,
        requirements: list_at(row, 8)?,
        spec_digest: text_at(row, 9)?.unwrap_or_default(),
        wall_time_ms: integer_at(row, 10)?,
        cpu_time_ms: optional_integer(row, 11)?,
        priority: i32::try_from(integer_at(row, 12)?).unwrap_or(0),
        // A state Reef did not write reads as expired: never claimable.
        state: text_at(row, 13)?
            .as_deref()
            .and_then(JobState::parse)
            .unwrap_or(JobState::Expired),
        outcome: text_at(row, 14)?,
        end_reason: text_at(row, 15)?,
        message: text_at(row, 16)?,
        attempt: small(integer_at(row, 17)?),
        max_attempts: small(integer_at(row, 18)?),
        submitted_at: Timestamp(integer_at(row, 19)?),
        queue_expires_at: Timestamp(integer_at(row, 20)?),
        requested_by_agent: text_at(row, 21)?,
        requested_by_token: text_at(row, 22)?,
        claim_digest: bytes_at(row, 23)?,
        claimed_at: optional_time(row, 24)?,
        lease_ms: optional_integer(row, 25)?,
        lease_expires_at: optional_time(row, 26)?,
        deadline_at: optional_time(row, 27)?,
        executor,
        progress: text_at(row, 36)?,
        usage_wall_ms: optional_integer(row, 37)?,
        usage_cpu_ms: optional_integer(row, 38)?,
        elapsed_ms: integer_at(row, 39)?,
        finished_at: optional_time(row, 40)?,
        cancelled_by_agent: text_at(row, 41)?,
        cancel_reason: text_at(row, 42)?,
    })
}

/// The values of every column of `job`, in [`COLUMNS`] order.
fn values(job: &JobRow) -> Vec<SqlValue> {
    let text = |value: &str| SqlValue::Text(value.to_owned());
    let optional_text = |value: &Option<String>| value.as_deref().map_or(SqlValue::Null, text);
    let optional_int = |value: Option<i64>| value.map_or(SqlValue::Null, SqlValue::Integer);
    let time = |value: Option<Timestamp>| value.map_or(SqlValue::Null, |t| SqlValue::Integer(t.0));
    let executor = job.executor.as_ref();
    vec![
        text(&job.repo_id.as_hex()),
        text(&job.id),
        SqlValue::Integer(i64::from(job.protocol)),
        text(&job.tree_id),
        optional_text(&job.commit_id),
        optional_text(&job.change_id),
        text(&json_list(&job.kinds)),
        text(&json_list(&job.targets)),
        text(&json_list(&job.requirements)),
        text(&job.spec_digest),
        SqlValue::Integer(job.wall_time_ms),
        optional_int(job.cpu_time_ms),
        SqlValue::Integer(i64::from(job.priority)),
        text(job.state.as_str()),
        optional_text(&job.outcome),
        optional_text(&job.end_reason),
        optional_text(&job.message),
        SqlValue::Integer(i64::from(job.attempt)),
        SqlValue::Integer(i64::from(job.max_attempts)),
        SqlValue::Integer(job.submitted_at.0),
        SqlValue::Integer(job.queue_expires_at.0),
        optional_text(&job.requested_by_agent),
        optional_text(&job.requested_by_token),
        job.claim_digest
            .clone()
            .map_or(SqlValue::Null, SqlValue::Blob),
        time(job.claimed_at),
        optional_int(job.lease_ms),
        time(job.lease_expires_at),
        time(job.deadline_at),
        executor.map_or(SqlValue::Null, |e| text(&e.name)),
        executor.map_or(SqlValue::Null, |e| text(&e.version)),
        executor.map_or(SqlValue::Null, |e| text(&json_list(&e.labels))),
        executor.map_or(SqlValue::Null, |e| text(&json_list(&e.kinds))),
        executor.map_or(SqlValue::Null, |e| SqlValue::Integer(i64::from(e.protocol))),
        executor.map_or(SqlValue::Null, |e| optional_text(&e.agent)),
        executor.map_or(SqlValue::Null, |e| optional_text(&e.token)),
        executor.map_or(SqlValue::Null, |e| optional_text(&e.identity_id)),
        optional_text(&job.progress),
        optional_int(job.usage_wall_ms),
        optional_int(job.usage_cpu_ms),
        SqlValue::Integer(job.elapsed_ms),
        time(job.finished_at),
        optional_text(&job.cancelled_by_agent),
        optional_text(&job.cancel_reason),
    ]
}

/// Inserts `job`.
pub fn insert_job_in(tx: &Transaction<'_>, job: &JobRow) -> Result<(), DbError> {
    let placeholders = vec!["?"; values(job).len()].join(", ");
    tx.execute(
        &format!("INSERT INTO jobs({COLUMNS}) VALUES ({placeholders})"),
        params_from_iter(values(job)),
    )?;
    Ok(())
}

/// Writes every column of `job` over its row.
pub fn update_job_in(tx: &Transaction<'_>, job: &JobRow) -> Result<(), DbError> {
    let names: Vec<&str> = COLUMNS.split(',').map(str::trim).collect();
    let set = names
        .iter()
        .skip(2)
        .map(|name| format!("{name} = ?"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut all = values(job);
    let key = all.drain(..2).collect::<Vec<_>>();
    all.extend(key);
    let changed = tx.execute(
        &format!("UPDATE jobs SET {set} WHERE repo_id = ? AND id = ?"),
        params_from_iter(all),
    )?;
    if changed == 0 {
        return Err(DbError::NotFound {
            what: WHAT,
            id: job.id.clone(),
        });
    }
    Ok(())
}

/// The job with the full id `id`, in any repository.
pub fn job_in(tx: &Transaction<'_>, id: &str) -> Result<Option<JobRow>, DbError> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM jobs WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
}

/// The open job of `repo` with the spec digest `digest` that `token`
/// submitted (`None`: the socket), the oldest first.
pub fn open_job_by_spec_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    digest: &str,
    token: Option<&str>,
) -> Result<Option<JobRow>, DbError> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM jobs
                 WHERE repo_id = ?1 AND spec_digest = ?2 AND requested_by_token IS ?3
                   AND state IN ('queued', 'running')
                 ORDER BY submitted_at, id LIMIT 1"
            ),
            params![repo, digest, token],
            from_row,
        )
        .optional()?)
}

/// Whether `repo` is still registered.
pub fn repository_exists_in(tx: &Transaction<'_>, repo: RepoId) -> Result<bool, DbError> {
    Ok(tx
        .query_row("SELECT 1 FROM repositories WHERE id = ?1", [repo], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// The `seq` the next evidence `job` accepts takes.
pub fn next_job_evidence_seq_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    job: &str,
) -> Result<i64, DbError> {
    let last: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM job_evidence WHERE repo_id = ?1 AND job_id = ?2",
        params![repo, job],
        |row| row.get(0),
    )?;
    Ok(last + 1)
}

/// How many jobs are queued or running across the daemon.
pub fn open_job_count_in(tx: &Transaction<'_>) -> Result<u64, DbError> {
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM jobs WHERE state IN ('queued', 'running')",
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Queued jobs, in claim order: priority, then submission, then id.
pub fn queued_jobs_in(tx: &Transaction<'_>) -> Result<Vec<JobRow>, DbError> {
    let mut stmt = tx.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM jobs WHERE state = 'queued'
         ORDER BY priority DESC, submitted_at, id"
    ))?;
    Ok(stmt
        .query_map([], from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Inserts `row`, unless the job already accepted the object: a re-sent
/// object keeps its first row, so it never becomes the job's latest word
/// on a pair. Returns whether it inserted.
pub fn insert_job_evidence_in(tx: &Transaction<'_>, row: &JobEvidenceRow) -> Result<bool, DbError> {
    let inserted = tx.execute(
        "INSERT INTO job_evidence(repo_id, job_id, evidence_id, seq, attempt, kind, target,
                                  created, fresh, accepted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(repo_id, job_id, evidence_id) DO NOTHING",
        params![
            row.repo_id,
            row.job_id,
            row.evidence_id,
            row.seq,
            row.attempt,
            row.kind,
            row.target,
            row.created,
            row.fresh,
            row.accepted_at
        ],
    )?;
    Ok(inserted == 1)
}

/// The evidence `job` accepted, oldest accepted first.
pub fn job_evidence_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    job: &str,
) -> Result<Vec<JobEvidenceRow>, DbError> {
    let mut stmt = tx.prepare_cached(
        "SELECT repo_id, job_id, evidence_id, attempt, kind, target, created, fresh, accepted_at,
                seq
         FROM job_evidence WHERE repo_id = ?1 AND job_id = ?2
         ORDER BY seq, evidence_id",
    )?;
    Ok(stmt
        .query_map(params![repo, job], evidence_from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn evidence_from_row(row: &Row<'_>) -> rusqlite::Result<JobEvidenceRow> {
    Ok(JobEvidenceRow {
        repo_id: row.get(0)?,
        job_id: text_at(row, 1)?.unwrap_or_default(),
        evidence_id: text_at(row, 2)?.unwrap_or_default(),
        attempt: small(integer_at(row, 3)?),
        kind: text_at(row, 4)?.unwrap_or_default(),
        target: text_at(row, 5)?,
        created: integer_at(row, 6)? != 0,
        fresh: integer_at(row, 7)? != 0,
        accepted_at: Timestamp(integer_at(row, 8)?),
        seq: integer_at(row, 9)?,
    })
}

impl Db {
    /// Runs `f` in an immediate transaction under the lock, committing if
    /// it returns `Ok` and rolling back otherwise, for callers whose errors
    /// are their own, then `then` with the committed value while the lock
    /// is still held. `f` and `then` must not call another `Db` method.
    /// What follows a commit (the executor queue's
    /// presence publishes) happens in commit order, so a late publish can
    /// never overtake a later transition's.
    pub(crate) fn transact_then<T, E: From<DbError>>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
        then: impl FnOnce(&T),
    ) -> Result<T, E> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(DbError::from)?;
        let value = f(&tx)?;
        tx.commit().map_err(DbError::from)?;
        then(&value);
        Ok(value)
    }

    /// The job with the full id `id`.
    pub fn job(&self, id: &str) -> Result<Option<JobRow>, DbError> {
        self.in_transaction(|tx| job_in(tx, id))
    }

    /// The job whose id starts with `job_` and `prefix` (lowercase hex), in
    /// `repo` when given.
    pub fn job_by_prefix(&self, repo: Option<RepoId>, prefix: &str) -> Result<JobRow, DbError> {
        let pattern = format!("job\\_{}%", prefix.replace('%', ""));
        let found: Vec<JobRow> = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM jobs WHERE id LIKE ?1 ESCAPE '\\'
                   AND (?2 IS NULL OR repo_id = ?2)
                 ORDER BY id LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map(params![pattern, repo], from_row)?.collect()
        })?;
        unique_match(WHAT, &format!("job_{prefix}"), found, |job| job.id.clone())
    }

    /// Every open job, for the executor module's start.
    pub fn open_jobs(&self) -> Result<Vec<JobRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM jobs WHERE state IN ('queued', 'running')
                 ORDER BY priority DESC, submitted_at, id"
            ))?;
            stmt.query_map([], from_row)?.collect()
        })
    }

    /// Jobs `filter` matches, after `cursor`, at most `limit`: newest
    /// submitted first, or, with a [`JobCursor::Queue`] cursor or
    /// `queue_order`, open jobs in claim order.
    pub fn list_jobs(
        &self,
        filter: &JobFilter,
        queue_order: bool,
        cursor: Option<&JobCursor>,
        limit: usize,
    ) -> Result<Vec<JobRow>, DbError> {
        let mut sql = format!("SELECT {COLUMNS} FROM jobs WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        let text = |value: &str| SqlValue::Text(value.to_owned());
        if let Some(repo) = filter.repo {
            sql.push_str(" AND repo_id = ?");
            values.push(text(&repo.as_hex()));
        }
        if queue_order {
            sql.push_str(" AND state IN ('queued', 'running')");
        }
        if !filter.states.is_empty() {
            sql.push_str(" AND state IN (");
            sql.push_str(&vec!["?"; filter.states.len()].join(", "));
            sql.push(')');
            values.extend(filter.states.iter().map(|state| text(state.as_str())));
        }
        for (column, wanted) in [
            ("tree_id", &filter.tree_id),
            ("change_id", &filter.change_id),
            ("executor_name", &filter.executor),
        ] {
            if let Some(wanted) = wanted {
                sql.push_str(" AND ");
                sql.push_str(column);
                sql.push_str(" = ?");
                values.push(text(wanted));
            }
        }
        if let Some(since) = filter.since {
            sql.push_str(" AND submitted_at >= ?");
            values.push(SqlValue::Integer(since.0));
        }
        match cursor {
            Some(JobCursor::Newest(at, id)) => {
                sql.push_str(" AND (submitted_at < ? OR (submitted_at = ? AND id > ?))");
                values.extend([SqlValue::Integer(at.0), SqlValue::Integer(at.0), text(id)]);
            }
            Some(JobCursor::Queue(priority, at, id)) => {
                sql.push_str(
                    " AND (priority < ? OR (priority = ? AND (submitted_at > ?
                           OR (submitted_at = ? AND id > ?))))",
                );
                values.extend([
                    SqlValue::Integer(i64::from(*priority)),
                    SqlValue::Integer(i64::from(*priority)),
                    SqlValue::Integer(at.0),
                    SqlValue::Integer(at.0),
                    text(id),
                ]);
            }
            None => {}
        }
        if queue_order {
            sql.push_str(" ORDER BY priority DESC, submitted_at, id LIMIT ?");
        } else {
            sql.push_str(" ORDER BY submitted_at DESC, id LIMIT ?");
        }
        values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), from_row)?
                .collect()
        })
    }

    /// The evidence `job` of `repo` accepted, oldest accepted first.
    pub fn job_evidence(&self, repo: RepoId, job: &str) -> Result<Vec<JobEvidenceRow>, DbError> {
        self.in_transaction(|tx| job_evidence_in(tx, repo, job))
    }

    /// How many evidence objects each of `jobs` of `repo` accepted.
    pub fn job_evidence_counts(
        &self,
        jobs: &[(RepoId, String)],
    ) -> Result<std::collections::HashMap<String, u32>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT count(*) FROM job_evidence WHERE repo_id = ?1 AND job_id = ?2",
            )?;
            let mut counts = std::collections::HashMap::new();
            for (repo, id) in jobs {
                let count: i64 = stmt.query_row(params![repo, id], |row| row.get(0))?;
                counts.insert(id.clone(), u32::try_from(count).unwrap_or(u32::MAX));
            }
            Ok(counts)
        })
    }

    /// How many finished jobs of `repo` finished before `before`: what GC's
    /// ninth step would delete.
    pub fn finished_job_count(&self, repo: RepoId, before: Timestamp) -> Result<u64, DbError> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM jobs
                 WHERE repo_id = ?1 AND finished_at IS NOT NULL AND finished_at < ?2
                   AND state NOT IN ('queued', 'running')",
                params![repo, before],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
    }

    /// Deletes the jobs of `repo` that finished before `before`, with their
    /// `job_evidence` rows, 1,000 per transaction. Returns how many went.
    pub fn delete_finished_jobs(&self, repo: RepoId, before: Timestamp) -> Result<u64, DbError> {
        let mut deleted = 0u64;
        loop {
            let batch = self.in_transaction(|tx| {
                let ids: Vec<String> = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT id FROM jobs
                         WHERE repo_id = ?1 AND finished_at IS NOT NULL AND finished_at < ?2
                           AND state NOT IN ('queued', 'running')
                         LIMIT {SWEEP_BATCH}"
                    ))?;
                    stmt.query_map(params![repo, before], |row| row.get(0))?
                        .collect::<rusqlite::Result<_>>()?
                };
                let mut evidence = tx.prepare_cached(
                    "DELETE FROM job_evidence WHERE repo_id = ?1 AND job_id = ?2",
                )?;
                let mut job =
                    tx.prepare_cached("DELETE FROM jobs WHERE repo_id = ?1 AND id = ?2")?;
                for id in &ids {
                    evidence.execute(params![repo, id])?;
                    job.execute(params![repo, id])?;
                }
                Ok(ids.len())
            })?;
            deleted += u64::try_from(batch).unwrap_or(u64::MAX);
            if batch < SWEEP_BATCH {
                return Ok(deleted);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A queued job of `repo` with id `id`, submitted at `at`.
    pub(crate) fn job(repo: RepoId, id: &str, at: i64) -> JobRow {
        JobRow {
            repo_id: repo,
            id: id.to_owned(),
            protocol: 1,
            tree_id: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned(),
            commit_id: None,
            change_id: None,
            kinds: vec!["test".to_owned()],
            targets: vec![],
            requirements: vec![],
            spec_digest: "d".repeat(64),
            wall_time_ms: 1_800_000,
            cpu_time_ms: None,
            priority: 0,
            state: JobState::Queued,
            outcome: None,
            end_reason: None,
            message: None,
            attempt: 0,
            max_attempts: 3,
            submitted_at: Timestamp(at),
            queue_expires_at: Timestamp(at + 3_600_000),
            requested_by_agent: Some("tester".to_owned()),
            requested_by_token: None,
            claim_digest: None,
            claimed_at: None,
            lease_ms: None,
            lease_expires_at: None,
            deadline_at: None,
            executor: None,
            progress: None,
            usage_wall_ms: None,
            usage_cpu_ms: None,
            elapsed_ms: 0,
            finished_at: None,
            cancelled_by_agent: None,
            cancel_reason: None,
        }
    }

    fn repo(db: &Db) -> RepoId {
        let id: RepoId = "00000000000000aa".parse().unwrap();
        db.with(|conn| {
            conn.execute(
                "INSERT INTO repositories(id, root, jj_repo_path, colocated, registered_at)
                 VALUES (?1, '/r', '/r/.jj/repo', 0, 0)",
                [id],
            )
        })
        .unwrap();
        id
    }

    #[test]
    fn insert_update_read_and_list() {
        let db = Db::open_in_memory().unwrap();
        let repo = repo(&db);
        let mut first = job(repo, "job_00000000000000a1", 10);
        first.targets = vec!["a".into(), "b".into()];
        let second = JobRow {
            priority: 5,
            ..job(repo, "job_00000000000000b2", 20)
        };
        db.in_transaction(|tx| {
            insert_job_in(tx, &first)?;
            insert_job_in(tx, &second)
        })
        .unwrap();
        assert_eq!(db.job(&first.id).unwrap().unwrap(), first);
        first.state = JobState::Running;
        first.attempt = 1;
        first.claim_digest = Some(vec![1, 2, 3]);
        first.executor = Some(ExecutorColumns {
            name: "runner".into(),
            version: "1".into(),
            labels: vec!["os=linux".into()],
            kinds: vec!["test".into()],
            protocol: 1,
            agent: None,
            token: Some("tok".into()),
            identity_id: Some("idn".into()),
        });
        db.in_transaction(|tx| update_job_in(tx, &first)).unwrap();
        assert_eq!(db.job(&first.id).unwrap().unwrap(), first);
        assert_eq!(
            db.job_by_prefix(None, "00000000000000a").unwrap().id,
            first.id
        );
        assert!(matches!(
            db.job_by_prefix(None, "0000"),
            Err(DbError::Ambiguous { .. })
        ));
        let newest = db
            .list_jobs(&JobFilter::default(), false, None, 10)
            .unwrap();
        assert_eq!(newest[0].id, second.id);
        let queue = db.list_jobs(&JobFilter::default(), true, None, 10).unwrap();
        assert_eq!(queue[0].id, second.id, "priority first");
        let after = db
            .list_jobs(
                &JobFilter::default(),
                true,
                Some(&JobCursor::Queue(5, Timestamp(20), second.id.clone())),
                10,
            )
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, first.id);
    }

    #[test]
    fn finished_jobs_are_deleted_with_their_evidence_rows() {
        let db = Db::open_in_memory().unwrap();
        let repo = repo(&db);
        let mut done = job(repo, "job_00000000000000c3", 10);
        done.state = JobState::Completed;
        done.finished_at = Some(Timestamp(100));
        let open = job(repo, "job_00000000000000d4", 10);
        db.in_transaction(|tx| {
            insert_job_in(tx, &done)?;
            insert_job_in(tx, &open)?;
            insert_job_evidence_in(
                tx,
                &JobEvidenceRow {
                    repo_id: repo,
                    job_id: done.id.clone(),
                    evidence_id: "e".repeat(64),
                    seq: 1,
                    attempt: 1,
                    kind: "test".into(),
                    target: None,
                    created: true,
                    fresh: true,
                    accepted_at: Timestamp(50),
                },
            )
        })
        .unwrap();
        assert_eq!(db.finished_job_count(repo, Timestamp(100)).unwrap(), 0);
        assert_eq!(db.finished_job_count(repo, Timestamp(101)).unwrap(), 1);
        assert_eq!(db.delete_finished_jobs(repo, Timestamp(101)).unwrap(), 1);
        assert!(db.job(&done.id).unwrap().is_none());
        assert!(db.job(&open.id).unwrap().is_some());
        assert_eq!(db.job_evidence(repo, &done.id).unwrap(), Vec::new());
    }
}
