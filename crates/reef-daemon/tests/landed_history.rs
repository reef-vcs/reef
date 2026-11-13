//! The landed history projection (R3.16, the brief's tests 4 to 9): a
//! summarized landing and its record; a batch's trailers, agents and
//! reviewers, with an agent's own `Reef-*` and `Reviewed-by` trailers
//! removed; `keep` and no `[history]` landing commits as written;
//! `builtin:description`; empty landings; and what a landing keeps after GC.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, CreateIdentityRequest, CreateWorkspaceRequest, DescribeChangeRequest,
    EditFilesRequest, FileEdit, GetChangeRequest, GetLandingRequest, IdentityKind as Kind,
    LandChangeRequest, LandingOutcome as Outcome, LandingReason as Reason, ListProvenanceRequest,
    TransitionChangeRequest, UndoOperationRequest, create_workspace_request, edit_files_request,
    file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Verdict, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNER: &str = "tester@host";
const DAY: Duration = Duration::from_hours(24);

/// A policy whose landings are judged on submitted trees, batched up to
/// three with a wait of `wait`, with `history` after it. Changes under
/// `signed/` need alice's sign-off.
fn policy(wait: &str, history: &str) -> String {
    format!(
        r#"
version = 1

[owners]
identities = ["alice"]

[[signoff]]
name = "review"
paths = ["signed/**"]
identities = ["alice"]

[landing]
checks = "proposal"
batch_max = 3
batch_wait = "{wait}"
{history}"#
    )
}

async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

fn reason(error: &ClientError) -> String {
    match error {
        ClientError::Rpc { reason, .. } => reason.clone(),
        other => panic!("{other:?}"),
    }
}

