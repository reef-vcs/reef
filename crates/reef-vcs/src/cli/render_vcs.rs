//! Text forms of the VCS commands' output (R2.27): short, one record per
//! line where there is a list, no colors, no pager. Changes are shown by 12
//! reverse-hex characters, commits and operations by 12 hex. The lists are
//! not tables, so they print through wrappers that serialize as their one
//! field.

use std::io::{self, Write};

use reef_api::v1::DiffFormat;
use reef_client::json::{
    Change, ChangeSummary, Diff, FileChange, FileDiff, FileEntry, FileRead, LogEntry, Operation,
    RepoStatus,
};
use serde::Serialize;

use super::out::{TextRender, block};

/// Ids in text: changes by reverse hex, commits and operations by hex.
const ID_WIDTH: usize = 12;

fn short(id: &str) -> String {
    id.chars().take(ID_WIDTH).collect()
}

/// A description's first line, or `(no description)`.
fn first_line(description: &str) -> &str {
    match description.lines().next() {
        Some(line) if !line.trim().is_empty() => line,
        _ => "(no description)",
    }
}

/// `<change> <commit> <first line>`.
fn change_line(change: &Change) -> String {
    format!(
        "{} {} {}",
        short(&change.change_id_reverse_hex),
        short(&change.commit_id),
        first_line(&change.description)
    )
}

/// `M path`, `R old -> new`, and the like.
fn file_change_line(file: &FileChange) -> String {
    let letter = match file.kind.as_deref() {
        Some("added") => "A",
        Some("modified") => "M",
        Some("deleted") => "D",
        Some("renamed") => "R",
        Some("copied") => "C",
        _ => "?",
    };
    match &file.old_path {
        Some(old) => format!("{letter} {old} -> {}", file.path),
        None => format!("{letter} {}", file.path),
    }
}

/// `@` when `workspace` is on the change, else a space; the change, the
/// commit, bookmarks, markers, and the description's first line.
fn log_line(entry: &LogEntry, workspace: &str) -> String {
    let change = &entry.change;
    let at = if !workspace.is_empty() && change.workspace_ids.iter().any(|id| id == workspace) {
        "@"
    } else {
        " "
    };
    let mut words = vec![
        at.to_owned(),
        short(&change.change_id_reverse_hex),
        short(&change.commit_id),
    ];
    words.extend(change.bookmarks.iter().cloned());
    for (set, marker) in [
        (change.conflict, "conflict"),
        (change.divergent, "divergent"),
        (change.empty, "empty"),
        (change.immutable, "immutable"),
        (change.hidden, "hidden"),
    ] {
        if set {
            words.push(marker.to_owned());
        }
    }
    words.push(first_line(&change.description).to_owned());
    words.join(" ")
}

impl TextRender for RepoStatus {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(ws) = &self.workspace_status {
            let id = ws.workspace.as_ref().map_or("-", |ws| ws.id.as_str());
            match &ws.working_copy {
                Some(change) => writeln!(w, "Workspace {id} on {}", change_line(change))?,
                None => writeln!(w, "Workspace {id}")?,
            }
            for parent in &ws.parents {
                writeln!(w, "Parent {}", change_line(parent))?;
            }
            if ws.changed_file_count == 0 {
                writeln!(w, "Changed files: none")?;
            } else {
                writeln!(w, "Changed files ({}):", ws.changed_file_count)?;
                for file in &ws.changed_files {
                    writeln!(w, "{}", file_change_line(file))?;
                }
                let more = (ws.changed_file_count as usize).saturating_sub(ws.changed_files.len());
                if more > 0 {
                    writeln!(w, "... and {more} more")?;
                }
            }
            if ws.conflicted_path_count > 0 {
                writeln!(w, "Conflicted paths ({}):", ws.conflicted_path_count)?;
                for path in &ws.conflicted_paths {
                    writeln!(w, "{path}")?;
                }
            }
        }
        if let Some(repo) = &self.repository {
            writeln!(w, "Repository {} at {}", repo.id, repo.root)?;
        }
        if let Some(repo) = &self.repository_status {
            match &repo.trunk {
                Some(trunk) => writeln!(
                    w,
                    "Trunk {} {} {}",
                    repo.trunk_bookmark.as_deref().unwrap_or("-"),
                    short(&trunk.change_id_reverse_hex),
                    short(&trunk.commit_id)
                )?,
                None => writeln!(w, "Trunk none")?,
            }
            writeln!(w, "Workspaces: {} active", repo.active_workspace_count)?;
            writeln!(w, "Heads: {}", repo.head_count)?;
            for (label, ids) in [
                ("Conflicted changes", &repo.conflicted_change_ids),
                ("Divergent changes", &repo.divergent_change_ids),
            ] {
                if !ids.is_empty() {
                    let ids: Vec<String> = ids
                        .iter()
                        .map(|id| short(&reef_client::json::reverse_hex(id)))
                        .collect();
                    writeln!(w, "{label}: {}", ids.join(" "))?;
                }
            }
        }
        let last_snapshot = self
            .workspace_status
            .as_ref()
            .is_some_and(|ws| ws.source.as_deref() == Some("last-snapshot"));
        if last_snapshot {
            writeln!(w, "(as of the last snapshot; edits since may be missing)")?;
        }
        Ok(())
    }
}

