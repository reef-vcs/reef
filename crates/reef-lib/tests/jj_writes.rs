//! The write side over jj-lib (R2.03, ADR 0031): the `reef.*` attributes
//! every write records, describing and abandoning commits (resolving a
//! divergent change), and reverting one operation with its refusals. Each
//! test builds its repository with jj-lib transactions on an internal git
//! store in its own temporary directory.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, WorkspaceName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::effects::{self, ChangeEffectKind};
use reef_lib::jj::oplog::{self, ATTR_AGENT, ATTR_KIND, ATTR_SNAPSHOT_KIND, ATTR_WORKSPACE};
use reef_lib::jj::revset::RevsetScope;
use reef_lib::jj::rewrite::{self, RewriteError};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::undo::{self, UndoError};
use reef_lib::jj::{CommitIdHex, OpKind, OpMeta, OperationIdHex, Repo, StartPoint};
use tempfile::TempDir;

const EMAIL: &str = "reef-test@example.com";

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: EMAIL.into(),
    })
    .expect("settings")
}

/// A repository at `<tmp>/main`, with the user's `default` workspace.
struct Fixture {
    dir: TempDir,
    repo: Repo,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main");
        std::fs::create_dir(&root).expect("create main");
        let repo = Repo::init_internal_git(&root, &settings())
            .await
            .expect("init repo");
        Self { dir, repo }
    }

    fn head(&self) -> Arc<ReadonlyRepo> {
        self.repo.readonly()
    }

    fn tx(&self) -> Transaction {
        self.repo.head().start_transaction()
    }

    async fn commit(&mut self, mut tx: Transaction, description: &str) -> OperationIdHex {
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit(description).await.unwrap();
        self.repo.set_head(repo);
        self.op()
    }

    fn op(&self) -> OperationIdHex {
        self.repo.op_id()
    }

    fn root(&self) -> &Path {
        self.repo.workspace_root()
    }

    fn scope(&self) -> RevsetScope<'_> {
        RevsetScope {
            workspace: None,
            user_email: EMAIL,
            root: self.root(),
        }
    }

    async fn get(&self, id: &CommitIdHex) -> Commit {
        self.head()
            .store()
            .get_commit_async(&CommitId::try_from_hex(id.as_str()).unwrap())
            .await
            .unwrap()
    }

    fn visible(&self, id: &CommitId) -> bool {
        let head = self.head();
        let index = head.index();
        head.view()
            .heads()
            .iter()
            .any(|head| pollster::block_on(index.is_ancestor(id, head)).unwrap())
    }

    fn wc(&self, name: &str) -> Option<CommitId> {
        self.head()
            .view()
            .get_wc_commit_id(WorkspaceName::new(name))
            .cloned()
    }

    fn local_bookmark(&self, name: &str) -> RefTarget {
        self.head()
            .view()
            .get_local_bookmark(RefName::new(name))
            .clone()
    }

    async fn writable(&self, commit: &Commit) -> Result<rewrite::Writable, RewriteError> {
        rewrite::writable(&self.head(), &commit.id().hex(), None, &self.scope()).await
    }

    async fn describe(&mut self, commit: &Commit, text: &str) -> Option<rewrite::Described> {
        self.repo
            .describe_commit(&hex(commit), text, &meta(OpKind::Describe))
            .await
            .unwrap()
    }

    async fn abandon(&mut self, commit: &Commit) -> rewrite::Abandoned {
        self.repo
            .abandon_commit(&hex(commit), &meta(OpKind::Abandon))
            .await
            .unwrap()
    }

    async fn revert(&mut self, op: &OperationIdHex) -> Result<Option<OperationIdHex>, UndoError> {
        self.repo.revert_operation(op, &meta(OpKind::Undo)).await
    }

    fn attributes(&self) -> std::collections::BTreeMap<String, String> {
        oplog::entry(self.head().operation()).attributes
    }
}

fn meta(kind: OpKind) -> OpMeta {
    OpMeta::new(kind, format!("test: {}", kind.as_str()))
        .workspace("0123456789abcdef")
        .agent("tester")
}

