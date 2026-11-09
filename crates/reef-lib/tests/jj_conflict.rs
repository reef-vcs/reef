//! Conflict objects (R3.25, ADR 0065): jj's stored conflicts read as Reef's
//! records, each term attributed to the commit it came from, and a file's
//! hunks. Conflicts are made with jj-lib's own rebase and merge, so their
//! labels are the ones jj writes.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::sync::Arc;

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::files::{FileMergeHunkLevel, MergeOptions};
use jj_lib::merge::SameChange;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::rewrite::{merge_commit_trees, rebase_commit};
use jj_lib::transaction::Transaction;
use reef_lib::jj::conflict::{
    self, Attribution, ConflictObject, FileHunks, FileKind, ReadOptions, Role, ValueType,
};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CommitIdHex, Repo};
use tempfile::TempDir;

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
            email: "reef-test@example.com".into(),
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

    /// `commit` written in a transaction of its own.
    async fn write(&mut self, parents: &[&Commit], entries: &[(&str, Entry<'_>)]) -> Commit {
        let mut tx = self.tx();
        let commit = new_commit(&mut tx, parents, entries, "").await;
        self.commit(tx).await;
        commit
    }

    /// `commit` rebased onto `onto`, as `jj rebase -r` would.
    async fn rebase(&mut self, commit: &Commit, onto: &[&Commit]) -> Commit {
        let mut tx = self.tx();
        let rebased = rebase_commit(
            tx.repo_mut(),
            commit.clone(),
            onto.iter().map(|c| c.id().clone()).collect(),
        )
        .await
        .unwrap();
        self.commit(tx).await;
        rebased
    }

    async fn read(&self, commit: &Commit) -> ConflictObject {
        conflict::read(&self.head(), &hex(commit), &all())
            .await
            .unwrap()
    }

    async fn sources(&self, commit: &Commit) -> Vec<Option<conflict::Source>> {
        let object = self.read(commit).await;
        conflict::attribute(&self.head(), &hex(commit), &object)
            .await
            .unwrap()
    }
}

fn all() -> ReadOptions {
    ReadOptions {
        limit: 1000,
        ..ReadOptions::default()
    }
}

fn path(text: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(text).unwrap()
}

fn hex(commit: &Commit) -> CommitIdHex {
    commit.id().hex().parse().unwrap()
}

enum Entry<'a> {
    File(&'a [u8]),
    Exec(&'a [u8]),
    Link(&'a str),
    Gone,
}

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
            Entry::File(bytes) | Entry::Exec(bytes) => Some(TreeValue::File {
                id: store.write_file(&path, &mut &bytes[..]).await.unwrap(),
                executable: matches!(entry, Entry::Exec(_)),
                copy_id: CopyId::placeholder(),
            }),
            Entry::Link(target) => Some(TreeValue::Symlink(
                store.write_symlink(&path, target).await.unwrap(),
            )),
            Entry::Gone => None,
        };
        builder.set_or_remove(path, value);
    }
    builder.write_tree().await.unwrap()
}

