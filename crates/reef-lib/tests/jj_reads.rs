//! The read side over jj-lib (R2.02, ADR 0030): revisions as revsets with
//! Reef's aliases, the log, changes, diffs, operation kinds and effects, and
//! operation lookup. Each test builds its repository with jj-lib
//! transactions on an internal git store in its own temporary directory.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{CommitIdHex, Repo};
use reef_lib::jj::{diff, effects, log, oplog};
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
    _dir: TempDir,
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
        Self { _dir: dir, repo }
    }

    fn head(&self) -> Arc<ReadonlyRepo> {
        self.repo.readonly()
    }

    fn tx(&self) -> Transaction {
        self.repo.head().start_transaction()
    }

    async fn commit(&mut self, mut tx: Transaction, description: &str) {
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit(description).await.unwrap();
        self.repo.set_head(repo);
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

    fn in_workspace<'a>(&'a self, workspace: &'a str) -> RevsetScope<'a> {
        RevsetScope {
            workspace: Some(workspace),
            ..self.scope()
        }
    }

    async fn resolve(&self, text: &str) -> Result<CommitIdHex, RevisionError> {
        revset::resolve_revision(&self.head(), text, &self.scope())
            .await
            .map(|resolved| resolved.commit_id)
    }

    /// The git directory of the internal store.
    fn git_dir(&self) -> std::path::PathBuf {
        self.root().join(".jj/repo/store/git")
    }
}

/// A new empty commit on `parents` (the root commit when empty).
async fn new_commit(tx: &mut Transaction, parents: &[&Commit], description: &str) -> Commit {
    let parents: Vec<CommitId> = if parents.is_empty() {
        vec![tx.repo().store().root_commit_id().clone()]
    } else {
        parents.iter().map(|commit| commit.id().clone()).collect()
    };
    let tree = tx.repo().store().root_commit().tree();
    tx.repo_mut()
        .new_commit(parents, tree)
        .set_description(description)
        .write()
        .await
        .unwrap()
}

/// A second commit of `change` on the root commit.
async fn same_change(tx: &mut Transaction, change: &ChangeId, description: &str) -> Commit {
    let root = tx.repo().store().root_commit();
    tx.repo_mut()
        .new_commit(vec![root.id().clone()], root.tree())
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

fn remote_bookmark(tx: &mut Transaction, name: &str, remote: &str, commit: &Commit) {
    tx.repo_mut().set_remote_bookmark(
        RefName::new(name).to_remote_symbol(RemoteName::new(remote)),
        RemoteRef {
            target: RefTarget::normal(commit.id().clone()),
            state: RemoteRefState::New,
        },
    );
}

fn hex(commit: &Commit) -> CommitIdHex {
    commit.id().hex().parse().unwrap()
}

fn change_hex(commit: &Commit) -> String {
    commit.change_id().hex()
}

// 1
#[tokio::test]
async fn revisions_follow_jj_order() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let a = new_commit(&mut tx, &[], "a").await;
    let b = new_commit(&mut tx, &[], "b").await;
    // A bookmark named after b's change id's first six hex digits names a.
    let b_prefix = &change_hex(&b)[..6];
    bookmark(&mut tx, b_prefix, &a);
    // A tag beats a bookmark of the same name.
    bookmark(&mut tx, "dup", &b);
    tx.repo_mut()
        .set_local_tag_target(RefName::new("dup"), RefTarget::normal(a.id().clone()));
    fx.commit(tx, "setup").await;

    assert_eq!(fx.resolve(b_prefix).await.unwrap(), hex(&a));
    assert_eq!(fx.resolve("dup").await.unwrap(), hex(&a));
    assert_eq!(fx.resolve(&a.id().hex()[..12]).await.unwrap(), hex(&a));
    let reverse = b.change_id().reverse_hex();
    assert_eq!(fx.resolve(&reverse[..8]).await.unwrap(), hex(&b));
    assert_eq!(fx.resolve(&change_hex(&b)[..8]).await.unwrap(), hex(&b));
    assert_eq!(fx.resolve(&change_hex(&b)).await.unwrap(), hex(&b));

    // Hex text that is a unique commit id prefix and also prefixes another
    // change's id names the commit. Add commits until one such prefix exists.
    let found = loop {
        let head = fx.head();
        let commits: Vec<Commit> = head
            .view()
            .heads()
            .iter()
            .map(|id| head.store().get_commit(id).unwrap())
            .collect();
        let mut ids: Vec<String> = commits.iter().map(|c| c.id().hex()).collect();
        ids.push(head.store().root_commit_id().hex());
        let found = commits.iter().find_map(|commit| {
            let id = commit.id().hex();
            (1..6).find_map(|len| {
                let prefix = &id[..len];
                let unique = ids.iter().filter(|other| other.starts_with(prefix)).count() == 1;
                let other_change = commits.iter().any(|other| {
                    other.id() != commit.id() && change_hex(other).starts_with(prefix)
                });
                (unique && other_change).then(|| (prefix.to_owned(), hex(commit)))
            })
        });
        if let Some(found) = found {
            break found;
        }
        assert!(
            commits.len() < 400,
            "no shared prefix in {} commits",
            commits.len()
        );
        let mut tx = fx.tx();
        for _ in 0..10 {
            new_commit(&mut tx, &[], "more").await;
        }
        fx.commit(tx, "more").await;
    };
    assert_eq!(fx.resolve(&found.0).await.unwrap(), found.1);

    // jj's `change_id()` takes reverse hex only.
    assert!(matches!(
        fx.resolve("change_id(\"0a1b\")").await,
        Err(RevisionError::Parse { .. })
    ));
}

// 2
#[tokio::test]
async fn revisions_are_exactly_one() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let mut commits = Vec::new();
    for n in 0..17 {
        commits.push(new_commit(&mut tx, &[], &format!("c{n}")).await);
    }
    let twin = same_change(&mut tx, commits[0].change_id(), "twin").await;
    bookmark(&mut tx, "nope2", &commits[1]);
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("split"),
        RefTarget::from_legacy_form(
            [commits[2].id().clone()],
            [commits[3].id().clone(), commits[4].id().clone()],
        ),
    );
    fx.commit(tx, "setup").await;

    assert!(matches!(
        fx.resolve("none()").await,
        Err(RevisionError::Empty { .. })
    ));
    let Err(RevisionError::Ambiguous {
        commit_ids, more, ..
    }) = fx.resolve("all()").await
    else {
        panic!("all() is several commits");
    };
    assert_eq!(commit_ids.len(), revset::MAX_LISTED);
    assert!(more);

    // A divergent change by change id names both of its commits.
    let error = fx.resolve(&change_hex(&twin)).await.unwrap_err();
    let RevisionError::Ambiguous { commit_ids, .. } = &error else {
        panic!("divergent: {error:?}");
    };
    assert_eq!(
        commit_ids.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([hex(&commits[0]), hex(&twin)])
    );
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    let reverse = twin.change_id().reverse_hex();
    assert!(matches!(
        fx.resolve(&reverse).await,
        Err(RevisionError::Ambiguous { .. })
    ));
    assert_eq!(fx.resolve(&twin.id().hex()).await.unwrap(), hex(&twin));
    let offset = fx
        .resolve(&format!("{}/1", change_hex(&twin)))
        .await
        .unwrap();
    assert!([hex(&commits[0]), hex(&twin)].contains(&offset), "{offset}");

    let error = fx.resolve("split").await.unwrap_err();
    let RevisionError::Ambiguous { commit_ids, .. } = &error else {
        panic!("conflicted bookmark: {error:?}");
    };
    assert_eq!(commit_ids.len(), 2);

    // Eighteen commit ids guarantee two share a first hex digit.
    let mut seen = BTreeSet::new();
    let shared = commits
        .iter()
        .chain([&twin])
        .map(|commit| commit.id().hex()[..1].to_owned())
        .find(|digit| !seen.insert(digit.clone()))
        .expect("pigeonhole");
    let error = fx.resolve(&shared).await.unwrap_err();
    assert!(
        matches!(&error, RevisionError::AmbiguousPrefix { prefix, .. } if *prefix == shared),
        "{error:?}"
    );

    let error = fx.resolve("nope").await.unwrap_err();
    let RevisionError::NotFound { candidates, .. } = &error else {
        panic!("nope: {error:?}");
    };
    assert!(candidates.iter().any(|c| c == "nope2"), "{candidates:?}");
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(matches!(
        fx.resolve("main |").await,
        Err(RevisionError::Parse { .. })
    ));
    assert!(matches!(
        revset::resolve_revision(&fx.head(), " ", &fx.scope()).await,
        Err(RevisionError::Required)
    ));

    // A commit a rewrite hid still resolves by its full id.
    let mut tx = fx.tx();
    let old = commits[5].clone();
    tx.repo_mut()
        .rewrite_commit(&old)
        .set_description("c5 again")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "rewrite").await;
    assert_eq!(fx.resolve(&old.id().hex()).await.unwrap(), hex(&old));
}