/// A new commit on `parents` (the root commit when empty) whose tree holds
/// `files`.
async fn new_commit(
    tx: &mut Transaction,
    parents: &[&Commit],
    files: &[(&str, &str)],
    description: &str,
) -> Commit {
    let parents: Vec<CommitId> = if parents.is_empty() {
        vec![tx.repo().store().root_commit_id().clone()]
    } else {
        parents.iter().map(|commit| commit.id().clone()).collect()
    };
    let tree = tree(tx, files).await;
    tx.repo_mut()
        .new_commit(parents, tree)
        .set_description(description)
        .write()
        .await
        .unwrap()
}

async fn tree(tx: &Transaction, files: &[(&str, &str)]) -> jj_lib::merged_tree::MergedTree {
    let store = tx.repo().store();
    let mut builder =
        jj_lib::merged_tree_builder::MergedTreeBuilder::new(store.empty_merged_tree());
    for (path, contents) in files {
        let path = jj_lib::repo_path::RepoPathBuf::from_internal_string(*path).unwrap();
        let id = store
            .write_file(&path, &mut contents.as_bytes())
            .await
            .unwrap();
        builder.set_or_remove(
            path,
            jj_lib::merge::Merge::normal(jj_lib::backend::TreeValue::File {
                id,
                executable: false,
                copy_id: jj_lib::backend::CopyId::placeholder(),
            }),
        );
    }
    builder.write_tree().await.unwrap()
}

/// A second commit of `change` on `parent`, with `files`.
async fn same_change(
    tx: &mut Transaction,
    change: &ChangeId,
    parent: &Commit,
    files: &[(&str, &str)],
    description: &str,
) -> Commit {
    let tree = tree(tx, files).await;
    tx.repo_mut()
        .new_commit(vec![parent.id().clone()], tree)
        .set_change_id(change.clone())
        .set_description(description)
        .write()
        .await
        .unwrap()
}

fn bookmark(tx: &mut Transaction, name: &str, commit: &Commit) {
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new(name), RefTarget::normal(commit.id().clone()));
}

fn set_wc(tx: &mut Transaction, name: &str, commit: &Commit) {
    tx.repo_mut()
        .set_wc_commit(name.into(), commit.id().clone())
        .unwrap();
}

fn hex(commit: &Commit) -> CommitIdHex {
    commit.id().hex().parse().unwrap()
}

fn id(hex: &CommitIdHex) -> CommitId {
    CommitId::try_from_hex(hex.as_str()).unwrap()
}

