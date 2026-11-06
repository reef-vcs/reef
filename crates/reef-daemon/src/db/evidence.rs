//! Queries on `evidence`, `evidence_keys` and `evidence_links` (R3.01, ADR
//! 0055). An evidence row holds the canonical envelope; every other column
//! is an index over its verified payload, written once at insert. Keys are
//! machine-wide; evidence and links are per repository, and only
//! [`Db::delete_repository`] deletes them.

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, unique_match};
use crate::model::{RepoId, Timestamp};

/// The length of an evidence id: 64 hex digits, a SHA-256.
pub const EVIDENCE_ID_LEN: usize = 64;

/// The record kind errors name.
const WHAT: &str = "evidence";

/// One row of `evidence_keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceKeyRow {
    /// SHA-256 of `public_key`, lowercase hex.
    pub id: String,
    /// `ed25519`.
    pub algorithm: String,
    /// The DER `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
    /// When it first signed evidence on this machine.
    pub first_seen_at: Timestamp,
}

/// One row of `evidence`, as inserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRow {
    /// The repository it was added in.
    pub repo_id: RepoId,
    /// SHA-256 of `envelope`, lowercase hex.
    pub id: String,
    /// The canonical envelope.
    pub envelope: Vec<u8>,
    /// Which reader the envelope needs: `reef_evidence::FORMAT`.
    pub format: u32,
    /// The subject's tree.
    pub tree_id: String,
    /// The predicate's `kind`.
    pub kind: String,
    /// `pass`, `fail` or `error`.
    pub result: String,
    /// The predicate's `target`.
    pub target: Option<String>,
    /// The signer's key id.
    pub key_id: String,
    /// The claim's `startedOn`, truncated to the millisecond.
    pub started_at: Timestamp,
    /// The claim's `finishedOn`.
    pub finished_at: Timestamp,
    /// When it was first added in the repository.
    pub added_at: Timestamp,
    /// The adding request's `reef-agent`.
    pub added_by_agent: Option<String>,
    /// The remote token of the adding request (R2.08).
    pub remote_token_id: Option<String>,
}

/// An evidence row with its key's public key, as reads return it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvidence {
    /// The row. Its columns are read leniently: a value of the wrong type,
    /// written behind the daemon, reads as zero or empty rather than
    /// failing the read, since reads take an object's fields from its
    /// verified payload and only compare the columns with it.
    pub row: EvidenceRow,
    /// The DER `SubjectPublicKeyInfo` of `row.key_id`; `None` when no key
    /// row has that id, which reads as an invalid signature.
    pub public_key: Option<Vec<u8>>,
}

/// One row of `evidence_links`: evidence added for a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceLinkRow {
    /// The change, full hex.
    pub change_id: String,
    /// The evidence.
    pub evidence_id: String,
    /// The commit the change's revision resolved to when it was linked.
    pub commit_id: String,
    /// When.
    pub linked_at: Timestamp,
}

/// What an insert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inserted {
    /// The evidence row is new; false when the object was already stored.
    pub created: bool,
}

/// Which evidence of a repository a listing returns: the fields AND, the
/// values within one OR, and an empty list matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvidenceFilter {
    /// Only evidence about this tree.
    pub tree_id: Option<String>,
    /// Only evidence linked to this change.
    pub change_id: Option<String>,
    /// Only these kinds.
    pub kinds: Vec<String>,
    /// Only these results (`pass`, `fail`, `error`).
    pub results: Vec<String>,
    /// Only this target.
    pub target: Option<String>,
    /// Only evidence signed by these keys.
    pub key_ids: Vec<String>,
}

const COLUMNS: &str = "e.repo_id, e.id, e.envelope, e.format, e.tree_id, e.kind, e.result, \
     e.target, e.key_id, e.started_at, e.finished_at, e.added_at, e.added_by_agent, \
     e.remote_token_id, k.public_key";

/// The evidence rows and their keys: a row whose key is missing still reads.
const FROM: &str = "evidence e LEFT JOIN evidence_keys k ON k.id = e.key_id";

/// Column `idx` as text, whatever was stored: a number as its digits, a
/// blob as lossy UTF-8, NULL as `None`.
fn text_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get::<_, SqlValue>(idx)? {
        SqlValue::Null => None,
        SqlValue::Integer(n) => Some(n.to_string()),
        SqlValue::Real(x) => Some(x.to_string()),
        SqlValue::Text(text) => Some(text),
        SqlValue::Blob(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
    })
}

