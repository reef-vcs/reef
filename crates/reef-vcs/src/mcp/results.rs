//! The server's own result shapes: lists wrapped in an object (structured
//! content is an object), with the page token beside them, and the reads
//! whose CLI form is not one message. Every value inside is
//! `reef_client::json`'s.

use reef_client::json::{
    Change, Diff, FileEntry, FileRead, LogEntry, Operation, Repository, Workspace,
};
use serde::Serialize;

/// `list_repositories`.
#[derive(Debug, Serialize)]
pub(super) struct Repositories {
    pub(super) repositories: Vec<Repository>,
    pub(super) next_page_token: Option<String>,
}

/// `list_workspaces`.
#[derive(Debug, Serialize)]
pub(super) struct Workspaces {
    pub(super) workspaces: Vec<Workspace>,
    pub(super) next_page_token: Option<String>,
}

/// `list_changes`.
#[derive(Debug, Serialize)]
pub(super) struct Changes {
    pub(super) entries: Vec<LogEntry>,
    pub(super) at_operation: String,
    pub(super) next_page_token: Option<String>,
}

/// `list_operations`.
#[derive(Debug, Serialize)]
pub(super) struct Operations {
    pub(super) operations: Vec<Operation>,
    pub(super) next_page_token: Option<String>,
}

/// `list_files`.
#[derive(Debug, Serialize)]
pub(super) struct Files {
    pub(super) entries: Vec<FileEntry>,
    pub(super) commit_id: String,
    pub(super) at_operation: String,
    pub(super) next_page_token: Option<String>,
}

/// `get_change`: the change without its files (`get_diff` has them).
#[derive(Debug, Serialize)]
pub(super) struct ChangeAt {
    pub(super) change: Option<Change>,
    pub(super) at_operation: String,
}

/// `get_diff`: one page of files.
#[derive(Debug, Serialize)]
pub(super) struct DiffPage {
    #[serde(flatten)]
    pub(super) diff: Diff,
    pub(super) next_page_token: Option<String>,
}

/// `read_file`: one range of the file, and where the next one starts.
#[derive(Debug, Serialize)]
pub(super) struct FileRange {
    #[serde(flatten)]
    pub(super) file: FileRead,
    pub(super) offset: u64,
    /// `null` at the end of the file.
    pub(super) next_offset: Option<u64>,
}

/// `wait_for_operations`.
#[derive(Debug, Serialize)]
pub(super) struct Waited {
    /// New matching operations, oldest first; empty on a timeout.
    pub(super) operations: Vec<Operation>,
    /// What to pass back as `after_operation` to wait for what comes next.
    pub(super) last_operation_id: Option<String>,
    pub(super) timed_out: bool,
}

/// A page token as the results print it: `null` on the last page.
pub(super) fn token(token: String) -> Option<String> {
    (!token.is_empty()).then_some(token)
}