// 1
#[tokio::test]
async fn writes_are_tagged() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("a", "1")], "base").await;
    let other = new_commit(&mut tx, &[], &[("b", "1")], "other").await;
    fx.commit(tx, "setup").await;
    // GC below drops the operations before this and rewrites the rest.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let cut = SystemTime::now();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let expect = |attributes: &std::collections::BTreeMap<String, String>, kind: OpKind| {
        assert_eq!(
            attributes.get(ATTR_KIND).map(String::as_str),
            Some(kind.as_str())
        );
        assert_eq!(
            attributes.get(ATTR_WORKSPACE).map(String::as_str),
            Some("0123456789abcdef")
        );
        assert_eq!(
            attributes.get(ATTR_AGENT).map(String::as_str),
            Some("tester")
        );
    };

    let (root, state) = (fx.dir.path().join("ws-x"), fx.dir.path().join("wc-x"));
    std::fs::create_dir(&root).unwrap();
    let (ws, _) = fx
        .repo
        .create_workspace(
            "reef-x",
            &root,
            &state,
            StartPoint::NewOn(hex(&base)),
            &meta(OpKind::CreateWorkspace),
        )
        .await
        .unwrap();
    drop(ws);
    expect(&fx.attributes(), OpKind::CreateWorkspace);

    let moved = fx
        .repo
        .move_pointer(
            "reef-x",
            StartPoint::Edit(hex(&other)),
            &meta(OpKind::MoveWorkspace),
        )
        .await
        .unwrap();
    expect(&fx.attributes(), OpKind::MoveWorkspace);
    fx.repo
        .restore_pointer("reef-x", &moved.from, true, &meta(OpKind::MoveWorkspace))
        .await
        .unwrap();
    expect(&fx.attributes(), OpKind::MoveWorkspace);

    fx.describe(&base, "described").await.unwrap();
    expect(&fx.attributes(), OpKind::Describe);
    let class = oplog::classify(&oplog::entry(fx.head().operation()));
    assert_eq!(class.kind, Some(OpKind::Describe));
    assert_eq!(class.workspace_id.as_deref(), Some("0123456789abcdef"));
    let described = fx.op();

    fx.abandon(&other).await;
    expect(&fx.attributes(), OpKind::Abandon);
    fx.revert(&described).await.unwrap().unwrap();
    expect(&fx.attributes(), OpKind::Undo);
    let abandon_op = fx.repo.op_log(3).await.unwrap()[1].id.clone();
    fx.revert(&abandon_op).await.unwrap().unwrap();
    expect(&fx.attributes(), OpKind::Undo);

    fx.repo
        .forget_workspace("reef-x", &meta(OpKind::ReleaseWorkspace))
        .await
        .unwrap();
    expect(&fx.attributes(), OpKind::ReleaseWorkspace);

    // Empty values are left out; a snapshot kind is written.
    let sparse = OpMeta::new(OpKind::Snapshot, "sparse")
        .snapshot_kind("explicit")
        .workspace("")
        .agent("");
    let lone = new_commit_op(&mut fx, &sparse).await;
    let attributes = oplog::entry(&lone).attributes;
    assert_eq!(
        attributes.get(ATTR_KIND).map(String::as_str),
        Some("snapshot")
    );
    assert_eq!(
        attributes.get(ATTR_SNAPSHOT_KIND).map(String::as_str),
        Some("explicit")
    );
    assert!(!attributes.contains_key(ATTR_WORKSPACE));
    assert!(!attributes.contains_key(ATTR_AGENT));
    // External writes nothing.
    let external = new_commit_op(&mut fx, &OpMeta::new(OpKind::External, "outside")).await;
    assert!(oplog::entry(&external).attributes.is_empty());

    // GC rewrites the log and keeps the attributes.
    let before = fx.op();
    fx.repo.gc(cut).await.unwrap();
    assert_ne!(fx.op(), before, "GC rewrote the log");
    let entries = fx.repo.op_log(50).await.unwrap();
    let undo = entries
        .iter()
        .find(|entry| entry.attributes.get(ATTR_KIND).map(String::as_str) == Some("undo"))
        .expect("the undo survives GC");
    assert_eq!(
        undo.attributes.get(ATTR_AGENT).map(String::as_str),
        Some("tester")
    );
}

/// One operation recorded with `meta`: a describe of a fresh commit, so
/// the public API writes it.
async fn new_commit_op(fx: &mut Fixture, meta: &OpMeta) -> jj_lib::operation::Operation {
    let mut tx = fx.tx();
    let commit = new_commit(&mut tx, &[], &[("e", fx.op().as_str())], "fresh").await;
    fx.commit(tx, "fresh commit").await;
    fx.repo
        .describe_commit(&hex(&commit), "fresh, described", meta)
        .await
        .unwrap()
        .unwrap();
    fx.head().operation().clone()
}

