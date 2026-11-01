//! The brief form of [`json`]'s values (issue #114, ADR 0045): what `reef`
//! prints on a pipe unless asked for `-o json`, and what `reef mcp`'s
//! `list_workspaces`, `get_status` and `get_diff` return. Each view borrows
//! the full value, so the two cannot drift, and holds to three rules:
//!
//! 1. **Subset.** Every key is where the full document has it, with the
//!    same value. Keys are only left out, never renamed; values are never
//!    shortened.
//! 2. **Zero values are left out**: `null`, `false`, `0`, `""` and `[]`. A
//!    reader takes a missing key as its zero value. A view keeps some keys
//!    whatever their value, such as a count that answers a question, where
//!    0 must show; each type says which.
//! 3. **Dropped keys** are left out whatever their value; each type lists
//!    them.
//!
//! The full JSON stays the interface for programs; which keys a view keeps
//! may change in a minor release, the rules may not.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::Serialize;

use crate::json::{self, FileSymbolDiff, Hunk, SymbolDiffSummary};

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` passes a reference"
)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// A workspace: who holds it and how many files it changed. Always `id`
/// and `owner`; `changed_file_count` when counted, even 0; `state` when not
/// `active`; `divergent` and `attention` when set; `repo_id` only in a
/// list spanning several repositories ([`workspaces`]). Dropped: `purpose`,
/// `change_id`, `change_id_reverse_hex`, `commit_id`, `backend`, `path`,
/// `created_at`, `last_snapshot_at`, `last_snapshot_op`, `lease`,
/// `watcher`, `watch_state`, `local_snapshot_count`, `stores`; `ws inspect
/// <id>` has them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.Workspace")]
pub struct Workspace<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_id: Option<&'a str>,
    owner: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    divergent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_file_count: Option<u32>,
}

impl<'a> From<&'a json::Workspace> for Workspace<'a> {
    fn from(ws: &'a json::Workspace) -> Self {
        Self {
            id: &ws.id,
            repo_id: None,
            owner: &ws.owner,
            state: (!ws.state.is_empty() && ws.state != "active").then_some(ws.state.as_str()),
            divergent: ws.divergent,
            attention: ws.attention.as_deref(),
            changed_file_count: ws.changed_file_count,
        }
    }
}

/// `list`'s brief views, each with its `repo_id` when the list spans more
/// than one repository: for a list read whole, as `reef ws list` reads it.
pub fn workspaces(list: &[json::Workspace]) -> Vec<Workspace<'_>> {
    let repos: BTreeSet<&str> = list.iter().map(|ws| ws.repo_id.as_str()).collect();
    if repos.len() > 1 {
        workspaces_with_repo(list)
    } else {
        list.iter().map(Workspace::from).collect()
    }
}

/// `list`'s brief views, each with its `repo_id`: for a page of a list
/// that may span repositories, whose other pages this one cannot see.
pub fn workspaces_with_repo(list: &[json::Workspace]) -> Vec<Workspace<'_>> {
    list.iter()
        .map(|ws| Workspace {
            repo_id: Some(&ws.repo_id),
            ..Workspace::from(ws)
        })
        .collect()
}

/// A change. Always `change_id_reverse_hex`; `commit_id` only when
/// `divergent`, since then the change id names more than one commit; the
/// description and flags when set. Dropped: `change_id`,
/// `parent_commit_ids`, `parent_change_ids`, `author`, `committer`,
/// `tree_id`, `jj_workspaces`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.Change")]
#[expect(clippy::struct_excessive_bools, reason = "the full type's flags")]
#[expect(clippy::struct_field_names, reason = "the full type's keys")]
pub struct Change<'a> {
    change_id_reverse_hex: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_id: Option<&'a str>,
    #[serde(skip_serializing_if = "str::is_empty")]
    description: &'a str,
    #[serde(skip_serializing_if = "is_false")]
    empty: bool,
    #[serde(skip_serializing_if = "is_false")]
    conflict: bool,
    #[serde(skip_serializing_if = "is_false")]
    divergent: bool,
    #[serde(skip_serializing_if = "is_false")]
    immutable: bool,
    #[serde(skip_serializing_if = "is_false")]
    hidden: bool,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    bookmarks: &'a [String],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tags: &'a [String],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    workspace_ids: &'a [String],
}

