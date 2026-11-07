//! Provenance records through the daemon (R3.04, tests 5 to 14): records on
//! snapshots and edits, declarations, the identity a session binds, held
//! keys' life and revocation, tampered rows, reads, GC, and recording that
//! never fails a snapshot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, AbandonChangeRequest, CreateIdentityRequest, CreateWorkspaceRequest,
    DeclareProvenanceRequest, DescribeChangeRequest, EditFilesRequest, FileEdit,
    GetProvenanceRequest, IdentityKind as Kind, IdentityStatus, ListProvenanceRequest,
    ProvenanceDeclaration, RemoteScope, RevokeIdentityRequest, RevokeSessionKeysRequest,
    SessionActor, SignatureCheck, Workspace, file_edit, get_provenance_request,
    revoke_session_keys_request,
};
use reef_daemon::client::Client;
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::config::Config;
use reef_daemon::provenance::FailAt;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn create(
    client: &Client,
    repo: &str,
    provenance: Option<ProvenanceDeclaration>,
) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            provenance,
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Writes `file` in `ws` and snapshots it; the snapshot's commit.
async fn write(client: &Client, ws: &Workspace, file: &str, text: &str) -> String {
    std::fs::write(Path::new(&ws.path).join(file), text).unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    snapshot.commit_id
}

async fn records(
    client: &Client,
    repo: &str,
    request: ListProvenanceRequest,
) -> Vec<v1::ProvenanceRecord> {
    client
        .list_provenance(
            ListProvenanceRequest {
                repo: repo.to_owned(),
                ..request
            },
            0,
        )
        .await
        .unwrap()
        .items
}

async fn of_workspace(client: &Client, repo: &str, ws: &Workspace) -> Vec<v1::ProvenanceRecord> {
    records(
        client,
        repo,
        ListProvenanceRequest {
            workspace_id: ws.id.clone(),
            ..ListProvenanceRequest::default()
        },
    )
    .await
}

async fn by_commit(client: &Client, repo: &str, commit: &str) -> v1::ProvenanceRecord {
    client
        .get_provenance(GetProvenanceRequest {
            repo: repo.to_owned(),
            target: Some(get_provenance_request::Target::CommitId(commit.to_owned())),
            ..GetProvenanceRequest::default()
        })
        .await
        .unwrap()
        .record
        .unwrap()
}

fn status(record: &v1::ProvenanceRecord) -> IdentityStatus {
    record.identity.as_ref().unwrap().status()
}

fn key_of(record: &v1::ProvenanceRecord) -> &v1::SessionKey {
    record.identity.as_ref().unwrap().key.as_ref().unwrap()
}

