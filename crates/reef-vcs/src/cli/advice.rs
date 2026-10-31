//! The CLI's advice for the API's error reasons (R2.27, ADR 0034). The
//! daemon's hints are in the API's words and, for these reasons, carry no
//! `next`: it cannot know which command sent the request. The CLI does, so
//! it replaces the hint and builds the `next` from known values: the scope,
//! the command's own arguments, and the error's metadata. An error that
//! already carries the daemon's `next` keeps it, and a reason not in the
//! table keeps the daemon's advice.

use std::collections::BTreeMap;

use reef_api::error as reason;
use reef_client::ClientError;
use reef_error::Advice;

use super::CliError;
use super::scope::{PATH_HINT, Scope};

/// Change ids in hints, by their reverse hex; commits by hex.
const ID_WIDTH: usize = 12;

/// What the advice for one command's errors may use.
#[derive(Clone, Debug)]
pub(crate) struct AdviceCtx {
    /// The scope the command resolved.
    pub(crate) scope: Scope,
    /// The revision the command named, for `--expected-commit`'s `reef show`.
    pub(crate) revision: Option<String>,
    /// Whether the command was given an operation (`--at-op`, `undo`, `op
    /// show`): a missing one is then the caller's, not a page's.
    pub(crate) operation_given: bool,
    /// Whether the command snapshots first and takes `--no-snapshot`.
    pub(crate) snapshots_first: bool,
    /// The process's arguments with `reef` first, for "run it again".
    pub(crate) argv: Vec<String>,
}

impl AdviceCtx {
    /// For a command run in `scope`.
    pub(crate) fn new(scope: &Scope) -> Self {
        Self {
            scope: scope.clone(),
            revision: None,
            operation_given: false,
            snapshots_first: false,
            argv: own_argv(),
        }
    }

