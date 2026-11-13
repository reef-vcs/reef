//! `destination_pushes` and the landings' push columns (R3.17, ADR 0077):
//! one row per landing destination whose policy pushes it to a git remote,
//! with its push state, its last failure and its retry schedule.

use std::collections::HashMap;

use rusqlite::{OptionalExtension as _, Row, params};

use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// One destination's push state, as `destination_pushes` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationPushRow {
    /// The repository.
    pub repo_id: RepoId,
    /// The destination bookmark.
    pub destination: String,
    /// The git remote its policy names.
    pub git_remote: String,
    /// The remote branch: the destination's own name.
    pub branch: String,
    /// `pushed`, `pending`, `retrying`, `diverged`, `refused` or `disabled`.
    pub state: String,
    /// The destination's head as last seen.
    pub local_commit: Option<String>,
    /// The remote branch as last seen; `None` when absent.
    pub remote_commit: Option<String>,
    /// Landings onto the destination the remote lacks.
    pub unpushed_count: u32,
    /// The last failure's reason constant.
    pub reason: Option<String>,
    /// Its message.
    pub message: Option<String>,
    /// Git's stderr, redacted and capped.
    pub git_stderr: Option<String>,
    /// What to do about it.
    pub hint: Option<String>,
    /// The command that does it, as JSON argv.
    pub next: Option<String>,
    /// The remote's head when diverged, as JSON.
    pub remote_head: Option<String>,
    /// Failed attempts since the last success.
    pub attempts: u32,
    /// When a `retrying` destination tries again.
    pub next_attempt_at: Option<Timestamp>,
    /// Since when it has failed.
    pub failing_since: Option<Timestamp>,
    /// The last successful push.
    pub pushed_at: Option<Timestamp>,
    /// Its operation.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

impl DestinationPushRow {
    /// A `pending` row of `destination` pushing to `remote`, as of `now`.
    pub fn new(repo: RepoId, destination: &str, remote: &str, now: Timestamp) -> Self {
        DestinationPushRow {
            repo_id: repo,
            destination: destination.to_owned(),
            git_remote: remote.to_owned(),
            branch: destination.to_owned(),
            state: "pending".to_owned(),
            local_commit: None,
            remote_commit: None,
            unpushed_count: 0,
            reason: None,
            message: None,
            git_stderr: None,
            hint: None,
            next: None,
            remote_head: None,
            attempts: 0,
            next_attempt_at: None,
            failing_since: None,
            pushed_at: None,
            operation_id: None,
            operation_collected: false,
            updated_at: now,
        }
    }
}

const COLUMNS: &str = "repo_id, destination, git_remote, branch, state, local_commit,
     remote_commit, unpushed_count, reason, message, git_stderr, hint, next, remote_head,
     attempts, next_attempt_at, failing_since, pushed_at, operation_id, operation_collected,
     updated_at";

