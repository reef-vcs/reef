//! Queries on `artifacts` and `evidence_artifacts` (R3.02, ADR 0056): the
//! index of the artifact store, whose bytes are files
//! ([`crate::artifacts`]). A row with `collected_at` set is a tombstone:
//! retention deleted the bytes while evidence still names them.
//! `evidence_artifacts` lists the digests each evidence object's payload
//! names; it is written with the evidence ([`super::insert_evidence_in`])
//! and has no foreign key to `artifacts`, since evidence may name bytes
//! that were never uploaded.

use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, unique_match};
use crate::model::{RepoId, Timestamp};

/// The length of an artifact digest: 64 hex digits, a SHA-256.
pub const ARTIFACT_DIGEST_LEN: usize = 64;

/// How many evidence ids [`Db::artifact_evidence`] lists at most.
pub const ARTIFACT_EVIDENCE_IDS: usize = 20;

/// Digests per `IN` list, well under SQLite's variable limit.
const IN_CHUNK: usize = 500;

/// The record kind errors name.
const WHAT: &str = "artifact";

/// One row of `artifacts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRow {
    /// The repository.
    pub repo_id: RepoId,
    /// SHA-256 of the bytes, lowercase hex.
    pub sha256: String,
    /// Their length.
    pub size: u64,
    /// When the bytes were last stored.
    pub stored_at: Timestamp,
    /// When they were last stored, uploaded again, or named by evidence
    /// added.
    pub used_at: Timestamp,
    /// When retention deleted the bytes; `None` while stored.
    pub collected_at: Option<Timestamp>,
    /// The `reef-agent` of the upload that last stored them.
    pub added_by_agent: Option<String>,
    /// That upload's remote token (R2.08).
    pub remote_token_id: Option<String>,
}

impl ArtifactRow {
    /// Whether the bytes are stored: not a tombstone.
    pub fn is_stored(&self) -> bool {
        self.collected_at.is_none()
    }
}

/// What the index says of a digest an evidence object names: its size, and
/// when it was collected (`None` while stored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactState {
    /// The artifact's size.
    pub size: u64,
    /// When retention deleted its bytes.
    pub collected_at: Option<Timestamp>,
}

/// One artifact as GC's step plans it (R3.02): its row, and which evidence
/// names it once the evidence the step deletes is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedArtifact {
    /// The row.
    pub row: ArtifactRow,
    /// Some evidence that stays names it.
    pub named: bool,
    /// Some pinned evidence that stays names it.
    pub pinned: bool,
}

const COLUMNS: &str = "repo_id, sha256, size, stored_at, used_at, collected_at, added_by_agent, \
     remote_token_id";

fn from_row(row: &Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        repo_id: row.get(0)?,
        sha256: row.get(1)?,
        size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        stored_at: row.get(3)?,
        used_at: row.get(4)?,
        collected_at: row.get(5)?,
        added_by_agent: row.get(6)?,
        remote_token_id: row.get(7)?,
    })
}

fn size_value(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

/// Records in `tx` that the evidence `evidence_id` of `repo` names
/// `digests`, replacing what was recorded for it, and marks each digest
/// with a row used at `now`.
pub fn index_artifacts_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    evidence_id: &str,
    digests: &[String],
    now: Timestamp,
) -> Result<(), DbError> {
    tx.execute(
        "DELETE FROM evidence_artifacts WHERE repo_id = ?1 AND evidence_id = ?2",
        params![repo, evidence_id],
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO evidence_artifacts(repo_id, evidence_id, sha256) VALUES (?1, ?2, ?3)
         ON CONFLICT DO NOTHING",
    )?;
    let mut touch = tx.prepare_cached(
        "UPDATE artifacts SET used_at = max(used_at, ?3) WHERE repo_id = ?1 AND sha256 = ?2",
    )?;
    for digest in digests {
        insert.execute(params![repo, evidence_id, digest])?;
        touch.execute(params![repo, digest, now])?;
    }
    Ok(())
}