/// Column `idx` as an integer; 0 when it holds something else.
fn integer_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<i64> {
    Ok(match row.get::<_, SqlValue>(idx)? {
        SqlValue::Integer(n) => n,
        _ => 0,
    })
}

/// Column `idx` as bytes; text as its UTF-8, anything else empty.
fn bytes_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Vec<u8>>> {
    Ok(match row.get::<_, SqlValue>(idx)? {
        SqlValue::Blob(bytes) => Some(bytes),
        SqlValue::Text(text) => Some(text.into_bytes()),
        SqlValue::Null => None,
        _ => Some(Vec::new()),
    })
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<StoredEvidence> {
    Ok(StoredEvidence {
        row: EvidenceRow {
            repo_id: row.get(0)?,
            id: text_at(row, 1)?.unwrap_or_default(),
            envelope: bytes_at(row, 2)?.unwrap_or_default(),
            format: u32::try_from(integer_at(row, 3)?).unwrap_or(0),
            tree_id: text_at(row, 4)?.unwrap_or_default(),
            kind: text_at(row, 5)?.unwrap_or_default(),
            result: text_at(row, 6)?.unwrap_or_default(),
            target: text_at(row, 7)?,
            key_id: text_at(row, 8)?.unwrap_or_default(),
            started_at: Timestamp(integer_at(row, 9)?),
            finished_at: Timestamp(integer_at(row, 10)?),
            added_at: Timestamp(integer_at(row, 11)?),
            added_by_agent: text_at(row, 12)?,
            remote_token_id: text_at(row, 13)?,
        },
        public_key: bytes_at(row, 14)?,
    })
}

/// Stores `row`, signed by `key` and linked by `link`, in `tx`: the key,
/// the evidence and the link each inserted unless already there.
///
/// `key` and `row` come from a verified envelope, so they are what the
/// stored rows must hold. An evidence row already stored keeps its first
/// `added_at`, adder and token, but its envelope and index columns are
/// rewritten from `row` where they differ: re-adding the genuine envelope
/// repairs a row changed behind the daemon. A key row is repaired the same
/// way; its id is the SHA-256 of its public key, so there is one right
/// value. Either way `created` is false.
pub fn insert_evidence_in(
    tx: &Transaction<'_>,
    key: &EvidenceKeyRow,
    row: &EvidenceRow,
    link: Option<&EvidenceLinkRow>,
) -> Result<Inserted, DbError> {
    let key_existed = tx
        .query_row(
            "SELECT 1 FROM evidence_keys WHERE id = ?1",
            [&key.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let changed_key = tx.execute(
        "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET algorithm = excluded.algorithm,
                                       public_key = excluded.public_key
         WHERE evidence_keys.algorithm IS NOT excluded.algorithm
            OR evidence_keys.public_key IS NOT excluded.public_key",
        params![key.id, key.algorithm, key.public_key, key.first_seen_at],
    )?;
    let repaired_key = key_existed && changed_key > 0;
    if repaired_key {
        tracing::warn!(key_id = %key.id, "a stored evidence key differed from its id; repaired");
    }
    let exists = tx
        .query_row(
            "SELECT 1 FROM evidence WHERE repo_id = ?1 AND id = ?2",
            params![row.repo_id, row.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        let repaired = tx.execute(
            "UPDATE evidence SET envelope = ?3, format = ?4, tree_id = ?5, kind = ?6,
                                 result = ?7, target = ?8, key_id = ?9, started_at = ?10,
                                 finished_at = ?11
             WHERE repo_id = ?1 AND id = ?2
               AND (envelope IS NOT ?3 OR format IS NOT ?4 OR tree_id IS NOT ?5
                    OR kind IS NOT ?6 OR result IS NOT ?7 OR target IS NOT ?8
                    OR key_id IS NOT ?9 OR started_at IS NOT ?10 OR finished_at IS NOT ?11)",
            params![
                row.repo_id,
                row.id,
                row.envelope,
                row.format,
                row.tree_id,
                row.kind,
                row.result,
                row.target,
                row.key_id,
                row.started_at,
                row.finished_at,
            ],
        )?;
        if repaired > 0 {
            tracing::warn!(
                evidence_id = %row.id,
                "a stored evidence row differed from the verified envelope; repaired"
            );
        }
    } else {
        tx.execute(
            "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, target,
                                  key_id, started_at, finished_at, added_at, added_by_agent,
                                  remote_token_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                row.repo_id,
                row.id,
                row.envelope,
                row.format,
                row.tree_id,
                row.kind,
                row.result,
                row.target,
                row.key_id,
                row.started_at,
                row.finished_at,
                row.added_at,
                row.added_by_agent,
                row.remote_token_id,
            ],
        )?;
    }
    if let Some(link) = link {
        tx.execute(
            "INSERT INTO evidence_links(repo_id, change_id, evidence_id, commit_id, linked_at)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
            params![
                row.repo_id,
                link.change_id,
                link.evidence_id,
                link.commit_id,
                link.linked_at
            ],
        )?;
    }
    Ok(Inserted { created: !exists })
}

impl Db {
    /// [`insert_evidence_in`] in its own transaction.
    pub fn insert_evidence(
        &self,
        key: &EvidenceKeyRow,
        row: &EvidenceRow,
        link: Option<&EvidenceLinkRow>,
    ) -> Result<Inserted, DbError> {
        self.in_transaction(|tx| insert_evidence_in(tx, key, row, link))
    }

    /// The evidence of `repo` whose id starts with `prefix` (4 to 64 hex
    /// digits, either case). [`DbError::InvalidPrefix`] for another
    /// prefix, [`DbError::NotFound`] when none matches,
    /// [`DbError::Ambiguous`] when several do. The lookup is a range of the
    /// primary key, as the other tables' are.
    pub fn evidence_by_prefix(
        &self,
        repo: RepoId,
        prefix: &str,
    ) -> Result<StoredEvidence, DbError> {
        let pattern = format!(
            "{}*",
            super::normalize_prefix_up_to(WHAT, prefix, EVIDENCE_ID_LEN)?
        );
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM {FROM}
                 WHERE e.repo_id = ?1 AND e.id GLOB ?2
                 ORDER BY e.id LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map(params![repo, pattern], from_row)?.collect()
        })?;
        unique_match(WHAT, prefix, matches, |found| found.row.id.clone())
    }

    /// The evidence of `repo` with the full id `id`, if stored.
    pub fn evidence_by_id(
        &self,
        repo: RepoId,
        id: &str,
    ) -> Result<Option<StoredEvidence>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM {FROM} WHERE e.repo_id = ?1 AND e.id = ?2"),
                params![repo, id],
                from_row,
            )
            .optional()
        })
    }

    /// At most `limit` of `repo`'s evidence matching `filter`, newest added
    /// first (then by id), after `cursor` (the `(added_at, id)` of the last
    /// one of the previous page).
    pub fn list_evidence(
        &self,
        repo: RepoId,
        filter: &EvidenceFilter,
        cursor: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<StoredEvidence>, DbError> {
        let mut sql = format!("SELECT {COLUMNS} FROM {FROM} WHERE e.repo_id = ?");
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(repo.as_hex())];
        let text = |value: &str| SqlValue::Text(value.to_owned());
        if let Some(tree) = &filter.tree_id {
            sql.push_str(" AND e.tree_id = ?");
            values.push(text(tree));
        }
        if let Some(change) = &filter.change_id {
            sql.push_str(
                " AND e.id IN (SELECT evidence_id FROM evidence_links
                               WHERE repo_id = e.repo_id AND change_id = ?)",
            );
            values.push(text(change));
        }
        if let Some(target) = &filter.target {
            sql.push_str(" AND e.target = ?");
            values.push(text(target));
        }
        for (column, wanted) in [
            ("e.kind", &filter.kinds),
            ("e.result", &filter.results),
            ("e.key_id", &filter.key_ids),
        ] {
            if wanted.is_empty() {
                continue;
            }
            sql.push_str(" AND ");
            sql.push_str(column);
            sql.push_str(" IN (");
            sql.push_str(&vec!["?"; wanted.len()].join(", "));
            sql.push(')');
            values.extend(wanted.iter().map(|value| text(value)));
        }
        if let Some((at, id)) = cursor {
            sql.push_str(" AND (e.added_at < ? OR (e.added_at = ? AND e.id > ?))");
            values.extend([SqlValue::Integer(at.0), SqlValue::Integer(at.0), text(id)]);
        }
        sql.push_str(" ORDER BY e.added_at DESC, e.id LIMIT ?");
        values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), from_row)?
                .collect()
        })
    }

    /// The changes each of `ids` was added for, oldest link first, in
    /// `repo`; ids without a link are absent.
    pub fn links_of(
        &self,
        repo: RepoId,
        ids: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<String>>, DbError> {
        let mut links: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(links);
        }
        self.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT evidence_id, change_id FROM evidence_links
                 WHERE repo_id = ?1 AND evidence_id = ?2 ORDER BY linked_at, change_id",
            )?;
            for id in ids {
                let rows = stmt.query_map(params![repo, id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for found in rows {
                    let (evidence, change) = found?;
                    links.entry(evidence).or_default().push(change);
                }
            }
            Ok(())
        })?;
        Ok(links)
    }

    /// The key with this id, if it has signed evidence here.
    pub fn evidence_key(&self, id: &str) -> Result<Option<EvidenceKeyRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT id, algorithm, public_key, first_seen_at FROM evidence_keys WHERE id = ?1",
                [id],
                |row| {
                    Ok(EvidenceKeyRow {
                        id: row.get(0)?,
                        algorithm: row.get(1)?,
                        public_key: row.get(2)?,
                        first_seen_at: row.get(3)?,
                    })
                },
            )
            .optional()
        })
    }

    /// How many rows `evidence`, `evidence_links` and `evidence_keys` have,
    /// in all repositories.
    pub fn evidence_counts(&self) -> Result<(u64, u64, u64), DbError> {
        self.with(|conn| {
            let count = |table: &str| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|n| u64::try_from(n).unwrap_or_default())
            };
            Ok((
                count("evidence")?,
                count("evidence_links")?,
                count("evidence_keys")?,
            ))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::repositories::tests::repo;

    pub(crate) fn key(id: &str) -> EvidenceKeyRow {
        EvidenceKeyRow {
            id: id.to_owned(),
            algorithm: "ed25519".into(),
            public_key: vec![1, 2, 3],
            first_seen_at: Timestamp(1),
        }
    }

    pub(crate) fn row(repo_id: RepoId, id: &str, added_at: i64) -> EvidenceRow {
        EvidenceRow {
            repo_id,
            id: id.to_owned(),
            envelope: b"{}".to_vec(),
            format: 1,
            tree_id: "t1".into(),
            kind: "test".into(),
            result: "pass".into(),
            target: None,
            key_id: "k1".into(),
            started_at: Timestamp(10),
            finished_at: Timestamp(20),
            added_at: Timestamp(added_at),
            added_by_agent: Some("agent".into()),
            remote_token_id: None,
        }
    }

    #[test]
    fn insert_is_idempotent_and_links_once() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let link = EvidenceLinkRow {
            change_id: "c1".into(),
            evidence_id: "e1".into(),
            commit_id: "d1".into(),
            linked_at: Timestamp(5),
        };
        let first = db
            .insert_evidence(&key("k1"), &row(record.id, "e1", 100), Some(&link))
            .unwrap();
        assert!(first.created);
        let again = db
            .insert_evidence(&key("k1"), &row(record.id, "e1", 200), Some(&link))
            .unwrap();
        assert!(!again.created);
        assert_eq!(db.evidence_counts().unwrap(), (1, 1, 1));
        let found = db.evidence_by_id(record.id, "e1").unwrap().unwrap();
        assert_eq!(found.row.added_at, Timestamp(100), "the first add is kept");
        assert_eq!(found.public_key, Some(vec![1, 2, 3]));
        assert_eq!(
            db.links_of(record.id, &["e1", "e2"]).unwrap(),
            [("e1".to_owned(), vec!["c1".to_owned()])].into()
        );
    }

    // R3.01 review F6: re-adding a verified row over a changed one repairs
    // it, and a row whose key is missing or whose times are not integers
    // still reads.
    #[test]
    fn changed_rows_read_and_are_repaired() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let genuine = row(record.id, "e1", 100);
        db.insert_evidence(&key("k1"), &genuine, None).unwrap();
        db.with(|conn| {
            conn.execute(
                "UPDATE evidence SET result = 'fail', kind = 'build', started_at = 'soon'",
                [],
            )?;
            conn.execute("UPDATE evidence_keys SET public_key = x'00'", [])
        })
        .unwrap();
        let changed = db.evidence_by_id(record.id, "e1").unwrap().unwrap();
        assert_eq!(changed.row.kind, "build");
        assert_eq!(changed.row.started_at, Timestamp(0), "read leniently");
        let again = db.insert_evidence(&key("k1"), &genuine, None).unwrap();
        assert!(!again.created);
        let repaired = db.evidence_by_id(record.id, "e1").unwrap().unwrap();
        assert_eq!(repaired.row, genuine);
        assert_eq!(repaired.public_key, Some(vec![1, 2, 3]));
        // A key id that names no key still lists, with no key.
        db.with(|conn| {
            conn.execute_batch(
                "PRAGMA foreign_keys = OFF; UPDATE evidence SET key_id = 'gone'; PRAGMA foreign_keys = ON;",
            )
        })
        .unwrap();
        let listed = db
            .list_evidence(record.id, &EvidenceFilter::default(), None, 10)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].public_key, None);
    }

    // R3.01 review F5: a prefix lookup searches the primary key; it does
    // not scan the repository's rows.
    #[test]
    fn prefix_lookups_use_the_key() {
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .with(|conn| {
                let mut stmt = conn.prepare(&format!(
                    "EXPLAIN QUERY PLAN SELECT {COLUMNS} FROM {FROM}
                     WHERE e.repo_id = ?1 AND e.id GLOB ?2"
                ))?;
                stmt.query_map(params!["r", "abcd*"], |row| row.get::<_, String>(3))?
                    .collect()
            })
            .unwrap();
        let first = &plan[0];
        assert!(
            first.contains("SEARCH e USING PRIMARY KEY (repo_id=? AND id>? AND id<?)"),
            "{plan:?}"
        );
    }

    #[test]
    fn prefixes_find_one_or_say_which() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        for id in ["abcd01", "abcd02", "ffff01"] {
            db.insert_evidence(&key("k1"), &row(record.id, id, 1), None)
                .unwrap();
        }
        assert_eq!(
            db.evidence_by_prefix(record.id, "ffff").unwrap().row.id,
            "ffff01"
        );
        let error = db.evidence_by_prefix(record.id, "abcd").unwrap_err();
        assert!(
            matches!(&error, DbError::Ambiguous { candidates, .. } if candidates.len() == 2),
            "{error:?}"
        );
        let other = "aaaa000000000002".parse().unwrap();
        assert!(matches!(
            db.evidence_by_prefix(other, "ffff"),
            Err(DbError::NotFound { .. })
        ));
    }

    #[test]
    fn listings_filter_and_page_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        for n in 0..10_i64 {
            let mut evidence = row(record.id, &format!("e{n:02}"), 100 + n / 2);
            evidence.kind = if n % 2 == 0 { "test" } else { "lint" }.into();
            evidence.result = if n < 5 { "pass" } else { "fail" }.into();
            db.insert_evidence(&key("k1"), &evidence, None).unwrap();
        }
        let all = EvidenceFilter::default();
        let mut seen = Vec::new();
        let mut cursor: Option<(Timestamp, String)> = None;
        loop {
            let page = db
                .list_evidence(
                    record.id,
                    &all,
                    cursor.as_ref().map(|(at, id)| (*at, id.as_str())),
                    3,
                )
                .unwrap();
            let Some(last) = page.last() else { break };
            cursor = Some((last.row.added_at, last.row.id.clone()));
            seen.extend(page.into_iter().map(|found| found.row.id));
        }
        assert_eq!(
            seen,
            [
                "e08", "e09", "e06", "e07", "e04", "e05", "e02", "e03", "e00", "e01"
            ]
        );
        let tests_failed = EvidenceFilter {
            kinds: vec!["test".into()],
            results: vec!["fail".into()],
            ..EvidenceFilter::default()
        };
        let ids: Vec<String> = db
            .list_evidence(record.id, &tests_failed, None, 100)
            .unwrap()
            .into_iter()
            .map(|found| found.row.id)
            .collect();
        assert_eq!(ids, ["e08", "e06"]);
    }
}