async fn new_commit(
    tx: &mut Transaction,
    parents: &[&Commit],
    entries: &[(&str, Entry<'_>)],
    description: &str,
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
        .set_description(description)
        .write()
        .await
        .unwrap()
}

/// A base, two sides that change one line of `f` from it, and the second
/// side rebased onto the first: `(base, left, right, rebased)`.
async fn rebase_conflict(fx: &mut Fixture) -> (Commit, Commit, Commit, Commit) {
    let base = fx
        .write(
            &[],
            &[
                ("f", Entry::File(b"a\nb\nc\nd\ne\n")),
                ("g", Entry::File(b"g\n")),
            ],
        )
        .await;
    let left = fx
        .write(&[&base], &[("f", Entry::File(b"a\nLEFT\nc\nd\ne\n"))])
        .await;
    let right = fx
        .write(&[&base], &[("f", Entry::File(b"a\nRIGHT\nc\nd\nE\n"))])
        .await;
    let rebased = fx.rebase(&right, &[&left]).await;
    assert!(rebased.tree().has_conflict());
    (base, left, right, rebased)
}

fn merge_options() -> MergeOptions {
    MergeOptions {
        hunk_level: FileMergeHunkLevel::Line,
        same_change: SameChange::Accept,
    }
}

// 1
#[tokio::test]
async fn read_reports_terms_files_and_labels() {
    let mut fx = Fixture::new().await;
    let (_, _, _, rebased) = rebase_conflict(&mut fx).await;
    let object = fx.read(&rebased).await;
    assert_eq!(object.arity, 2);
    assert_eq!(object.terms.len(), 3);
    let roles: Vec<Role> = object.terms.iter().map(|t| t.role).collect();
    assert_eq!(roles, [Role::Side, Role::Base, Role::Side]);
    let origins: Vec<&str> = object.terms.iter().map(|t| t.origin.as_str()).collect();
    assert_eq!(
        origins,
        [
            "rebase destination",
            "parents of rebased revision",
            "rebased revision"
        ]
    );
    assert_eq!(object.files_total, 1);
    assert_eq!(object.introduced, 1);
    let file = &object.files[0];
    assert_eq!(file.path, "f");
    assert_eq!(file.kind, FileKind::Content);
    assert!(!file.inherited);
    assert_eq!(file.terms.len(), 3, "per-path arity is the tree's");
    assert!(file.terms.iter().all(|t| t.kind == ValueType::File));
    assert!(object.id.starts_with("cfl_") && object.id.len() == 36);
    assert!(!object.too_wide);
}

// 1: each file kind, once each, and a file against a directory reported
// once.
#[tokio::test]
async fn read_reports_each_file_kind() {
    let mut fx = Fixture::new().await;
    let base = fx
        .write(
            &[],
            &[
                ("content", Entry::File(b"base\n")),
                ("exec", Entry::File(b"same\n")),
                ("deleted", Entry::File(b"base\n")),
                ("typed", Entry::File(b"base\n")),
            ],
        )
        .await;
    let left = fx
        .write(
            &[&base],
            &[
                ("content", Entry::File(b"left\n")),
                ("exec", Entry::Exec(b"same\n")),
                ("deleted", Entry::Gone),
                ("added", Entry::File(b"left\n")),
                ("typed", Entry::Link("target")),
                ("dir", Entry::File(b"file\n")),
            ],
        )
        .await;
    let right = fx
        .write(
            &[&base],
            &[
                ("content", Entry::File(b"right\n")),
                ("exec", Entry::File(b"changed\n")),
                ("deleted", Entry::File(b"right\n")),
                ("added", Entry::File(b"right\n")),
                ("typed", Entry::File(b"right\n")),
                ("dir/x", Entry::File(b"x\n")),
            ],
        )
        .await;
    let merged = fx.write(&[&left, &right], &[]).await;
    let object = fx.read(&merged).await;
    let kinds: Vec<(&str, FileKind)> = object
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("added", FileKind::AddAdd),
            ("content", FileKind::Content),
            ("deleted", FileKind::ModifyDelete),
            ("dir", FileKind::Type),
            ("typed", FileKind::Type),
        ]
    );
    // One side sets the bit and the other changes the content: that merges
    // cleanly (`exec` is not listed); against a deletion it conflicts.
    let base = fx.write(&[], &[("bit", Entry::File(b"x\n"))]).await;
    let one = fx.write(&[&base], &[("bit", Entry::Exec(b"x\n"))]).await;
    let two = fx.write(&[&base], &[("bit", Entry::Gone)]).await;
    let merged = fx.write(&[&one, &two], &[]).await;
    assert_eq!(fx.read(&merged).await.files[0].kind, FileKind::ModifyDelete);
}

// 1: a merge of a conflicted commit and a third side is three-sided.
#[tokio::test]
async fn three_sided_conflicts_report_five_terms() {
    let mut fx = Fixture::new().await;
    let (base, _, _, rebased) = rebase_conflict(&mut fx).await;
    let third = fx
        .write(&[&base], &[("f", Entry::File(b"a\nTHIRD\nc\nd\ne\n"))])
        .await;
    let again = fx.rebase(&third, &[&rebased]).await;
    let object = fx.read(&again).await;
    assert_eq!(object.arity, 3);
    assert_eq!(object.terms.len(), 5);
    assert_eq!(object.files[0].terms.len(), 5);
}