    /// `error` with the CLI's advice when the table has some for its reason.
    pub(crate) fn refine(&self, error: CliError) -> CliError {
        let CliError::Daemon(error) = error else {
            return error;
        };
        let daemon = error.advice();
        if daemon
            .as_ref()
            .is_some_and(|advice| !advice.next.is_empty())
        {
            return CliError::Daemon(error);
        }
        match self.advice(&error, daemon) {
            Some(advice) => CliError::Advised {
                error: Box::new(error),
                advice,
            },
            None => CliError::Daemon(error),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one row per reason")]
    fn advice(&self, error: &ClientError, daemon: Option<Advice>) -> Option<Advice> {
        let meta = error.metadata();
        let get = |key: &str| meta.get(key).map(String::as_str).unwrap_or_default();
        let daemon_hint = daemon.map(|advice| advice.hint).unwrap_or_default();
        let advice = match error.reason() {
            reason::WORKSPACE_REQUIRED => Advice::hint(
                "`@` is a workspace's change: run this inside a workspace, pass --workspace, or name a revision.",
            ),
            reason::INVALID_REVSET => Advice::hint(
                "Revisions are jj revsets: a change or commit id prefix, a bookmark, `@`, `trunk()`, `x::y`; quote them for the shell.",
            ),
            reason::AMBIGUOUS_ID_PREFIX => {
                Advice::hint("That prefix matches several ids; use more characters.")
            }
            reason::AMBIGUOUS_REVISION if !get("revision").is_empty() => Advice {
                hint: or(daemon_hint, "That revision names several commits; name one."),
                next: self.scoped(&["log", "-r", &every_commit(get("revision"), get("commit_ids"))]),
            },
            reason::REVISION_NOT_FOUND => Advice {
                hint: or(
                    daemon_hint,
                    "No such revision; `reef log` lists the changes in view.",
                ),
                next: self.scoped(&["log"]),
            },
            reason::REVISION_REQUIRED => {
                Advice::hint("Name a revision; `@` is the workspace's change.")
            }
            reason::WORKING_COPY_NOT_FOUND => Advice::hint(
                "The workspace had no working copy at that operation; name a revision instead of `@`.",
            ),
            reason::INVALID_OPERATION_ID | reason::AMBIGUOUS_OPERATION_ID => Advice {
                hint: "Name an operation by id or a unique prefix of lowercase hex; `reef op log` lists them.".to_owned(),
                next: self.op_log(),
            },
            reason::OPERATION_NOT_FOUND if self.operation_given => Advice {
                hint: "No such operation; `reef op log` lists them.".to_owned(),
                next: self.op_log(),
            },
            reason::OPERATION_NOT_FOUND | reason::INVALID_PAGE_TOKEN => Advice {
                hint: "The operation log changed while the pages were read; run the command again."
                    .to_owned(),
                next: self.argv.clone(),
            },
            reason::INVALID_PATH => Advice::hint(PATH_HINT),
            reason::INVALID_COMMIT_ID => Advice {
                hint: "--expected-commit takes a full commit id; `reef show` prints it.".to_owned(),
                next: self.scoped(&["show", self.revision.as_deref().unwrap_or("@")]),
            },
            reason::INVALID_DESCRIPTION => Advice::hint("A description may not contain a NUL byte."),
            reason::EXPECTED_COMMIT_MISMATCH if !get("commit_id").is_empty() => Advice {
                hint: format!(
                    "The revision now names {}; check it, then retry with --expected-commit {}.",
                    get("commit_id"),
                    get("commit_id")
                ),
                next: self.scoped(&["show", get("commit_id")]),
            },
            reason::IMMUTABLE_REVISION => Advice::hint(
                "Trunk, tags and untracked remote bookmarks are immutable; describe or abandon a change on top of them.",
            ),
            reason::HIDDEN_REVISION => Advice::hint(
                "That commit was rewritten since it was read (a snapshot, or another write); name the change, or its current commit from `reef log`.",
            ),
            reason::OPERATION_NOT_UNDOABLE => Advice::hint(
                "Creating, releasing and moving workspaces, git ref imports, merges and the root cannot be undone; release or move the workspace instead.",
            ),
            reason::OPERATION_NOT_IN_HISTORY => Advice {
                hint: "That operation is not in the current history; `reef op log` lists the ones that are.".to_owned(),
                next: self.op_log(),
            },
            reason::TOO_FAR_BEHIND => Advice {
                hint: "The stream's position is too far back; read history with `reef op log`, then watch from now.".to_owned(),
                next: self.op_log(),
            },
            reason::OPERATION_LOG_REWRITTEN => Advice {
                hint: "Garbage collection rewrote the operation log since that point; look it up again with --at, or read history with `reef op log`.".to_owned(),
                next: self.op_log(),
            },
            reason::OPERATION_NOT_RETAINED => Advice::hint(format!(
                "The operation log starts at {}: older operations were garbage-collected, or the repository is newer; pick a time at or after it.",
                get("oldest_at")
            )),
            reason::TOO_FAR_BACK => Advice {
                hint: "More than 100,000 operations ended after that time; page back with `reef op log --at-op <OP>` from an older operation.".to_owned(),
                next: self.op_log(),
            },
            reason::OPERATION_SUPERSEDED => Advice {
                hint: "Later operations changed what it did; undo them first, newest first."
                    .to_owned(),
                next: self.op_log(),
            },
            reason::INVALID_CHANGE_ID => {
                Advice::hint("--change takes a change id prefix, in hex or reverse hex.")
            }
            reason::TOO_MANY_PRESENCE_ENTRIES => Advice::hint(
                "Replace your entry instead of reporting a new one: pass --id with the id a report printed, or the same --id-file each time; reef presence clear ends one.",
            ),
            reason::PATH_NOT_FOUND if !get("commit_id").is_empty() => {
                let commit = get("commit_id");
                let mut words = vec!["file", "list"];
                let parent = parent(get("path"));
                if !parent.is_empty() {
                    words.push(parent);
                }
                words.extend(["-r", commit]);
                Advice {
                    hint: format!("No such path at {}; list its directory.", short(commit)),
                    next: self.scoped(&words),
                }
            }
            reason::NOT_A_FILE | reason::NOT_A_DIRECTORY if is_submodule(meta) => {
                Advice::hint("It is a git submodule; Reef reads no files inside one.")
            }
            reason::NOT_A_FILE if !get("commit_id").is_empty() => Advice {
                hint: "It is a directory; list it instead.".to_owned(),
                next: self.scoped(&["file", "list", get("path"), "-r", get("commit_id")]),
            },
            reason::NOT_A_DIRECTORY if !get("commit_id").is_empty() => Advice {
                hint: "It is not a directory; read it instead.".to_owned(),
                next: self.scoped(&["file", "read", get("path"), "-r", get("commit_id")]),
            },
            reason::SNAPSHOT_FAILED if self.snapshots_first => Advice {
                hint: "The workspace could not be snapshotted; --no-snapshot reads its last snapshot."
                    .to_owned(),
                next: with_no_snapshot(&self.argv),
            },
            _ => return None,
        };
        Some(advice)
    }

    /// `reef <words>` and the scope's flags.
    fn scoped(&self, words: &[&str]) -> Vec<String> {
        let mut argv = vec!["reef".to_owned()];
        argv.extend(words.iter().map(|word| (*word).to_owned()));
        argv.extend(self.scope.flags());
        argv
    }

    /// `reef op log --repo <id>`.
    fn op_log(&self) -> Vec<String> {
        ["reef", "op", "log", "--repo", &self.scope.repo_id]
            .map(str::to_owned)
            .to_vec()
    }
}

/// A revset naming the commits an ambiguous `revision` names: the commit
/// ids the error lists (up to ten), which `log` takes whatever the revision
/// was (a divergent change id, which jj refuses as a bare symbol, a
/// conflicted bookmark, a revset of several); the revision itself when the
/// error lists none.
fn every_commit(revision: &str, commit_ids: &str) -> String {
    if commit_ids.is_empty() {
        revision.to_owned()
    } else {
        commit_ids.replace(',', "|")
    }
}

fn or(hint: String, default: &str) -> String {
    if hint.is_empty() {
        default.to_owned()
    } else {
        hint
    }
}

fn short(id: &str) -> String {
    id.chars().take(ID_WIDTH).collect()
}

/// The directory holding a repository-relative path; empty for the root.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn is_submodule(meta: &BTreeMap<String, String>) -> bool {
    meta.get("type").map(String::as_str) == Some("ENTRY_TYPE_SUBMODULE")
}

/// `argv` with `--no-snapshot` before a `--`, else at the end.
fn with_no_snapshot(argv: &[String]) -> Vec<String> {
    let mut next = argv.to_vec();
    let at = next
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(next.len());
    next.insert(at, "--no-snapshot".to_owned());
    next
}

/// The process's arguments with `reef` as the first word: the command as
/// it ran, which runs again from the same directory.
fn own_argv() -> Vec<String> {
    let mut argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match argv.first_mut() {
        Some(first) => "reef".clone_into(first),
        None => argv.push("reef".to_owned()),
    }
    argv
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use reef_api::v1;
    use reef_error::{Classify as _, ErrorKind};

    use super::*;

    const REPO: &str = "aaaa000000000001";
    const WS: &str = "bbbb000000000001";
    const COMMIT: &str = "c0ffee0000000000000000000000000000000000";

    fn ctx() -> AdviceCtx {
        AdviceCtx {
            scope: Scope {
                repo_id: REPO.into(),
                workspace: Some(v1::Workspace {
                    id: WS.into(),
                    repo_id: REPO.into(),
                    ..v1::Workspace::default()
                }),
                dir: PathBuf::from("/ws"),
                from_flags: false,
            },
            revision: Some("abc".into()),
            operation_given: false,
            snapshots_first: true,
            argv: ["reef", "status", "--max-files", "5"]
                .map(str::to_owned)
                .to_vec(),
        }
    }

    /// A daemon error with `reason`, `metadata` and the daemon's `advice`,
    /// as a client receives it.
    fn daemon_error(
        kind: ErrorKind,
        reason_name: &str,
        metadata: &[(&str, &str)],
        advice: Option<Advice>,
    ) -> CliError {
        let advice = advice.unwrap_or_default();
        let detail = v1::ErrorDetail {
            kind: reef_client::error::proto_kind(kind).into(),
            reason: reason_name.to_owned(),
            chain: vec!["refused".to_owned()],
            hint: advice.hint,
            next: advice.next,
            metadata: metadata
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        };
        let status = reef_api::error::status(
            reef_client::error::code_of(kind),
            "refused".to_owned(),
            &detail,
        );
        CliError::Daemon(ClientError::from(status))
    }

    fn refined(ctx: &AdviceCtx, reason_name: &str, metadata: &[(&str, &str)]) -> Advice {
        let error = ctx.refine(daemon_error(
            ErrorKind::InvalidArgument,
            reason_name,
            metadata,
            Some(Advice::hint("the daemon's words")),
        ));
        assert_eq!(error.reason(), reason_name);
        error.advice().unwrap()
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// A row: the reason, its metadata, and the `next` it gets.
    type Row = (&'static str, Vec<(&'static str, &'static str)>, Vec<String>);

    /// R2.27 test 21: each row's hint and `next`, every `next` a real
    /// `reef` argv with no placeholder.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one row per reason")]
    fn advice_table() {
        let ctx = ctx();
        let rows: Vec<Row> = vec![
            (reason::WORKSPACE_REQUIRED, vec![], vec![]),
            (reason::INVALID_REVSET, vec![], vec![]),
            (reason::AMBIGUOUS_ID_PREFIX, vec![("prefix", "ab")], vec![]),
            (
                reason::AMBIGUOUS_REVISION,
                vec![("revision", "zyx"), ("commit_ids", "a,b")],
                argv(&["reef", "log", "-r", "a|b", "--workspace", WS]),
            ),
            (
                reason::REVISION_NOT_FOUND,
                vec![("revision", "nope")],
                argv(&["reef", "log", "--workspace", WS]),
            ),
            (reason::REVISION_REQUIRED, vec![], vec![]),
            (reason::WORKING_COPY_NOT_FOUND, vec![], vec![]),
            (
                reason::INVALID_OPERATION_ID,
                vec![],
                argv(&["reef", "op", "log", "--repo", REPO]),
            ),
            (
                reason::AMBIGUOUS_OPERATION_ID,
                vec![],
                argv(&["reef", "op", "log", "--repo", REPO]),
            ),
            (reason::INVALID_PATH, vec![("path", "/x")], vec![]),
            (
                reason::INVALID_COMMIT_ID,
                vec![],
                argv(&["reef", "show", "abc", "--workspace", WS]),
            ),
            (reason::INVALID_DESCRIPTION, vec![], vec![]),
            (
                reason::EXPECTED_COMMIT_MISMATCH,
                vec![("commit_id", COMMIT), ("expected_commit_id", "f")],
                argv(&["reef", "show", COMMIT, "--workspace", WS]),
            ),
            (reason::IMMUTABLE_REVISION, vec![], vec![]),
            (reason::HIDDEN_REVISION, vec![], vec![]),
            (reason::OPERATION_NOT_UNDOABLE, vec![], vec![]),
            (
                reason::OPERATION_NOT_IN_HISTORY,
                vec![],
                argv(&["reef", "op", "log", "--repo", REPO]),
            ),
            (
                reason::OPERATION_SUPERSEDED,
                vec![],
                argv(&["reef", "op", "log", "--repo", REPO]),
            ),
            (reason::INVALID_CHANGE_ID, vec![], vec![]),
            (
                reason::PATH_NOT_FOUND,
                vec![("path", "src/nope.rs"), ("commit_id", COMMIT)],
                argv(&[
                    "reef",
                    "file",
                    "list",
                    "src",
                    "-r",
                    COMMIT,
                    "--workspace",
                    WS,
                ]),
            ),
            (
                reason::NOT_A_FILE,
                vec![
                    ("path", "src"),
                    ("type", "ENTRY_TYPE_DIRECTORY"),
                    ("commit_id", COMMIT),
                ],
                argv(&[
                    "reef",
                    "file",
                    "list",
                    "src",
                    "-r",
                    COMMIT,
                    "--workspace",
                    WS,
                ]),
            ),
            (
                reason::NOT_A_DIRECTORY,
                vec![
                    ("path", "a.rs"),
                    ("type", "ENTRY_TYPE_FILE"),
                    ("commit_id", COMMIT),
                ],
                argv(&[
                    "reef",
                    "file",
                    "read",
                    "a.rs",
                    "-r",
                    COMMIT,
                    "--workspace",
                    WS,
                ]),
            ),
            (
                reason::SNAPSHOT_FAILED,
                vec![("workspace_id", WS)],
                argv(&["reef", "status", "--max-files", "5", "--no-snapshot"]),
            ),
        ];
        for (reason_name, metadata, next) in rows {
            let advice = refined(&ctx, reason_name, &metadata);
            assert!(!advice.hint.is_empty(), "{reason_name}");
            assert_eq!(advice.next, next, "{reason_name}");
            for word in &advice.next {
                assert!(!word.contains(['<', '>']), "{reason_name}: {word}");
            }
        }
        // A path at the root lists the root.
        let advice = refined(
            &ctx,
            reason::PATH_NOT_FOUND,
            &[("path", "nope.rs"), ("commit_id", COMMIT)],
        );
        assert_eq!(
            advice.next,
            argv(&["reef", "file", "list", "-r", COMMIT, "--workspace", WS])
        );
        assert!(advice.hint.contains("c0ffee000000;"), "{}", advice.hint);
        // A submodule has no next.
        let advice = refined(
            &ctx,
            reason::NOT_A_FILE,
            &[
                ("path", "vendor"),
                ("type", "ENTRY_TYPE_SUBMODULE"),
                ("commit_id", COMMIT),
            ],
        );
        assert!(advice.next.is_empty() && advice.hint.contains("submodule"));
        // The daemon's hint stays where the row keeps it.
        let advice = refined(&ctx, reason::REVISION_NOT_FOUND, &[("revision", "x")]);
        assert_eq!(advice.hint, "the daemon's words");
    }

    #[test]
    fn the_daemons_next_wins_and_unknown_reasons_keep_its_advice() {
        let ctx = ctx();
        let theirs = Advice::run("Pick one.", argv(&["reef", "ws", "list", "--all"]));
        let error = ctx.refine(daemon_error(
            ErrorKind::NotFound,
            reason::WORKSPACE_NOT_FOUND,
            &[],
            Some(theirs.clone()),
        ));
        assert!(matches!(error, CliError::Daemon(_)));
        assert_eq!(error.advice(), Some(theirs.clone()));
        // A write's snapshot failure keeps `reef ws inspect`.
        let inspect = Advice::run("Inspect it.", argv(&["reef", "ws", "inspect", WS]));
        let error = ctx.refine(daemon_error(
            ErrorKind::Conflict,
            reason::SNAPSHOT_FAILED,
            &[("workspace_id", WS)],
            Some(inspect.clone()),
        ));
        assert_eq!(error.advice(), Some(inspect));
        assert_eq!(error.kind(), ErrorKind::Conflict);
        let error = ctx.refine(daemon_error(
            ErrorKind::InvalidArgument,
            reason::TEXT_NOT_FOUND,
            &[],
            Some(Advice::hint("the daemon's words")),
        ));
        assert_eq!(error.advice().unwrap().hint, "the daemon's words");
    }

    /// R2.27 test 18: a page error, or a missing operation the command was
    /// not given, runs the command again; one it was given lists them.
    #[test]
    fn page_errors_rerun() {
        let mut ctx = ctx();
        for reason_name in [reason::INVALID_PAGE_TOKEN, reason::OPERATION_NOT_FOUND] {
            let advice = refined(&ctx, reason_name, &[]);
            assert!(
                advice.hint.contains("run the command again"),
                "{reason_name}"
            );
            assert_eq!(advice.next, ctx.argv, "{reason_name}");
        }
        ctx.operation_given = true;
        let advice = refined(&ctx, reason::OPERATION_NOT_FOUND, &[("operation_id", "ab")]);
        assert_eq!(advice.next, argv(&["reef", "op", "log", "--repo", REPO]));
    }

    #[test]
    fn ambiguous_revisions_list_their_commits() {
        assert_eq!(every_commit("zypo", "c0ffee,d00d"), "c0ffee|d00d");
        assert_eq!(every_commit("cafe", ""), "cafe");
    }

    #[test]
    fn no_snapshot_goes_before_a_double_dash() {
        assert_eq!(
            with_no_snapshot(&argv(&["reef", "diff", "--", "-weird"])),
            argv(&["reef", "diff", "--no-snapshot", "--", "-weird"])
        );
    }

    #[test]
    fn metadata_survives_refinement() {
        let error = ctx().refine(daemon_error(
            ErrorKind::Conflict,
            reason::EXPECTED_COMMIT_MISMATCH,
            &[("commit_id", COMMIT), ("expected_commit_id", "f")],
            None,
        ));
        assert_eq!(error.metadata().get("commit_id").unwrap(), COMMIT);
        assert_eq!(error.exit_code(), 5);
    }
}
