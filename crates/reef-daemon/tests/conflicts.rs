//! Conflict objects through the daemon (R3.25, tests 7 to 10): two
//! workspaces write one file from one base, and the second change is
//! rebased onto the first outside the daemon, as a user's `jj rebase`
//! would; `ListConflicts` and `GetConflict` then report the conflict, each
//! side's source verified by content, and each side's provenance.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use reef_api::v1::{
    self, ConflictAttribution, ConflictFileKind, ConflictTermRole, CreateWorkspaceRequest,
    GetConflictRequest, ListConflictsRequest, Workspace,
};
use reef_daemon::client::Client;
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::synth::Preset;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        config.file.remote.enabled = true;
        "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
        config.file.remote.names = vec!["localhost".to_owned()];
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn create(client: &Client, repo: &str, owner: &str) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn write(client: &Client, ws: &Workspace, file: &str, text: &str) -> String {
    std::fs::write(Path::new(&ws.path).join(file), text).unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    snapshot.commit_id
}

/// The repository at `root`, loaded at its head with jj-lib.
fn load(root: &Path) -> std::sync::Arc<ReadonlyRepo> {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    let settings = test_settings();
    let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &settings,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap()
}

/// `commit` rebased onto `onto` outside the daemon, as `jj rebase -r` would;
/// the new commit's id.
fn rebase(root: &Path, commit: &str, onto: &str) -> String {
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let old = pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(commit).unwrap()))
        .unwrap();
    let new = pollster::block_on(jj_lib::rewrite::rebase_commit(
        tx.repo_mut(),
        old.clone(),
        vec![CommitId::try_from_hex(onto).unwrap()],
    ))
    .unwrap();
    tx.repo_mut()
        .set_rewritten_commit(old.id().clone(), new.id().clone());
    pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
    pollster::block_on(tx.commit("rebase")).unwrap();
    new.id().hex()
}