impl<'a> From<&'a json::Change> for Change<'a> {
    fn from(change: &'a json::Change) -> Self {
        Self {
            change_id_reverse_hex: &change.change_id_reverse_hex,
            commit_id: change.divergent.then_some(change.commit_id.as_str()),
            description: &change.description,
            empty: change.empty,
            conflict: change.conflict,
            divergent: change.divergent,
            immutable: change.immutable,
            hidden: change.hidden,
            bookmarks: &change.bookmarks,
            tags: &change.tags,
            workspace_ids: &change.workspace_ids,
        }
    }
}

/// A changed path. Always `path` and `kind`; `old_path` when set; a side's
/// type only when it is not `file`: a missing type is `file` when that side
/// exists, and no side when it does not (`added` has no old side, `deleted`
/// no new side).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.FileChange")]
pub struct FileChange<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_type: Option<&'a str>,
}

/// A side's type as the brief keeps it: `None` for `file` and for no side.
fn entry_type(entry: Option<&str>) -> Option<&str> {
    entry.filter(|entry| *entry != "file")
}

impl<'a> From<&'a json::FileChange> for FileChange<'a> {
    fn from(change: &'a json::FileChange) -> Self {
        Self {
            path: &change.path,
            kind: change.kind.as_deref(),
            old_path: change.old_path.as_deref(),
            old_type: entry_type(change.old_type.as_deref()),
            new_type: entry_type(change.new_type.as_deref()),
        }
    }
}

/// One file of a diff: its [`FileChange`], and its flags, line counts,
/// hunks and symbols when set. Nothing is dropped: the view is lossless.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.FileDiff")]
#[expect(clippy::struct_excessive_bools, reason = "the full type's flags")]
pub struct FileDiff<'a> {
    #[serde(flatten)]
    change: FileChange<'a>,
    #[serde(skip_serializing_if = "is_false")]
    binary: bool,
    #[serde(skip_serializing_if = "is_false")]
    conflict: bool,
    #[serde(skip_serializing_if = "is_zero")]
    lines_added: u32,
    #[serde(skip_serializing_if = "is_zero")]
    lines_removed: u32,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    hunks: &'a [Hunk],
    #[serde(skip_serializing_if = "is_false")]
    truncated: bool,
    #[serde(skip_serializing_if = "is_false")]
    old_no_final_newline: bool,
    #[serde(skip_serializing_if = "is_false")]
    new_no_final_newline: bool,
    #[serde(skip_serializing_if = "is_false")]
    too_large: bool,
    #[serde(skip_serializing_if = "is_false")]
    invalid_utf8: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols: Option<&'a FileSymbolDiff>,
}

impl<'a> From<&'a json::FileDiff> for FileDiff<'a> {
    fn from(diff: &'a json::FileDiff) -> Self {
        Self {
            change: FileChange::from(&diff.change),
            binary: diff.binary,
            conflict: diff.conflict,
            lines_added: diff.lines_added,
            lines_removed: diff.lines_removed,
            hunks: &diff.hunks,
            truncated: diff.truncated,
            old_no_final_newline: diff.old_no_final_newline,
            new_no_final_newline: diff.new_no_final_newline,
            too_large: diff.too_large,
            invalid_utf8: diff.invalid_utf8,
            symbols: diff.symbols.as_ref(),
        }
    }
}

/// `reef diff`: always the two sides, `total_files` and `files`; the symbol
/// summary when present. Dropped: `at_operation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.Diff")]
pub struct Diff<'a> {
    from_commit_ids: &'a [String],
    to_commit_id: &'a str,
    total_files: u32,
    files: Vec<FileDiff<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_summary: Option<&'a SymbolDiffSummary>,
}

impl<'a> From<&'a json::Diff> for Diff<'a> {
    fn from(diff: &'a json::Diff) -> Self {
        Self {
            from_commit_ids: &diff.from_commit_ids,
            to_commit_id: &diff.to_commit_id,
            total_files: diff.total_files,
            files: diff.files.iter().map(FileDiff::from).collect(),
            symbol_summary: diff.symbol_summary.as_ref(),
        }
    }
}

/// `reef status`. Dropped: `repository` and `at_operation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.RepoStatus")]
pub struct RepoStatus<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_status: Option<RepositoryStatus<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_status: Option<WorkspaceStatus<'a>>,
}