// 2
#[tokio::test]
async fn ids_follow_the_conflicted_content() {
    let mut fx = Fixture::new().await;
    let (_, _, _, rebased) = rebase_conflict(&mut fx).await;
    let first = fx.read(&rebased).await;
    // An edit to another path keeps both ids.
    let mut tx = fx.tx();
    let tree = tree_with(&tx, rebased.tree(), &[("g", Entry::File(b"edited\n"))]).await;
    let edited = tx
        .repo_mut()
        .rewrite_commit(&rebased)
        .set_tree(tree)
        .set_description("described")
        .write()
        .await
        .unwrap();
    fx.commit(tx).await;
    let second = fx.read(&edited).await;
    assert_eq!(first.files[0].id, second.files[0].id);
    assert_eq!(first.id, second.id);
    // A child inherits the files' ids, not the conflict's.
    let child = fx.write(&[&edited], &[("h", Entry::File(b"h\n"))]).await;
    let inherited = fx.read(&child).await;
    assert_eq!(inherited.files[0].id, first.files[0].id);
    assert_ne!(inherited.id, first.id);
    assert!(inherited.files[0].inherited);
    // Changing a side changes both.
    let other = fx
        .write(
            &[&rebased.parents().await.unwrap()[0].parents().await.unwrap()[0]],
            &[("f", Entry::File(b"a\nOTHER\nc\nd\ne\n"))],
        )
        .await;
    let moved = fx.rebase(&edited, &[&other]).await;
    let third = fx.read(&moved).await;
    assert_ne!(third.files[0].id, first.files[0].id);
    assert_ne!(third.id, first.id);
}

// 3 (S-127, S-128)
#[tokio::test]
async fn sources_are_verified_by_content() {
    let mut fx = Fixture::new().await;
    let (base, left, right, rebased) = rebase_conflict(&mut fx).await;
    let sources = fx.sources(&rebased).await;
    let found: Vec<(String, Attribution)> = sources
        .iter()
        .map(|s| {
            let s = s.as_ref().expect("a source");
            (s.commit_id.to_string(), s.attribution)
        })
        .collect();
    assert_eq!(
        found,
        [
            (left.id().hex(), Attribution::Verified),
            (base.id().hex(), Attribution::Verified),
            (right.id().hex(), Attribution::Verified),
        ]
    );
    // A merge's sides verify to its parents.
    let other = fx
        .write(&[&base], &[("f", Entry::File(b"a\nOTHER\nc\nd\ne\n"))])
        .await;
    let merged = fx.write(&[&left, &other], &[]).await;
    let sources = fx.sources(&merged).await;
    assert_eq!(
        sources[0].as_ref().unwrap().commit_id.as_str(),
        left.id().hex()
    );
    assert_eq!(
        sources[2].as_ref().unwrap().commit_id.as_str(),
        other.id().hex()
    );
}

// 3 (S-127): a label naming an unrelated commit is declared, not verified;
// one naming nothing is none; an unlabeled conflict still verifies.
#[tokio::test]
async fn forged_labels_do_not_verify() {
    let mut fx = Fixture::new().await;
    let (_, left, _, rebased) = rebase_conflict(&mut fx).await;
    let unrelated = fx.write(&[], &[("elsewhere", Entry::File(b"x\n"))]).await;
    let short = |c: &Commit| {
        format!(
            "{} {}",
            &c.change_id().reverse_hex()[..8],
            &c.id().hex()[..8]
        )
    };
    let labels = vec![
        format!("{} \"forged\" (rebase destination)", short(&unrelated)),
        "kkkkkkkk 0badc0de (parents of rebased revision)".to_owned(),
        String::new(),
    ];
    let mut tx = fx.tx();
    let tree = MergedTree::new(
        tx.repo().store().clone(),
        rebased.tree_ids().clone(),
        jj_lib::conflict_labels::ConflictLabels::from_vec(labels),
    );
    // A sibling holding the same conflict with forged labels, on another
    // parent so the parents are no help.
    let forged = tx
        .repo_mut()
        .new_commit(vec![unrelated.id().clone()], tree)
        .write()
        .await
        .unwrap();
    fx.commit(tx).await;
    let sources = fx.sources(&forged).await;
    let side0 = sources[0].as_ref().unwrap();
    assert_eq!(side0.commit_id.as_str(), unrelated.id().hex());
    assert_eq!(side0.attribution, Attribution::Label);
    assert!(sources[1].is_none(), "{:?}", sources[1]);
    assert!(sources[2].is_none(), "{:?}", sources[2]);
    let _ = left;
}