// 3
#[tokio::test]
async fn aliases() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let main = new_commit(&mut tx, &[], "main").await;
    let master = new_commit(&mut tx, &[], "master").await;
    let develop = new_commit(&mut tx, &[], "develop").await;
    let tagged = new_commit(&mut tx, &[&main], "tagged").await;
    let work = new_commit(&mut tx, &[&tagged], "work").await;
    bookmark(&mut tx, "main", &main);
    bookmark(&mut tx, "master", &master);
    remote_bookmark(&mut tx, "main", "origin", &develop);
    tx.repo_mut()
        .set_local_tag_target(RefName::new("v1"), RefTarget::normal(tagged.id().clone()));
    fx.commit(tx, "setup").await;

    let trunk = revset::trunk(&fx.head()).unwrap();
    assert_eq!(
        (trunk.commit_id, trunk.bookmark.as_str()),
        (hex(&main), "main")
    );
    assert_eq!(fx.resolve("trunk()").await.unwrap(), hex(&main));
    // immutable_heads() includes a tag, so `tagged` is immutable and `work`
    // is not.
    let tagged_id = tagged.id().hex();
    let work_id = work.id().hex();
    assert_eq!(
        fx.resolve(&format!("{tagged_id} & immutable()"))
            .await
            .unwrap(),
        hex(&tagged)
    );
    assert!(matches!(
        fx.resolve(&format!("{tagged_id} & mutable()")).await,
        Err(RevisionError::Empty { .. })
    ));
    assert_eq!(
        fx.resolve(&format!("{work_id} & mutable()")).await.unwrap(),
        hex(&work)
    );
    let head = fx.head();
    let count = |text: &str| {
        let parsed = revset::parse(&head, text, &fx.scope()).unwrap();
        revset::count(&head, &parsed).unwrap()
    };
    assert_eq!(count("mutable() & immutable()"), 0);
    assert_eq!(count("mutable()") + count("immutable()"), count("all()"));
    assert!(matches!(
        fx.resolve("my_alias()").await,
        Err(RevisionError::Parse { .. })
    ));

    // Without `main`: `master` comes before `main@origin`.
    let mut tx = fx.tx();
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("main"), RefTarget::absent());
    fx.commit(tx, "drop main").await;
    let trunk = revset::trunk(&fx.head()).unwrap();
    assert_eq!(
        (trunk.commit_id, trunk.bookmark.as_str()),
        (hex(&master), "master")
    );

    // origin's HEAD naming `develop`, and a local `develop`: it beats `master`.
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(fx.git_dir())
        .args([
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut tx = fx.tx();
    bookmark(&mut tx, "develop", &develop);
    fx.commit(tx, "develop").await;
    let trunk = revset::trunk(&fx.head()).unwrap();
    assert_eq!(trunk.bookmark, "develop");

    // A conflicted `main` is skipped for `develop` (still before `master`).
    let mut tx = fx.tx();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("main"),
        RefTarget::from_legacy_form(
            [main.id().clone()],
            [master.id().clone(), develop.id().clone()],
        ),
    );
    fx.commit(tx, "conflict main").await;
    assert_eq!(revset::trunk(&fx.head()).unwrap().bookmark, "develop");

    // No candidate at all: trunk() is root().
    let mut tx = fx.tx();
    for name in ["main", "master", "develop"] {
        tx.repo_mut()
            .set_local_bookmark_target(RefName::new(name), RefTarget::absent());
    }
    tx.repo_mut().set_remote_bookmark(
        RefName::new("main").to_remote_symbol(RemoteName::new("origin")),
        RemoteRef::absent(),
    );
    fx.commit(tx, "no trunk").await;
    assert_eq!(revset::trunk(&fx.head()), None);
    assert_eq!(
        fx.resolve("trunk()").await.unwrap().as_str(),
        fx.head().store().root_commit_id().hex()
    );
}

// 4
#[tokio::test]
async fn at_sign_needs_a_workspace() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    new_commit(&mut tx, &[], "x").await;
    fx.commit(tx, "setup").await;
    let head = fx.head();

    for text in ["@", "@-", "present(@)", "x | @", "@::"] {
        let error = revset::parse(&head, text, &fx.scope()).unwrap_err();
        assert!(
            matches!(error, RevisionError::WorkspaceRequired { .. }),
            "{text}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        let parsed = revset::parse(&head, text, &fx.in_workspace("default")).unwrap();
        assert!(parsed.uses_working_copy(), "{text}");
    }
    for text in [
        "default@",
        "working_copies()",
        "description(\"@\")",
        "reef-0123456789abcdef@",
        "files(\"a\")",
    ] {
        let parsed = revset::parse(&head, text, &fx.scope()).unwrap();
        assert!(!parsed.uses_working_copy(), "{text}");
    }
    // A relative date resolves once per parse (from the review: two
    // instants made the probes differ about one time in twenty).
    for _ in 0..2000 {
        let text = "committer_date(after:\"1 hour ago\")";
        let parsed = revset::parse(&head, text, &fx.scope()).unwrap();
        assert!(!parsed.uses_working_copy());
    }

    let wc = fx.repo.wc_commit().unwrap();
    let at = revset::resolve_revision(&head, "@", &fx.in_workspace("default"))
        .await
        .unwrap();
    assert_eq!(at.commit_id.as_str(), wc.id().hex());
    let error = revset::resolve_revision(&head, "@", &fx.in_workspace("reef-0000000000000000"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RevisionError::WorkingCopyNotFound { workspace, at_operation }
            if workspace == "reef-0000000000000000" && *at_operation == head.op_id().hex()),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::NotFound);
    // `files()` filters without a request workspace: nothing touches `a`.
    let parsed = revset::parse(&head, "files(\"a\")", &fx.scope()).unwrap();
    assert_eq!(revset::count(&head, &parsed).unwrap(), 0);
}

