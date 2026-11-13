//! Queries on `workspace_disk`: each active workspace's disk figure, kept
//! by the daemon (#272, ADR 0076). A row is written after each walk and
//! deleted when the workspace ends (`finish_workspace`); a workspace never
//! walked has none. #274's eviction columns are never written here.

use std::collections::HashMap;

use rusqlite::{OptionalExtension as _, Row, params};

use super::{Db, DbError};
use crate::disk::{DiskFigure, DiskPathFigure};
use crate::model::{RepoId, Timestamp, WorkspaceId};

const COLUMNS: &str = "workspace_id, tracked_bytes, build_bytes, unshared_bytes, files, largest, \
     missing_dependencies, incomplete, measured_at";

/// A count stored as SQLite's signed integer; anything past `i64::MAX`
/// saturates (no disk is that large).
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<(WorkspaceId, DiskFigure)> {
    let largest: String = row.get(5)?;
    let missing: String = row.get(6)?;
    Ok((
        row.get(0)?,
        DiskFigure {
            tracked_bytes: unsigned(row.get(1)?),
            build_bytes: unsigned(row.get(2)?),
            unshared_bytes: row.get::<_, Option<i64>>(3)?.map(unsigned),
            files: unsigned(row.get(4)?),
            // Written by this module only; a row it cannot read back reads
            // as no directories rather than failing the list.
            largest: serde_json::from_str::<Vec<DiskPathFigure>>(&largest).unwrap_or_default(),
            missing_dependencies: serde_json::from_str(&missing).unwrap_or_default(),
            incomplete: row.get(7)?,
            measured_at: Timestamp(row.get(8)?),
        },
    ))
}

impl Db {
    /// Writes `workspace`'s figure, replacing the one it had and keeping
    /// #274's columns.
    pub fn put_workspace_disk(
        &self,
        repo: RepoId,
        workspace: WorkspaceId,
        figure: &DiskFigure,
    ) -> Result<(), DbError> {
        let largest = serde_json::to_string(&figure.largest).unwrap_or_else(|_| "[]".to_owned());
        let missing =
            serde_json::to_string(&figure.missing_dependencies).unwrap_or_else(|_| "[]".to_owned());
        self.with(|conn| {
            conn.execute(
                "INSERT INTO workspace_disk(repo_id, workspace_id, tracked_bytes, build_bytes,
                     unshared_bytes, files, largest, missing_dependencies, incomplete, measured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(repo_id, workspace_id) DO UPDATE SET
                     tracked_bytes = excluded.tracked_bytes, build_bytes = excluded.build_bytes,
                     unshared_bytes = excluded.unshared_bytes, files = excluded.files,
                     largest = excluded.largest,
                     missing_dependencies = excluded.missing_dependencies,
                     incomplete = excluded.incomplete, measured_at = excluded.measured_at",
                params![
                    repo,
                    workspace,
                    signed(figure.tracked_bytes),
                    signed(figure.build_bytes),
                    figure.unshared_bytes.map(signed),
                    signed(figure.files),
                    largest,
                    missing,
                    figure.incomplete,
                    figure.measured_at.0,
                ],
            )
            .map(drop)
        })
    }

    /// `workspace`'s figure, if it has one.
    pub fn workspace_disk(&self, workspace: WorkspaceId) -> Result<Option<DiskFigure>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM workspace_disk WHERE workspace_id = ?1"),
                [workspace],
                from_row,
            )
            .optional()
            .map(|row| row.map(|(_, figure)| figure))
        })
    }

    /// Every figure, of `repo`'s workspaces or of all.
    pub fn workspace_disks(
        &self,
        repo: Option<RepoId>,
    ) -> Result<HashMap<WorkspaceId, DiskFigure>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM workspace_disk WHERE ?1 IS NULL OR repo_id = ?1"
            ))?;
            statement
                .query_map([repo], from_row)?
                .collect::<rusqlite::Result<HashMap<_, _>>>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn figure(tracked: u64) -> DiskFigure {
        DiskFigure {
            tracked_bytes: tracked,
            build_bytes: 7,
            unshared_bytes: Some(3),
            files: 4,
            largest: vec![DiskPathFigure {
                path: "target".to_owned(),
                bytes: 7,
            }],
            missing_dependencies: vec!["web".to_owned()],
            incomplete: true,
            measured_at: Timestamp(1_000),
        }
    }

    #[test]
    fn put_read_replace() {
        let db = Db::open_in_memory().unwrap();
        let (repo, ws) = (RepoId::generate(), WorkspaceId::generate());
        assert_eq!(db.workspace_disk(ws).unwrap(), None);
        db.put_workspace_disk(repo, ws, &figure(1)).unwrap();
        assert_eq!(db.workspace_disk(ws).unwrap(), Some(figure(1)));
        db.put_workspace_disk(repo, ws, &figure(2)).unwrap();
        assert_eq!(db.workspace_disk(ws).unwrap(), Some(figure(2)));
        let other = WorkspaceId::generate();
        db.put_workspace_disk(RepoId::generate(), other, &DiskFigure::default())
            .unwrap();
        assert_eq!(db.workspace_disks(Some(repo)).unwrap().len(), 1);
        assert_eq!(db.workspace_disks(None).unwrap().len(), 2);
    }
}