// 4 (S-129)
#[tokio::test]
async fn wide_conflicts_carry_paths_only() {
    let mut fx = Fixture::new().await;
    let base = fx.write(&[], &[("f", Entry::File(b"base\n"))]).await;
    let mut sides = Vec::new();
    for n in 0..33 {
        let text = format!("side {n}\n");
        sides.push(
            fx.write(&[&base], &[("f", Entry::File(text.as_bytes()))])
                .await,
        );
    }
    let parents: Vec<&Commit> = sides.iter().collect();
    let merged = fx.write(&parents, &[]).await;
    let object = fx.read(&merged).await;
    assert_eq!(object.arity, 33);
    assert!(object.too_wide);
    assert_eq!(object.terms, Vec::new());
    assert_eq!(object.files.len(), 1);
    assert_eq!(object.files[0].terms, Vec::new());
    let sources = conflict::attribute(&fx.head(), &hex(&merged), &object)
        .await
        .unwrap();
    assert_eq!(sources, Vec::new());
}

// 5
#[tokio::test]
async fn children_inherit_and_edits_introduce() {
    let mut fx = Fixture::new().await;
    let (base, _, _, rebased) = rebase_conflict(&mut fx).await;
    let _ = base;
    let child = fx.write(&[&rebased], &[("h", Entry::File(b"h\n"))]).await;
    let object = fx.read(&child).await;
    assert_eq!(object.introduced, 0);
    assert!(object.files.iter().all(|f| f.inherited));
}