/// One path's content in [`tree_with`].
enum Entry<'a> {
    File(&'a [u8]),
    Exec(&'a [u8]),
    Link(&'a str),
    Gone,
}

/// `base` with `entries` written over it.
async fn tree_with(
    repo: &ReadonlyRepo,
    base: &jj_lib::merged_tree::MergedTree,
    entries: &[(&str, Entry<'_>)],
) -> jj_lib::merged_tree::MergedTree {
    use jj_lib::backend::{CopyId, TreeValue};
    use jj_lib::merge::Merge;
    use jj_lib::repo_path::RepoPathBuf;
    let store = repo.store();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(base.clone());
    for (path, entry) in entries {
        let path = RepoPathBuf::from_internal_string(*path).unwrap();
        let value = match entry {
            Entry::File(bytes) | Entry::Exec(bytes) => {
                let id = store.write_file(&path, &mut &bytes[..]).await.unwrap();
                Some(TreeValue::File {
                    id,
                    executable: matches!(entry, Entry::Exec(_)),
                    copy_id: CopyId::placeholder(),
                })
            }
            Entry::Link(target) => Some(TreeValue::Symlink(
                store.write_symlink(&path, target).await.unwrap(),
            )),
            Entry::Gone => None,
        };
        builder.set_or_remove(path, value.map_or_else(Merge::absent, Merge::normal));
    }
    builder.write_tree().await.unwrap()
}

async fn commit_tree(
    tx: &mut Transaction,
    parents: &[&Commit],
    tree: jj_lib::merged_tree::MergedTree,
    description: &str,
) -> Commit {
    let parents = parents.iter().map(|commit| commit.id().clone()).collect();
    tx.repo_mut()
        .new_commit(parents, tree)
        .set_description(description)
        .write()
        .await
        .unwrap()
}

fn set_wc(tx: &mut Transaction, name: &str, commit: &Commit) {
    tx.repo_mut()
        .set_wc_commit(name.into(), commit.id().clone())
        .unwrap();
}

async fn log_ids(
    fx: &Fixture,
    text: &str,
) -> Vec<(CommitIdHex, Vec<(CommitIdHex, log::EdgeKind)>)> {
    let head = fx.head();
    let parsed = revset::parse(&head, text, &fx.scope()).unwrap();
    let page = log::log_page(&head, &parsed, 0, 1000, &|| false)
        .await
        .unwrap();
    assert!(!page.more);
    page.nodes
        .into_iter()
        .map(|node| (node.commit_id, node.edges))
        .collect()
}

// 5
#[tokio::test]
async fn default_log_set() {
    let mut fx = Fixture::new().await;
    let default_wc = fx.repo.wc_commit().unwrap();
    let mut tx = fx.tx();
    let trunk = new_commit(&mut tx, &[], "trunk").await;
    bookmark(&mut tx, "main", &trunk);
    let wa = new_commit(&mut tx, &[&trunk], "wa").await;
    let b1 = new_commit(&mut tx, &[&trunk], "b1").await;
    let b2 = new_commit(&mut tx, &[&b1], "b2").await;
    let o1 = new_commit(&mut tx, &[&trunk], "o1").await;
    let o2 = new_commit(&mut tx, &[&o1], "o2").await;
    let t1 = new_commit(&mut tx, &[&o2], "tagged").await;
    let t2 = new_commit(&mut tx, &[&t1], "after tag").await;
    tx.repo_mut()
        .set_local_tag_target(RefName::new("v1"), RefTarget::normal(t1.id().clone()));
    set_wc(&mut tx, "reef-aaaaaaaaaaaaaaaa", &wa);
    set_wc(&mut tx, "reef-bbbbbbbbbbbbbbbb", &b2);
    set_wc(&mut tx, "reef-cccccccccccccccc", &trunk);
    fx.commit(tx, "setup").await;

    let entries = log_ids(&fx, revset::DEFAULT_LOG).await;
    let ids: Vec<&CommitIdHex> = entries.iter().map(|(id, _)| id).collect();
    // The tag makes t1 and its ancestors immutable: o1, o2 and t1 drop out;
    // t2 stays, reaching trunk through them.
    let expected: BTreeSet<CommitIdHex> = [&default_wc, &trunk, &wa, &b1, &b2, &t2]
        .into_iter()
        .map(hex)
        .collect();
    assert_eq!(
        ids.iter().copied().cloned().collect::<BTreeSet<_>>(),
        expected
    );
    let at = |commit: &Commit| ids.iter().position(|id| **id == hex(commit)).unwrap();
    assert_eq!(at(&b2) + 1, at(&b1), "a stack's commits are adjacent");
    for child in [&wa, &b1, &t2] {
        assert!(at(child) < at(&trunk), "children first");
    }
    let edges = |commit: &Commit| entries[at(commit)].1.clone();
    assert_eq!(edges(&b2), [(hex(&b1), log::EdgeKind::Direct)]);
    assert_eq!(edges(&t2), [(hex(&trunk), log::EdgeKind::Indirect)]);
    let root = fx.head().store().root_commit_id().hex().parse().unwrap();
    assert_eq!(edges(&trunk), [(root, log::EdgeKind::Missing)]);
    assert!(!o1.id().hex().is_empty() && !o2.id().hex().is_empty());
}

// 6
#[tokio::test]
async fn log_pages_resume() {
    let mut fx = Fixture::new().await;
    let mut tx = fx.tx();
    let mut last = new_commit(&mut tx, &[], "c0").await;
    for n in 1..25 {
        last = new_commit(&mut tx, &[&last], &format!("c{n}")).await;
    }
    fx.commit(tx, "setup").await;
    let head = fx.head();
    let parsed = revset::parse(&head, "root()..", &fx.scope()).unwrap();
    let all = log::log_page(&head, &parsed, 0, 1000, &|| false)
        .await
        .unwrap();
    assert_eq!(
        all.nodes.len(),
        26,
        "25 commits and the default working copy"
    );
    let mut paged = Vec::new();
    let mut skip = 0;
    loop {
        let page = log::log_page(&head, &parsed, skip, 10, &|| false)
            .await
            .unwrap();
        if skip > 0 {
            assert_eq!(
                page.last_skipped.as_ref(),
                Some(
                    &paged
                        .last()
                        .map(|n: &log::LogNode| n.commit_id.clone())
                        .unwrap()
                )
            );
        }
        skip += page.nodes.len();
        paged.extend(page.nodes);
        if !page.more {
            break;
        }
    }
    assert_eq!(paged, all.nodes);
    // A page that stops early says there is more.
    let stopped = log::log_page(&head, &parsed, 0, 10, &|| true)
        .await
        .unwrap();
    assert!(stopped.nodes.is_empty() && stopped.more);
}

// 7
#[tokio::test]
async fn change_fields() {
    use jj_lib::backend::{MillisSinceEpoch, Signature, Timestamp};
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root_tree = head.store().root_commit().tree();
    let mut tx = fx.tx();
    let base_tree = tree_with(&head, &root_tree, &[("a", Entry::File(b"0\n"))]).await;
    let trunk = commit_tree(
        &mut tx,
        &[&head.store().root_commit()],
        base_tree.clone(),
        "trunk",
    )
    .await;
    bookmark(&mut tx, "main", &trunk);
    remote_bookmark(&mut tx, "main", "origin", &trunk);
    remote_bookmark(&mut tx, "main", "git", &trunk);
    tx.repo_mut()
        .set_local_tag_target(RefName::new("v1"), RefTarget::normal(trunk.id().clone()));
    let empty = commit_tree(&mut tx, &[&trunk], base_tree.clone(), "empty").await;
    let x_tree = tree_with(&head, &base_tree, &[("a", Entry::File(b"x\n"))]).await;
    let y_tree = tree_with(&head, &base_tree, &[("a", Entry::File(b"y\n"))]).await;
    let x = commit_tree(&mut tx, &[&trunk], x_tree, "x").await;
    let y = commit_tree(&mut tx, &[&trunk], y_tree, "y").await;
    let merged = jj_lib::rewrite::merge_commit_trees(tx.repo(), &[x.clone(), y.clone()])
        .await
        .unwrap();
    let at = Timestamp {
        timestamp: MillisSinceEpoch(1_700_000_000_123),
        tz_offset: 60,
    };
    let signature = Signature {
        name: "Ada".into(),
        email: "ada@example.com".into(),
        timestamp: at,
    };
    let conflicted = tx
        .repo_mut()
        .new_commit(vec![x.id().clone(), y.id().clone()], merged)
        .set_description("merge")
        .set_author(signature.clone())
        .write()
        .await
        .unwrap();
    let twin = same_change(&mut tx, empty.change_id(), "twin").await;
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("split"),
        RefTarget::from_legacy_form([trunk.id().clone()], [x.id().clone(), y.id().clone()]),
    );
    set_wc(&mut tx, "reef-0123456789abcdef", &x);
    fx.commit(tx, "setup").await;
    let mut tx = fx.tx();
    let gone = y.clone();
    tx.repo_mut()
        .rewrite_commit(&gone)
        .set_description("y again")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "rewrite y").await;

    let head = fx.head();
    let describer = log::Describer::new(&head, &fx.scope()).unwrap();
    let describe = async |commit: &Commit| describer.describe(&hex(commit)).await.unwrap();
    let trunk_info = describe(&trunk).await;
    assert!(trunk_info.immutable && !trunk_info.empty && !trunk_info.conflict);
    // `split`'s removed term is not a target.
    assert_eq!(trunk_info.bookmarks, ["main", "main@origin"]);
    assert_eq!(trunk_info.tags, ["v1"]);
    assert_eq!(
        trunk_info.tree_id.as_deref(),
        Some(trunk.tree_ids().as_resolved().unwrap().hex().as_str())
    );
    let empty_info = describe(&empty).await;
    assert!(empty_info.empty && empty_info.divergent && !empty_info.immutable);
    assert!(describe(&twin).await.divergent);
    let conflict_info = describe(&conflicted).await;
    assert!(conflict_info.conflict);
    assert_eq!(conflict_info.tree_id, None);
    // x was rewritten too: y's rewrite rebased the merge, not x. The merge's
    // parents are its own, in order.
    let merge_now = head
        .view()
        .heads()
        .iter()
        .map(|id| head.store().get_commit(id).unwrap())
        .find(|commit| commit.description() == "merge")
        .unwrap();
    let merge_info = describer.describe(&hex(&merge_now)).await.unwrap();
    assert_eq!(merge_info.parent_commit_ids.len(), 2);
    assert_eq!(merge_info.parent_commit_ids[0], hex(&x));
    assert_eq!(merge_info.parent_change_ids[0].as_str(), change_hex(&x));
    assert_eq!(merge_info.parent_change_ids[1].as_str(), change_hex(&y));
    assert_eq!(merge_info.author.name, "Ada");
    assert_eq!(
        merge_info.author.at,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123)
    );
    let x_info = describe(&x).await;
    assert_eq!(x_info.bookmarks, ["split"]);
    assert_eq!(x_info.workspaces, ["reef-0123456789abcdef"]);
    let gone_info = describe(&gone).await;
    assert!(gone_info.hidden);
    assert!(!describe(&x).await.hidden);
}

fn options(format: diff::DiffFormat) -> diff::DiffOptions {
    diff::DiffOptions {
        format,
        context: 3,
        max_lines: 1000,
        max_hunk_bytes: 1 << 20,
        max_file_bytes: 16 << 20,
        paths: Vec::new(),
        after: None,
        limit: 1000,
    }
}

async fn diff_of(
    fx: &Fixture,
    from: &diff::DiffBase,
    to: &Commit,
    options: &diff::DiffOptions,
) -> diff::DiffPage {
    diff::diff_page(&fx.head(), from, &hex(to), options, &|| false)
        .await
        .unwrap()
}

// 8
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one fixture, every summary case")]
async fn diff_summary_and_copies() {
    use diff::{ChangeKind, DiffBase, DiffFormat, EntryKind};
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root = head.store().root_commit();
    let long: Vec<u8> = (0..200)
        .flat_map(|n| format!("line {n}\n").into_bytes())
        .collect();
    let mut edited = long.clone();
    edited.extend_from_slice(b"one more\n");
    let base = tree_with(
        &head,
        &root.tree(),
        &[
            ("keep", Entry::File(b"k\n")),
            ("modify", Entry::File(b"m\n")),
            ("delete", Entry::File(b"d\n")),
            ("old/name", Entry::File(&long)),
            ("source", Entry::File(&long)),
            ("exec", Entry::File(b"#!/bin/sh\n")),
            ("link", Entry::Link("a")),
            ("src/a", Entry::File(b"a\n")),
            ("srcx/b", Entry::File(b"b\n")),
        ],
    )
    .await;
    let next = tree_with(
        &head,
        &base,
        &[
            ("modify", Entry::File(b"M\n")),
            ("delete", Entry::Gone),
            ("added", Entry::File(b"new\n")),
            ("old/name", Entry::Gone),
            ("new/name", Entry::File(&long)),
            ("source", Entry::File(&edited)),
            ("copy", Entry::File(&long)),
            ("exec", Entry::Exec(b"#!/bin/sh\n")),
            ("link", Entry::Link("b")),
            ("src/a", Entry::File(b"A\n")),
            ("srcx/b", Entry::File(b"B\n")),
        ],
    )
    .await;
    let mut tx = fx.tx();
    let first = commit_tree(&mut tx, &[&root], base, "first").await;
    let second = commit_tree(&mut tx, &[&first], next, "second").await;
    fx.commit(tx, "setup").await;

    let page = diff_of(
        &fx,
        &DiffBase::Parents,
        &second,
        &options(DiffFormat::Summary),
    )
    .await;
    let got: Vec<(&str, ChangeKind, Option<&str>)> = page
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.kind.unwrap(), f.old_path.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("added", ChangeKind::Added, None),
            ("copy", ChangeKind::Copied, Some("source")),
            ("delete", ChangeKind::Deleted, None),
            ("exec", ChangeKind::Modified, None),
            ("link", ChangeKind::Modified, None),
            ("modify", ChangeKind::Modified, None),
            ("new/name", ChangeKind::Renamed, Some("old/name")),
            ("source", ChangeKind::Modified, None),
            ("src/a", ChangeKind::Modified, None),
            ("srcx/b", ChangeKind::Modified, None),
        ]
    );
    assert_eq!(page.total, 10);
    assert_eq!(page.from, [hex(&first)]);
    let exec = &page.files[3];
    assert_eq!(
        (exec.old_type, exec.new_type),
        (Some(EntryKind::File), Some(EntryKind::Executable))
    );
    assert_eq!(page.files[4].new_type, Some(EntryKind::Symlink));
    assert_eq!(page.files[0].old_type, None);

    // Path prefixes match whole components.
    let mut only_src = options(DiffFormat::Summary);
    only_src.paths = diff::check_paths(&["src".to_owned()]).unwrap();
    let page = diff_of(&fx, &DiffBase::Parents, &second, &only_src).await;
    assert_eq!(
        page.files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["src/a"]
    );
    for bad in ["../x", "/abs", "a//b"] {
        assert!(diff::check_paths(&[bad.to_owned()]).is_err(), "{bad}");
    }

    // Paging: after a path, and the total over every page.
    let mut paged = options(DiffFormat::Summary);
    paged.limit = 4;
    let page = diff_of(&fx, &DiffBase::Parents, &second, &paged).await;
    assert_eq!(page.files.len(), 4);
    assert!(page.more);
    paged.after = diff::check_paths(&[page.files[3].path.clone()])
        .unwrap()
        .pop();
    let page = diff_of(&fx, &DiffBase::Parents, &second, &paged).await;
    assert_eq!(page.files[0].path, "link");
    assert_eq!(page.total, 10);

    // A merge diffs against its merged parents.
    let mut tx = fx.tx();
    let side_tree = tree_with(&fx.head(), &first.tree(), &[("side", Entry::File(b"s\n"))]).await;
    let side = commit_tree(&mut tx, &[&first], side_tree, "side").await;
    let merge_tree =
        jj_lib::rewrite::merge_commit_trees(tx.repo(), &[second.clone(), side.clone()])
            .await
            .unwrap();
    let merge_tree = tree_with(&fx.head(), &merge_tree, &[("merged", Entry::File(b"!\n"))]).await;
    let merge = commit_tree(&mut tx, &[&second, &side], merge_tree, "merge").await;
    fx.commit(tx, "merge").await;
    let page = diff_of(
        &fx,
        &DiffBase::Parents,
        &merge,
        &options(DiffFormat::Summary),
    )
    .await;
    assert_eq!(
        page.files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["merged"]
    );
    assert_eq!(page.from, [hex(&second), hex(&side)]);
    let page = diff_of(
        &fx,
        &DiffBase::Commit(hex(&first)),
        &side,
        &options(DiffFormat::Summary),
    )
    .await;
    assert_eq!(
        page.files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["side"]
    );
}