// 2
#[tokio::test]
async fn describe_commit() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let a = new_commit(&mut tx, &[], &[("a", "1")], "a").await;
    let b = new_commit(&mut tx, &[&a], &[("a", "1"), ("b", "1")], "b").await;
    let c = new_commit(&mut tx, &[&b], &[("a", "1"), ("b", "1"), ("c", "1")], "c").await;
    bookmark(&mut tx, "bm", &b);
    set_wc(&mut tx, "reef-1", &b);
    set_wc(&mut tx, "reef-2", &b);
    set_wc(&mut tx, "reef-3", &c);
    fx.commit(tx, "setup").await;

    let writable = fx.writable(&b).await.unwrap();
    assert_eq!(writable.workspaces, ["reef-1", "reef-2", "reef-3"]);
    assert!(!writable.divergent);

    let described = fx.describe(&b, "msg").await.unwrap();
    let new_b = fx.get(&described.commit_id).await;
    assert_ne!(new_b.id(), b.id());
    assert_eq!(new_b.change_id(), b.change_id());
    assert_eq!(new_b.tree_ids(), b.tree_ids());
    assert_eq!(new_b.description(), "msg\n");
    assert_eq!(new_b.author(), b.author());
    assert!(new_b.committer().timestamp.timestamp >= b.committer().timestamp.timestamp);
    assert_eq!(
        fx.local_bookmark("bm"),
        RefTarget::normal(new_b.id().clone())
    );
    assert_eq!(fx.wc("reef-1").as_ref(), Some(new_b.id()));
    assert_eq!(fx.wc("reef-2").as_ref(), Some(new_b.id()));
    let new_c = fx
        .get(&fx.wc("reef-3").unwrap().hex().parse().unwrap())
        .await;
    assert_eq!(new_c.change_id(), c.change_id());
    assert_eq!(new_c.parent_ids(), [new_b.id().clone()]);
    assert_eq!(new_c.tree_ids(), c.tree_ids());

    // Unchanged text writes nothing; empty stays empty.
    let op = fx.op();
    assert!(fx.describe(&new_b, "msg\n").await.is_none());
    assert_eq!(fx.op(), op);
    let emptied = fx.describe(&new_b, "").await.unwrap();
    assert_eq!(fx.get(&emptied.commit_id).await.description(), "");
    let nul = fx
        .repo
        .describe_commit(&emptied.commit_id, "a\0b", &meta(OpKind::Describe))
        .await
        .unwrap_err();
    assert!(matches!(nul, RewriteError::InvalidDescription));
    assert_eq!(nul.kind(), ErrorKind::InvalidArgument);
}

// 3
#[tokio::test]
async fn abandon_commit_plain() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let a = new_commit(&mut tx, &[], &[("a", "1")], "a").await;
    let b = new_commit(&mut tx, &[&a], &[("a", "1"), ("b", "1")], "b").await;
    let c = new_commit(&mut tx, &[&b], &[("a", "1"), ("b", "1"), ("c", "1")], "c").await;
    let leaf = new_commit(&mut tx, &[&a], &[("a", "2")], "leaf").await;
    bookmark(&mut tx, "local", &leaf);
    tx.repo_mut().set_remote_bookmark(
        RefName::new("remote").to_remote_symbol(RemoteName::new("origin")),
        RemoteRef {
            target: RefTarget::normal(leaf.id().clone()),
            state: RemoteRefState::New,
        },
    );
    set_wc(&mut tx, "reef-1", &b);
    set_wc(&mut tx, "reef-2", &b);
    fx.commit(tx, "setup").await;

    let abandoned = fx.abandon(&leaf).await;
    assert_eq!(abandoned.successor, None);
    assert!(!fx.visible(leaf.id()));
    assert!(fx.local_bookmark("local").is_absent());
    let remote = fx
        .head()
        .view()
        .get_remote_bookmark(RefName::new("remote").to_remote_symbol(RemoteName::new("origin")))
        .target
        .clone();
    assert_eq!(remote, RefTarget::normal(leaf.id().clone()));
    let effects = effects::operation_effects(&fx.head(), fx.head().operation(), 100)
        .await
        .unwrap();
    assert!(
        effects
            .changes
            .iter()
            .any(|change| change.kind == ChangeEffectKind::Abandoned
                && change.previous_commit_id == Some(hex(&leaf)))
    );

    // Mid-stack: descendants move onto its parents; working copies share
    // one new empty commit on them.
    fx.abandon(&b).await;
    let wc1 = fx.wc("reef-1").unwrap();
    assert_eq!(fx.wc("reef-2").as_ref(), Some(&wc1));
    let wc1 = fx.get(&wc1.hex().parse().unwrap()).await;
    assert_eq!(wc1.parent_ids(), [a.id().clone()]);
    assert_ne!(wc1.change_id(), b.change_id());
    let rebased = fx
        .head()
        .view()
        .heads()
        .iter()
        .map(|id| pollster::block_on(fx.head().store().get_commit_async(id)).unwrap())
        .find(|commit| commit.change_id() == c.change_id())
        .unwrap();
    assert_eq!(rebased.parent_ids(), [a.id().clone()]);
}