impl<'a> From<&'a json::RepoStatus> for RepoStatus<'a> {
    fn from(status: &'a json::RepoStatus) -> Self {
        Self {
            repository_status: status
                .repository_status
                .as_ref()
                .map(RepositoryStatus::from),
            workspace_status: status.workspace_status.as_ref().map(WorkspaceStatus::from),
        }
    }
}

/// A repository's state. Always both counts; `trunk`, `trunk_bookmark` and
/// the conflicted and divergent change ids when set. Dropped:
/// `active_workspaces` (`reef ws list` has them, with counts), `heads`
/// (`reef log -r 'heads(mutable())'`), `last_operation` (`reef op log`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.RepositoryStatus")]
pub struct RepositoryStatus<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    trunk: Option<Change<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trunk_bookmark: Option<&'a str>,
    active_workspace_count: u32,
    head_count: u32,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    conflicted_change_ids: &'a [String],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    divergent_change_ids: &'a [String],
}

impl<'a> From<&'a json::RepositoryStatus> for RepositoryStatus<'a> {
    fn from(status: &'a json::RepositoryStatus) -> Self {
        Self {
            trunk: status.trunk.as_ref().map(Change::from),
            trunk_bookmark: status.trunk_bookmark.as_deref(),
            active_workspace_count: status.active_workspace_count,
            head_count: status.head_count,
            conflicted_change_ids: &status.conflicted_change_ids,
            divergent_change_ids: &status.divergent_change_ids,
        }
    }
}

/// A workspace's state. Always `changed_file_count`; the rest when set.
/// Dropped: `snapshot`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "brief.WorkspaceStatus")]
pub struct WorkspaceStatus<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<Workspace<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_copy: Option<Change<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<Change<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    changed_files: Vec<FileChange<'a>>,
    changed_file_count: u32,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    conflicted_paths: &'a [String],
    #[serde(skip_serializing_if = "is_zero")]
    conflicted_path_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
}

