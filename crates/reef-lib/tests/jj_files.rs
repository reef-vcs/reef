//! Files without a working copy over jj-lib (R2.04, ADR 0032): reading a
//! file and listing a directory at a commit, edits applied in order to an
//! overlay, and recorded as a rewritten commit or a new change. Each test
//! builds its repository with jj-lib transactions on an internal git store
//! in its own temporary directory.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::sync::Arc;

use jj_lib::backend::{CommitId, CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::{RefName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::transaction::Transaction;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::diff::{self, ChangeKind, DiffBase, DiffFormat, DiffOptions, EntryKind};
use reef_lib::jj::files::{
    self, Edit, EditAction, EditError, EditProblem, FileError, ListOptions, MAX_EDITS,
};
use reef_lib::jj::oplog::{self, ATTR_AGENT, ATTR_KIND, ATTR_WORKSPACE};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::rewrite::{self, RewriteError};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::undo::UndoError;
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta, Repo};
use tempfile::TempDir;

const EMAIL: &str = "reef-test@example.com";

struct Fixture {
    _dir: TempDir,
    repo: Repo,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main");
        std::fs::create_dir(&root).expect("create main");
        let settings = user_settings(&Identity {
            name: "Reef Test".into(),
            email: EMAIL.into(),
        })
        .expect("settings");
        let repo = Repo::init_internal_git(&root, &settings)
            .await
            .expect("init repo");
        Self { _dir: dir, repo }
    }

    fn head(&self) -> Arc<ReadonlyRepo> {
        self.repo.readonly()
    }

    fn tx(&self) -> Transaction {
        self.repo.head().start_transaction()
    }

    async fn commit(&mut self, mut tx: Transaction) {
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit("test").await.unwrap();
        self.repo.set_head(repo);
    }

    fn scope(&self) -> RevsetScope<'_> {
        RevsetScope {
            workspace: None,
            user_email: EMAIL,
            root: self.repo.workspace_root(),
        }
    }

    async fn get(&self, id: &CommitIdHex) -> Commit {
        self.head()
            .store()
            .get_commit_async(&commit_id(id))
            .await
            .unwrap()
    }

    fn wc(&self, name: &str) -> CommitId {
        self.head()
            .view()
            .get_wc_commit_id(WorkspaceName::new(name))
            .cloned()
            .unwrap()
    }

    async fn read(&self, commit: &CommitIdHex, text: &str) -> Result<files::FileRead, FileError> {
        files::read_file(&self.head(), commit, &path(text), 0, 0).await
    }

    async fn bytes(&self, commit: &CommitIdHex, text: &str) -> Vec<u8> {
        self.read(commit, text).await.unwrap().content
    }

    async fn list(
        &self,
        commit: &CommitIdHex,
        dir: &str,
        recursive: bool,
    ) -> Result<Vec<(String, EntryKind)>, FileError> {
        let options = ListOptions {
            dir: (!dir.is_empty()).then(|| path(dir)),
            recursive,
            after: None,
            limit: 1000,
        };
        let page = files::list_files(&self.head(), commit, &options, &|| false).await?;
        assert!(!page.more);
        Ok(page
            .entries
            .into_iter()
            .map(|entry| (entry.path, entry.kind))
            .collect())
    }
}

fn path(text: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(text).unwrap()
}

fn hex(commit: &Commit) -> CommitIdHex {
    commit.id().hex().parse().unwrap()
}

fn commit_id(id: &CommitIdHex) -> CommitId {
    CommitId::try_from_hex(id.as_str()).unwrap()
}

fn meta() -> OpMeta {
    OpMeta::new(OpKind::Edit, "test: edit")
        .workspace("0123456789abcdef")
        .agent("tester")
}