// 4
#[tokio::test]
async fn abandon_commit_resolves_divergence() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("f", "base")], "base").await;
    let ours = new_commit(&mut tx, &[&base], &[("f", "ours")], "work").await;
    let theirs = same_change(&mut tx, ours.change_id(), &base, &[("f", "theirs")], "work").await;
    let child = new_commit(
        &mut tx,
        &[&theirs],
        &[("f", "theirs"), ("g", "child")],
        "child",
    )
    .await;
    bookmark(&mut tx, "bm", &theirs);
    set_wc(&mut tx, "reef-a", &ours);
    set_wc(&mut tx, "reef-b", &theirs);
    fx.commit(tx, "setup").await;

    let writable = fx.writable(&theirs).await.unwrap();
    assert!(writable.divergent);
    assert_eq!(writable.workspaces, ["reef-b"]);

    let abandoned = fx.abandon(&theirs).await;
    assert_eq!(abandoned.successor, Some(hex(&ours)));
    assert!(!fx.visible(theirs.id()));
    assert!(fx.visible(ours.id()));
    let info = reef_lib::jj::log::Describer::new(&fx.head(), &fx.scope())
        .unwrap()
        .describe(&hex(&ours))
        .await
        .unwrap();
    assert!(!info.divergent, "one visible commit left");
    let effects = effects::operation_effects(&fx.head(), fx.head().operation(), 100)
        .await
        .unwrap();
    assert!(
        effects
            .changes
            .iter()
            .any(|change| change.kind == ChangeEffectKind::Hidden
                && change.previous_commit_id == Some(hex(&theirs))),
        "{effects:?}"
    );
    assert_eq!(
        fx.local_bookmark("bm"),
        RefTarget::normal(ours.id().clone())
    );
    assert_eq!(fx.wc("reef-b").as_ref(), Some(ours.id()));
    let rebased = fx
        .head()
        .view()
        .heads()
        .iter()
        .map(|id| pollster::block_on(fx.head().store().get_commit_async(id)).unwrap())
        .find(|commit| commit.change_id() == child.change_id())
        .unwrap();
    assert_eq!(rebased.parent_ids(), [ours.id().clone()]);

    // Three visible commits: the newest is the successor; one that descends
    // from the abandoned commit is skipped.
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("f", "base")], "base").await;
    let first = new_commit(&mut tx, &[&base], &[("f", "1")], "w").await;
    let change = first.change_id().clone();
    let second = same_change(&mut tx, &change, &base, &[("f", "2")], "w").await;
    let descendant = same_change(&mut tx, &change, &first, &[("f", "3")], "w").await;
    fx.commit(tx, "setup").await;
    let abandoned = fx.abandon(&first).await;
    assert_eq!(
        abandoned.successor,
        Some(hex(&second)),
        "{descendant:?} is skipped"
    );
    let mut tx = fx.tx();
    let third = same_change(&mut tx, &change, &base, &[("f", "4")], "w").await;
    fx.commit(tx, "a newer commit of the change").await;
    let abandoned = fx.abandon(&second).await;
    assert_eq!(abandoned.successor, Some(hex(&third)), "the newest");

    // Only a descendant left: a plain abandon.
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("f", "base")], "base").await;
    let parent = new_commit(&mut tx, &[&base], &[("f", "1")], "w").await;
    same_change(&mut tx, parent.change_id(), &parent, &[("f", "2")], "w").await;
    fx.commit(tx, "setup").await;
    assert_eq!(fx.abandon(&parent).await.successor, None);
}