// 6
#[tokio::test]
async fn hunks_rebuild_each_side() {
    let mut fx = Fixture::new().await;
    let (_, _, _, rebased) = rebase_conflict(&mut fx).await;
    let found = conflict::hunks(&rebased.tree(), "f", &merge_options(), 1 << 20)
        .await
        .unwrap();
    let FileHunks::Hunks(hunks) = found else {
        panic!("not hunks: {found:?}");
    };
    let conflicted: Vec<_> = hunks.iter().filter(|h| !h.resolved).collect();
    assert_eq!(conflicted.len(), 1, "{hunks:?}");
    let terms = &conflicted[0].terms;
    assert_eq!(terms.len(), 3);
    assert_eq!(terms[0].text, "LEFT\n");
    assert_eq!(terms[1].text, "b\n");
    assert_eq!(terms[2].text, "RIGHT\n");
    assert!(terms.iter().all(|t| t.start_line == 2 && t.line_count == 1));
    // Each term's file, rebuilt from the hunks, is that term with the
    // other terms' non-conflicting changes applied (what jj's markers show).
    let expected = [
        "a\nLEFT\nc\nd\nE\n",
        "a\nb\nc\nd\nE\n",
        "a\nRIGHT\nc\nd\nE\n",
    ];
    for (position, expected) in expected.iter().enumerate() {
        let mut text = String::new();
        for hunk in &hunks {
            if hunk.resolved {
                text.push_str(&hunk.text);
            } else {
                assert_eq!(hunk.terms[position].index, u32::try_from(position).unwrap());
                text.push_str(&hunk.terms[position].text);
            }
        }
        assert_eq!(&text, expected);
    }
    // A NUL is binary; a term over the limit is too large.
    let base = fx.write(&[], &[("b", Entry::File(b"x\0\n"))]).await;
    let one = fx.write(&[&base], &[("b", Entry::File(b"y\0\n"))]).await;
    let two = fx.write(&[&base], &[("b", Entry::File(b"z\0\n"))]).await;
    let merged = fx.write(&[&one, &two], &[]).await;
    let found = conflict::hunks(&merged.tree(), "b", &merge_options(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(found, FileHunks::Binary);
    let found = conflict::hunks(&rebased.tree(), "f", &merge_options(), 4)
        .await
        .unwrap();
    assert_eq!(found, FileHunks::TooLarge);
}

// 3: an unlabeled conflict still verifies through the commit's parents,
// and a term no candidate holds has no source.
#[tokio::test]
async fn unlabeled_conflicts_verify_through_parents() {
    let mut fx = Fixture::new().await;
    let (_, left, _, rebased) = rebase_conflict(&mut fx).await;
    let mut tx = fx.tx();
    let tree = MergedTree::new(
        tx.repo().store().clone(),
        rebased.tree_ids().clone(),
        jj_lib::conflict_labels::ConflictLabels::unlabeled(),
    );
    let unlabeled = tx
        .repo_mut()
        .new_commit(vec![left.id().clone()], tree)
        .write()
        .await
        .unwrap();
    fx.commit(tx).await;
    let object = fx.read(&unlabeled).await;
    assert!(
        object
            .terms
            .iter()
            .all(|t| t.label.is_empty() && t.origin.is_empty())
    );
    let sources = fx.sources(&unlabeled).await;
    let side0 = sources[0].as_ref().unwrap();
    assert_eq!(side0.commit_id.as_str(), left.id().hex());
    assert_eq!(side0.attribution, Attribution::Verified);
    assert!(sources[2].is_none());
}

/// A commit holding `tree_of`'s conflict with `labels`, on `parent`.
async fn relabeled(
    fx: &mut Fixture,
    tree_of: &Commit,
    labels: Vec<String>,
    parent: &Commit,
) -> Commit {
    let mut tx = fx.tx();
    let tree = MergedTree::new(
        tx.repo().store().clone(),
        tree_of.tree_ids().clone(),
        jj_lib::conflict_labels::ConflictLabels::from_vec(labels),
    );
    let commit = tx
        .repo_mut()
        .new_commit(vec![parent.id().clone()], tree)
        .write()
        .await
        .unwrap();
    fx.commit(tx).await;
    commit
}

fn short_label(commit: &Commit, origin: &str) -> String {
    format!(
        "{} {} \"x\" ({origin})",
        &commit.change_id().reverse_hex()[..8],
        &commit.id().hex()[..8]
    )
}

// 3 (S-127): a label naming an unlinked commit that holds the same content
// is declared, not verified; neither is a deleting side matched by any
// commit that lacks the file.
#[tokio::test]
async fn unlinked_and_absent_matches_do_not_verify() {
    let mut fx = Fixture::new().await;
    let (_, left, _, rebased) = rebase_conflict(&mut fx).await;
    // A copy of `left`'s content on an unrelated branch.
    let copy = fx
        .write(&[], &[("f", Entry::File(b"a\nLEFT\nc\nd\ne\n"))])
        .await;
    let other = fx.write(&[], &[("unrelated", Entry::File(b"u\n"))]).await;
    let forged = relabeled(
        &mut fx,
        &rebased,
        vec![
            short_label(&copy, "rebase destination"),
            String::new(),
            String::new(),
        ],
        &other,
    )
    .await;
    let sources = fx.sources(&forged).await;
    let side0 = sources[0].as_ref().unwrap();
    assert_eq!(side0.commit_id.as_str(), copy.id().hex());
    assert_eq!(side0.attribution, Attribution::Label);
    let _ = left;

    // Modify/delete: the deleting side's label names a commit without the
    // file; absence everywhere verifies nothing.
    let base = fx.write(&[], &[("m", Entry::File(b"base\n"))]).await;
    let keep = fx.write(&[&base], &[("m", Entry::File(b"kept\n"))]).await;
    let gone = fx.write(&[&base], &[("m", Entry::Gone)]).await;
    let merged = fx.write(&[&keep, &gone], &[]).await;
    let stranger = fx.write(&[], &[("s", Entry::File(b"s\n"))]).await;
    let forged = relabeled(
        &mut fx,
        &merged,
        vec![
            String::new(),
            String::new(),
            short_label(&stranger, "squash destination"),
        ],
        &stranger,
    )
    .await;
    let sources = fx.sources(&forged).await;
    let side1 = sources[2].as_ref().unwrap();
    assert_eq!(side1.commit_id.as_str(), stranger.id().hex());
    assert_eq!(side1.attribution, Attribution::Label, "{side1:?}");
}

// Paging follows jj's path order, where `/` sorts first: `a/b` before
// `a.txt`.
#[tokio::test]
async fn pages_follow_jj_path_order() {
    let mut fx = Fixture::new().await;
    let base = fx
        .write(
            &[],
            &[("a/b", Entry::File(b"0\n")), ("a.txt", Entry::File(b"0\n"))],
        )
        .await;
    let one = fx
        .write(
            &[&base],
            &[("a/b", Entry::File(b"1\n")), ("a.txt", Entry::File(b"1\n"))],
        )
        .await;
    let two = fx
        .write(
            &[&base],
            &[("a/b", Entry::File(b"2\n")), ("a.txt", Entry::File(b"2\n"))],
        )
        .await;
    let merged = fx.write(&[&one, &two], &[]).await;
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = conflict::read(
            &fx.head(),
            &hex(&merged),
            &ReadOptions {
                after: after.clone(),
                limit: 1,
                ..ReadOptions::default()
            },
        )
        .await
        .unwrap();
        let Some(file) = page.files.first() else {
            break;
        };
        seen.push(file.path.clone());
        after = Some(file.path.clone());
        if !page.more {
            break;
        }
    }
    assert_eq!(seen, ["a/b", "a.txt"]);
}