/// One path's content in [`tree_with`].
enum Entry<'a> {
    File(&'a [u8]),
    Exec(&'a [u8]),
    Link(&'a str),
    Submodule,
}

/// `base` with `entries` written over it.
async fn tree_with(
    tx: &Transaction,
    base: MergedTree,
    entries: &[(&str, Entry<'_>)],
) -> MergedTree {
    let store = tx.repo().store();
    let mut builder = MergedTreeBuilder::new(base);
    for (text, entry) in entries {
        let path = path(text);
        let value = match entry {
            Entry::File(bytes) | Entry::Exec(bytes) => TreeValue::File {
                id: store.write_file(&path, &mut &bytes[..]).await.unwrap(),
                executable: matches!(entry, Entry::Exec(_)),
                copy_id: CopyId::placeholder(),
            },
            Entry::Link(target) => {
                TreeValue::Symlink(store.write_symlink(&path, target).await.unwrap())
            }
            Entry::Submodule => TreeValue::GitSubmodule(store.root_commit_id().clone()),
        };
        builder.set_or_remove(path, Merge::normal(value));
    }
    builder.write_tree().await.unwrap()
}

/// A new commit on `parents` (the root when empty) holding `entries`.
async fn new_commit(
    tx: &mut Transaction,
    parents: &[&Commit],
    entries: &[(&str, Entry<'_>)],
) -> Commit {
    let parent_commits: Vec<Commit> = parents.iter().map(|commit| (*commit).clone()).collect();
    let base = if parents.is_empty() {
        tx.repo().store().empty_merged_tree()
    } else {
        merge_commit_trees(tx.repo(), &parent_commits)
            .await
            .unwrap()
    };
    let tree = tree_with(tx, base, entries).await;
    let parents = if parents.is_empty() {
        vec![tx.repo().store().root_commit_id().clone()]
    } else {
        parents.iter().map(|commit| commit.id().clone()).collect()
    };
    tx.repo_mut()
        .new_commit(parents, tree)
        .write()
        .await
        .unwrap()
}

fn edit(text: &str, action: EditAction) -> Edit {
    Edit {
        path: text.into(),
        action: Some(action),
        executable: None,
    }
}

fn content(text: &str, bytes: &str) -> Edit {
    edit(text, EditAction::Content(bytes.as_bytes().to_vec()))
}

fn replace(text: &str, old: &str, new: &str, all: bool) -> Edit {
    edit(
        text,
        EditAction::Replace {
            old: old.into(),
            new: new.into(),
            all,
        },
    )
}

fn delete(text: &str) -> Edit {
    edit(text, EditAction::Delete)
}

fn copy(text: &str, from: &str) -> Edit {
    edit(text, EditAction::CopyFrom(from.into()))
}

/// The problem and index of a failed edit.
fn problem(error: EditError) -> (usize, EditProblem) {
    match error {
        EditError::Edit { index, problem, .. } => (index, problem),
        other => panic!("not an edit's problem: {other:?}"),
    }
}

/// `base`'s tree with `edits` applied, or the first edit's problem.
async fn applied(base: &Commit, edits: &[Edit]) -> Result<MergedTree, (usize, EditProblem)> {
    files::apply(&base.tree(), edits, true)
        .await
        .map(Option::unwrap)
        .map_err(problem)
}

async fn value(tree: &MergedTree, text: &str) -> Option<TreeValue> {
    tree.path_value(&path(text))
        .await
        .unwrap()
        .into_resolved()
        .unwrap()
}

async fn tree_bytes(tree: &MergedTree, text: &str) -> Vec<u8> {
    use futures::AsyncReadExt as _;
    let Some(TreeValue::File { id, .. }) = value(tree, text).await else {
        panic!("{text} is not a file");
    };
    let mut bytes = Vec::new();
    tree.store()
        .read_file(&path(text), &id)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    bytes
}

/// A commit whose tree conflicts at `f` (a file conflict) and at `d` (a
/// file against a directory), on two parents.
async fn conflicted(fx: &mut Fixture) -> Commit {
    let mut tx = fx.tx();
    let base = new_commit(
        &mut tx,
        &[],
        &[("f", Entry::File(b"base\n")), ("g", Entry::File(b"g\n"))],
    )
    .await;
    let left = new_commit(
        &mut tx,
        &[&base],
        &[("f", Entry::File(b"left\n")), ("d", Entry::File(b"file\n"))],
    )
    .await;
    let right = new_commit(
        &mut tx,
        &[&base],
        &[("f", Entry::File(b"right\n")), ("d/x", Entry::File(b"x\n"))],
    )
    .await;
    let merged = new_commit(&mut tx, &[&left, &right], &[]).await;
    fx.commit(tx).await;
    assert!(merged.tree().has_conflict());
    merged
}

// 1
#[tokio::test]
async fn read_file_entries() {
    let mut fx = Fixture::new().await;
    let big: Vec<u8> = (0..3 * (1 << 20) + 17)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let mut tx = fx.tx();
    let commit = new_commit(
        &mut tx,
        &[],
        &[
            ("a.txt", Entry::File(b"hello\n")),
            ("run.sh", Entry::Exec(b"#!/bin/sh\n")),
            ("link", Entry::Link("a.txt")),
            ("dir/inner", Entry::File(b"inner\n")),
            ("big.bin", Entry::File(&big)),
            ("sub", Entry::Submodule),
        ],
    )
    .await;
    fx.commit(tx).await;
    let id = hex(&commit);

    let read = fx.read(&id, "a.txt").await.unwrap();
    assert_eq!(
        (read.entry.kind, read.content.as_slice(), read.size),
        (EntryKind::File, &b"hello\n"[..], 6)
    );
    assert!(read.entry.id.is_some());
    assert_eq!(
        fx.read(&id, "run.sh").await.unwrap().entry.kind,
        EntryKind::Executable
    );
    let link = fx.read(&id, "link").await.unwrap();
    assert_eq!(
        (link.entry.kind, link.content.as_slice()),
        (EntryKind::Symlink, &b"a.txt"[..])
    );

    // Paged: 1 MiB at a time until `size`; past the end is empty.
    let mut whole = Vec::new();
    loop {
        let offset = u64::try_from(whole.len()).unwrap();
        let page = files::read_file(&fx.head(), &id, &path("big.bin"), offset, 0)
            .await
            .unwrap();
        assert_eq!(page.size, u64::try_from(big.len()).unwrap());
        assert!(page.content.len() <= files::MAX_READ_BYTES);
        if page.content.is_empty() {
            break;
        }
        whole.extend(page.content);
    }
    assert_eq!(whole, big);
    let past = files::read_file(&fx.head(), &id, &path("a.txt"), 100, 3)
        .await
        .unwrap();
    assert!(past.content.is_empty() && past.size == 6);
    let range = files::read_file(&fx.head(), &id, &path("a.txt"), 1, 3)
        .await
        .unwrap();
    assert_eq!(range.content, b"ell");

    for (text, kind) in [("dir", EntryKind::Directory), ("sub", EntryKind::Submodule)] {
        let error = fx.read(&id, text).await.unwrap_err();
        assert!(
            matches!(&error, FileError::NotAFile { kind: k, .. } if *k == kind),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Conflict);
    }
    for text in ["absent", "link/x", "a.txt/x"] {
        let error = fx.read(&id, text).await.unwrap_err();
        assert!(
            matches!(error, FileError::NotFound { .. }),
            "{text}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    // Conflicts: markers for a file conflict, jj's description otherwise.
    let merged = conflicted(&mut fx).await;
    let read = fx.read(&hex(&merged), "f").await.unwrap();
    assert_eq!(read.entry.kind, EntryKind::Conflict);
    assert!(read.entry.id.is_none());
    let text = String::from_utf8(read.content).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains("left") && text.contains("right"),
        "{text}"
    );
    let read = fx.read(&hex(&merged), "d").await.unwrap();
    assert_eq!(read.entry.kind, EntryKind::Conflict);
    assert_ne!(read.content, b"");

    // A hidden commit, by id, still reads.
    let mut tx = fx.tx();
    let old = fx.get(&id).await;
    tx.repo_mut().record_abandoned_commit(&old);
    fx.commit(tx).await;
    assert_eq!(fx.bytes(&id, "a.txt").await, b"hello\n");
}

// 2
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one fixture, every listing case")]
async fn list_files() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let commit = new_commit(
        &mut tx,
        &[],
        &[
            ("a.rs", Entry::File(b"1")),
            ("a/b", Entry::File(b"2")),
            ("a/c/d", Entry::File(b"3")),
            ("link", Entry::Link("a")),
            ("z", Entry::Exec(b"4")),
        ],
    )
    .await;
    fx.commit(tx).await;
    let id = hex(&commit);
    let names = |entries: Vec<(String, EntryKind)>| entries;
    assert_eq!(
        names(fx.list(&id, "", false).await.unwrap()),
        [
            ("a".into(), EntryKind::Directory),
            ("a.rs".into(), EntryKind::File),
            ("link".into(), EntryKind::Symlink),
            ("z".into(), EntryKind::Executable),
        ]
    );
    let recursive: Vec<String> = fx
        .list(&id, "", true)
        .await
        .unwrap()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(recursive, ["a/b", "a/c/d", "a.rs", "link", "z"]);
    assert_eq!(
        fx.list(&id, "a", false).await.unwrap(),
        [
            ("a/b".into(), EntryKind::File),
            ("a/c".into(), EntryKind::Directory)
        ]
    );
    assert_eq!(fx.list(&id, "a", true).await.unwrap().len(), 2);
    let error = fx.list(&id, "a.rs", false).await.unwrap_err();
    assert!(
        matches!(
            error,
            FileError::NotADirectory {
                kind: EntryKind::File,
                ..
            }
        ),
        "{error:?}"
    );
    let error = fx.list(&id, "link", true).await.unwrap_err();
    assert!(
        matches!(
            error,
            FileError::NotADirectory {
                kind: EntryKind::Symlink,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(matches!(
        fx.list(&id, "nope", false).await.unwrap_err(),
        FileError::NotFound { .. }
    ));

    // Pages of 2 with a cursor concatenate to the one-shot listing.
    for recursive in [false, true] {
        let whole = fx.list(&id, "", recursive).await.unwrap();
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let options = ListOptions {
                dir: None,
                recursive,
                after: after.clone(),
                limit: 2,
            };
            let page = files::list_files(&fx.head(), &id, &options, &|| false)
                .await
                .unwrap();
            seen.extend(
                page.entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.kind)),
            );
            if !page.more {
                break;
            }
            after = Some(path(&page.entries.last().unwrap().path));
        }
        assert_eq!(seen, whole, "recursive {recursive}");
    }

    // In a conflicted tree, a directory differing between sides is a
    // directory; a file against a directory is a conflict.
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("dir/x", Entry::File(b"x"))]).await;
    let left = new_commit(
        &mut tx,
        &[&base],
        &[("dir/x", Entry::File(b"l")), ("both", Entry::File(b"f"))],
    )
    .await;
    let right = new_commit(
        &mut tx,
        &[&base],
        &[("dir/x", Entry::File(b"r")), ("both/y", Entry::File(b"y"))],
    )
    .await;
    let merged = new_commit(&mut tx, &[&left, &right], &[]).await;
    fx.commit(tx).await;
    let listed = fx.list(&hex(&merged), "", false).await.unwrap();
    assert_eq!(
        listed,
        [
            ("both".into(), EntryKind::Conflict),
            ("dir".into(), EntryKind::Directory)
        ]
    );
}

// 4
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one fixture, every edit action")]
async fn edits_apply_in_order() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(
        &mut tx,
        &[],
        &[
            ("f", Entry::File(b"one two\n")),
            ("x", Entry::Exec(b"x\n")),
            ("link", Entry::Link("f")),
            ("dir/a", Entry::File(b"a\n")),
            ("only/last", Entry::File(b"l\n")),
            ("aaa", Entry::File(b"aaa")),
        ],
    )
    .await;
    fx.commit(tx).await;

    // content: a new file's bit is false; an executable keeps its bit; the
    // bit alone flips.
    let tree = applied(&base, &[content("new", "n"), content("x", "y\n")])
        .await
        .unwrap();
    assert!(matches!(
        value(&tree, "new").await,
        Some(TreeValue::File {
            executable: false,
            ..
        })
    ));
    assert!(matches!(
        value(&tree, "x").await,
        Some(TreeValue::File {
            executable: true,
            ..
        })
    ));
    let bit = Edit {
        path: "f".into(),
        action: None,
        executable: Some(true),
    };
    let tree = applied(&base, &[bit]).await.unwrap();
    assert!(matches!(
        value(&tree, "f").await,
        Some(TreeValue::File {
            executable: true,
            ..
        })
    ));
    assert_eq!(tree_bytes(&tree, "f").await, b"one two\n");

    // replace: once, all, after a content in the same request.
    let tree = applied(&base, &[replace("f", "two", "2", false)])
        .await
        .unwrap();
    assert_eq!(tree_bytes(&tree, "f").await, b"one 2\n");
    let tree = applied(&base, &[replace("aaa", "a", "b", true)])
        .await
        .unwrap();
    assert_eq!(tree_bytes(&tree, "aaa").await, b"bbb");
    let tree = applied(
        &base,
        &[content("f", "x y x"), replace("f", "y", "z", false)],
    )
    .await
    .unwrap();
    assert_eq!(tree_bytes(&tree, "f").await, b"x z x");
    let (index, error) = applied(&base, &[replace("f", "nope", "", false)])
        .await
        .unwrap_err();
    assert!(
        matches!(error, EditProblem::TextNotFound(_)) && index == 0,
        "{error:?}"
    );
    let (_, error) = applied(&base, &[replace("aaa", "aa", "b", false)])
        .await
        .unwrap_err();
    assert!(
        matches!(error, EditProblem::TextNotUnique { occurrences: 2, .. }),
        "{error:?}"
    );
    let (_, error) = applied(&base, &[replace("absent", "a", "b", false)])
        .await
        .unwrap_err();
    assert!(matches!(error, EditProblem::PathNotFound(_)), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::NotFound);

    // A symlink is replaced only after a delete; nothing is written under
    // a file or a symlink.
    let (_, error) = applied(&base, &[content("link", "x")]).await.unwrap_err();
    assert!(
        matches!(
            error,
            EditProblem::NotAFile {
                kind: EntryKind::Symlink,
                ..
            }
        ),
        "{error:?}"
    );
    let tree = applied(&base, &[delete("link"), content("link", "now a file")])
        .await
        .unwrap();
    assert!(matches!(
        value(&tree, "link").await,
        Some(TreeValue::File { .. })
    ));
    for (text, ancestor) in [("link/x", "link"), ("f/x", "f"), ("f/y/z", "f")] {
        let (index, error) = applied(&base, &[content("ok", "1"), content(text, "x")])
            .await
            .unwrap_err();
        assert_eq!(index, 1);
        assert!(
            matches!(&error, EditProblem::NotADirectory { path, .. } if path == ancestor),
            "{text}: {error:?}"
        );
    }

    // delete: nothing is an error, a directory too; the last file of a
    // directory takes the directory with it.
    let (_, error) = applied(&base, &[delete("absent")]).await.unwrap_err();
    assert!(matches!(error, EditProblem::PathNotFound(_)), "{error:?}");
    let (_, error) = applied(&base, &[delete("dir")]).await.unwrap_err();
    assert!(
        matches!(
            error,
            EditProblem::NotAFile {
                kind: EntryKind::Directory,
                ..
            }
        ),
        "{error:?}"
    );
    let tree = applied(&base, &[delete("only/last"), content("only", "a file now")])
        .await
        .unwrap();
    assert!(matches!(
        value(&tree, "only").await,
        Some(TreeValue::File { .. })
    ));

    // copy_from keeps the id and bit, copies a symlink, and with a delete
    // is a rename.
    let tree = applied(&base, &[copy("y", "x"), copy("link2", "link")])
        .await
        .unwrap();
    let (
        Some(TreeValue::File { id: from, .. }),
        Some(TreeValue::File {
            id: to, executable, ..
        }),
    ) = (value(&base.tree(), "x").await, value(&tree, "y").await)
    else {
        panic!("not files");
    };
    assert_eq!((from, executable), (to, true));
    assert!(matches!(
        value(&tree, "link2").await,
        Some(TreeValue::Symlink(_))
    ));
    let (_, error) = applied(&base, &[copy("y", "dir")]).await.unwrap_err();
    assert!(matches!(error, EditProblem::NotAFile { .. }), "{error:?}");
    let (_, error) = applied(&base, &[copy("y", "gone")]).await.unwrap_err();
    assert!(
        matches!(&error, EditProblem::PathNotFound(path) if path == "gone"),
        "{error:?}"
    );

    // A rename is reported as one.
    let mut tx = fx.tx();
    let tree = applied(&base, &[copy("g", "f"), delete("f")])
        .await
        .unwrap();
    let renamed = tx
        .repo_mut()
        .new_commit(vec![base.id().clone()], tree)
        .write()
        .await
        .unwrap();
    fx.commit(tx).await;
    let page = diff::diff_page(
        &fx.head(),
        &DiffBase::Parents,
        &hex(&renamed),
        &DiffOptions {
            format: DiffFormat::Summary,
            context: 3,
            max_lines: 100,
            max_hunk_bytes: 1 << 20,
            max_file_bytes: 1 << 20,
            paths: Vec::new(),
            after: None,
            limit: 100,
        },
        &|| false,
    )
    .await
    .unwrap();
    assert_eq!(page.files.len(), 1, "{page:?}");
    assert_eq!(page.files[0].kind, Some(ChangeKind::Renamed));

    // Too large to replace in.
    let mut tx = fx.tx();
    let huge = vec![b'a'; usize::try_from(files::MAX_REPLACE_BYTES).unwrap() + 1];
    let big = new_commit(&mut tx, &[], &[("huge", Entry::File(&huge))]).await;
    fx.commit(tx).await;
    let (_, error) = applied(&big, &[replace("huge", "a", "b", false)])
        .await
        .unwrap_err();
    assert!(
        matches!(error, EditProblem::FileTooLarge { .. }),
        "{error:?}"
    );

    // Malformed edits and reserved names fail with their index; a failing
    // edit after good ones writes nothing, in a dry run or not.
    let invalid = applied(&base, &[content("ok", "1"), content(".git/config", "x")])
        .await
        .unwrap_err();
    assert!(
        matches!(invalid, (1, EditProblem::InvalidPath(_))),
        "{invalid:?}"
    );
    let many = vec![content("a", "1"); MAX_EDITS + 1];
    assert!(matches!(
        files::apply(&base.tree(), &many, false).await,
        Err(EditError::TooMany(_))
    ));
    let dry = files::apply(&base.tree(), &[content("ok", "1")], false)
        .await
        .unwrap();
    assert!(dry.is_none());
}

// 5
#[tokio::test]
async fn edit_commit() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("f", Entry::File(b"1\n2\n3\n"))]).await;
    let mid = new_commit(&mut tx, &[&base], &[("m", Entry::File(b"m\n"))]).await;
    let child = new_commit(&mut tx, &[&mid], &[("f", Entry::File(b"1\nchild\n3\n"))]).await;
    tx.repo_mut()
        .set_wc_commit("w1".into(), mid.id().clone())
        .unwrap();
    tx.repo_mut()
        .set_wc_commit("w2".into(), child.id().clone())
        .unwrap();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("mid"),
        jj_lib::op_store::RefTarget::normal(mid.id().clone()),
    );
    fx.commit(tx).await;

    // Identical content writes nothing.
    let none = fx
        .repo
        .edit_commit(&hex(&mid), &[content("m", "m\n")], None, &meta())
        .await
        .unwrap();
    assert!(none.is_none());

    let edited = fx
        .repo
        .edit_commit(
            &hex(&mid),
            &[replace("f", "2", "two", false)],
            None,
            &meta(),
        )
        .await
        .unwrap()
        .unwrap();
    let new = fx.get(&edited.commit_id).await;
    assert_eq!(new.change_id(), mid.change_id());
    assert_eq!(new.description(), mid.description());
    assert_eq!(new.author().email, mid.author().email);
    assert_eq!(tree_bytes(&new.tree(), "f").await, b"1\ntwo\n3\n");
    // The descendant edited the same line: it is rebased and conflicted.
    let w2 = fx
        .head()
        .store()
        .get_commit_async(&fx.wc("w2"))
        .await
        .unwrap();
    assert_eq!(w2.change_id(), child.change_id());
    assert_eq!(w2.parent_ids(), [new.id().clone()]);
    assert!(w2.tree().has_conflict());
    assert_eq!(fx.wc("w1"), *new.id());
    assert_eq!(
        fx.head()
            .view()
            .get_local_bookmark(RefName::new("mid"))
            .as_normal(),
        Some(new.id())
    );
    let attributes = oplog::entry(fx.head().operation()).attributes;
    assert_eq!(attributes.get(ATTR_KIND).map(String::as_str), Some("edit"));
    assert_eq!(
        attributes.get(ATTR_WORKSPACE).map(String::as_str),
        Some("0123456789abcdef")
    );
    assert_eq!(
        attributes.get(ATTR_AGENT).map(String::as_str),
        Some("tester")
    );

    // A description set gains its newline.
    let described = fx
        .repo
        .edit_commit(&edited.commit_id, &[], Some("message"), &meta())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fx.get(&described.commit_id).await.description(),
        "message\n"
    );

    // On a conflicted commit, writing the conflicted path resolves it and
    // another conflicted path stays.
    let merged = conflicted(&mut fx).await;
    let resolved = fx
        .repo
        .edit_commit(&hex(&merged), &[content("f", "resolved\n")], None, &meta())
        .await
        .unwrap()
        .unwrap();
    let tree = fx.get(&resolved.commit_id).await.tree();
    assert_eq!(tree_bytes(&tree, "f").await, b"resolved\n");
    assert!(
        tree.path_value(RepoPath::from_internal_string("d").unwrap())
            .await
            .unwrap()
            .as_resolved()
            .is_none()
    );
}