impl<'a> From<&'a json::WorkspaceStatus> for WorkspaceStatus<'a> {
    fn from(status: &'a json::WorkspaceStatus) -> Self {
        Self {
            workspace: status.workspace.as_ref().map(Workspace::from),
            working_copy: status.working_copy.as_ref().map(Change::from),
            parents: status.parents.iter().map(Change::from).collect(),
            changed_files: status.changed_files.iter().map(FileChange::from).collect(),
            changed_file_count: status.changed_file_count,
            conflicted_paths: &status.conflicted_paths,
            conflicted_path_count: status.conflicted_path_count,
            source: status.source.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::json::{Lease, Signature};

    fn full_workspace(id: &str, repo: &str) -> json::Workspace {
        json::Workspace {
            id: id.into(),
            repo_id: repo.into(),
            owner: "bot-0".into(),
            purpose: "fix the parser".into(),
            change_id: "0123".into(),
            change_id_reverse_hex: "zyxw".into(),
            commit_id: "abcd".into(),
            backend: "apfs".into(),
            path: "/ws/1".into(),
            state: "active".into(),
            divergent: false,
            attention: None,
            created_at: Some("2026-10-31T00:00:00.000Z".into()),
            last_snapshot_at: None,
            last_snapshot_op: Some("ff".repeat(64)),
            lease: Some(Lease {
                id: "00000000000000bb".into(),
                owner: "bot-0".into(),
                issued_at: None,
                expires_at: None,
                expires_at_ms: None,
                last_heartbeat_at: None,
                length_ms: 0,
                grace_ms: 0,
                state: "active".into(),
                end_reason: None,
                heartbeat_every_ms: None,
            }),
            watcher: "notify".into(),
            watch_state: Some("attaching".into()),
            local_snapshot_count: 2,
            stores: Vec::new(),
            changed_file_count: Some(3),
            remote_token_id: None,
        }
    }

    fn signature() -> Signature {
        Signature {
            name: "A".into(),
            email: "a@example.com".into(),
            at: Some("2026-10-31T00:00:00.000Z".into()),
        }
    }

    fn full_change(description: &str) -> json::Change {
        json::Change {
            change_id: "0123".into(),
            change_id_reverse_hex: "zyxw".into(),
            commit_id: "abcd".into(),
            parent_commit_ids: vec!["ef01".into()],
            parent_change_ids: vec!["4567".into()],
            description: description.into(),
            author: Some(signature()),
            committer: Some(signature()),
            tree_id: Some("9999".into()),
            empty: false,
            conflict: false,
            divergent: false,
            immutable: false,
            hidden: false,
            bookmarks: Vec::new(),
            tags: Vec::new(),
            workspace_ids: Vec::new(),
            jj_workspaces: vec!["reef-1".into()],
        }
    }

    fn file_change(
        path: &str,
        kind: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) -> json::FileChange {
        json::FileChange {
            path: path.into(),
            kind: Some(kind.into()),
            old_path: None,
            old_type: old.map(Into::into),
            new_type: new.map(Into::into),
        }
    }

    fn file_diff(change: json::FileChange, all_set: bool) -> json::FileDiff {
        json::FileDiff {
            change,
            binary: all_set,
            conflict: all_set,
            lines_added: u32::from(all_set) * 4,
            lines_removed: u32::from(all_set) * 2,
            hunks: if all_set {
                vec![Hunk {
                    old_start: 1,
                    old_lines: 2,
                    new_start: 1,
                    new_lines: 4,
                    lines: "-a\n+b\n".into(),
                }]
            } else {
                Vec::new()
            },
            truncated: all_set,
            old_no_final_newline: all_set,
            new_no_final_newline: all_set,
            too_large: all_set,
            invalid_utf8: all_set,
            symbols: None,
        }
    }

    fn full_diff() -> json::Diff {
        let mut renamed = file_change("b.rs", "renamed", Some("file"), Some("file"));
        renamed.old_path = Some("a.rs".into());
        json::Diff {
            from_commit_ids: vec!["ef01".into()],
            to_commit_id: "abcd".into(),
            at_operation: "77".repeat(64),
            total_files: 5,
            files: vec![
                file_diff(
                    file_change("m.rs", "modified", Some("file"), Some("file")),
                    false,
                ),
                file_diff(
                    file_change("x.bin", "modified", Some("file"), Some("file")),
                    true,
                ),
                file_diff(renamed, false),
                file_diff(
                    file_change("link", "modified", Some("file"), Some("symlink")),
                    false,
                ),
                file_diff(file_change("new.rs", "added", None, Some("file")), true),
            ],
            symbol_summary: None,
        }
    }

    fn full_status(workspace: bool) -> json::RepoStatus {
        json::RepoStatus {
            repository: None,
            at_operation: "77".repeat(64),
            repository_status: Some(json::RepositoryStatus {
                trunk: Some(json::Change {
                    immutable: true,
                    bookmarks: vec!["main".into()],
                    ..full_change("trunk")
                }),
                trunk_bookmark: Some("main".into()),
                active_workspaces: vec![full_workspace("00000000000000aa", "r1")],
                active_workspace_count: 1,
                heads: vec![full_change("a head")],
                head_count: 1,
                conflicted_change_ids: Vec::new(),
                divergent_change_ids: Vec::new(),
                last_operation: None,
            }),
            workspace_status: workspace.then(|| json::WorkspaceStatus {
                workspace: Some(json::Workspace {
                    changed_file_count: None,
                    ..full_workspace("00000000000000aa", "r1")
                }),
                working_copy: Some(full_change("")),
                parents: vec![full_change("the parent")],
                changed_files: vec![file_change("m.rs", "modified", Some("file"), Some("file"))],
                changed_file_count: 0,
                conflicted_paths: Vec::new(),
                conflicted_path_count: 0,
                source: Some("index".into()),
                walk_reason: None,
                snapshot: None,
            }),
        }
    }

    fn value<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap()
    }

    /// Every key path of `brief` is in `full`, with an equal value (rule 1).
    fn assert_subset(brief: &Value, full: &Value, at: &str) {
        match (brief, full) {
            (Value::Object(brief), Value::Object(full)) => {
                for (key, value) in brief {
                    let Some(full) = full.get(key) else {
                        panic!("{at}.{key} is not in the full document");
                    };
                    assert_subset(value, full, &format!("{at}.{key}"));
                }
            }
            (Value::Array(brief), Value::Array(full)) => {
                assert_eq!(brief.len(), full.len(), "{at}");
                for (index, (brief, full)) in brief.iter().zip(full).enumerate() {
                    assert_subset(brief, full, &format!("{at}[{index}]"));
                }
            }
            _ => assert_eq!(brief, full, "{at}"),
        }
    }

    /// Every key in `value`, at any depth.
    fn keys(value: &Value) -> BTreeSet<&str> {
        let mut found = BTreeSet::new();
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    found.insert(key.as_str());
                    found.extend(keys(value));
                }
            }
            Value::Array(values) => values.iter().for_each(|value| found.extend(keys(value))),
            _ => {}
        }
        found
    }

    #[test]
    fn brief_workspace_keeps_what_an_agent_acts_on() {
        let full = full_workspace("00000000000000aa", "r1");
        assert_eq!(
            serde_json::to_string(&Workspace::from(&full)).unwrap(),
            r#"{"id":"00000000000000aa","owner":"bot-0","changed_file_count":3}"#
        );
        let flagged = json::Workspace {
            state: "releasing".into(),
            divergent: true,
            attention: Some("divergent".into()),
            ..full.clone()
        };
        let brief = value(&Workspace::from(&flagged));
        assert_eq!(brief["state"], "releasing");
        assert_eq!(brief["divergent"], true);
        assert_eq!(brief["attention"], "divergent");
        let none = json::Workspace {
            changed_file_count: Some(0),
            ..full.clone()
        };
        assert_eq!(value(&Workspace::from(&none))["changed_file_count"], 0);
        let uncounted = json::Workspace {
            changed_file_count: None,
            ..full
        };
        assert!(
            value(&Workspace::from(&uncounted))
                .get("changed_file_count")
                .is_none()
        );
    }

    #[test]
    fn brief_lists_name_the_repository_only_across_several() {
        let one = [
            full_workspace("00000000000000aa", "r1"),
            full_workspace("00000000000000bb", "r1"),
        ];
        for ws in workspaces(&one) {
            assert!(value(&ws).get("repo_id").is_none());
        }
        let two = [
            full_workspace("00000000000000aa", "r1"),
            full_workspace("00000000000000bb", "r2"),
        ];
        let listed: Vec<Value> = workspaces(&two).iter().map(value).collect();
        assert_eq!(listed[0]["repo_id"], "r1");
        assert_eq!(listed[1]["repo_id"], "r2");
        // A page names its repositories whatever it holds.
        for ws in workspaces_with_repo(&one) {
            assert_eq!(value(&ws)["repo_id"], "r1");
        }
    }

    #[test]
    fn brief_change_keeps_ids_description_and_set_flags() {
        let plain = full_change("fix the parser");
        assert_eq!(
            value(&Change::from(&plain)),
            json!({"change_id_reverse_hex": "zyxw", "description": "fix the parser"})
        );
        let undescribed = full_change("");
        assert_eq!(
            value(&Change::from(&undescribed)),
            json!({"change_id_reverse_hex": "zyxw"})
        );
        let trunk = json::Change {
            immutable: true,
            bookmarks: vec!["main".into()],
            ..plain.clone()
        };
        let brief = value(&Change::from(&trunk));
        assert_eq!(brief["immutable"], true);
        assert_eq!(brief["bookmarks"], json!(["main"]));
        assert!(brief.get("commit_id").is_none());
        let divergent = json::Change {
            divergent: true,
            ..plain
        };
        assert_eq!(value(&Change::from(&divergent))["commit_id"], "abcd");
    }

    #[test]
    fn brief_file_types_mean_file_when_missing() {
        let modified = file_change("m.rs", "modified", Some("file"), Some("file"));
        assert_eq!(
            value(&FileChange::from(&modified)),
            json!({"path": "m.rs", "kind": "modified"})
        );
        let link = file_change("l", "modified", Some("file"), Some("symlink"));
        assert_eq!(
            value(&FileChange::from(&link)),
            json!({"path": "l", "kind": "modified", "new_type": "symlink"})
        );
        let added = file_change("a.rs", "added", None, Some("file"));
        assert_eq!(
            value(&FileChange::from(&added)),
            json!({"path": "a.rs", "kind": "added"})
        );
        let deleted = file_change("d.rs", "deleted", Some("file"), None);
        assert_eq!(
            value(&FileChange::from(&deleted)),
            json!({"path": "d.rs", "kind": "deleted"})
        );
        let mut renamed = file_change("b.rs", "renamed", Some("file"), Some("file"));
        renamed.old_path = Some("a.rs".into());
        assert_eq!(value(&FileChange::from(&renamed))["old_path"], "a.rs");
    }

    /// `brief`, a file of a brief diff, with its zero values and types put
    /// back.
    fn expand_file(brief: &Value) -> Value {
        let mut file = brief.as_object().unwrap().clone();
        let kind = file["kind"].as_str().unwrap().to_owned();
        file.entry("old_path").or_insert(Value::Null);
        let side = |exists: bool| if exists { json!("file") } else { Value::Null };
        file.entry("old_type").or_insert(side(kind != "added"));
        file.entry("new_type").or_insert(side(kind != "deleted"));
        for key in [
            "binary",
            "conflict",
            "truncated",
            "old_no_final_newline",
            "new_no_final_newline",
            "too_large",
            "invalid_utf8",
        ] {
            file.entry(key).or_insert(json!(false));
        }
        for key in ["lines_added", "lines_removed"] {
            file.entry(key).or_insert(json!(0));
        }
        file.entry("hunks").or_insert(json!([]));
        Value::Object(file)
    }

    /// A brief diff's JSON with every zero value put back, which equals the
    /// full diff's without `at_operation`.
    fn expand_diff(brief: &Value) -> Value {
        let mut diff = brief.as_object().unwrap().clone();
        let files = diff["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(expand_file)
            .collect();
        diff.insert("files".into(), Value::Array(files));
        Value::Object(diff)
    }

    #[test]
    fn brief_diff_is_lossless() {
        let full = full_diff();
        let mut expected = value(&full);
        expected.as_object_mut().unwrap().remove("at_operation");
        assert_eq!(expand_diff(&value(&Diff::from(&full))), expected);
        let brief = value(&Diff::from(&full));
        assert_eq!(
            brief["files"][0],
            json!({"path": "m.rs", "kind": "modified"})
        );
    }

    #[test]
    fn brief_status_drops_the_repository_listing() {
        let full = full_status(true);
        let brief = value(&RepoStatus::from(&full));
        let keys = keys(&brief);
        for key in [
            "repository",
            "at_operation",
            "active_workspaces",
            "heads",
            "last_operation",
            "snapshot",
        ] {
            assert!(!keys.contains(key), "{key} in {brief}");
        }
        let repo = &brief["repository_status"];
        assert_eq!(repo["active_workspace_count"], 1);
        assert_eq!(repo["head_count"], 1);
        assert_eq!(repo["trunk_bookmark"], "main");
        assert_eq!(repo["trunk"]["description"], "trunk");
        let ws = &brief["workspace_status"];
        assert_eq!(ws["source"], "index");
        assert_eq!(ws["changed_file_count"], 0);
        assert_eq!(ws["parents"][0]["description"], "the parent");
        assert_eq!(
            ws["workspace"],
            json!({"id": "00000000000000aa", "owner": "bot-0"})
        );
        let repository = full_status(false);
        assert!(
            value(&RepoStatus::from(&repository))
                .get("workspace_status")
                .is_none()
        );
    }

    #[test]
    fn brief_is_a_subset_of_full() {
        let workspace = full_workspace("00000000000000aa", "r1");
        let flagged = json::Workspace {
            state: "releasing".into(),
            divergent: true,
            attention: Some("divergent".into()),
            ..workspace.clone()
        };
        for ws in [&workspace, &flagged] {
            assert_subset(&value(&Workspace::from(ws)), &value(ws), "workspace");
        }
        let two = [
            full_workspace("00000000000000aa", "r1"),
            full_workspace("00000000000000bb", "r2"),
        ];
        assert_subset(&value(&workspaces(&two)), &value(&two), "list");
        let divergent = json::Change {
            divergent: true,
            bookmarks: vec!["main".into()],
            ..full_change("x")
        };
        assert_subset(
            &value(&Change::from(&divergent)),
            &value(&divergent),
            "change",
        );
        let diff = full_diff();
        assert_subset(&value(&Diff::from(&diff)), &value(&diff), "diff");
        for status in [full_status(true), full_status(false)] {
            assert_subset(
                &value(&RepoStatus::from(&status)),
                &value(&status),
                "status",
            );
        }
    }
}