/// `reef log`: the entries, and the scope's workspace for the `@` mark.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct LogLines {
    pub(crate) entries: Vec<LogEntry>,
    #[serde(skip)]
    pub(crate) workspace: String,
}

impl TextRender for LogLines {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.entries {
            writeln!(w, "{}", log_line(entry, &self.workspace))?;
        }
        Ok(())
    }
}

impl TextRender for ChangeSummary {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let change = &self.change;
        let signature = |signature: &Option<reef_client::json::Signature>| {
            signature.as_ref().map_or_else(
                || "-".to_owned(),
                |s| {
                    format!(
                        "{} <{}> {}",
                        s.name,
                        s.email,
                        s.at.as_deref().unwrap_or("-")
                    )
                },
            )
        };
        let parents: Vec<String> = change
            .parent_commit_ids
            .iter()
            .map(|id| short(id))
            .collect();
        let mut pairs = vec![
            ("change", change.change_id_reverse_hex.clone()),
            ("commit", change.commit_id.clone()),
            ("author", signature(&change.author)),
            ("committer", signature(&change.committer)),
        ];
        if !change.bookmarks.is_empty() {
            pairs.push(("bookmarks", change.bookmarks.join(" ")));
        }
        pairs.push(("parents", parents.join(" ")));
        block(w, &pairs)?;
        writeln!(w)?;
        if change.description.trim().is_empty() {
            writeln!(w, "    (no description)")?;
        } else {
            for line in change.description.lines() {
                writeln!(w, "    {line}")?;
            }
        }
        writeln!(w)?;
        for file in &self.files {
            writeln!(w, "{}", file_change_line(file))?;
        }
        Ok(())
    }
}

/// `reef diff`: the diff, and the format asked for, which decides its
/// text.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct DiffText {
    pub(crate) diff: Diff,
    #[serde(skip)]
    pub(crate) format: DiffFormat,
}

impl TextRender for DiffText {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        match self.format {
            DiffFormat::Summary => {
                for file in &self.diff.files {
                    writeln!(w, "{}", file_change_line(&file.change))?;
                }
            }
            DiffFormat::Stat => {
                let (mut added, mut removed) = (0_u64, 0_u64);
                for file in &self.diff.files {
                    added += u64::from(file.lines_added);
                    removed += u64::from(file.lines_removed);
                    writeln!(
                        w,
                        "{} | +{} -{}",
                        file.change.path, file.lines_added, file.lines_removed
                    )?;
                }
                writeln!(
                    w,
                    "{} file{} changed, +{added} -{removed}",
                    self.diff.total_files,
                    if self.diff.total_files == 1 { "" } else { "s" }
                )?;
            }
            DiffFormat::Hunks | DiffFormat::Unspecified => {
                for file in &self.diff.files {
                    file_diff(w, file)?;
                }
            }
        }
        Ok(())
    }
}