// 6
#[tokio::test]
async fn new_change_on() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let trunk = new_commit(&mut tx, &[], &[("f", Entry::File(b"f\n"))]).await;
    tx.repo_mut()
        .set_wc_commit("w".into(), trunk.id().clone())
        .unwrap();
    fx.commit(tx).await;
    let made = fx
        .repo
        .new_change_on(&hex(&trunk), &[content("g", "g\n")], None, &meta())
        .await
        .unwrap();
    let new = fx.get(&made.commit_id).await;
    assert_ne!(new.change_id(), trunk.change_id());
    assert_eq!(new.parent_ids(), [trunk.id().clone()]);
    assert_eq!(new.description(), "");
    assert_eq!(tree_bytes(&new.tree(), "f").await, b"f\n");
    assert_eq!(tree_bytes(&new.tree(), "g").await, b"g\n");
    assert_eq!(fx.wc("w"), *trunk.id(), "no working copy moves");
    let empty = fx
        .repo
        .new_change_on(&hex(&trunk), &[], Some("empty"), &meta())
        .await
        .unwrap();
    let empty = fx.get(&empty.commit_id).await;
    assert_eq!(empty.tree_ids(), trunk.tree_ids());
    assert_eq!(empty.description(), "empty\n");

    // `buildable` allows the immutable root and refuses a hidden parent.
    let root = fx.head().store().root_commit_id().hex();
    let found = rewrite::buildable(&fx.head(), &root, None, &fx.scope())
        .await
        .unwrap();
    assert_eq!(found.workspaces, Vec::<String>::new());
    let found = rewrite::buildable(&fx.head(), &trunk.id().hex(), None, &fx.scope())
        .await
        .unwrap();
    assert_eq!(found.workspaces, ["w"]);
    let mut tx = fx.tx();
    tx.repo_mut().record_abandoned_commit(&new);
    fx.commit(tx).await;
    let error = rewrite::buildable(&fx.head(), &new.id().hex(), None, &fx.scope())
        .await
        .unwrap_err();
    assert!(matches!(error, RewriteError::Hidden(_)), "{error:?}");
}