async fn workspace(client: &Client, id: &str) -> Workspace {
    client
        .get_workspace(v1::GetWorkspaceRequest {
            workspace: id.to_owned(),
            skip_disk_usage: true,
            ..v1::GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn declare(
    client: &Client,
    ws: &Workspace,
    declaration: ProvenanceDeclaration,
) -> Result<v1::DeclareProvenanceResponse, reef_daemon::client::ClientError> {
    client
        .declare_provenance(DeclareProvenanceRequest {
            workspace: ws.id.clone(),
            declaration: Some(declaration),
            ..DeclareProvenanceRequest::default()
        })
        .await
}

// 5.
#[tokio::test(flavor = "multi_thread")]
async fn records_on_snapshots() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let session = ws.provenance.clone().unwrap();
    assert!(session.session_id.starts_with("ses_"));
    assert_eq!(session.actor(), SessionActor::Agent);
    assert!(!session.identity.is_empty(), "the default identity");
    assert_eq!(session.problem, "");

    let commit = write(client, &ws, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(record.signature(), SignatureCheck::Valid);
    assert_eq!(record.commit_id, commit);
    assert_eq!(record.workspace_id, ws.id);
    assert_eq!(record.session_id, session.session_id);
    assert_eq!(record.kind, "explicit");
    assert_eq!(record.sequence, 1);
    assert_eq!(record.agent, OWNER);
    assert_eq!(record.change_id.len(), 32);
    assert_eq!(record.tree_id.len(), 40);
    assert_ne!(record.envelope, Vec::<u8>::new());
    assert_eq!(status(&record), IdentityStatus::Bound);
    let key = key_of(&record);
    assert_eq!(key.custody(), v1::KeyCustody::Daemon);
    assert_eq!(key.session_id, session.session_id);
    assert_eq!(key.actor(), SessionActor::Agent);
    assert_eq!(
        record.identity.as_ref().unwrap().witnessed_at,
        record.recorded_at
    );

    // Nothing changed: no record.
    let again = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(!again.changed);
    assert_eq!(of_workspace(client, &repo.id, &ws).await.len(), 1);

    let second = write(client, &ws, "b.txt", "b\n").await;
    assert_eq!(by_commit(client, &repo.id, &second).await.sequence, 2);
    let listed = workspace(client, &ws.id).await.provenance.unwrap();
    assert_eq!(listed.records, 2);

    // Release records the final snapshot and ends the session.
    std::fs::write(Path::new(&ws.path).join("c.txt"), "c\n").unwrap();
    let released = client
        .release_workspace("", &ws.id, true, false)
        .await
        .unwrap();
    assert!(released.snapshot_taken);
    let all = of_workspace(client, &repo.id, &ws).await;
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].kind, "final");
    let db = &daemon.handle.state().db;
    let ended = db.provenance_session(&session.session_id).unwrap().unwrap();
    assert!(ended.ended_at.is_some());
}

// 5, disabled.
#[tokio::test(flavor = "multi_thread")]
async fn disabled_records_nothing() {
    let (daemon, repo) = fixture_with(|config| config.file.provenance.enabled = false).await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    assert!(ws.provenance.is_none());
    write(client, &ws, "a.txt", "a\n").await;
    assert_eq!(of_workspace(client, &repo.id, &ws).await, Vec::new());
    let error = declare(
        client,
        &ws,
        ProvenanceDeclaration {
            model: Some("m/1".into()),
            ..ProvenanceDeclaration::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), "PROVENANCE_DISABLED");
    let error = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            provenance: Some(ProvenanceDeclaration {
                task: Some("T-1".into()),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "PROVENANCE_DISABLED");
}

// 6.
#[tokio::test(flavor = "multi_thread")]
async fn records_on_edits() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let edit = |workspace: &str| EditFilesRequest {
        repo: repo.id.clone(),
        workspace: workspace.to_owned(),
        target: Some(v1::edit_files_request::Target::Revision("@".into())),
        edits: vec![FileEdit {
            path: "edited.txt".into(),
            action: Some(file_edit::Action::Content(b"x\n".to_vec())),
            ..FileEdit::default()
        }],
        ..EditFilesRequest::default()
    };
    let edited = client.edit_files(edit(&ws.id)).await.unwrap();
    let commit = edited.change.unwrap().commit_id;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(record.kind, "edit");
    assert_eq!(record.signature(), SignatureCheck::Valid);
    assert_eq!(status(&record), IdentityStatus::Bound);

    // A describe writes no new content: no record, and the lookup says why.
    let described = client
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".into(),
            description: "described".into(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let new_commit = described.change.unwrap().commit_id;
    let error = client
        .get_provenance(GetProvenanceRequest {
            repo: repo.id.clone(),
            target: Some(get_provenance_request::Target::CommitId(new_commit)),
            ..GetProvenanceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "PROVENANCE_NOT_FOUND");
    // The change still finds its records.
    let of_change = records(
        client,
        &repo.id,
        ListProvenanceRequest {
            change_ids: vec![record.change_id.clone()],
            ..ListProvenanceRequest::default()
        },
    )
    .await;
    assert_eq!(of_change.len(), 1);
}

// 7.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the declaration rules in order")]
async fn declarations() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let first = reef_evidence::sha256_hex(b"add a parser test");
    let ws = create(
        client,
        &repo.id,
        Some(ProvenanceDeclaration {
            model: Some("example/model-x".into()),
            task: Some("T-1".into()),
            instruction_sha256: Some(first.clone()),
            ..ProvenanceDeclaration::default()
        }),
    )
    .await;
    let context = ws.provenance.as_ref().unwrap().context.clone().unwrap();
    assert_eq!(context.model, "example/model-x");
    let commit = write(client, &ws, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    let recorded = record.context.unwrap();
    assert_eq!(recorded.instruction_sha256, first);
    assert_eq!(recorded.task, "T-1");

    // An edit on disk, then a new instruction: the edit records under the
    // old one, by the declaration's snapshot.
    std::fs::write(Path::new(&ws.path).join("b.txt"), "b\n").unwrap();
    let second = reef_evidence::sha256_hex(b"now fix the tests");
    let declared = declare(
        client,
        &ws,
        ProvenanceDeclaration {
            instruction_sha256: Some(second.clone()),
            task: Some(String::new()),
            ..ProvenanceDeclaration::default()
        },
    )
    .await
    .unwrap();
    assert!(declared.operation.is_some(), "it snapshotted");
    let session = declared.session.unwrap();
    let now = session.context.unwrap();
    assert_eq!(now.instruction_sha256, second);
    assert_eq!(now.task, "", "an empty value clears");
    assert_eq!(now.model, "example/model-x", "an unset one keeps");
    let all = of_workspace(client, &repo.id, &ws).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].context.as_ref().unwrap().instruction_sha256, first);
    let after = write(client, &ws, "c.txt", "c\n").await;
    let record = by_commit(client, &repo.id, &after).await;
    assert_eq!(record.context.as_ref().unwrap().instruction_sha256, second);
    assert_eq!(record.context.as_ref().unwrap().task, "");

    // `no_snapshot` skips the snapshot.
    std::fs::write(Path::new(&ws.path).join("d.txt"), "d\n").unwrap();
    let skipped = client
        .declare_provenance(DeclareProvenanceRequest {
            workspace: ws.id.clone(),
            declaration: Some(ProvenanceDeclaration {
                harness: Some("example-harness/2.1.0".into()),
                ..ProvenanceDeclaration::default()
            }),
            no_snapshot: true,
            ..DeclareProvenanceRequest::default()
        })
        .await
        .unwrap();
    assert!(skipped.operation.is_none());
    assert_eq!(of_workspace(client, &repo.id, &ws).await.len(), 3);

    // Bad fields, and fixed ones.
    for (declaration, field) in [
        (
            ProvenanceDeclaration {
                model: Some("model x".into()),
                ..ProvenanceDeclaration::default()
            },
            "model",
        ),
        (
            ProvenanceDeclaration {
                harness: Some("h".repeat(129)),
                ..ProvenanceDeclaration::default()
            },
            "harness",
        ),
        (
            ProvenanceDeclaration {
                task: Some("T\n1".into()),
                ..ProvenanceDeclaration::default()
            },
            "task",
        ),
        (
            ProvenanceDeclaration {
                instruction_sha256: Some("add a parser test".into()),
                ..ProvenanceDeclaration::default()
            },
            "instruction_sha256",
        ),
        (
            ProvenanceDeclaration {
                client_session: Some("a b".into()),
                ..ProvenanceDeclaration::default()
            },
            "client_session",
        ),
        (
            ProvenanceDeclaration {
                identity: "alice".into(),
                ..ProvenanceDeclaration::default()
            },
            "identity",
        ),
        (
            ProvenanceDeclaration {
                actor: SessionActor::Human.into(),
                ..ProvenanceDeclaration::default()
            },
            "actor",
        ),
    ] {
        let error = declare(client, &ws, declaration).await.unwrap_err();
        assert_eq!(error.reason(), "INVALID_PROVENANCE", "{field}");
        assert_eq!(
            error.metadata().get("field").map(String::as_str),
            Some(field),
            "{field}"
        );
    }
    // No instruction text anywhere in the database.
    let db_path = daemon.state_dir().join("reef.sqlite3");
    let bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !bytes
            .windows(b"add a parser test".len())
            .any(|w| w == b"add a parser test")
    );
}

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn listening(daemon: &TestDaemon) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            return addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn remote_client(daemon: &TestDaemon, name: &str, identity: &str) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(
            name,
            RemoteScope::Write,
            Some(Duration::from_hours(1)),
            identity,
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

async fn create_identity(client: &Client, name: &str, kind: Kind) {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: kind.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
}

// 8.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the identity table in order")]
async fn identity_selection() {
    common::logs();
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    create_identity(client, "alice", Kind::Person).await;
    create_identity(client, "ci-main", Kind::Ci).await;

    // The socket: a named person identity, and human.
    let ws = create(
        client,
        &repo.id,
        Some(ProvenanceDeclaration {
            identity: "alice".into(),
            actor: SessionActor::Human.into(),
            ..ProvenanceDeclaration::default()
        }),
    )
    .await;
    let session = ws.provenance.clone().unwrap();
    assert_eq!(session.identity, "alice");
    assert_eq!(session.actor(), SessionActor::Human);
    let commit = write(client, &ws, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(status(&record), IdentityStatus::Bound);
    assert_eq!(key_of(&record).identity, "alice");
    assert_eq!(key_of(&record).presence(), v1::SessionPresence::None);

    // A ci identity over the socket refuses the creation.
    let error = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            provenance: Some(ProvenanceDeclaration {
                identity: "ci-main".into(),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "SESSION_KEY_REFUSED");
    assert_eq!(
        error.metadata().get("rule").map(String::as_str),
        Some("ci_over_socket")
    );
    let error = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            provenance: Some(ProvenanceDeclaration {
                identity: "nobody".into(),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "IDENTITY_NOT_FOUND");

    // Remote, linked to ci-main.
    let ci = remote_client(&daemon, "ci", "ci-main").await;
    let ws_ci = create(&ci, &repo.id, None).await;
    assert_eq!(ws_ci.provenance.as_ref().unwrap().identity, "ci-main");
    let commit = write(client, &ws_ci, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(status(&record), IdentityStatus::Bound);
    assert_eq!(key_of(&record).identity, "ci-main");
    for declaration in [
        ProvenanceDeclaration {
            identity: "alice".into(),
            ..ProvenanceDeclaration::default()
        },
        ProvenanceDeclaration {
            actor: SessionActor::Human.into(),
            ..ProvenanceDeclaration::default()
        },
    ] {
        let error = ci
            .create_workspace(CreateWorkspaceRequest {
                repo: repo.id.clone(),
                owner: OWNER.to_owned(),
                provenance: Some(declaration),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.reason(), "SESSION_KEY_REFUSED");
    }

    // Remote, unlinked: unbound.
    let loose = remote_client(&daemon, "loose", "").await;
    let ws_loose = create(&loose, &repo.id, None).await;
    assert_eq!(
        ws_loose.provenance.as_ref().unwrap().problem,
        "token_not_linked"
    );
    let commit = write(client, &ws_loose, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(record.signature(), SignatureCheck::Valid);
    assert_eq!(status(&record), IdentityStatus::Unbound);

    // One token cannot declare for another's workspace.
    let error = loose
        .declare_provenance(DeclareProvenanceRequest {
            workspace: ws_ci.id.clone(),
            declaration: Some(ProvenanceDeclaration::default()),
            no_snapshot: true,
            ..DeclareProvenanceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "REMOTE_WORKSPACE_NOT_OWNED");
    ci.declare_provenance(DeclareProvenanceRequest {
        workspace: ws_ci.id.clone(),
        declaration: Some(ProvenanceDeclaration {
            task: Some("T-9".into()),
            ..ProvenanceDeclaration::default()
        }),
        no_snapshot: true,
        ..DeclareProvenanceRequest::default()
    })
    .await
    .unwrap();
}

// 8, no usable default.
#[tokio::test(flavor = "multi_thread")]
async fn no_default_identity_records_unbound() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.session_keys.default_identity = "nobody".into();
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    assert_eq!(
        ws.provenance.as_ref().unwrap().problem,
        "no_default_identity"
    );
    let commit = write(client, &ws, "a.txt", "a\n").await;
    let record = by_commit(client, &repo.id, &commit).await;
    assert_eq!(record.signature(), SignatureCheck::Valid);
    assert_eq!(status(&record), IdentityStatus::Unbound);
}

// 9.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::many_single_char_names,
    reason = "records a to e, in the order they were written"
)]
async fn held_key_life() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let a = by_commit(client, &repo.id, &write(client, &ws, "a.txt", "a\n").await).await;
    let b = by_commit(client, &repo.id, &write(client, &ws, "b.txt", "b\n").await).await;
    assert_eq!(a.key_id, b.key_id, "one key signs until it nears its end");
    let max = daemon.handle.state().config.file.session_keys.max_lifetime;
    let key = key_of(&a);
    let window = key.not_after.unwrap().seconds - key.not_before.unwrap().seconds;
    assert_eq!(u64::try_from(window).unwrap(), max.as_secs());

    // A key within its renewal margin is renewed in the same session: keys
    // of five minutes are within it from the start.
    let provenance = &daemon.handle.state().provenance;
    provenance.set_key_lifetime(Some(Duration::from_mins(5)));
    provenance.revoked(&reef_daemon::provenance::Revoked::Key(b.key_id.clone()));
    let c = by_commit(client, &repo.id, &write(client, &ws, "c.txt", "c\n").await).await;
    let d = by_commit(client, &repo.id, &write(client, &ws, "d.txt", "d\n").await).await;
    assert_ne!(c.key_id, b.key_id);
    assert_ne!(d.key_id, c.key_id);
    assert_eq!(d.session_id, a.session_id);
    assert_eq!(status(&d), IdentityStatus::Bound);

    // A restart: same session, a new key.
    let daemon = daemon.restart().await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    let e = by_commit(client, &repo.id, &write(client, &ws, "e.txt", "e\n").await).await;
    assert_eq!(e.session_id, a.session_id);
    assert_ne!(e.key_id, d.key_id);
    assert_eq!(status(&e), IdentityStatus::Bound);
    assert_eq!(e.sequence, 5);
    // Older records still read bound.
    assert_eq!(
        status(&by_commit(client, &repo.id, &a.commit_id).await),
        IdentityStatus::Bound
    );

    // No private key anywhere under the state directory but the
    // authority's file.
    for entry in walk(daemon.state_dir()) {
        if entry.ends_with("identity/authority.pem") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&entry) else {
            continue;
        };
        assert!(
            !bytes.windows(11).any(|w| w == b"PRIVATE KEY"),
            "{}",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files.extend(walk(&path)),
            Ok(kind) if kind.is_file() => files.push(path),
            _ => {}
        }
    }
    files
}

// 10.
#[tokio::test(flavor = "multi_thread")]
async fn revocation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice", Kind::Person).await;
    let ws = create(
        client,
        &repo.id,
        Some(ProvenanceDeclaration {
            identity: "alice".into(),
            ..ProvenanceDeclaration::default()
        }),
    )
    .await;
    let a = by_commit(client, &repo.id, &write(client, &ws, "a.txt", "a\n").await).await;

    // The session revoked now: earlier records stay bound, the next one
    // starts a new session.
    client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::SessionId(
                a.session_id.clone(),
            )),
            ..RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    let a_again = by_commit(client, &repo.id, &a.commit_id).await;
    assert_eq!(status(&a_again), IdentityStatus::Bound);
    assert!(key_of(&a_again).revocation.is_some());
    let b = by_commit(client, &repo.id, &write(client, &ws, "b.txt", "b\n").await).await;
    assert_ne!(b.session_id, a.session_id);
    assert_eq!(b.sequence, 1);
    assert_eq!(status(&b), IdentityStatus::Bound);
    assert_eq!(
        workspace(client, &ws.id)
            .await
            .provenance
            .unwrap()
            .session_id,
        b.session_id
    );

    // Backdated to the session's start: its records read revoked.
    client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::SessionId(
                b.session_id.clone(),
            )),
            since_issued: true,
            ..RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        status(&by_commit(client, &repo.id, &b.commit_id).await),
        IdentityStatus::Revoked
    );

    // The identity revoked: later records are unbound.
    client
        .revoke_identity(RevokeIdentityRequest {
            name: "alice".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let c = by_commit(client, &repo.id, &write(client, &ws, "c.txt", "c\n").await).await;
    assert_eq!(status(&c), IdentityStatus::Unbound);
    assert_eq!(
        workspace(client, &ws.id).await.provenance.unwrap().problem,
        "identity_revoked"
    );
}

// 11.
#[tokio::test(flavor = "multi_thread")]
async fn tampering() {
    use rusqlite::types::Value;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let db = Arc::clone(&daemon.handle.state().db);
    let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
    let mut ids = Vec::new();
    for (i, file) in ["a", "b", "c", "d"].iter().enumerate() {
        let record = by_commit(
            client,
            &repo.id,
            &write(client, &ws, file, &format!("{i}\n")).await,
        )
        .await;
        ids.push(record);
    }
    db.tamper_provenance(repo_id, &ids[0].id, "model", &Value::Text("forged".into()))
        .unwrap();
    db.tamper_provenance(repo_id, &ids[1].id, "recorded_at", &Value::Integer(1))
        .unwrap();
    let mut envelope = ids[2].envelope.clone();
    let at = envelope.len() / 2;
    envelope[at] ^= 1;
    db.tamper_provenance(repo_id, &ids[2].id, "envelope", &Value::Blob(envelope))
        .unwrap();
    db.tamper_provenance(
        repo_id,
        &ids[3].id,
        "change_id",
        &Value::Text("0".repeat(32)),
    )
    .unwrap();
    for record in &ids {
        let read = by_commit(client, &repo.id, &record.commit_id).await;
        assert_eq!(read.signature(), SignatureCheck::Invalid, "{}", record.id);
        assert_eq!(status(&read), IdentityStatus::Invalid);
    }
    // The payload's values, where it parsed.
    let read = by_commit(client, &repo.id, &ids[0].commit_id).await;
    assert_eq!(read.context.unwrap().model, "");
}

// 12.
#[tokio::test(flavor = "multi_thread")]
async fn reads() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let other = create(client, &repo.id, None).await;
    for i in 0..5 {
        write(client, &ws, &format!("f{i}"), "x\n").await;
    }
    write(client, &other, "g", "x\n").await;

    // `@` after an unsnapshotted write includes it.
    std::fs::write(Path::new(&ws.path).join("h"), "h\n").unwrap();
    let at = client
        .get_provenance(GetProvenanceRequest {
            workspace: ws.id.clone(),
            target: Some(get_provenance_request::Target::Revision("@".into())),
            ..GetProvenanceRequest::default()
        })
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(at.sequence, 6);
    let by_id = client
        .get_provenance(GetProvenanceRequest {
            repo: repo.id.clone(),
            target: Some(get_provenance_request::Target::Id(at.id[..12].to_owned())),
            ..GetProvenanceRequest::default()
        })
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(by_id.id, at.id);

    let mine = of_workspace(client, &repo.id, &ws).await;
    assert_eq!(mine.len(), 6);
    assert!(
        mine.windows(2)
            .all(|w| w[0].recorded_at.as_ref().unwrap().seconds
                >= w[1].recorded_at.as_ref().unwrap().seconds),
        "newest first"
    );
    // Paged.
    let page = client
        .list_provenance_page(ListProvenanceRequest {
            repo: repo.id.clone(),
            page_size: 4,
            ..ListProvenanceRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(page.records.len(), 4);
    let rest = client
        .list_provenance_page(ListProvenanceRequest {
            repo: repo.id.clone(),
            page_size: 4,
            page_token: page.next_page_token,
            ..ListProvenanceRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(rest.records.len(), 3);
    assert_eq!(rest.next_page_token, "");
    // By session.
    let session = records(
        client,
        &repo.id,
        ListProvenanceRequest {
            session_id: at.session_id.clone(),
            ..ListProvenanceRequest::default()
        },
    )
    .await;
    assert_eq!(session.len(), 6);
    // More than 100 changes is refused.
    let error = client
        .list_provenance_page(ListProvenanceRequest {
            repo: repo.id.clone(),
            change_ids: vec!["0".repeat(32); 101],
            ..ListProvenanceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "INVALID_PROVENANCE");
}

// 13.
#[tokio::test(flavor = "multi_thread")]
async fn gc() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let state = daemon.handle.state();
    let db = Arc::clone(&state.db);
    let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
    let ws = create(client, &repo.id, None).await;
    let mut written = Vec::new();
    for i in 0..5 {
        written.push(
            by_commit(
                client,
                &repo.id,
                &write(client, &ws, &format!("f{i}"), "x\n").await,
            )
            .await,
        );
    }
    // A second workspace whose change is then abandoned: three records, the
    // middle one both compactable and expirable.
    let gone = create(client, &repo.id, None).await;
    let mut gone_records = Vec::new();
    for i in 0..3 {
        gone_records.push(
            by_commit(
                client,
                &repo.id,
                &write(client, &gone, &format!("g{i}"), "x\n").await,
            )
            .await,
        );
    }
    let gone_record = gone_records[2].clone();
    client
        .release_workspace("", &gone.id, false, false)
        .await
        .unwrap();
    // A third, released with nothing written: an ended session with no
    // record, which GC deletes.
    let empty = create(client, &repo.id, None).await;
    let empty_session = empty.provenance.clone().unwrap().session_id;
    client
        .release_workspace("", &empty.id, false, false)
        .await
        .unwrap();
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: gone_record.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let now = reef_daemon::model::Timestamp::now().0;
    let days = |n: i64| reef_daemon::model::Timestamp(now - n * 86_400_000);
    for record in &written {
        db.age_provenance(repo_id, &record.id, days(8)).unwrap();
    }
    for record in &gone_records {
        db.age_provenance(repo_id, &record.id, days(91)).unwrap();
    }

    // The middle `gone` record counts once, as a compaction, dry or not.
    let dry = client.collect_garbage(&repo.id, true).await.unwrap();
    let report = &dry.reports[0];
    assert_eq!(report.provenance_compacted, 4, "{report:?}");
    assert_eq!(report.provenance_removed, 2, "{report:?}");
    assert_eq!(
        db.provenance_counts().unwrap().0,
        8,
        "a dry run deletes nothing"
    );
    assert!(db.provenance_session(&empty_session).unwrap().is_some());
    let ran = client.collect_garbage(&repo.id, false).await.unwrap();
    assert_eq!(ran.reports[0].provenance_compacted, 4);
    assert_eq!(ran.reports[0].provenance_removed, 2);
    assert_eq!(db.provenance_counts().unwrap().0, 2);
    assert!(
        db.provenance_session(&empty_session).unwrap().is_none(),
        "an ended session with no record goes"
    );
    assert_eq!(ran.reports[0].provenance_error, "");
    let mut left: Vec<u64> = of_workspace(client, &repo.id, &ws)
        .await
        .iter()
        .map(|record| record.sequence)
        .collect();
    left.sort_unstable();
    assert_eq!(left, [1, 5], "the run's first and last");
    // The held key the remaining records name is pinned.
    let key = &written[0].key_id;
    assert_eq!(db.session_keys_by_id(&[key.as_str()]).unwrap().len(), 1);

    // Forget deletes the repository's records and sessions.
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    client.forget_repository(&repo.id).await.unwrap();
    assert_eq!(db.provenance_counts().unwrap(), (0, 0));
}

// 14.
#[tokio::test(flavor = "multi_thread")]
async fn never_fails() {
    common::logs();
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let provenance = Arc::clone(&daemon.handle.state().provenance);
    let ws = create(client, &repo.id, None).await;
    for (i, step) in [FailAt::Issue, FailAt::Sign, FailAt::Insert]
        .into_iter()
        .enumerate()
    {
        provenance.set_fail_at(step);
        write(client, &ws, &format!("f{i}"), "x\n").await;
    }
    provenance.set_fail_at(FailAt::Nothing);
    let session = workspace(client, &ws.id).await.provenance.unwrap();
    assert_eq!(session.missing_records, 3);
    assert_eq!(session.records, 0);
    let record = by_commit(client, &repo.id, &write(client, &ws, "ok", "x\n").await).await;
    assert_eq!(record.sequence, 1, "no gap for the missing ones");
    assert!(common::logs().has_line(&["could not record provenance"]));
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Snapshots of a one-file edit, timed: the median and the p95.
async fn snapshot_times(client: &Client, ws: &Workspace, runs: usize) -> (Duration, Duration) {
    let mut samples = Vec::with_capacity(runs);
    for i in 0..runs {
        std::fs::write(Path::new(&ws.path).join("timed.txt"), format!("{i}\n")).unwrap();
        let begun = Instant::now();
        let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
        samples.push(begun.elapsed());
        assert!(snapshot.changed);
    }
    samples.sort_unstable();
    (samples[runs / 2], samples[runs * 95 / 100])
}

// Performance expectations: the snapshot overhead, a page of 50, and a
// declaration. Prints its numbers; `cargo nextest run --release -p
// reef-daemon --test provenance --success-output immediate -E
// 'test(timings)'` gives docs/benchmarks.md's.
#[tokio::test(flavor = "multi_thread")]
async fn timings() {
    const RUNS: usize = 60;
    let (on, repo_on) = fixture().await;
    let (off, repo_off) = fixture_with(|config| config.file.provenance.enabled = false).await;
    let ws_on = create(&on.client, &repo_on.id, None).await;
    let ws_off = create(&off.client, &repo_off.id, None).await;
    // Warm both, and issue the held key.
    snapshot_times(&on.client, &ws_on, 5).await;
    snapshot_times(&off.client, &ws_off, 5).await;
    let mut with = Vec::new();
    let mut without = Vec::new();
    for _ in 0..3 {
        with.push(snapshot_times(&on.client, &ws_on, RUNS / 3).await);
        without.push(snapshot_times(&off.client, &ws_off, RUNS / 3).await);
    }
    let with_median = median(with.iter().map(|(m, _)| *m).collect());
    let without_median = median(without.iter().map(|(m, _)| *m).collect());
    let with_p95 = with.iter().map(|(_, p)| *p).max().unwrap();
    let without_p95 = without.iter().map(|(_, p)| *p).max().unwrap();
    println!(
        "snapshot of a one-file edit: median {with_median:?} with provenance, {without_median:?} without; p95 {with_p95:?} against {without_p95:?}"
    );

    // A page of 50, every record checked.
    let mut pages = Vec::new();
    for _ in 0..20 {
        let begun = Instant::now();
        let page = on
            .client
            .list_provenance_page(ListProvenanceRequest {
                repo: repo_on.id.clone(),
                page_size: 50,
                ..ListProvenanceRequest::default()
            })
            .await
            .unwrap();
        pages.push(begun.elapsed());
        assert_eq!(page.records.len(), 50);
    }
    println!("ListProvenance page of 50: median {:?}", median(pages));

    // A declaration with nothing to snapshot.
    let mut declarations = Vec::new();
    for i in 0..20 {
        let begun = Instant::now();
        declare(
            &on.client,
            &ws_on,
            ProvenanceDeclaration {
                task: Some(format!("T-{i}")),
                ..ProvenanceDeclaration::default()
            },
        )
        .await
        .unwrap();
        declarations.push(begun.elapsed());
    }
    println!(
        "DeclareProvenance, nothing to snapshot: median {:?}",
        median(declarations)
    );
    // The record alone, as a snapshot's blocking half adds it: renewal
    // check, signing, and the insert, for commits that need not exist.
    let provenance = Arc::clone(&on.handle.state().provenance);
    let workspace_id: reef_daemon::model::WorkspaceId = ws_on.id.parse().unwrap();
    let repo_id: reef_daemon::model::RepoId = repo_on.id.parse().unwrap();
    let change = ws_on.change_id.clone();
    let owner = ws_on.owner.clone();
    let records = tokio::task::spawn_blocking(move || {
        let mut samples = Vec::new();
        for i in 0..200_u32 {
            let commit = format!("{i:040x}");
            let begun = Instant::now();
            provenance.record(&reef_daemon::provenance::record::Commit {
                at: reef_daemon::provenance::session::For {
                    repo_id,
                    workspace_id,
                    owner: &owner,
                    remote_token_id: None,
                },
                commit_id: &commit,
                change_id: &change,
                tree_id: None,
                kind: "watcher",
                recorded_at: reef_daemon::model::Timestamp::now(),
            });
            samples.push(begun.elapsed());
        }
        samples
    })
    .await
    .unwrap();
    let mut sorted = records.clone();
    sorted.sort_unstable();
    println!(
        "Provenance::record alone: median {:?}, p99 {:?}",
        sorted[sorted.len() / 2],
        sorted[sorted.len() * 99 / 100]
    );
    let (count, _) = on.handle.state().db.provenance_counts().unwrap();
    let bytes = std::fs::metadata(on.state_dir().join("reef.sqlite3"))
        .unwrap()
        .len();
    println!("{count} records; database {bytes} bytes");
}

// Review F1: only the workspace's own change, and only its own token's
// edits, record in its session.
#[tokio::test(flavor = "multi_thread")]
async fn edits_record_only_the_workspaces_own_work() {
    use v1::edit_files_request::Target;

    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    create_identity(client, "ci-main", Kind::Ci).await;
    let edit = |workspace: &str, target: Target| EditFilesRequest {
        repo: repo.id.clone(),
        workspace: workspace.to_owned(),
        target: Some(target),
        edits: vec![FileEdit {
            path: "edited.txt".into(),
            action: Some(file_edit::Action::Content(b"x\n".to_vec())),
            ..FileEdit::default()
        }],
        ..EditFilesRequest::default()
    };

    // Over the socket, an edit naming W that makes a new change elsewhere
    // is not W's work.
    let ws = create(client, &repo.id, None).await;
    let elsewhere = client
        .edit_files(edit(&ws.id, Target::NewOn("root()".into())))
        .await
        .unwrap();
    assert!(
        client
            .get_provenance(GetProvenanceRequest {
                repo: repo.id.clone(),
                target: Some(get_provenance_request::Target::CommitId(
                    elsewhere.change.unwrap().commit_id
                )),
                ..GetProvenanceRequest::default()
            })
            .await
            .is_err()
    );

    // Token B, linked to ci-main, makes W; unlinked token A edits W's own
    // change: no record bound to ci-main is signed for A.
    let b = remote_client(&daemon, "b", "ci-main").await;
    let a = remote_client(&daemon, "a", "").await;
    let w = create(&b, &repo.id, None).await;
    let by_a = a
        .edit_files(edit(&w.id, Target::Revision("@".into())))
        .await
        .unwrap();
    assert!(
        client
            .get_provenance(GetProvenanceRequest {
                repo: repo.id.clone(),
                target: Some(get_provenance_request::Target::CommitId(
                    by_a.change.unwrap().commit_id
                )),
                ..GetProvenanceRequest::default()
            })
            .await
            .is_err(),
        "another token's edit is not the workspace's work"
    );
    let by_b = b
        .edit_files(edit(&w.id, Target::Revision("@".into())))
        .await
        .unwrap();
    let record = by_commit(client, &repo.id, &by_b.change.unwrap().commit_id).await;
    assert_eq!(status(&record), IdentityStatus::Bound);
    assert_eq!(key_of(&record).identity, "ci-main");
}

// Review F3: an ended workspace gets no new session.
#[tokio::test(flavor = "multi_thread")]
async fn ended_workspaces_get_no_session() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    client
        .release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    let error = client
        .declare_provenance(DeclareProvenanceRequest {
            workspace: ws.id.clone(),
            declaration: Some(ProvenanceDeclaration {
                task: Some("T-1".into()),
                ..ProvenanceDeclaration::default()
            }),
            no_snapshot: true,
            ..DeclareProvenanceRequest::default()
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is released"), "{error}");
    let db = &daemon.handle.state().db;
    assert!(db.live_provenance_session(&ws.id).unwrap().is_none());
}

/// A record about a commit that does not exist, as `stored` but signed by
/// `key` naming `session`, inserted behind the daemon.
fn forge(
    db: &reef_daemon::db::Db,
    repo: &str,
    like: &v1::ProvenanceRecord,
    commit: &str,
    session: &str,
    key: &reef_evidence::SigningKey,
) {
    use reef_evidence::provenance::{Context, Record, context_digest, sign};
    let record = Record {
        commit_id: commit.to_owned(),
        change_id: like.change_id.clone(),
        tree_id: Some(like.tree_id.clone()),
        workspace: like.workspace_id.clone(),
        kind: "explicit".into(),
        session: session.to_owned(),
        sequence: 99,
        agent: Some(OWNER.to_owned()),
        recorded_on_ms: reef_daemon::model::Timestamp::now().0,
        context: Context::default(),
    };
    let envelope = sign(key, &record).unwrap().to_bytes();
    let public = key.public_key();
    db.insert_provenance(
        &reef_daemon::db::EvidenceKeyRow {
            id: public.id().to_string(),
            algorithm: "ed25519".into(),
            public_key: public.spki().to_vec(),
            first_seen_at: reef_daemon::model::Timestamp::now(),
        },
        &reef_daemon::db::ProvenanceRow {
            repo_id: repo.parse().unwrap(),
            id: reef_evidence::sha256_hex(&envelope),
            envelope,
            format: 1,
            commit_id: commit.to_owned(),
            change_id: record.change_id.clone(),
            tree_id: record.tree_id.clone(),
            workspace_id: record.workspace.clone(),
            session_id: session.to_owned(),
            sequence: 99,
            kind: record.kind.clone(),
            key_id: public.id().to_string(),
            agent: record.agent.clone(),
            declared: reef_daemon::db::DeclaredColumns::default(),
            context_digest: context_digest(&record.context),
            recorded_at: reef_daemon::model::Timestamp(record.recorded_on_ms),
        },
    )
    .unwrap();
}

// Review F5 (test 11): a client-held key certified for the session, and a
// daemon-held key certified for another session, both read INVALID.
#[tokio::test(flavor = "multi_thread")]
async fn foreign_keys_read_invalid() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let state = daemon.handle.state();
    let ws = create(client, &repo.id, None).await;
    let genuine = by_commit(client, &repo.id, &write(client, &ws, "a", "a\n").await).await;

    // A client key, in the same session.
    let client_key = client
        .issue_session_key(
            "",
            reef_evidence::session::Actor::Agent,
            None,
            &genuine.session_id,
        )
        .await
        .unwrap();
    let custody = "c".repeat(40);
    forge(
        &state.db,
        &repo.id,
        &genuine,
        &custody,
        &genuine.session_id,
        &client_key.signing_key,
    );
    let read = by_commit(client, &repo.id, &custody).await;
    assert_eq!(read.signature(), SignatureCheck::Valid);
    assert_eq!(status(&read), IdentityStatus::Invalid, "custody");

    // A daemon-held key of another session, naming this one.
    let identity = state.identity.find("").unwrap();
    let other = state
        .identity
        .issue_held(
            &identity,
            reef_evidence::session::Actor::Agent,
            None,
            "ses_00000000000000aa",
            "socket",
            Duration::from_hours(1),
        )
        .unwrap();
    let crossed = "d".repeat(40);
    forge(
        &state.db,
        &repo.id,
        &genuine,
        &crossed,
        &genuine.session_id,
        &other.key,
    );
    let read = by_commit(client, &repo.id, &crossed).await;
    assert_eq!(read.signature(), SignatureCheck::Valid);
    assert_eq!(status(&read), IdentityStatus::Invalid, "session");
    // The genuine record is untouched.
    assert_eq!(
        status(&by_commit(client, &repo.id, &genuine.commit_id).await),
        IdentityStatus::Bound
    );
}

// Test 10: rotating the authority with `revoke` drops the held key; the
// next one comes from the new authority.
#[tokio::test(flavor = "multi_thread")]
async fn rotation_drops_held_keys() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id, None).await;
    let a = by_commit(client, &repo.id, &write(client, &ws, "a", "a\n").await).await;
    client
        .rotate_authority(v1::RotateAuthorityRequest {
            revoke: true,
            ..v1::RotateAuthorityRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        status(&by_commit(client, &repo.id, &a.commit_id).await),
        IdentityStatus::Revoked
    );
    let b = by_commit(client, &repo.id, &write(client, &ws, "b", "b\n").await).await;
    assert_ne!(b.key_id, a.key_id);
    assert_ne!(key_of(&b).authority_key_id, key_of(&a).authority_key_id);
    assert_eq!(status(&b), IdentityStatus::Bound);
    assert_eq!(b.session_id, a.session_id);
}

// Test 13: `0s` turns each rule off, and nothing younger than `keep_newer`
// is touched.
#[tokio::test(flavor = "multi_thread")]
async fn gc_switches_and_floor() {
    for (configure, label) in [
        (
            (|config: &mut Config| {
                config.file.provenance.compact_after = Duration::ZERO;
                config.file.provenance.retention = Duration::ZERO;
            }) as fn(&mut Config),
            "0s",
        ),
        (
            |config: &mut Config| config.file.gc.undo_window = Duration::from_hours(100 * 24),
            "keep_newer",
        ),
    ] {
        let (daemon, repo) = fixture_with(configure).await;
        let client = &daemon.client;
        let db = Arc::clone(&daemon.handle.state().db);
        let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
        let ws = create(client, &repo.id, None).await;
        let now = reef_daemon::model::Timestamp::now().0;
        for i in 0..4 {
            let record = by_commit(
                client,
                &repo.id,
                &write(client, &ws, &format!("f{i}"), "x\n").await,
            )
            .await;
            db.age_provenance(
                repo_id,
                &record.id,
                reef_daemon::model::Timestamp(now - 91 * 86_400_000),
            )
            .unwrap();
        }
        let ran = client.collect_garbage(&repo.id, false).await.unwrap();
        assert_eq!(ran.reports[0].provenance_compacted, 0, "{label}");
        assert_eq!(db.provenance_counts().unwrap().0, 4, "{label}");
    }
}