// 9
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one fixture, every hunk case")]
async fn diff_hunks() {
    use diff::{DiffBase, DiffFormat};
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root = head.store().root_commit();
    let lines = |n: usize, tag: &str| -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("{tag} {i}\n").into_bytes())
            .collect()
    };
    let mut middle = lines(20, "l");
    middle.splice(50..50, b"inserted\n".iter().copied());
    let mut binary = b"bin".to_vec();
    binary.push(0);
    let big = vec![b'x'; 70 * 1024];
    let latin1 = b"caf\xe9\n".to_vec();
    let base = tree_with(
        &head,
        &root.tree(),
        &[
            ("ctx", Entry::File(&lines(20, "l"))),
            ("long", Entry::File(b"")),
            ("open", Entry::File(b"a\nb")),
            ("bin", Entry::File(b"text\n")),
            ("big", Entry::File(b"small\n")),
            ("latin", Entry::File(b"cafe\n")),
        ],
    )
    .await;
    let next = tree_with(
        &head,
        &base,
        &[
            ("ctx", Entry::File(&middle)),
            ("long", Entry::File(&lines(1500, "n"))),
            ("open", Entry::File(b"a\nc")),
            ("bin", Entry::File(&binary)),
            ("big", Entry::File(&big)),
            ("latin", Entry::File(&latin1)),
        ],
    )
    .await;
    let mut tx = fx.tx();
    let first = commit_tree(&mut tx, &[&root], base, "first").await;
    let second = commit_tree(&mut tx, &[&first], next, "second").await;
    fx.commit(tx, "setup").await;

    let mut hunks = options(DiffFormat::Hunks);
    hunks.max_file_bytes = 64 * 1024;
    let page = diff_of(&fx, &DiffBase::Parents, &second, &hunks).await;
    let file = |path: &str| page.files.iter().find(|f| f.path == path).unwrap().clone();

    let ctx = file("ctx");
    assert_eq!((ctx.lines_added, ctx.lines_removed), (1, 0));
    assert_eq!(ctx.hunks.len(), 1);
    let hunk = &ctx.hunks[0];
    // Inserted after line 12: three lines of context from line 10.
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (10, 6, 10, 7)
    );
    assert!(hunk.lines.starts_with(" l 9\n"), "{}", hunk.lines);
    assert!(hunk.lines.contains("+inserted\n"));
    let mut zero = hunks.clone();
    zero.context = 0;
    let page0 = diff_of(&fx, &DiffBase::Parents, &second, &zero).await;
    let ctx0 = page0.files.iter().find(|f| f.path == "ctx").unwrap();
    assert_eq!(ctx0.hunks[0].lines, "+inserted\n");
    assert_eq!(
        (ctx0.hunks[0].old_start, ctx0.hunks[0].old_lines),
        (12, 0),
        "git's line before"
    );
    assert_eq!((ctx0.hunks[0].new_start, ctx0.hunks[0].new_lines), (13, 1));

    let long = file("long");
    assert_eq!(long.lines_added, 1500, "counts cover the whole file");
    assert!(long.truncated);
    let kept: u32 = long.hunks.iter().map(|h| h.new_lines).sum();
    assert_eq!(kept, 1000);
    assert_eq!(
        long.hunks
            .iter()
            .map(|h| h.lines.lines().count())
            .sum::<usize>(),
        1000
    );
    let mut bytes = hunks.clone();
    bytes.max_lines = 1_000_000;
    bytes.max_hunk_bytes = 100;
    let cut = diff_of(&fx, &DiffBase::Parents, &second, &bytes).await;
    let long = cut.files.iter().find(|f| f.path == "long").unwrap();
    assert!(long.truncated);
    assert!(long.hunks.iter().map(|h| h.lines.len()).sum::<usize>() <= 100);

    let open = file("open");
    assert!(open.old_no_final_newline && open.new_no_final_newline);
    assert!(
        open.hunks[0].lines.ends_with("+c\n"),
        "{}",
        open.hunks[0].lines
    );

    let bin = file("bin");
    assert!(bin.binary && bin.hunks.is_empty() && bin.lines_added == 0);
    let summary = diff_of(
        &fx,
        &DiffBase::Parents,
        &second,
        &options(DiffFormat::Summary),
    )
    .await;
    assert!(
        summary
            .files
            .iter()
            .find(|f| f.path == "bin")
            .unwrap()
            .binary
    );

    let big = file("big");
    assert!(big.too_large && big.hunks.is_empty() && big.lines_added == 0);

    let latin = file("latin");
    assert!(latin.invalid_utf8);
    assert_eq!((latin.lines_added, latin.lines_removed), (1, 1));
    assert!(latin.hunks[0].lines.contains('\u{fffd}'));

    let stat = diff_of(&fx, &DiffBase::Parents, &second, &options(DiffFormat::Stat)).await;
    let ctx = stat.files.iter().find(|f| f.path == "ctx").unwrap();
    assert_eq!((ctx.lines_added, ctx.lines_removed), (1, 0));
    assert_eq!(ctx.hunks, []);
}