async fn change_at(client: &Client, repo: &str, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// A new commit on `on` writing each of `files`, described `description`.
async fn commit_on(
    client: &Client,
    repo: &str,
    on: &str,
    files: &[(&str, &str)],
    description: &str,
) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: files
                .iter()
                .map(|(path, text)| FileEdit {
                    path: (*path).to_owned(),
                    action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
                    executable: None,
                })
                .collect(),
            description: Some(description.to_owned()),
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`
/// would.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&name),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

/// `alice`, a person with an email, and `main` with `policy` on a base.
async fn setup(client: &Client, repo: &SyntheticRepo, policy: &str) -> v1::Change {
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: Kind::Person.into(),
            display_name: "Alice Example".to_owned(),
            email: "alice@example.com".to_owned(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[("README.md", "base\n"), ("reef-policy.toml", policy)],
        "base",
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    main
}

async fn propose(client: &Client, repo: &str, commit: &str) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: commit.to_owned(),
            to: v1::LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

/// Proposes and asks to land `commit`, waiting `wait_ms`.
async fn land(client: &Client, repo: &str, commit: &str, wait_ms: u32) -> v1::Landing {
    propose(client, repo, commit).await;
    client
        .land_change(LandChangeRequest {
            repo: repo.to_owned(),
            revision: commit.to_owned(),
            wait_ms,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap()
}

/// The change's latest landing, once it is no longer queued.
async fn finished(client: &Client, repo: &str, change: &str) -> v1::Landing {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(response) = client
            .get_landing(GetLandingRequest {
                repo: repo.to_owned(),
                change: change.to_owned(),
                ..GetLandingRequest::default()
            })
            .await
        {
            let landing = response.landing.unwrap();
            if landing.outcome() != Outcome::Queued {
                return landing;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the landing never finished"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Alice's sign-off on `tree`, as a person.
async fn sign_off(client: &Client, repo: &str, tree: &str) {
    let key: SigningKey = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap()
        .signing_key;
    let started = SystemTime::now();
    let claim = Claim {
        kind: "signoff".to_owned(),
        result: Verdict::Pass,
        target: None,
        started_on: started,
        finished_on: started + Duration::from_secs(1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    };
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: String::new(),
            envelope: claim
                .sign(&TreeDigest::git(tree).unwrap(), &key)
                .unwrap()
                .to_bytes(),
            public_key: key.public_key().spki().to_vec(),
            revision: String::new(),
        })
        .await
        .unwrap();
}

/// A workspace on a new change on `on`, with `file` written and
/// snapshotted (a provenance record) and described `description`; its
/// change.
async fn workspace_change(
    client: &Client,
    repo: &str,
    on: &str,
    file: &str,
    description: &str,
) -> (v1::Workspace, v1::Change) {
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            start: Some(create_workspace_request::Start::NewOn(on.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let path = Path::new(&ws.path).join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "agent\n").unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    let described = client
        .describe_change(DescribeChangeRequest {
            repo: repo.to_owned(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: description.to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let change = described.change.unwrap();
    (ws, change)
}

/// Unix milliseconds `ago` before now.
fn ms_ago(ago: Duration) -> i64 {
    let at = SystemTime::now() - ago;
    i64::try_from(at.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

// 4 and 7. A summarized landing: the description with its own sign-off
// kept and the default trailers appended, the same change and tree, the
// record; an undescribed change is blocked by builtin:description.
#[tokio::test(flavor = "multi_thread")]
async fn summarized_landing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("0s", "[history]\n")).await;
    let change = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("a.txt", "a\n")],
        "Fix the parser\n\nSigned-off-by: A B <a@b>\n",
    )
    .await;
    let landing = land(client, &repo.id, &change.commit_id, 60_000).await;
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    let history = landing.history.clone().unwrap();
    assert_eq!(history.message, "summary");
    assert_eq!(history.trailers, ["change", "landing"]);
    let landed = &landing.changes[0];
    assert!(landed.summarized && !landed.rebased);
    assert_ne!(landed.landed_commit_id, change.commit_id);
    assert_eq!(landed.landed_tree_id, change.tree_id);
    let head = change_at(client, &repo.id, "main").await;
    assert_eq!(head.commit_id, landed.landed_commit_id);
    assert_eq!(head.change_id, change.change_id);
    assert_eq!(head.tree_id, change.tree_id);
    assert_eq!(
        head.description,
        format!(
            "Fix the parser\n\nSigned-off-by: A B <a@b>\nReef-Change: {}\nReef-Landing: {}\n",
            change.change_id, landing.id
        )
    );
    // The queued commit is still readable through the operation log.
    let queued = change_at(client, &repo.id, &change.commit_id).await;
    assert!(queued.hidden);
    assert_eq!(
        queued.description,
        "Fix the parser\n\nSigned-off-by: A B <a@b>\n"
    );
    // The land is one operation, which undo refuses.
    let error = client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: landing.operation_id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "OPERATION_NOT_UNDOABLE");

    // No description: the gate blocks it under `summary`.
    let bare = commit_on(client, &repo.id, &head.commit_id, &[("b.txt", "b\n")], "").await;
    let blocked = land(client, &repo.id, &bare.commit_id, 60_000).await;
    assert_eq!(blocked.outcome(), Outcome::Blocked, "{blocked:#?}");
    assert_eq!(blocked.reason(), Reason::Policy);
    let rule = blocked.changes[0]
        .evaluation
        .as_ref()
        .unwrap()
        .rules
        .iter()
        .find(|rule| rule.name == "builtin:description")
        .cloned()
        .unwrap();
    assert_eq!(rule.status(), v1::PolicyRuleStatus::Unsatisfied);
    assert!(rule.message.contains("reef describe"), "{}", rule.message);
    daemon.stop().await;
}

// 5. A batch of three, each its own commit with every trailer set: the
// batch, the agent of a workspace's change, a reviewer; forged trailers
// gone.
#[tokio::test(flavor = "multi_thread")]
async fn batch_projection() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(
        client,
        &repo,
        &policy(
            "5s",
            "[history]\ntrailers = [\"change\", \"landing\", \"agents\", \"signoffs\"]\n",
        ),
    )
    .await;
    let (_ws, agents) = workspace_change(
        client,
        &repo.id,
        &main.commit_id,
        "w.txt",
        "From a workspace\n\nReef-Landing: lnd_forged\nReviewed-by: Mallory <m@x>\n",
    )
    .await;
    let plain = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("p.txt", "p\n")],
        "Plain",
    )
    .await;
    let signed = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("signed/s.txt", "s\n")],
        "Signed",
    )
    .await;
    sign_off(client, &repo.id, &signed.tree_id).await;
    for change in [&agents, &plain, &signed] {
        let queued = land(client, &repo.id, &change.commit_id, 0).await;
        assert_eq!(queued.outcome(), Outcome::Queued, "{queued:#?}");
    }
    let mut batch = None;
    let mut descriptions = Vec::new();
    for change in [&agents, &plain, &signed] {
        let landing = finished(client, &repo.id, &change.change_id).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        assert!(landing.changes[0].summarized);
        assert_eq!(
            landing.history.as_ref().unwrap().trailers,
            ["change", "landing", "agents", "signoffs"]
        );
        let batch_id = batch.get_or_insert_with(|| landing.batch_id.clone());
        assert_eq!(*batch_id, landing.batch_id, "one batch");
        let landed = change_at(client, &repo.id, &landing.changes[0].landed_commit_id).await;
        assert_eq!(landed.change_id, change.change_id);
        assert!(
            landed
                .description
                .contains(&format!("\nReef-Batch: {batch_id}\n")),
            "{}",
            landed.description
        );
        assert!(
            landed
                .description
                .contains(&format!("Reef-Landing: {}\n", landing.id))
        );
        descriptions.push(landed.description);
    }
    assert!(
        descriptions[0].starts_with("From a workspace\n\nReef-Change: "),
        "{}",
        descriptions[0]
    );
    assert!(
        descriptions[0].contains("\nReef-Agent: "),
        "{}",
        descriptions[0]
    );
    assert!(!descriptions[0].contains("lnd_forged") && !descriptions[0].contains("Mallory"));
    assert!(
        !descriptions[1].contains("Reef-Agent"),
        "{}",
        descriptions[1]
    );
    assert!(
        descriptions[2].ends_with("Reviewed-by: Alice Example <alice@example.com>\n"),
        "{}",
        descriptions[2]
    );
    daemon.stop().await;
}

// 6 and 7. With no `[history]`, and with `keep`, a fast-forward lands its
// queued commit unchanged, and a change needs no description.
#[tokio::test(flavor = "multi_thread")]
async fn keep_and_no_table() {
    for history in ["", "[history]\nmessage = \"keep\"\n"] {
        let (daemon, repo) = fixture().await;
        let client = &daemon.client;
        let main = setup(client, &repo, &policy("0s", history)).await;
        let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")], "").await;
        let landing = land(client, &repo.id, &change.commit_id, 60_000).await;
        assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
        assert_eq!(landing.history.as_ref().unwrap().message, "keep");
        assert_eq!(
            landing.history.as_ref().unwrap().trailers,
            Vec::<String>::new()
        );
        assert!(!landing.changes[0].summarized);
        assert_eq!(landing.changes[0].landed_commit_id, change.commit_id);
        daemon.stop().await;
    }
}

// 8. Two proposals with the same edit in one batch: the first lands, the
// second returns `empty`; a third with the same edit is blocked at the
// gate.
#[tokio::test(flavor = "multi_thread")]
async fn empty_member() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("3s", "")).await;
    let edit = [("same.txt", "same\n")];
    let first = commit_on(client, &repo.id, &main.commit_id, &edit, "First").await;
    let second = commit_on(client, &repo.id, &main.commit_id, &edit, "Second").await;
    for change in [&first, &second] {
        let queued = land(client, &repo.id, &change.commit_id, 0).await;
        assert_eq!(queued.outcome(), Outcome::Queued, "{queued:#?}");
    }
    let landed = finished(client, &repo.id, &first.change_id).await;
    assert_eq!(landed.outcome(), Outcome::Landed, "{landed:#?}");
    let returned = finished(client, &repo.id, &second.change_id).await;
    assert_eq!(returned.outcome(), Outcome::Dequeued, "{returned:#?}");
    assert_eq!(returned.reason(), Reason::Empty);
    assert!(
        returned.message.contains("already on main"),
        "{returned:#?}"
    );
    let state = change_at(client, &repo.id, &second.commit_id).await;
    assert_eq!(
        state.lifecycle.unwrap().state(),
        v1::LifecycleState::Proposed
    );

    let third = commit_on(client, &repo.id, &main.commit_id, &edit, "Third").await;
    let blocked = land(client, &repo.id, &third.commit_id, 60_000).await;
    assert_eq!(blocked.outcome(), Outcome::Blocked, "{blocked:#?}");
    assert_eq!(blocked.reason(), Reason::Empty);
    daemon.stop().await;
}

// 9. After GC drops the land operation, the record, the change's provenance
// and its evidence pins are still there.
#[tokio::test(flavor = "multi_thread")]
async fn history_after_gc() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = Duration::ZERO;
        config.file.provenance.retention = DAY;
        config.file.provenance.compact_after = DAY;
        config.file.evidence.retention = DAY;
    })
    .await;
    let client = &daemon.client;
    let main = setup(client, &repo, &policy("0s", "[history]\n")).await;
    let (ws, change) = workspace_change(
        client,
        &repo.id,
        &main.commit_id,
        "signed/w.txt",
        "From a workspace",
    )
    .await;
    sign_off(client, &repo.id, &change.tree_id).await;
    let landing = land(client, &repo.id, &change.commit_id, 60_000).await;
    assert_eq!(landing.outcome(), Outcome::Landed, "{landing:#?}");
    assert!(landing.changes[0].summarized);
    let pinned = landing.changes[0].pinned_evidence_ids.clone();
    assert_eq!(pinned.len(), 1, "{landing:#?}");
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let records = async || {
        client
            .list_provenance(
                ListProvenanceRequest {
                    repo: repo.id.clone(),
                    change_ids: vec![change.change_id.clone()],
                    ..ListProvenanceRequest::default()
                },
                0,
            )
            .await
            .unwrap()
            .items
            .len()
    };
    assert!(records().await > 0);
    {
        let conn = rusqlite::Connection::open(daemon.state_dir().join("reef.sqlite3")).unwrap();
        conn.execute(
            "UPDATE provenance SET recorded_at = ?1 WHERE change_id = ?2",
            rusqlite::params![ms_ago(30 * DAY), change.change_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE evidence SET added_at = ?1",
            rusqlite::params![ms_ago(30 * DAY)],
        )
        .unwrap();
    }
    let reports = client
        .collect_garbage(&repo.id, false)
        .await
        .unwrap()
        .reports;
    assert_eq!(reports[0].provenance_error, "", "{reports:?}");
    assert_eq!(reports[0].evidence_error, "", "{reports:?}");
    let after = client
        .get_landing(GetLandingRequest {
            repo: repo.id.clone(),
            id: landing.id.clone(),
            ..GetLandingRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap();
    assert!(after.operation_collected, "{after:#?}");
    assert_eq!(after.changes[0].queued_commit_id, change.commit_id);
    assert_eq!(
        after.changes[0].landed_commit_id,
        landing.changes[0].landed_commit_id
    );
    assert!(after.changes[0].summarized);
    assert_eq!(after.history, landing.history);
    assert_eq!(after.changes[0].pinned_evidence_ids, pinned);
    assert!(client.get_evidence(&repo.id, &pinned[0]).await.is_ok());
    assert!(records().await > 0, "the landed change's provenance stays");
    daemon.stop().await;
}