/// One file, git-style; for reading, not promised to `git apply`.
fn file_diff(w: &mut dyn Write, file: &FileDiff) -> io::Result<()> {
    let change = &file.change;
    let new = &change.path;
    let old = change.old_path.as_deref().unwrap_or(new);
    writeln!(w, "diff --git a/{old} b/{new}")?;
    match change.kind.as_deref() {
        Some("renamed") => writeln!(w, "rename from {old}\nrename to {new}")?,
        Some("copied") => writeln!(w, "copy from {old}\ncopy to {new}")?,
        _ => {}
    }
    if file.binary {
        return writeln!(w, "Binary files differ");
    }
    if file.too_large {
        return writeln!(w, "File too large to diff (over 16 MiB)");
    }
    if change.kind.as_deref() == Some("added") {
        writeln!(w, "--- /dev/null")?;
    } else {
        writeln!(w, "--- a/{old}")?;
    }
    if change.kind.as_deref() == Some("deleted") {
        writeln!(w, "+++ /dev/null")?;
    } else {
        writeln!(w, "+++ b/{new}")?;
    }
    if file.conflict {
        writeln!(w, "(conflict)")?;
    }
    for hunk in &file.hunks {
        writeln!(
            w,
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        )?;
        w.write_all(hunk.lines.as_bytes())?;
    }
    if !file.hunks.is_empty() && (file.old_no_final_newline || file.new_no_final_newline) {
        writeln!(w, "\\ No newline at end of file")?;
    }
    if file.truncated {
        writeln!(w, "(diff truncated)")?;
    }
    Ok(())
}

/// `<op> <ended_at> <kind>[/<snapshot kind>] <workspace or -> <agent or
/// -> <first description line>`.
fn op_line(op: &Operation) -> String {
    let mut kind = op.kind.clone().unwrap_or_else(|| "-".to_owned());
    if let Some(snapshot) = &op.snapshot_kind {
        kind.push('/');
        kind.push_str(snapshot);
    }
    format!(
        "{} {} {kind} {} {} {}",
        short(&op.id),
        op.ended_at.as_deref().unwrap_or("-"),
        op.workspace_id.as_deref().unwrap_or("-"),
        op.agent.as_deref().unwrap_or("-"),
        op.description.lines().next().unwrap_or_default()
    )
}

/// `reef op log`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct OpLines(pub(crate) Vec<Operation>);

impl TextRender for OpLines {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for op in &self.0 {
            writeln!(w, "{}", op_line(op))?;
        }
        Ok(())
    }
}

/// One operation of `reef op watch`: `op log`'s line, or the object
/// `op show` prints as one JSON line.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct OpLine(pub(crate) Operation);

impl TextRender for OpLine {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", op_line(&self.0))
    }
}

/// `reef op show`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct OpShow(pub(crate) Operation);

impl TextRender for OpShow {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let op = &self.0;
        let or_dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
        let mut kind = or_dash(op.kind.as_deref());
        if let Some(snapshot) = &op.snapshot_kind {
            kind.push('/');
            kind.push_str(snapshot);
        }
        let parents: Vec<String> = op.parent_ids.iter().map(|id| short(id)).collect();
        block(
            w,
            &[
                ("operation", op.id.clone()),
                ("ended", or_dash(op.ended_at.as_deref())),
                ("kind", kind),
                ("workspace", or_dash(op.workspace_id.as_deref())),
                ("agent", or_dash(op.agent.as_deref())),
                ("user", format!("{}@{}", op.username, op.hostname)),
                ("parents", parents.join(" ")),
                ("description", first_line(&op.description).to_owned()),
            ],
        )?;
        let Some(effects) = &op.effects else {
            return Ok(());
        };
        let id = |id: &Option<String>| id.as_deref().map_or_else(|| "-".to_owned(), short);
        for change in &effects.changes {
            let rh = short(&change.change_id_reverse_hex);
            match change.kind.as_deref() {
                Some("rewritten") => writeln!(
                    w,
                    "rewritten {rh} {} -> {}",
                    id(&change.previous_commit_id),
                    id(&change.commit_id)
                )?,
                Some("abandoned" | "hidden") => writeln!(
                    w,
                    "{} {rh} {}",
                    change.kind.as_deref().unwrap_or_default(),
                    id(&change.previous_commit_id)
                )?,
                kind => writeln!(
                    w,
                    "{} {rh} {}",
                    kind.unwrap_or("changed"),
                    id(&change.commit_id)
                )?,
            }
        }
        for bookmark in &effects.bookmarks {
            let name = match &bookmark.remote {
                Some(remote) => format!("{}@{remote}", bookmark.name),
                None => bookmark.name.clone(),
            };
            writeln!(w, "bookmark {name} -> {}", id(&bookmark.commit_id))?;
        }
        for wc in &effects.working_copies {
            let ws = wc.workspace_id.as_deref().unwrap_or(&wc.jj_workspace);
            writeln!(w, "working copy {ws} -> {}", id(&wc.commit_id))?;
        }
        Ok(())
    }
}