// 7
#[tokio::test]
async fn edits_respect_store_mounts() {
    let mut fx = Fixture::new().await;
    let spec = "[[store]]\nname = \"deps\"\nlockfile = \"package-lock.json\"\nmount = \"node_modules\"\nbuild = [\"npm\", \"ci\"]\n";
    let mut tx = fx.tx();
    let base = new_commit(
        &mut tx,
        &[],
        &[
            ("reef.toml", Entry::File(spec.as_bytes())),
            ("package-lock.json", Entry::File(b"{}")),
        ],
    )
    .await;
    let without = new_commit(&mut tx, &[], &[("package-lock.json", Entry::File(b"{}"))]).await;
    fx.commit(tx).await;
    let (index, error) = applied(&base, &[content("ok", "1"), content("node_modules/x", "x")])
        .await
        .unwrap_err();
    assert_eq!(index, 1);
    assert!(
        matches!(&error, EditProblem::StoreMountPath { mount, .. } if mount == "node_modules"),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::Conflict);
    applied(&base, &[content("node_modules_x", "x")])
        .await
        .unwrap();
    // A store added in the same request protects its mount too.
    let (_, error) = applied(
        &without,
        &[content("node_modules/x", "x"), content("reef.toml", spec)],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, EditProblem::StoreMountPath { .. }),
        "{error:?}"
    );
    // An unparsable reef.toml afterwards falls back to the base tree's.
    let (_, error) = applied(
        &base,
        &[
            content("reef.toml", "not toml ["),
            content("node_modules/y", "y"),
        ],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, EditProblem::StoreMountPath { .. }),
        "{error:?}"
    );
}