impl Db {
    /// The row of `repo`'s artifact `sha256`, stored or a tombstone.
    pub fn artifact(&self, repo: RepoId, sha256: &str) -> Result<Option<ArtifactRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM artifacts WHERE repo_id = ?1 AND sha256 = ?2"),
                params![repo, sha256],
                from_row,
            )
            .optional()
        })
    }

    /// The artifact row of `repo` whose digest starts with `prefix` (4 to
    /// 64 lowercase hex digits): [`DbError::NotFound`] when none does,
    /// [`DbError::Ambiguous`] when several do. A range of the primary key.
    pub fn artifact_by_prefix(&self, repo: RepoId, prefix: &str) -> Result<ArtifactRow, DbError> {
        let pattern = format!(
            "{}*",
            super::normalize_prefix_up_to(WHAT, prefix, ARTIFACT_DIGEST_LEN)?
        );
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM artifacts WHERE repo_id = ?1 AND sha256 GLOB ?2
                 ORDER BY sha256 LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map(params![repo, pattern], from_row)?.collect()
        })?;
        unique_match(WHAT, prefix, matches, |row| row.sha256.clone())
    }

    /// Marks `repo`'s stored artifact `sha256` used at `now`; whether it is
    /// stored.
    pub fn touch_artifact(
        &self,
        repo: RepoId,
        sha256: &str,
        now: Timestamp,
    ) -> Result<Option<ArtifactRow>, DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                "UPDATE artifacts SET used_at = max(used_at, ?3)
                 WHERE repo_id = ?1 AND sha256 = ?2 AND collected_at IS NULL",
                params![repo, sha256, now],
            )?;
            Ok(tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM artifacts
                         WHERE repo_id = ?1 AND sha256 = ?2 AND collected_at IS NULL"
                    ),
                    params![repo, sha256],
                    from_row,
                )
                .optional()?)
        })
    }

    /// Records `row`'s bytes as stored: a new row, or a tombstone or an
    /// older row replaced (its size, times and adder). Returns the row.
    /// [`DbError::NotFound`] for a repository that is not registered (one
    /// forgotten while the upload ran): no row outlives its repository.
    pub fn store_artifact(&self, row: &ArtifactRow) -> Result<ArtifactRow, DbError> {
        self.in_transaction(|tx| {
            let changed = tx.execute(
                "INSERT INTO artifacts(repo_id, sha256, size, stored_at, used_at, collected_at,
                                       added_by_agent, remote_token_id)
                 SELECT ?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7
                 WHERE EXISTS (SELECT 1 FROM repositories WHERE id = ?1)
                 ON CONFLICT(repo_id, sha256) DO UPDATE SET
                     size = excluded.size, stored_at = excluded.stored_at,
                     used_at = max(artifacts.used_at, excluded.used_at), collected_at = NULL,
                     added_by_agent = excluded.added_by_agent,
                     remote_token_id = excluded.remote_token_id",
                params![
                    row.repo_id,
                    row.sha256,
                    size_value(row.size),
                    row.stored_at,
                    row.used_at,
                    row.added_by_agent,
                    row.remote_token_id,
                ],
            )?;
            if changed == 0 {
                return Err(DbError::NotFound {
                    what: "repository",
                    id: row.repo_id.as_hex(),
                });
            }
            Ok(tx.query_row(
                &format!("SELECT {COLUMNS} FROM artifacts WHERE repo_id = ?1 AND sha256 = ?2"),
                params![row.repo_id, row.sha256],
                from_row,
            )?)
        })
    }

    /// Makes `repo`'s stored artifact `sha256` a tombstone collected at
    /// `now`, because its bytes were found damaged or missing and dropped:
    /// readers then see it collected, and an upload of the genuine bytes
    /// stores them again. With `stored_at`, only while the row was stored
    /// then (an upload that repaired it meanwhile stored it later). Whether
    /// the row changed.
    pub fn tombstone_artifact(
        &self,
        repo: RepoId,
        sha256: &str,
        stored_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<bool, DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE artifacts SET collected_at = ?4
                 WHERE repo_id = ?1 AND sha256 = ?2 AND collected_at IS NULL
                   AND (?3 IS NULL OR stored_at = ?3)",
                params![repo, sha256, stored_at, now],
            )
            .map(|changed| changed > 0)
        })
    }

    /// The index's state of each of `digests` that has a row in `repo`;
    /// digests without one are absent.
    pub fn artifact_states(
        &self,
        repo: RepoId,
        digests: &[&str],
    ) -> Result<HashMap<String, ArtifactState>, DbError> {
        let mut states = HashMap::new();
        for chunk in digests.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT sha256, size, collected_at FROM artifacts
                 WHERE repo_id = ? AND sha256 IN ({})",
                vec!["?"; chunk.len()].join(", ")
            );
            let mut values = vec![SqlValue::Text(repo.as_hex())];
            values.extend(chunk.iter().map(|d| SqlValue::Text((*d).to_owned())));
            self.with(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(values), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        ArtifactState {
                            size: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                            collected_at: row.get(2)?,
                        },
                    ))
                })?;
                for row in rows {
                    let (sha256, state) = row?;
                    states.insert(sha256, state);
                }
                Ok(())
            })?;
        }
        Ok(states)
    }

    /// How many evidence objects of `repo` name `sha256`, and the ids of the
    /// newest [`ARTIFACT_EVIDENCE_IDS`], newest added first.
    pub fn artifact_evidence(
        &self,
        repo: RepoId,
        sha256: &str,
    ) -> Result<(u32, Vec<String>), DbError> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM evidence_artifacts WHERE repo_id = ?1 AND sha256 = ?2",
                params![repo, sha256],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare_cached(
                "SELECT e.id FROM evidence_artifacts a
                 JOIN evidence e ON e.repo_id = a.repo_id AND e.id = a.evidence_id
                 WHERE a.repo_id = ?1 AND a.sha256 = ?2
                 ORDER BY e.added_at DESC, e.id LIMIT ?3",
            )?;
            let ids = stmt
                .query_map(
                    params![
                        repo,
                        sha256,
                        i64::try_from(ARTIFACT_EVIDENCE_IDS).unwrap_or(20)
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((u32::try_from(count).unwrap_or(u32::MAX), ids))
        })
    }

    /// The digests the evidence `evidence_id` of `repo` names, sorted.
    pub fn artifacts_of_evidence(
        &self,
        repo: RepoId,
        evidence_id: &str,
    ) -> Result<Vec<String>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT sha256 FROM evidence_artifacts WHERE repo_id = ?1 AND evidence_id = ?2
                 ORDER BY sha256",
            )?;
            stmt.query_map(params![repo, evidence_id], |row| row.get(0))?
                .collect()
        })
    }

    /// Every artifact row of `repo` (stored or tombstone), with whether
    /// evidence still names it and whether pinned evidence does, for GC's
    /// step: evidence in `temp.gc_doomed` counts as gone. Needs the step's
    /// temporary tables ([`Db::create_live_tables`]).
    pub fn plan_artifacts(&self, repo: RepoId) -> Result<Vec<PlannedArtifact>, DbError> {
        // Live work pins artifacts; a landing pins only its evidence, so a
        // landed check's log keeps its own retention (R3.13).
        let pinned = super::evidence::LIVE_PINNED;
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS},
                   EXISTS (SELECT 1 FROM evidence_artifacts n
                           WHERE n.repo_id = a.repo_id AND n.sha256 = a.sha256
                             AND n.evidence_id NOT IN (SELECT id FROM temp.gc_doomed)),
                   EXISTS (SELECT 1 FROM evidence_artifacts n
                           JOIN evidence e ON e.repo_id = n.repo_id AND e.id = n.evidence_id
                           WHERE n.repo_id = a.repo_id AND n.sha256 = a.sha256
                             AND e.id NOT IN (SELECT id FROM temp.gc_doomed)
                             AND {pinned})
                 FROM artifacts a WHERE a.repo_id = ?1"
            ))?;
            stmt.query_map([repo], |row| {
                Ok(PlannedArtifact {
                    row: from_row(row)?,
                    named: row.get(8)?,
                    pinned: row.get(9)?,
                })
            })?
            .collect()
        })
    }

    /// Applies GC's decisions on `repo`'s artifacts in one transaction:
    /// each of `delete` (digest, the `used_at` planned with) loses its row,
    /// each of `collect` becomes a tombstone collected at `now`, and each of
    /// `forget` (a tombstone) loses its row, each only while its row still
    /// has the planned `used_at` and, for a deletion, no evidence names it.
    /// Returns the digests whose files may now go: those deleted or
    /// collected.
    pub fn apply_artifact_plan(
        &self,
        repo: RepoId,
        delete: &[(String, Timestamp)],
        collect: &[(String, Timestamp)],
        forget: &[String],
        now: Timestamp,
    ) -> Result<Vec<String>, DbError> {
        self.in_transaction(|tx| {
            let mut gone = Vec::new();
            let unnamed = "NOT EXISTS (SELECT 1 FROM evidence_artifacts n
                                        WHERE n.repo_id = artifacts.repo_id
                                          AND n.sha256 = artifacts.sha256)";
            {
                let mut stmt = tx.prepare_cached(&format!(
                    "DELETE FROM artifacts WHERE repo_id = ?1 AND sha256 = ?2 AND used_at = ?3
                       AND {unnamed}"
                ))?;
                for (sha256, used_at) in delete {
                    if stmt.execute(params![repo, sha256, used_at])? > 0 {
                        gone.push(sha256.clone());
                    }
                }
            }
            {
                let mut stmt = tx.prepare_cached(
                    "UPDATE artifacts SET collected_at = ?4
                     WHERE repo_id = ?1 AND sha256 = ?2 AND used_at = ?3 AND collected_at IS NULL",
                )?;
                for (sha256, used_at) in collect {
                    if stmt.execute(params![repo, sha256, used_at, now])? > 0 {
                        gone.push(sha256.clone());
                    }
                }
            }
            let mut stmt = tx.prepare_cached(&format!(
                "DELETE FROM artifacts WHERE repo_id = ?1 AND sha256 = ?2
                   AND collected_at IS NOT NULL AND {unnamed}"
            ))?;
            for sha256 in forget {
                stmt.execute(params![repo, sha256])?;
            }
            Ok(gone)
        })
    }

    /// The digests of `repo`'s stored rows, for the tidy's orphan check.
    pub fn stored_artifact_digests(
        &self,
        repo: RepoId,
    ) -> Result<std::collections::HashSet<String>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sha256 FROM artifacts WHERE repo_id = ?1 AND collected_at IS NULL",
            )?;
            stmt.query_map([repo], |row| row.get(0))?.collect()
        })
    }

    /// How many rows `artifacts` and `evidence_artifacts` have, in all
    /// repositories.
    pub fn artifact_counts(&self) -> Result<(u64, u64), DbError> {
        self.with(|conn| {
            let count = |table: &str| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|n| u64::try_from(n).unwrap_or_default())
            };
            Ok((count("artifacts")?, count("evidence_artifacts")?))
        })
    }

    /// Sets a column of `repo`'s artifact rows' times, for tests that age
    /// them: `used_at` (and `stored_at`) to `at`.
    #[doc(hidden)]
    pub fn age_artifact(&self, repo: RepoId, sha256: &str, at: Timestamp) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE artifacts SET used_at = ?3, stored_at = min(stored_at, ?3)
                 WHERE repo_id = ?1 AND sha256 = ?2",
                params![repo, sha256, at],
            )
            .map(drop)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::evidence::tests::{key, row as evidence_row};
    use crate::db::repositories::tests::repo;

    fn artifact(repo_id: RepoId, sha256: &str, at: i64) -> ArtifactRow {
        ArtifactRow {
            repo_id,
            sha256: sha256.to_owned(),
            size: 10,
            stored_at: Timestamp(at),
            used_at: Timestamp(at),
            collected_at: None,
            added_by_agent: Some("agent".into()),
            remote_token_id: None,
        }
    }

    #[test]
    fn store_touch_and_tombstones() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let sha = "ab".repeat(32);
        let stored = db.store_artifact(&artifact(record.id, &sha, 100)).unwrap();
        assert!(stored.is_stored());
        assert_eq!(
            db.touch_artifact(record.id, &sha, Timestamp(200))
                .unwrap()
                .unwrap()
                .used_at,
            Timestamp(200)
        );
        // A later store never moves `used_at` back.
        let again = db.store_artifact(&artifact(record.id, &sha, 150)).unwrap();
        assert_eq!(again.used_at, Timestamp(200));
        let gone = db
            .apply_artifact_plan(
                record.id,
                &[],
                &[(sha.clone(), Timestamp(200))],
                &[],
                Timestamp(300),
            )
            .unwrap();
        assert_eq!(gone, std::slice::from_ref(&sha));
        let tombstone = db.artifact(record.id, &sha).unwrap().unwrap();
        assert_eq!(tombstone.collected_at, Some(Timestamp(300)));
        assert!(
            db.touch_artifact(record.id, &sha, Timestamp(400))
                .unwrap()
                .is_none()
        );
        // Damaged bytes dropped: a tombstone, only for the row as stored.
        let row = db.store_artifact(&artifact(record.id, &sha, 320)).unwrap();
        assert!(
            !db.tombstone_artifact(record.id, &sha, Some(Timestamp(1)), Timestamp(330))
                .unwrap(),
            "stored again since"
        );
        assert!(
            db.tombstone_artifact(record.id, &sha, Some(row.stored_at), Timestamp(330))
                .unwrap()
        );
        assert!(!db.artifact(record.id, &sha).unwrap().unwrap().is_stored());
        // No row for a repository that is gone.
        let error = db
            .store_artifact(&artifact("bbbb000000000002".parse().unwrap(), &sha, 1))
            .unwrap_err();
        assert!(
            matches!(
                error,
                DbError::NotFound {
                    what: "repository",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(db.artifact_counts().unwrap().0, 1);
        // Stored again, the tombstone is a row again.
        let restored = db.store_artifact(&artifact(record.id, &sha, 500)).unwrap();
        assert!(restored.is_stored());
        assert_eq!(restored.stored_at, Timestamp(500));
        assert_eq!(
            db.artifact_by_prefix(record.id, "abab").unwrap().sha256,
            sha
        );
    }

    #[test]
    fn evidence_indexes_digests_and_marks_them_used() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let (stored, absent) = ("aa".repeat(32), "bb".repeat(32));
        db.store_artifact(&artifact(record.id, &stored, 100))
            .unwrap();
        let row = evidence_row(record.id, "e1", 1000);
        db.in_transaction(|tx| {
            super::super::insert_evidence_in(tx, &key("k1"), &row, None)?;
            index_artifacts_in(
                tx,
                record.id,
                "e1",
                &[stored.clone(), absent.clone()],
                Timestamp(1000),
            )
        })
        .unwrap();
        assert_eq!(db.artifact_counts().unwrap(), (1, 2));
        assert_eq!(
            db.artifact(record.id, &stored).unwrap().unwrap().used_at,
            Timestamp(1000)
        );
        let states = db
            .artifact_states(record.id, &[stored.as_str(), absent.as_str()])
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[&stored].size, 10);
        assert_eq!(
            db.artifact_evidence(record.id, &absent).unwrap(),
            (1, vec!["e1".to_owned()])
        );
        // A named artifact is never deleted outright; a tombstone named by
        // evidence stays.
        let gone = db
            .apply_artifact_plan(
                record.id,
                &[(stored.clone(), Timestamp(1000))],
                &[],
                &[],
                Timestamp(2000),
            )
            .unwrap();
        assert_eq!(gone.len(), 0);
        // A plan made with an older `used_at` does nothing.
        let gone = db
            .apply_artifact_plan(
                record.id,
                &[],
                &[(stored.clone(), Timestamp(100))],
                &[],
                Timestamp(2000),
            )
            .unwrap();
        assert_eq!(gone.len(), 0);
    }
}