// 5
#[tokio::test]
async fn rewrites_refuse_immutable_and_hidden() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let old = new_commit(&mut tx, &[], &[("a", "1")], "old").await;
    let tagged = new_commit(&mut tx, &[&old], &[("a", "2")], "tagged").await;
    tx.repo_mut()
        .set_local_tag_target(RefName::new("v1"), RefTarget::normal(tagged.id().clone()));
    let work = new_commit(&mut tx, &[&tagged], &[("a", "3")], "work").await;
    fx.commit(tx, "setup").await;
    fx.describe(&work, "rewritten").await.unwrap();

    let root = fx.head().store().root_commit();
    for commit in [&tagged, &old, &root] {
        let error = fx.writable(commit).await.unwrap_err();
        assert!(matches!(error, RewriteError::Immutable(_)), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Conflict);
    }
    let error = fx.writable(&work).await.unwrap_err();
    assert!(matches!(error, RewriteError::Hidden(_)), "{error:?}");

    // The expected id: full and current only.
    let head = fx.head();
    assert!(matches!(
        rewrite::parse_expected(&head, "abc"),
        Err(RewriteError::InvalidCommitId(_))
    ));
    assert_eq!(rewrite::parse_expected(&head, "").unwrap(), None);
    let stale = rewrite::parse_expected(&head, &work.id().hex()).unwrap();
    let current = fx.wc_free_head_of(work.change_id());
    let error = rewrite::writable(&head, &current.hex(), stale.as_ref(), &fx.scope())
        .await
        .unwrap_err();
    assert!(matches!(error, RewriteError::Mismatch { .. }), "{error:?}");
}

impl Fixture {
    /// The visible commit of `change`.
    fn wc_free_head_of(&self, change: &ChangeId) -> CommitId {
        let head = self.head();
        head.view()
            .heads()
            .iter()
            .find(|id| {
                pollster::block_on(head.store().get_commit_async(id))
                    .unwrap()
                    .change_id()
                    == change
            })
            .cloned()
            .unwrap()
    }
}

// 6
#[tokio::test]
async fn revert_operation() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let a = new_commit(&mut tx, &[], &[("a", "1")], "a").await;
    let other = new_commit(&mut tx, &[], &[("o", "1")], "other").await;
    let leaf = new_commit(&mut tx, &[&a], &[("a", "1"), ("l", "1")], "leaf").await;
    bookmark(&mut tx, "bm", &a);
    set_wc(&mut tx, "reef-1", &a);
    fx.commit(tx, "setup").await;

    // A describe, then a later describe of another change, then the first
    // reverted: only what it touched goes back.
    let described = fx.describe(&a, "renamed").await.unwrap();
    let describe_op = described.op_id.clone();
    let other_described = fx.describe(&other, "other renamed").await.unwrap();
    let reverted = fx.revert(&describe_op).await.unwrap().unwrap();
    let a_now = fx.wc("reef-1").unwrap();
    assert_eq!(
        fx.get(&a_now.hex().parse().unwrap()).await.description(),
        "a"
    );
    assert_eq!(fx.local_bookmark("bm"), RefTarget::normal(a_now.clone()));
    assert!(
        fx.visible(&id(&other_described.commit_id)),
        "the later describe stays"
    );
    // Reverting the revert re-applies; reverting it again is superseded.
    fx.revert(&reverted).await.unwrap().unwrap();
    let again = fx.wc("reef-1").unwrap();
    assert_eq!(
        fx.get(&again.hex().parse().unwrap()).await.description(),
        "renamed\n"
    );
    let error = fx.revert(&reverted).await.unwrap_err();
    assert!(matches!(error, UndoError::Superseded { .. }), "{error:?}");

    // A plain abandon, reverted, brings the commit back (the describes
    // rebased the leaf).
    let leaf = fx.wc_free_head_of(leaf.change_id());
    let leaf = fx.get(&leaf.hex().parse().unwrap()).await;
    let abandon = fx.abandon(&leaf).await;
    assert!(!fx.visible(leaf.id()));
    fx.revert(&abandon.op_id).await.unwrap().unwrap();
    assert!(fx.visible(leaf.id()));

    // A resolving abandon, reverted, makes the change divergent again.
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let base = new_commit(&mut tx, &[], &[("f", "base")], "base").await;
    let ours = new_commit(&mut tx, &[&base], &[("f", "ours")], "work").await;
    let theirs = same_change(&mut tx, ours.change_id(), &base, &[("f", "theirs")], "work").await;
    set_wc(&mut tx, "reef-b", &theirs);
    fx.commit(tx, "setup").await;
    let resolved = fx.abandon(&theirs).await;
    assert_eq!(fx.wc("reef-b").as_ref(), Some(ours.id()));
    fx.revert(&resolved.op_id).await.unwrap().unwrap();
    assert!(fx.visible(theirs.id()));
    assert_eq!(fx.wc("reef-b").as_ref(), Some(theirs.id()));
}