async fn effects_of_head(fx: &Fixture, cap: usize) -> effects::OpEffects {
    let head = fx.head();
    effects::operation_effects(&head, head.operation(), cap)
        .await
        .unwrap()
}

fn change_effects(
    effects: &effects::OpEffects,
) -> Vec<(
    String,
    effects::ChangeEffectKind,
    Option<String>,
    Option<String>,
)> {
    effects
        .changes
        .iter()
        .map(|e| {
            (
                e.change_id.to_string(),
                e.kind,
                e.commit_id.as_ref().map(ToString::to_string),
                e.previous_commit_id.as_ref().map(ToString::to_string),
            )
        })
        .collect()
}

// 10
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one history, every effect case in order"
)]
async fn effects() {
    use effects::ChangeEffectKind::{Abandoned, Created, Hidden, Rewritten};
    let mut fx = Fixture::new().await;
    let wc = fx.repo.wc_commit().unwrap();

    // A rewrite of the working-copy commit, as a snapshot records it.
    let mut tx = fx.tx();
    let snapshot = tx
        .repo_mut()
        .rewrite_commit(&wc)
        .set_description("snapshot")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "snapshot working copy").await;
    let found = effects_of_head(&fx, 1000).await;
    assert_eq!(
        change_effects(&found),
        [(
            change_hex(&wc),
            Rewritten,
            Some(snapshot.id().hex()),
            Some(wc.id().hex())
        )]
    );
    assert_eq!(found.working_copies.len(), 1);
    let moved = &found.working_copies[0];
    assert_eq!(moved.workspace, "default");
    assert_eq!(
        moved.commit_id.as_ref().map(ToString::to_string),
        Some(snapshot.id().hex())
    );
    assert_eq!(
        moved.previous_commit_id.as_ref().map(ToString::to_string),
        Some(wc.id().hex())
    );

    // A new change.
    let mut tx = fx.tx();
    let y = new_commit(&mut tx, &[], "y").await;
    fx.commit(tx, "new").await;
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(change_hex(&y), Created, Some(y.id().hex()), None)]
    );

    // Two concurrent rewrites of y, then their merge.
    let base = fx.head();
    let mut tx = fx.tx();
    let y1 = tx
        .repo_mut()
        .rewrite_commit(&y)
        .set_description("y1")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "rewrite y1").await;
    let mut tx = base.start_transaction();
    let y2 = tx
        .repo_mut()
        .rewrite_commit(&y)
        .set_description("y2")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "rewrite y2").await;
    let only_y2 = fx
        .head()
        .operation()
        .view()
        .await
        .unwrap()
        .store_view()
        .clone();
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(
            change_hex(&y),
            Rewritten,
            Some(y2.id().hex()),
            Some(y.id().hex())
        )]
    );
    fx.repo.reload_at_head().await.unwrap();
    let merge = fx.head();
    assert_eq!(merge.operation().parent_ids().len(), 2, "a merge of both");
    let found = effects_of_head(&fx, 1000).await;
    assert!(
        found.changes.is_empty(),
        "nothing differs from both parents: {found:?}"
    );
    let describer = log::Describer::new(&merge, &fx.scope()).unwrap();
    assert!(describer.describe(&hex(&y1)).await.unwrap().divergent);

    // Abandoning one of the two hides it; the change stays visible.
    let mut tx = fx.tx();
    tx.repo_mut().record_abandoned_commit(&y2);
    fx.commit(tx, "abandon y2").await;
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(change_hex(&y), Hidden, None, Some(y2.id().hex()))]
    );
    let before_abandon = fx.head().operation().clone();

    // Abandoning the last one abandons the change.
    let mut tx = fx.tx();
    tx.repo_mut().record_abandoned_commit(&y1);
    fx.commit(tx, "abandon y1").await;
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(change_hex(&y), Abandoned, None, Some(y1.id().hex()))]
    );

    // Restoring an old view brings back y1 as a rewrite.
    let old_view = before_abandon.view().await.unwrap().store_view().clone();
    let mut tx = fx.tx();
    let mut view = old_view.clone();
    view.wc_commit_ids = fx.head().view().store_view().wc_commit_ids.clone();
    tx.repo_mut().set_view(view);
    fx.commit(tx, "restore").await;
    let restored = change_effects(&effects_of_head(&fx, 1000).await);
    // y1 comes back; its change was not visible before, so it is new again
    // (no predecessor is recorded for a restore).
    assert_eq!(
        restored,
        [(change_hex(&y), Created, Some(y1.id().hex()), None)]
    );
    // Restoring a view where another commit of the visible change was
    // visible rewrites it back, from the one commit hidden.
    let mut tx = fx.tx();
    let mut view = only_y2;
    view.wc_commit_ids = fx.head().view().store_view().wc_commit_ids.clone();
    tx.repo_mut().set_view(view);
    fx.commit(tx, "restore y2").await;
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(
            change_hex(&y),
            Rewritten,
            Some(y2.id().hex()),
            Some(y1.id().hex())
        )]
    );

    // Bookmarks: set, moved, deleted, conflicted; `git` left out.
    let mut tx = fx.tx();
    let z = new_commit(&mut tx, &[], "z").await;
    bookmark(&mut tx, "feat", &snapshot);
    bookmark(&mut tx, "gone", &snapshot);
    remote_bookmark(&mut tx, "feat", "origin", &snapshot);
    remote_bookmark(&mut tx, "feat", "git", &snapshot);
    fx.commit(tx, "bookmarks").await;
    let found = effects_of_head(&fx, 1000).await;
    let names: Vec<(Option<&str>, &str)> = found
        .bookmarks
        .iter()
        .map(|b| (b.remote.as_deref(), b.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [(None, "feat"), (None, "gone"), (Some("origin"), "feat")]
    );
    assert!(
        found
            .bookmarks
            .iter()
            .all(|b| b.previous_commit_id.is_none())
    );
    let mut tx = fx.tx();
    bookmark(&mut tx, "feat", &z);
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new("gone"), RefTarget::absent());
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("split"),
        RefTarget::from_legacy_form([snapshot.id().clone()], [z.id().clone(), y1.id().clone()]),
    );
    fx.commit(tx, "move bookmarks").await;
    let found = effects_of_head(&fx, 1000).await;
    let by_name = |name: &str| {
        found
            .bookmarks
            .iter()
            .find(|b| b.name == name && b.remote.is_none())
            .unwrap()
    };
    assert_eq!(
        by_name("feat").commit_id.as_ref().map(ToString::to_string),
        Some(z.id().hex())
    );
    assert_eq!(
        by_name("feat")
            .previous_commit_id
            .as_ref()
            .map(ToString::to_string),
        Some(snapshot.id().hex())
    );
    assert_eq!(by_name("gone").commit_id, None);
    assert!(by_name("split").conflicted);
    assert_eq!(found.bookmark_count, 3);

    // Working copies added, moved and forgotten.
    let mut tx = fx.tx();
    set_wc(&mut tx, "reef-0123456789abcdef", &z);
    fx.commit(tx, "add").await;
    let found = effects_of_head(&fx, 1000).await;
    assert_eq!(found.working_copies.len(), 1);
    assert_eq!(found.working_copies[0].workspace, "reef-0123456789abcdef");
    assert_eq!(found.working_copies[0].previous_commit_id, None);
    let mut tx = fx.tx();
    tx.repo_mut()
        .remove_workspace(jj_lib::ref_name::WorkspaceName::new(
            "reef-0123456789abcdef",
        ))
        .await
        .unwrap();
    fx.commit(tx, "forget").await;
    let found = effects_of_head(&fx, 1000).await;
    assert_eq!(found.working_copies.len(), 1);
    assert_eq!(found.working_copies[0].commit_id, None);

    // The root operation's effects are empty.
    let head = fx.head();
    let root = head.loader().root_operation().await;
    assert_eq!(
        effects::operation_effects(&head, &root, 1000)
            .await
            .unwrap(),
        effects::OpEffects::default()
    );

    // A cap keeps the entries and counts them all.
    let mut tx = fx.tx();
    for n in 0..1005 {
        new_commit(&mut tx, &[], &format!("bulk {n}")).await;
    }
    fx.commit(tx, "bulk").await;
    let found = effects_of_head(&fx, 1000).await;
    assert_eq!((found.changes.len(), found.change_count), (1000, 1005));
    // A filter by change still matches past the cap (from the review).
    let last = found.capped_change_ids.last().expect("five past the cap");
    assert!(found.touches(last.as_str()));
    assert!(!found.changes.iter().any(|c| &c.change_id == last));
}