// 8
#[tokio::test]
async fn edits_are_undoable() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let trunk = new_commit(&mut tx, &[], &[("f", Entry::File(b"f\n"))]).await;
    let change = new_commit(&mut tx, &[&trunk], &[("g", Entry::File(b"g\n"))]).await;
    fx.commit(tx).await;
    let undo = OpMeta::new(OpKind::Undo, "test: undo");

    let edited = fx
        .repo
        .edit_commit(&hex(&change), &[content("g", "h\n")], None, &meta())
        .await
        .unwrap()
        .unwrap();
    fx.repo
        .revert_operation(&edited.op_id, &undo)
        .await
        .unwrap();
    let view = fx.head();
    let heads: Vec<_> = view.view().heads().iter().cloned().collect();
    assert!(heads.contains(change.id()), "the rewrite is undone");

    let made = fx
        .repo
        .new_change_on(&hex(&trunk), &[content("n", "n\n")], None, &meta())
        .await
        .unwrap();
    fx.repo.revert_operation(&made.op_id, &undo).await.unwrap();
    assert!(
        !fx.head()
            .view()
            .heads()
            .contains(&commit_id(&made.commit_id)),
        "the new change is hidden"
    );

    // An edit rewritten again by a later edit is superseded.
    let first = fx
        .repo
        .edit_commit(&hex(&change), &[content("g", "1\n")], None, &meta())
        .await
        .unwrap()
        .unwrap();
    fx.repo
        .edit_commit(&first.commit_id, &[content("g", "2\n")], None, &meta())
        .await
        .unwrap()
        .unwrap();
    let error = fx
        .repo
        .revert_operation(&first.op_id, &undo)
        .await
        .unwrap_err();
    assert!(matches!(error, UndoError::Superseded { .. }), "{error:?}");

    // Both kinds classify as edits, with attributes and without.
    let entry = oplog::entry(fx.head().operation());
    assert_eq!(oplog::classify(&entry).kind, Some(OpKind::Edit));
    for description in [
        format!("{}{}", oplog::EDIT_PREFIX, change.id().hex()),
        format!("{}{}", oplog::NEW_CHANGE_PREFIX, trunk.id().hex()),
    ] {
        let edited = fx
            .repo
            .new_change_on(
                &hex(&trunk),
                &[],
                None,
                &OpMeta::new(OpKind::External, description),
            )
            .await
            .unwrap();
        let operation = oplog::resolve_operation(&fx.head(), edited.op_id.as_str())
            .await
            .unwrap();
        let entry = oplog::entry(&operation);
        assert!(!entry.attributes.contains_key(ATTR_KIND));
        assert_eq!(oplog::classify(&entry).kind, Some(OpKind::Edit));
    }
}