// 7
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one repository walked through every refusal in turn"
)]
async fn revert_refusals() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let a = new_commit(&mut tx, &[], &[("a", "1")], "a").await;
    bookmark(&mut tx, "bm", &a);
    set_wc(&mut tx, "reef-1", &a);
    fx.commit(tx, "setup").await;
    let not_undoable = |error: UndoError| {
        assert!(matches!(error, UndoError::NotUndoable { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    };

    let root: OperationIdHex = fx
        .head()
        .op_store()
        .root_operation_id()
        .hex()
        .parse()
        .unwrap();
    not_undoable(fx.revert(&root).await.unwrap_err());

    // Creates, moves, restores and forgets.
    let (dir, state) = (fx.dir.path().join("ws-x"), fx.dir.path().join("wc-x"));
    std::fs::create_dir(&dir).unwrap();
    let (ws, create_op) = fx
        .repo
        .create_workspace(
            "reef-x",
            &dir,
            &state,
            StartPoint::NewOn(hex(&a)),
            &meta(OpKind::CreateWorkspace),
        )
        .await
        .unwrap();
    drop(ws);
    not_undoable(fx.revert(&create_op).await.unwrap_err());
    let moved = fx
        .repo
        .move_pointer(
            "reef-x",
            StartPoint::Edit(hex(&a)),
            &meta(OpKind::MoveWorkspace),
        )
        .await
        .unwrap();
    not_undoable(fx.revert(moved.op_id.as_ref().unwrap()).await.unwrap_err());
    let restored = fx
        .repo
        .restore_pointer("reef-x", &moved.from, false, &meta(OpKind::MoveWorkspace))
        .await
        .unwrap();
    not_undoable(fx.revert(&restored).await.unwrap_err());
    let forgot = fx
        .repo
        .forget_workspace("reef-x", &meta(OpKind::ReleaseWorkspace))
        .await
        .unwrap();
    not_undoable(fx.revert(&forgot).await.unwrap_err());

    // A merge of concurrent operations.
    let base = fx.head();
    let mut one = base.start_transaction();
    new_commit(&mut one, &[], &[("x", "1")], "x").await;
    one.repo_mut().rebase_descendants().await.unwrap();
    one.commit("one").await.unwrap();
    let mut two = base.start_transaction();
    new_commit(&mut two, &[], &[("y", "1")], "y").await;
    two.repo_mut().rebase_descendants().await.unwrap();
    two.commit("two").await.unwrap();
    let merged = fx.head().loader().load_at_head().await.unwrap();
    fx.repo.set_head(merged);
    assert!(fx.head().operation().parent_ids().len() > 1);
    let merge_op = fx.op();
    not_undoable(fx.revert(&merge_op).await.unwrap_err());

    // Written and never published.
    let mut tx = fx.tx();
    new_commit(&mut tx, &[], &[("z", "1")], "z").await;
    tx.repo_mut().rebase_descendants().await.unwrap();
    let unpublished = tx.write("unpublished").await.unwrap();
    let unpublished: OperationIdHex = unpublished.operation().id().hex().parse().unwrap();
    let error = fx.revert(&unpublished).await.unwrap_err();
    assert!(matches!(error, UndoError::NotInHistory(_)), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Conflict);

    // Superseded: a describe rewritten again; a bookmark moved later; a
    // working copy moved later.
    let first = fx.describe(&a, "one").await.unwrap();
    let a1 = fx.get(&first.commit_id).await;
    fx.describe(&a1, "two").await.unwrap();
    let error = fx.revert(&first.op_id).await.unwrap_err();
    assert!(matches!(error, UndoError::Superseded { .. }), "{error:?}");

    let mut tx = fx.tx();
    let b = new_commit(&mut tx, &[], &[("b", "1")], "b").await;
    let c = new_commit(&mut tx, &[], &[("c", "1")], "c").await;
    set_wc(&mut tx, "reef-2", &c);
    fx.commit(tx, "b and c").await;
    let mut tx = fx.tx();
    bookmark(&mut tx, "moving", &b);
    set_wc(&mut tx, "reef-2", &b);
    let marked = fx.commit(tx, "mark b").await;
    let mut tx = fx.tx();
    bookmark(&mut tx, "moving", &c);
    fx.commit(tx, "move the bookmark").await;
    match fx.revert(&marked).await.unwrap_err() {
        UndoError::Superseded { what, .. } => assert!(what.contains("bookmark moving"), "{what}"),
        error => panic!("{error:?}"),
    }
    let mut tx = fx.tx();
    bookmark(&mut tx, "moving", &b);
    fx.commit(tx, "move it back").await;
    let mut tx = fx.tx();
    set_wc(&mut tx, "reef-2", &c);
    fx.commit(tx, "move the working copy").await;
    match fx.revert(&marked).await.unwrap_err() {
        UndoError::Superseded { what, .. } => assert!(what.contains("reef-2"), "{what}"),
        error => panic!("{error:?}"),
    }

    // An abandon whose commit a later operation made visible again.
    let abandoned = fx.abandon(&c).await;
    let mut tx = fx.tx();
    tx.repo_mut().add_head(&c).await.unwrap();
    fx.commit(tx, "bring c back").await;
    let error = fx.revert(&abandoned.op_id).await.unwrap_err();
    assert!(matches!(error, UndoError::Superseded { .. }), "{error:?}");

    // The plan lists the working copies the revert moves.
    let on = fx.wc("reef-2").unwrap();
    let on = fx.get(&on.hex().parse().unwrap()).await;
    let described = fx.describe(&on, "renamed").await.unwrap();
    let plan = undo::plan_revert(&fx.head(), &described.op_id)
        .await
        .unwrap();
    assert!(plan.workspaces.contains(&"reef-2".to_owned()), "{plan:?}");
}

// 6, colocated: git refs and git's HEAD are never touched, and their
// import cannot be reverted.
#[tokio::test]
async fn revert_keeps_git_refs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("colocated");
    std::fs::create_dir(&root).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "1").unwrap();
    git(&["add", "f"]);
    git(&["commit", "-q", "-m", "one"]);
    let mut repo = Repo::init_in_git_repo(&root, &settings(), Some("registrar"))
        .await
        .unwrap();
    let head = repo.readonly();
    let entry = oplog::entry(head.operation());
    assert_eq!(entry.description, "import git head");
    assert_eq!(
        entry.attributes.get(ATTR_KIND).map(String::as_str),
        Some("other")
    );
    assert_eq!(
        entry.attributes.get(ATTR_AGENT).map(String::as_str),
        Some("registrar")
    );
    let init_op = repo.op_id();
    let error = repo
        .revert_operation(&init_op, &meta(OpKind::Undo))
        .await
        .unwrap_err();
    assert!(matches!(error, UndoError::NotUndoable { .. }), "{error:?}");

    git(&["branch", "side"]);
    let imported = repo
        .import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
        .await
        .unwrap();
    let import_op = imported.op_id.unwrap();
    let error = repo
        .revert_operation(&import_op, &meta(OpKind::Undo))
        .await
        .unwrap_err();
    assert!(matches!(error, UndoError::NotUndoable { .. }), "{error:?}");

    // A describe reverted leaves the git portion as it was.
    let wc = repo.wc_change().unwrap();
    let main = repo
        .readonly()
        .view()
        .get_local_bookmark(RefName::new("main"))
        .as_normal()
        .cloned()
        .unwrap();
    let described = repo
        .describe_commit(&wc.commit_id, "work", &meta(OpKind::Describe))
        .await
        .unwrap()
        .unwrap();
    let before = repo.readonly().view().store_view().clone();
    repo.revert_operation(&described.op_id, &meta(OpKind::Undo))
        .await
        .unwrap()
        .unwrap();
    let after = repo.readonly().view().store_view().clone();
    assert_eq!(after.git_refs, before.git_refs);
    assert_eq!(after.git_heads, before.git_heads);
    assert_eq!(
        repo.readonly()
            .view()
            .get_local_bookmark(RefName::new("main"))
            .as_normal(),
        Some(&main)
    );
}