/// A child of `parent` adding `file`, outside the daemon; its id.
fn child(root: &Path, parent: &str, file: &str) -> String {
    use jj_lib::backend::{CopyId, TreeValue};
    use jj_lib::repo_path::RepoPathBuf;
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let parent =
        pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(parent).unwrap()))
            .unwrap();
    let path = RepoPathBuf::from_internal_string(file).unwrap();
    let id = pollster::block_on(store.write_file(&path, &mut &b"child\n"[..])).unwrap();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(parent.tree());
    builder.set_or_remove(
        path,
        Some(TreeValue::File {
            id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    let tree = pollster::block_on(builder.write_tree()).unwrap();
    let commit = pollster::block_on(
        tx.repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description("child")
            .write(),
    )
    .unwrap();
    pollster::block_on(tx.commit("child")).unwrap();
    commit.id().hex()
}

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`
/// would.
fn set_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let repo = load(root);
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

struct Scene {
    a: Workspace,
    b: Workspace,
    a_commit: String,
    b_commit: String,
    rebased: String,
    before: String,
}

/// Workspaces `a` and `b` each write `conflict.txt`; `b` is released and
/// its change rebased onto `a`'s.
async fn scene(daemon: &TestDaemon, repo: &SyntheticRepo) -> Scene {
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a@test").await;
    let b = create(client, &repo.id, "agent-b@test").await;
    let a_commit = write(client, &a, "conflict.txt", "one\nfrom a\nthree\n").await;
    let b_commit = write(client, &b, "conflict.txt", "one\nfrom b\nthree\n").await;
    client
        .release_workspace(&repo.id, &b.id, false, false)
        .await
        .unwrap();
    let before = client
        .list_operations_limited(
            v1::ListOperationsRequest {
                repo: repo.id.clone(),
                ..v1::ListOperationsRequest::default()
            },
            1,
        )
        .await
        .unwrap()
        .items[0]
        .id
        .clone();
    let rebased = rebase(&repo.root, &b_commit, &a_commit);
    Scene {
        a,
        b,
        a_commit,
        b_commit,
        rebased,
        before,
    }
}

fn source(term: &v1::ConflictTerm) -> &v1::ConflictSource {
    term.source.as_ref().unwrap()
}

// 7, 9
#[tokio::test]
async fn lists_conflicts_with_verified_sides_and_provenance() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let listed = client
        .list_conflicts(
            ListConflictsRequest {
                repo: repo.id.clone(),
                ..ListConflictsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let conflict = &listed[0];
    assert_eq!(conflict.kind(), v1::ConflictKind::Tree);
    let change = conflict.change.as_ref().unwrap();
    assert_eq!(change.commit_id, s.rebased);
    assert_eq!(change.change_id, s.b.change_id);
    assert!(change.conflict);
    assert_eq!(conflict.arity, 2);
    assert_eq!(conflict.files_total, 1);
    assert_eq!(conflict.introduced, 1);
    assert_eq!(conflict.files[0].path, "conflict.txt");
    assert!(conflict.files[0].terms.is_empty(), "no values in a listing");
    assert!(conflict.id.starts_with("cfl_"));
    let roles: Vec<_> = conflict.terms.iter().map(v1::ConflictTerm::role).collect();
    assert_eq!(
        roles,
        [
            ConflictTermRole::Side,
            ConflictTermRole::Base,
            ConflictTermRole::Side
        ]
    );
    // Side 0 is `a`'s snapshot, side 1 `b`'s, now hidden; both verified.
    let side0 = &conflict.terms[0];
    let side1 = &conflict.terms[2];
    assert_eq!(side0.origin, "rebase destination");
    assert_eq!(side1.origin, "rebased revision");
    assert_eq!(source(side0).commit_id, s.a_commit);
    assert_eq!(source(side1).commit_id, s.b_commit);
    assert_eq!(source(side0).attribution(), ConflictAttribution::Verified);
    assert_eq!(source(side1).attribution(), ConflictAttribution::Verified);
    // Both sides add the file, so the base holds nothing to check: its
    // label's commit stands, declared.
    assert_eq!(
        source(&conflict.terms[1]).attribution(),
        ConflictAttribution::Label
    );
    assert!(conflict.terms[1].provenance.is_none(), "bases carry none");
    // Each side's provenance names its workspace's session.
    let p0 = side0.provenance.as_ref().unwrap();
    let p1 = side1.provenance.as_ref().unwrap();
    assert_eq!(p0.change_id, s.a.change_id);
    assert_eq!(p0.commit_record.as_ref().unwrap().commit_id, s.a_commit);
    assert_eq!(p0.sessions[0].agent, "agent-a@test");
    assert!(p0.sessions[0].ended_at.is_none(), "a is still active");
    assert_eq!(p1.commit_record.as_ref().unwrap().commit_id, s.b_commit);
    assert_eq!(p1.sessions[0].agent, "agent-b@test");
    assert!(p1.sessions[0].ended_at.is_some(), "b was released");
    assert!(p0.records >= 1 && p0.invalid_records == 0);
    assert_eq!(
        p1.latest_record.as_ref().unwrap().workspace_id,
        s.b.id,
        "{p1:?}"
    );
    // The conflicted change's own provenance is b's.
    let own = conflict.change_provenance.as_ref().unwrap();
    assert_eq!(own.change_id, s.b.change_id);
    assert!(own.commit_record.is_none(), "the rebase wrote no record");
    daemon.stop().await;
}

// 7: a child inherits the conflict; `introduced_only` leaves it out; pages
// return each once.
#[tokio::test]
async fn inherited_conflicts_and_pages() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let kid = child(&repo.root, &s.rebased, "kid.txt");
    let request = ListConflictsRequest {
        repo: repo.id.clone(),
        ..ListConflictsRequest::default()
    };
    let all = client
        .list_conflicts(request.clone(), 0)
        .await
        .unwrap()
        .items;
    let commits: Vec<&str> = all
        .iter()
        .map(|c| c.change.as_ref().unwrap().commit_id.as_str())
        .collect();
    assert_eq!(commits, [kid.as_str(), s.rebased.as_str()]);
    assert_eq!(all[0].introduced, 0);
    assert!(all[0].files[0].inherited);
    assert_eq!(all[0].files[0].id, all[1].files[0].id);
    assert_ne!(all[0].id, all[1].id);
    let introduced = client
        .list_conflicts(
            ListConflictsRequest {
                introduced_only: true,
                ..request.clone()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(introduced.len(), 1);
    assert_eq!(introduced[0].change.as_ref().unwrap().commit_id, s.rebased);
    // One per page.
    let first = client
        .list_conflicts_page(ListConflictsRequest {
            page_size: 1,
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(first.conflicts.len(), 1);
    assert_ne!(first.next_page_token, "");
    let second = client
        .list_conflicts_page(ListConflictsRequest {
            page_size: 1,
            page_token: first.next_page_token.clone(),
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(
        second.conflicts[0].change.as_ref().unwrap().commit_id,
        s.rebased
    );
    assert_eq!(second.at_operation, first.at_operation);
    daemon.stop().await;
}

// 8
#[tokio::test]
async fn gets_one_conflict_with_values_and_hunks() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let request = GetConflictRequest {
        repo: repo.id.clone(),
        revision: s.rebased.clone(),
        hunks: true,
        ..GetConflictRequest::default()
    };
    let got = client
        .get_conflict(request.clone())
        .await
        .unwrap()
        .conflict
        .unwrap();
    let file = &got.files[0];
    assert_eq!(file.terms.len(), 3);
    // Both sides add the file; the base has none.
    let types: Vec<_> = file.terms.iter().map(v1::ConflictValue::r#type).collect();
    assert_eq!(
        types,
        [
            v1::ConflictValueType::File,
            v1::ConflictValueType::Absent,
            v1::ConflictValueType::File
        ]
    );
    assert!(!file.terms[0].id.is_empty() && file.terms[1].id.is_empty());
    assert_eq!(file.kind(), ConflictFileKind::AddAdd);
    let conflicted: Vec<_> = file.hunks.iter().filter(|h| !h.resolved).collect();
    assert!(!conflicted.is_empty(), "{:?}", file.hunks);
    let texts: Vec<String> = conflicted
        .iter()
        .flat_map(|h| h.terms.iter().map(|t| t.text.clone()))
        .collect();
    assert!(texts.iter().any(|t| t.contains("from a")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("from b")), "{texts:?}");
    // `paths` filters; a path elsewhere returns no files.
    let none = client
        .get_conflict(GetConflictRequest {
            paths: vec!["elsewhere".into()],
            ..request.clone()
        })
        .await
        .unwrap()
        .conflict
        .unwrap();
    assert_eq!(none.files, Vec::new());
    assert_eq!(none.files_total, 1);
    // A clean revision.
    let error = client
        .get_conflict(GetConflictRequest {
            revision: s.a_commit.clone(),
            ..request.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert_eq!(error.reason(), reef_api::error::NOT_CONFLICTED);
    // Too many bytes asked.
    let error = client
        .get_conflict(GetConflictRequest {
            max_file_bytes: (16 << 20) + 1,
            ..request.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::MAX_FILE_BYTES_TOO_LARGE);
    // Before the rebase, the change was not conflicted.
    let error = client
        .get_conflict(GetConflictRequest {
            revision: s.b.change_id.clone(),
            at_operation: s.before.clone(),
            ..request.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::NOT_CONFLICTED, "{error}");
    daemon.stop().await;
}

// 8: a proposal of the conflicted change says where to look.
#[tokio::test]
async fn conflicted_refusals_point_at_the_conflict() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo).await;
    let base = client
        .get_change(v1::GetChangeRequest {
            repo: repo.id.clone(),
            revision: s.a_commit.clone(),
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .parent_commit_ids[0]
        .clone();
    set_bookmark(&repo.root, "main", &base);
    let error = client
        .transition_change(v1::TransitionChangeRequest {
            repo: repo.id.clone(),
            revision: s.rebased.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..v1::TransitionChangeRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        reef_api::error::CHANGE_CONFLICTED,
        "{error}"
    );
    let next = error.advice().unwrap().next.join(" ");
    assert!(next.starts_with("reef conflict show -r"), "{next}");
    let _ = s.a;
    daemon.stop().await;
}

/// A client through the remote listener with a `read` token.
async fn reader(daemon: &TestDaemon) -> Client {
    let deadline = Instant::now() + Duration::from_secs(10);
    let addr = loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            break addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(
            "reader",
            v1::RemoteScope::Read,
            Some(Duration::from_hours(1)),
            "",
        )
        .await
        .unwrap();
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    Client::connect_remote(&target).await.unwrap()
}

// 10 (S-129): a remote `read` token lists and gets conflicts.
#[tokio::test]
async fn remote_readers_list_and_get() {
    let (daemon, repo) = fixture().await;
    let s = scene(&daemon, &repo).await;
    let reader = reader(&daemon).await;
    let listed = reader
        .list_conflicts(
            ListConflictsRequest {
                repo: repo.id.clone(),
                ..ListConflictsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 1);
    let got = reader
        .get_conflict(GetConflictRequest {
            repo: repo.id.clone(),
            revision: s.rebased.clone(),
            hunks: true,
            ..GetConflictRequest::default()
        })
        .await
        .unwrap()
        .conflict
        .unwrap();
    assert_eq!(got.files.len(), 1);
    daemon.stop().await;
}