fn op_entry(description: &str, is_snapshot: bool, workspace: &str) -> oplog::OpEntry {
    oplog::OpEntry {
        id: "00".parse().unwrap(),
        parents: vec!["01".parse().unwrap()],
        description: description.to_owned(),
        start: std::time::UNIX_EPOCH,
        end: std::time::UNIX_EPOCH,
        hostname: String::new(),
        username: String::new(),
        is_snapshot,
        workspace: Some(workspace.to_owned()),
        attributes: std::collections::BTreeMap::new(),
    }
}

/// (description, `is_snapshot`, jj workspace, kind, snapshot kind, workspace id)
type Case<'a> = (
    String,
    bool,
    &'a str,
    Option<oplog::OpKind>,
    Option<&'a str>,
    Option<&'a str>,
);

// 11
#[test]
#[expect(clippy::too_many_lines, reason = "one row per description Reef writes")]
fn operations_are_classified() {
    use oplog::OpKind;
    const ID: &str = "0123456789abcdef";
    let reef = format!("reef-{ID}");
    let cases: Vec<Case<'_>> = vec![
        (
            format!("reef: snapshot workspace {ID}"),
            true,
            "default",
            Some(OpKind::Snapshot),
            None,
            Some(ID),
        ),
        (
            "custom".into(),
            true,
            reef.as_str(),
            Some(OpKind::Snapshot),
            None,
            Some(ID),
        ),
        (
            format!("reef: final snapshot of workspace {ID} (released)"),
            true,
            "default",
            Some(OpKind::Snapshot),
            Some("final"),
            Some(ID),
        ),
        (
            format!("reef: final snapshot of workspace {ID} (shutdown)"),
            true,
            "default",
            Some(OpKind::Snapshot),
            Some("shutdown"),
            Some(ID),
        ),
        (
            format!("reef: move workspace {ID} to abc"),
            true,
            "default",
            Some(OpKind::Snapshot),
            Some("move"),
            Some(ID),
        ),
        (
            format!("reef: move workspace {ID} to abc"),
            false,
            "default",
            Some(OpKind::MoveWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: move workspace {ID} to abc (move failed)"),
            false,
            "default",
            Some(OpKind::MoveWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: create workspace {ID}"),
            false,
            "default",
            Some(OpKind::CreateWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: release workspace {ID}"),
            false,
            "default",
            Some(OpKind::ReleaseWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: release workspace {ID} (create failed)"),
            false,
            "default",
            Some(OpKind::ReleaseWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: release workspace {ID} (directory missing)"),
            false,
            "default",
            Some(OpKind::ReleaseWorkspace),
            None,
            Some(ID),
        ),
        (
            format!("reef: rematerialize workspace {ID}"),
            false,
            "default",
            Some(OpKind::Other),
            None,
            Some(ID),
        ),
        (
            "reef: import git refs".into(),
            false,
            "default",
            Some(OpKind::ImportRefs),
            None,
            None,
        ),
        (
            "undo: restore to operation 00ff".into(),
            false,
            "default",
            Some(OpKind::Undo),
            None,
            None,
        ),
        (
            "reef: something new".into(),
            false,
            "default",
            Some(OpKind::Other),
            None,
            None,
        ),
        (
            "import git head".into(),
            false,
            "default",
            Some(OpKind::External),
            None,
            None,
        ),
        (
            "snapshot working copy".into(),
            true,
            "default",
            Some(OpKind::External),
            None,
            None,
        ),
        (
            "new empty commit".into(),
            false,
            reef.as_str(),
            Some(OpKind::External),
            None,
            Some(ID),
        ),
    ];
    for (description, is_snapshot, workspace, kind, snapshot, id) in cases {
        let class = oplog::classify(&op_entry(&description, is_snapshot, workspace));
        assert_eq!(class.kind, kind, "{description:?} in {workspace}");
        assert_eq!(class.snapshot_kind.as_deref(), snapshot, "{description:?}");
        assert_eq!(class.workspace_id.as_deref(), id, "{description:?}");
    }
    // `reef.kind` beats the description; an unknown value is `Other`.
    let mut entry = op_entry(&format!("reef: create workspace {ID}"), false, "default");
    entry
        .attributes
        .insert(oplog::ATTR_KIND.into(), "describe".into());
    assert_eq!(oplog::classify(&entry).kind, Some(OpKind::Describe));
    entry
        .attributes
        .insert(oplog::ATTR_KIND.into(), "teleport".into());
    assert_eq!(oplog::classify(&entry).kind, Some(OpKind::Other));
    entry
        .attributes
        .insert(oplog::ATTR_KIND.into(), "snapshot".into());
    entry
        .attributes
        .insert(oplog::ATTR_SNAPSHOT_KIND.into(), "watcher".into());
    entry
        .attributes
        .insert(oplog::ATTR_WORKSPACE.into(), "fedcba9876543210".into());
    let class = oplog::classify(&entry);
    assert_eq!(class.snapshot_kind.as_deref(), Some("watcher"));
    assert_eq!(class.workspace_id.as_deref(), Some("fedcba9876543210"));
    for kind in OpKind::WRITTEN {
        let mut entry = op_entry("x", false, "default");
        entry
            .attributes
            .insert(oplog::ATTR_KIND.into(), kind.as_str().into());
        assert_eq!(oplog::classify(&entry).kind, Some(kind));
    }
    // The root operation is not classified.
    let mut root = op_entry("", false, "default");
    root.parents.clear();
    assert_eq!(oplog::classify(&root).kind, None);
}

// 12
#[tokio::test]
async fn operation_lookup() {
    let mut fx = Fixture::new().await;
    for n in 0..17 {
        let mut tx = fx.tx();
        new_commit(&mut tx, &[], &format!("c{n}")).await;
        fx.commit(tx, &format!("op {n}")).await;
    }
    let head = fx.head();
    let id = head.op_id().hex();
    assert_eq!(oplog::lookup(&head, &id).await.unwrap().id.as_str(), id);
    // The shortest unique prefix, and the next odd length.
    let mut ids = Vec::new();
    let walk = oplog::walk_from(&head, &[id.parse().unwrap()], 0, async |op| {
        ids.push(op.id().hex());
        std::ops::ControlFlow::Continue(())
    })
    .await
    .unwrap();
    assert!(walk.ended);
    let unique = (1..id.len())
        .find(|&len| {
            ids.iter()
                .filter(|other| other.starts_with(&id[..len]))
                .count()
                == 1
        })
        .unwrap();
    let odd = if unique % 2 == 1 { unique } else { unique + 1 };
    for len in [unique, odd] {
        assert_eq!(
            oplog::lookup(&head, &id[..len]).await.unwrap().id.as_str(),
            id
        );
    }
    for bad in ["@-", "xyz", "", "ABC"] {
        let error = oplog::lookup(&head, bad).await.unwrap_err();
        assert!(
            matches!(error, oplog::OpLookupError::Invalid(_)),
            "{bad}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    }
    let unknown = "f".repeat(id.len());
    let error = oplog::lookup(&head, &unknown).await.unwrap_err();
    assert!(
        matches!(error, oplog::OpLookupError::NotFound(_)),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::NotFound);
    // Over sixteen operations, two share a first digit.
    let mut seen = BTreeSet::new();
    let shared = ids
        .iter()
        .map(|id| id[..1].to_owned())
        .find(|digit| !seen.insert(digit.clone()))
        .expect("pigeonhole");
    let error = oplog::lookup(&head, &shared).await.unwrap_err();
    assert!(
        matches!(error, oplog::OpLookupError::Ambiguous(_)),
        "{error:?}"
    );
}

// 13
#[tokio::test]
async fn op_walk_resumes() {
    use futures::StreamExt as _;
    let mut fx = Fixture::new().await;
    for n in 0..6 {
        let mut tx = fx.tx();
        new_commit(&mut tx, &[], &format!("c{n}")).await;
        fx.commit(tx, &format!("op {n}")).await;
    }
    // Two concurrent operations, left unmerged.
    let base = fx.head();
    let mut tx = fx.tx();
    new_commit(&mut tx, &[], "left").await;
    fx.commit(tx, "left").await;
    let mut tx = base.start_transaction();
    new_commit(&mut tx, &[], "right").await;
    let right = tx.commit("right").await.unwrap();
    let head = fx.head();
    let heads = oplog::op_heads(&head).await.unwrap();
    assert_eq!(heads.len(), 2);

    let ops: Vec<_> = heads
        .iter()
        .map(|id| async {
            head.loader()
                .load_operation(&jj_lib::op_store::OperationId::try_from_hex(id.as_str()).unwrap())
                .await
                .unwrap()
        })
        .collect::<futures::stream::FuturesOrdered<_>>()
        .collect()
        .await;
    let expected: Vec<String> = jj_lib::op_walk::walk_ancestors(&ops)
        .map(|op| op.unwrap().id().hex())
        .collect()
        .await;
    let walk = |skip: usize| {
        let head = head.clone();
        let heads = heads.clone();
        async move {
            let mut got = Vec::new();
            let walk = oplog::walk_from(&head, &heads, skip, async |op| {
                got.push(op.id().hex());
                std::ops::ControlFlow::Continue(())
            })
            .await
            .unwrap();
            (got, walk)
        }
    };
    let (all, full) = walk(0).await;
    assert_eq!(all, expected);
    assert!(all.contains(&right.op_id().hex()));
    assert_eq!(full.examined, expected.len());
    let (tail, resumed) = walk(3).await;
    assert_eq!(tail, expected[3..]);
    assert_eq!(
        resumed.last_skipped.as_ref().map(ToString::to_string),
        Some(expected[2].clone())
    );
    // Walking wrote nothing: the heads are as they were.
    assert_eq!(oplog::op_heads(&head).await.unwrap(), heads);
    // A visitor can stop the walk.
    let mut seen = 0;
    let stopped = oplog::walk_from(&head, &heads, 0, async |_| {
        seen += 1;
        if seen == 2 {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
    .await
    .unwrap();
    assert!(!stopped.ended);
    assert_eq!(stopped.examined, 2);
}

/// The ids `walk_after` returns from `heads` after `frontier`.
async fn after(
    head: &ReadonlyRepo,
    heads: &[&str],
    frontier: &[&str],
    limit: usize,
) -> oplog::OpRange {
    let heads: Vec<_> = heads.iter().map(|id| id.parse().unwrap()).collect();
    let frontier = oplog::Frontier::new(frontier.iter().map(|id| id.parse().unwrap()));
    oplog::walk_after(head, &heads, &frontier, limit)
        .await
        .unwrap()
}

fn range_ids(range: &oplog::OpRange) -> Vec<String> {
    range.operations.iter().map(|op| op.id().hex()).collect()
}

/// Records an operation on `fx` and returns its id.
async fn record(fx: &mut Fixture, name: &str) -> String {
    let mut tx = fx.tx();
    new_commit(&mut tx, &[], name).await;
    fx.commit(tx, name).await;
    fx.head().op_id().hex()
}

// R2.09 1
#[tokio::test]
async fn walk_after_is_parents_first() {
    let mut fx = Fixture::new().await;
    let start = fx.head().op_id().hex();
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(record(&mut fx, &format!("op {n}")).await);
    }
    let head = fx.head();
    let top = ids[4].as_str();
    let range = after(&head, &[top], &[&start], 100).await;
    assert_eq!(range_ids(&range), ids);
    assert!(!range.truncated);
    assert!(range.unreached.is_empty(), "{:?}", range.unreached);
    assert!(
        range_ids(&after(&head, &[top], &[top], 100).await).is_empty(),
        "{:?}",
        range_ids(&after(&head, &[top], &[top], 100).await)
    );
    let root = head.op_store().root_operation_id().hex();
    let all = after(&head, &[top], &[&root], 100).await;
    assert_eq!(all.operations.len(), 6, "the init operation and five");
    assert_eq!(range_ids(&all)[1..], ids);
    assert_eq!(range_ids(&after(&head, &[top], &[], 100).await)[0], root);
    let cut = after(&head, &[top], &[&start], 3).await;
    assert!(cut.truncated);
    assert!(cut.operations.is_empty(), "{:?}", cut.operations);
}

// R2.09 2
#[tokio::test]
async fn walk_after_on_concurrent_heads() {
    let mut fx = Fixture::new().await;
    let base_id = record(&mut fx, "base").await;
    let base = fx.head();
    let b1 = record(&mut fx, "b1").await;
    let mut tx = base.start_transaction();
    new_commit(&mut tx, &[], "b2").await;
    let b2 = tx.commit("b2").await.unwrap().op_id().hex();
    let heads_before = oplog::op_heads(&fx.head()).await.unwrap();
    assert_eq!(heads_before.len(), 2);
    let merged = fx.head().loader().load_at_head().await.unwrap();
    let m = merged.op_id().hex();
    assert_eq!(merged.operation().parent_ids().len(), 2);

    let range = after(&merged, &[&m], &[&base_id], 100).await;
    let got = range_ids(&range);
    assert_eq!(got.len(), 3);
    assert_eq!(got[2], m);
    assert_eq!(
        got[..2].iter().collect::<BTreeSet<_>>(),
        [&b1, &b2].into_iter().collect()
    );
    let mut frontier = oplog::Frontier::new([base_id.parse().unwrap()]);
    let mut seen = Vec::new();
    for op in &range.operations {
        frontier.advance(&oplog::entry(op));
        seen.push(
            frontier
                .ids()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        );
    }
    let mut both = vec![b1.clone(), b2.clone()];
    both.sort();
    assert_eq!(seen[1], both);
    assert_eq!(seen[2], vec![m.clone()]);
    let mut first = vec![got[0].clone()];
    first.sort();
    assert_eq!(seen[0], first);

    let other = if got[0] == b1 { &b2 } else { &b1 };
    assert_eq!(
        range_ids(&after(&merged, &[&m], &[&got[0]], 100).await),
        vec![other.clone(), m.clone()]
    );
    assert_eq!(
        range_ids(&after(&merged, &[&m], &[&b1, &b2], 100).await),
        vec![m.clone()]
    );
    // Walking wrote nothing.
    assert_eq!(
        oplog::op_heads(&merged).await.unwrap(),
        vec![m.parse().unwrap()]
    );
}

// R2.09 3
#[tokio::test]
async fn walk_after_reports_unreached() {
    let mut fx = Fixture::new().await;
    record(&mut fx, "base").await;
    let base = fx.head();
    let kept = record(&mut fx, "kept").await;
    let mut tx = base.start_transaction();
    new_commit(&mut tx, &[], "dropped").await;
    let dropped = tx.commit("dropped").await.unwrap().op_id().clone();
    // As `jj op abandon` leaves it: in the store, not a head.
    fx.head()
        .op_heads_store()
        .update_op_heads(
            std::slice::from_ref(&dropped),
            &jj_lib::op_store::OperationId::try_from_hex(&kept).unwrap(),
        )
        .await
        .unwrap();
    let head = fx.head();
    assert_eq!(
        oplog::op_heads(&head).await.unwrap(),
        vec![kept.parse().unwrap()]
    );
    let range = after(&head, &[&kept], &[&dropped.hex()], 100).await;
    assert_eq!(
        range
            .unreached
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![dropped.hex()]
    );
    assert!(range_ids(&range).contains(&kept));
    assert!(
        after(&head, &[&kept], &[&kept], 100)
            .await
            .unreached
            .is_empty(),
        "{:?}",
        after(&head, &[&kept], &[&kept], 100).await.unreached
    );
}

// 9a (an amendment): past git's rename limit, a rename is a delete and an add.
#[tokio::test]
async fn renames_stop_at_the_limit() {
    use diff::{ChangeKind, DiffBase, DiffFormat};
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root = head.store().root_commit();
    let long: Vec<u8> = (0..200)
        .flat_map(|n| format!("line {n}\n").into_bytes())
        .collect();
    let base = tree_with(&head, &root.tree(), &[("old", Entry::File(&long))]).await;
    let mut entries = vec![
        ("old".to_owned(), None),
        ("new".to_owned(), Some(long.clone())),
    ];
    for n in 0..diff::RENAME_LIMIT {
        entries.push((format!("bulk/{n:04}"), Some(format!("{n}\n").into_bytes())));
    }
    let entries: Vec<(&str, Entry<'_>)> = entries
        .iter()
        .map(|(path, bytes)| {
            (
                path.as_str(),
                bytes.as_deref().map_or(Entry::Gone, Entry::File),
            )
        })
        .collect();
    let next = tree_with(&head, &base, &entries).await;
    let mut tx = fx.tx();
    let first = commit_tree(&mut tx, &[&root], base, "first").await;
    let second = commit_tree(&mut tx, &[&first], next, "second").await;
    fx.commit(tx, "setup").await;
    let mut whole = options(DiffFormat::Summary);
    whole.limit = 2 * diff::RENAME_LIMIT;
    let page = diff_of(&fx, &DiffBase::Parents, &second, &whole).await;
    assert_eq!(page.total, diff::RENAME_LIMIT + 2);
    let kind = |path: &str| {
        page.files
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.kind)
    };
    assert_eq!(kind("new"), Some(ChangeKind::Added));
    assert_eq!(kind("old"), Some(ChangeKind::Deleted));
}

/// Every path of the diff, paged `limit` at a time.
async fn paged_paths(fx: &Fixture, to: &Commit, limit: usize) -> Vec<String> {
    let mut options = options(diff::DiffFormat::Summary);
    options.limit = limit;
    let mut paths = Vec::new();
    loop {
        let page = diff_of(fx, &diff::DiffBase::Parents, to, &options).await;
        paths.extend(page.files.iter().map(|f| f.path.clone()));
        if !page.more {
            return paths;
        }
        options.after = diff::check_paths(&[paths.last().unwrap().clone()])
            .unwrap()
            .pop();
    }
}

// 8a (from the review): pages follow jj's path order, by component, so a
// directory and a file named like it neither skip nor repeat a path.
#[tokio::test]
async fn diff_pages_follow_path_order() {
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root = head.store().root_commit();
    let tree = tree_with(
        &head,
        &root.tree(),
        &[
            ("src/foo.rs", Entry::File(b"a\n")),
            ("src/foo/bar.rs", Entry::File(b"b\n")),
            ("src/foo-x.rs", Entry::File(b"c\n")),
            ("src/zed.rs", Entry::File(b"d\n")),
        ],
    )
    .await;
    let mut tx = fx.tx();
    let commit = commit_tree(&mut tx, &[&root], tree, "files").await;
    fx.commit(tx, "setup").await;
    let all = paged_paths(&fx, &commit, 100).await;
    assert_eq!(all.len(), 4);
    for limit in 1..4 {
        assert_eq!(paged_paths(&fx, &commit, limit).await, all, "limit {limit}");
    }
}

// 9b (from the review): a hunk cut to its old side starts its emptied new
// side at the line before, as git writes it.
#[tokio::test]
async fn cut_hunks_start_empty_sides_before() {
    use diff::{DiffBase, DiffFormat};
    let mut fx = Fixture::new().await;
    let head = fx.head();
    let root = head.store().root_commit();
    let old = tree_with(&head, &root.tree(), &[("f", Entry::File(b"a\nx\nb\n"))]).await;
    let new = tree_with(&head, &old, &[("f", Entry::File(b"a\ny\nb\n"))]).await;
    let mut tx = fx.tx();
    let first = commit_tree(&mut tx, &[&root], old, "old").await;
    let second = commit_tree(&mut tx, &[&first], new, "new").await;
    fx.commit(tx, "setup").await;
    let mut cut = options(DiffFormat::Hunks);
    cut.context = 0;
    cut.max_lines = 1;
    let page = diff_of(&fx, &DiffBase::Parents, &second, &cut).await;
    let hunk = &page.files[0].hunks[0];
    assert_eq!(hunk.lines, "-x\n");
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (2, 1, 1, 0)
    );
    assert!(page.files[0].truncated);
}

// 10a (from the review): a commit rewritten twice in one transaction is one
// rewrite of the commit visible before, through the intermediate one.
#[tokio::test]
async fn effects_follow_rewrites_within_an_operation() {
    use effects::ChangeEffectKind::Rewritten;
    let mut fx = Fixture::new().await;
    let wc = fx.repo.wc_commit().unwrap();
    let mut tx = fx.tx();
    let once = tx
        .repo_mut()
        .rewrite_commit(&wc)
        .set_description("once")
        .write()
        .await
        .unwrap();
    let twice = tx
        .repo_mut()
        .rewrite_commit(&once)
        .set_description("twice")
        .write()
        .await
        .unwrap();
    fx.commit(tx, "rewrite twice").await;
    assert_eq!(
        change_effects(&effects_of_head(&fx, 1000).await),
        [(
            change_hex(&wc),
            Rewritten,
            Some(twice.id().hex()),
            Some(wc.id().hex())
        )]
    );
}