/// `reef file list`: one path per line, directories ending in `/`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct FileLines(pub(crate) Vec<FileEntry>);

impl TextRender for FileLines {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.0 {
            let slash = if entry.kind.as_deref() == Some("directory") {
                "/"
            } else {
                ""
            };
            writeln!(w, "{}{slash}", entry.path)?;
        }
        Ok(())
    }
}

impl TextRender for FileRead {
    /// Only for the trait: `file read` writes the bytes itself in text
    /// mode.
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(self.content.as_deref().unwrap_or_default().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use reef_api::v1;

    use super::*;

    fn render(value: &dyn TextRender) -> String {
        let mut buffer = Vec::new();
        value.render(&mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn diffs_print_git_style() {
        let diff = v1::GetDiffResponse {
            total_files: 2,
            files: vec![
                v1::FileDiff {
                    change: Some(v1::FileChange {
                        path: "a".into(),
                        kind: v1::FileChangeKind::Modified.into(),
                        ..v1::FileChange::default()
                    }),
                    lines_added: 1,
                    hunks: vec![v1::Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 2,
                        lines: " one\n+two\n".into(),
                    }],
                    new_no_final_newline: true,
                    ..v1::FileDiff::default()
                },
                v1::FileDiff {
                    change: Some(v1::FileChange {
                        path: "new".into(),
                        old_path: "old".into(),
                        kind: v1::FileChangeKind::Renamed.into(),
                        ..v1::FileChange::default()
                    }),
                    ..v1::FileDiff::default()
                },
            ],
            ..v1::GetDiffResponse::default()
        };
        let text = |format| {
            render(&DiffText {
                diff: Diff::from(&diff),
                format,
            })
        };
        assert_eq!(
            text(DiffFormat::Hunks),
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,1 +1,2 @@\n one\n+two\n\\ No newline at end of file\n\
             diff --git a/old b/new\nrename from old\nrename to new\n--- a/old\n+++ b/new\n"
        );
        assert_eq!(text(DiffFormat::Summary), "M a\nR old -> new\n");
        assert_eq!(
            text(DiffFormat::Stat),
            "a | +1 -0\nnew | +0 -0\n2 files changed, +1 -0\n"
        );
    }

    #[test]
    fn log_lines_mark_the_workspace_and_markers() {
        let entry = |workspace: &str, empty: bool| {
            LogEntry::from(&v1::LogEntry {
                change: Some(v1::Change {
                    change_id: "0123456789abcdef".into(),
                    commit_id: "c0ffee0000000000".into(),
                    description: "fix parser\n\nbody\n".into(),
                    empty,
                    bookmarks: vec!["main".into()],
                    workspace_ids: vec![workspace.into()],
                    ..v1::Change::default()
                }),
                edges: Vec::new(),
            })
        };
        let lines = LogLines {
            entries: vec![entry("ws1", false), entry("ws2", true)],
            workspace: "ws1".into(),
        };
        assert_eq!(
            render(&lines),
            "@ zyxwvutsrqpo c0ffee000000 main fix parser\n  zyxwvutsrqpo c0ffee000000 main empty fix parser\n"
        );
        let json = serde_json::to_value(&lines).unwrap();
        assert!(json.is_array(), "{json}");
    }

    #[test]
    fn file_lines_end_directories_in_a_slash() {
        let lines = FileLines(vec![
            FileEntry::from(&v1::FileEntry {
                path: "src".into(),
                r#type: v1::EntryType::Directory.into(),
                file_id: String::new(),
            }),
            FileEntry::from(&v1::FileEntry {
                path: "a.rs".into(),
                r#type: v1::EntryType::File.into(),
                file_id: "ff".into(),
            }),
        ]);
        assert_eq!(render(&lines), "src/\na.rs\n");
    }
}