fn count(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn push_from_row(row: &Row<'_>) -> rusqlite::Result<DestinationPushRow> {
    Ok(DestinationPushRow {
        repo_id: row.get(0)?,
        destination: row.get(1)?,
        git_remote: row.get(2)?,
        branch: row.get(3)?,
        state: row.get(4)?,
        local_commit: row.get(5)?,
        remote_commit: row.get(6)?,
        unpushed_count: count(row.get(7)?),
        reason: row.get(8)?,
        message: row.get(9)?,
        git_stderr: row.get(10)?,
        hint: row.get(11)?,
        next: row.get(12)?,
        remote_head: row.get(13)?,
        attempts: count(row.get(14)?),
        next_attempt_at: row.get(15)?,
        failing_since: row.get(16)?,
        pushed_at: row.get(17)?,
        operation_id: row.get(18)?,
        operation_collected: row.get::<_, i64>(19)? != 0,
        updated_at: row.get(20)?,
    })
}

impl Db {
    /// Writes `row`, replacing the destination's.
    pub fn put_destination_push(&self, row: &DestinationPushRow) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO destination_pushes({COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16, ?17, ?18, ?19, ?20, ?21)"
                ),
                params![
                    row.repo_id,
                    row.destination,
                    row.git_remote,
                    row.branch,
                    row.state,
                    row.local_commit,
                    row.remote_commit,
                    i64::from(row.unpushed_count),
                    row.reason,
                    row.message,
                    row.git_stderr,
                    row.hint,
                    row.next,
                    row.remote_head,
                    i64::from(row.attempts),
                    row.next_attempt_at,
                    row.failing_since,
                    row.pushed_at,
                    row.operation_id,
                    i64::from(row.operation_collected),
                    row.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    /// `destination`'s push state in `repo`.
    pub fn destination_push(
        &self,
        repo: RepoId,
        destination: &str,
    ) -> Result<Option<DestinationPushRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM destination_pushes
                     WHERE repo_id = ?1 AND destination = ?2"
                ),
                params![repo, destination],
                push_from_row,
            )
            .optional()
        })
    }

    /// `repo`'s pushing destinations' states, by destination; every
    /// repository's with `None`.
    pub fn destination_pushes(
        &self,
        repo: Option<RepoId>,
    ) -> Result<Vec<DestinationPushRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM destination_pushes
                 WHERE ?1 IS NULL OR repo_id = ?1
                 ORDER BY repo_id, destination"
            ))?;
            statement
                .query_map(params![repo], push_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// Forgets `destination`'s push state: its policy no longer pushes.
    pub fn delete_destination_push(&self, repo: RepoId, destination: &str) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "DELETE FROM destination_pushes WHERE repo_id = ?1 AND destination = ?2",
                params![repo, destination],
            )?;
            Ok(())
        })
    }

    /// How many destinations, of every repository, need a person: diverged,
    /// refused, or retrying since before `since`.
    pub fn pushes_needing_attention(&self, since: Timestamp) -> Result<u64, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM destination_pushes
                 WHERE state IN ('diverged', 'refused')
                    OR (state = 'retrying' AND failing_since IS NOT NULL AND failing_since < ?1)",
                params![since],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
        })
    }

    /// The landed landings onto `destination` no push has carried yet, as
    /// (landing id, the destination's head after it), oldest first.
    pub fn unpushed_landings(
        &self,
        repo: RepoId,
        destination: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, destination_after FROM landings
                 WHERE repo_id = ?1 AND destination = ?2 AND outcome = 'landed'
                   AND pushed_at IS NULL AND destination_after IS NOT NULL
                 ORDER BY finished_at, id",
            )?;
            statement
                .query_map(params![repo, destination], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()
        })
    }

    /// Whether `destination` has had a landing in `repo`.
    pub fn has_landings(&self, repo: RepoId, destination: &str) -> Result<bool, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM landings WHERE repo_id = ?1 AND destination = ?2)",
                params![repo, destination],
                |row| row.get::<_, i64>(0),
            )
            .map(|found| found != 0)
        })
    }

    /// Every (repository, destination) with a landed landing no push has
    /// carried: what the start sweep wakes.
    pub fn destinations_with_unpushed(&self) -> Result<Vec<(RepoId, String)>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT repo_id, destination FROM landings
                 WHERE outcome = 'landed' AND pushed_at IS NULL",
            )?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()
        })
    }

    /// Marks landings `ids` of `repo` as carried by the push `operation`
    /// at `at`, in chunks of 500.
    pub fn mark_landings_pushed(
        &self,
        repo: RepoId,
        ids: &[String],
        at: Timestamp,
        operation: Option<&str>,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            for chunk in ids.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(", ");
                let mut values: Vec<rusqlite::types::Value> = vec![
                    rusqlite::types::Value::Text(repo.as_hex()),
                    rusqlite::types::Value::Integer(at.0),
                    operation.map_or(rusqlite::types::Value::Null, |op| {
                        rusqlite::types::Value::Text(op.to_owned())
                    }),
                ];
                values.extend(
                    chunk
                        .iter()
                        .map(|id| rusqlite::types::Value::Text(id.clone())),
                );
                tx.execute(
                    &format!(
                        "UPDATE landings SET pushed_at = ?2, push_operation_id = ?3
                         WHERE repo_id = ?1 AND pushed_at IS NULL AND id IN ({marks})"
                    ),
                    rusqlite::params_from_iter(values),
                )?;
            }
            Ok(())
        })
    }

    /// GC's step for pushes: maps the destinations' and the landings' push
    /// operation ids through `rewritten`; a destination's operation not in
    /// it was dropped (`operation_collected`), a landing's keeps its id.
    pub fn map_push_operations(
        &self,
        repo: RepoId,
        rewritten: &HashMap<String, String>,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let rows: Vec<(String, String)> = {
                let mut statement = tx.prepare(
                    "SELECT destination, operation_id FROM destination_pushes
                     WHERE repo_id = ?1 AND operation_id IS NOT NULL AND operation_collected = 0",
                )?;
                statement
                    .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (destination, old) in rows {
                match rewritten.get(&old) {
                    Some(new) => tx.execute(
                        "UPDATE destination_pushes SET operation_id = ?3
                         WHERE repo_id = ?1 AND destination = ?2",
                        params![repo, destination, new],
                    )?,
                    None => tx.execute(
                        "UPDATE destination_pushes SET operation_collected = 1
                         WHERE repo_id = ?1 AND destination = ?2",
                        params![repo, destination],
                    )?,
                };
            }
            let landings: Vec<(String, String)> = {
                let mut statement = tx.prepare(
                    "SELECT id, push_operation_id FROM landings
                     WHERE repo_id = ?1 AND push_operation_id IS NOT NULL",
                )?;
                statement
                    .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (id, old) in landings {
                if let Some(new) = rewritten.get(&old) {
                    tx.execute(
                        "UPDATE landings SET push_operation_id = ?3 WHERE repo_id = ?1 AND id = ?2",
                        params![repo, id, new],
                    )?;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::tests::repo;

    #[test]
    fn rows_round_trip_and_count() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let mut row = DestinationPushRow::new(record.id, "main", "origin", Timestamp(2_000));
        "retrying".clone_into(&mut row.state);
        row.attempts = 2;
        row.failing_since = Some(Timestamp(1_000));
        row.operation_id = Some("op1".to_owned());
        db.put_destination_push(&row).unwrap();
        assert_eq!(
            db.destination_push(record.id, "main").unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            db.destination_pushes(Some(record.id)).unwrap(),
            [row.clone()]
        );
        assert_eq!(db.destination_pushes(None).unwrap().len(), 1);
        assert_eq!(db.pushes_needing_attention(Timestamp(500)).unwrap(), 0);
        assert_eq!(db.pushes_needing_attention(Timestamp(1_500)).unwrap(), 1);
        let rewritten = HashMap::from([("op1".to_owned(), "op2".to_owned())]);
        db.map_push_operations(record.id, &rewritten).unwrap();
        let mapped = db.destination_push(record.id, "main").unwrap().unwrap();
        assert_eq!(mapped.operation_id.as_deref(), Some("op2"));
        db.map_push_operations(record.id, &HashMap::new()).unwrap();
        assert!(
            db.destination_push(record.id, "main")
                .unwrap()
                .unwrap()
                .operation_collected
        );
        db.delete_destination_push(record.id, "main").unwrap();
        assert!(db.destination_push(record.id, "main").unwrap().is_none());
    }
}
