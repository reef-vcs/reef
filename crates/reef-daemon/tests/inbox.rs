//! The triage inbox (R3.33, ADR 0088): help requests, a sign-off and a
//! workspace's attention in their buckets with their keys, across two
//! repositories, kept live by their presence entries; the digest of what landed since the mark,
//! and a mark that only moves forward, is capped, and is refused to a
//! remote token linked to no identity; and the two presence kinds that
//! keep the inbox live (#362, #145).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reef_api::v1::{
    self, CommentKind, CreateCommentRequest, CreateIdentityRequest, DescribeChangeRequest,
    EditFilesRequest, FileEdit, GetInboxRequest, GetStatusRequest, HelpInput, HelpReason,
    IdentityKind, InboxMark, InboxReason, LandChangeRequest, MarkInboxSeenRequest, RemoteScope,
    TransitionChangeRequest, edit_files_request, file_edit,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::presence::DaemonKey;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Verdict, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use tonic::Code;

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        remote_on(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

async fn commit_on(client: &Client, repo: &str, on: &str, files: &[(&str, &str)]) -> v1::Change {
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
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Sets bookmark `name` of `repo` to `commit`, outside the daemon.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = reef_daemon::testing::test_settings();
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

/// `main` with `policy` on a base commit; alice exists.
async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo, policy: &str) -> v1::Change {
    let client = &daemon.client;
    let _ = client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: IdentityKind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await;
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[("README.md", "base\n"), ("reef-policy.toml", policy)],
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    main
}

async fn propose(client: &Client, repo: &str, change: &v1::Change) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

async fn inbox(client: &Client, repos: &[&str]) -> v1::GetInboxResponse {
    client
        .get_inbox(GetInboxRequest {
            repos: repos.iter().map(|repo| (*repo).to_owned()).collect(),
            ..GetInboxRequest::default()
        })
        .await
        .unwrap()
}

/// Waits until `found` holds of the inbox of `repos`, and returns it.
async fn inbox_until(
    client: &Client,
    repos: &[&str],
    what: &str,
    found: impl Fn(&v1::GetInboxResponse) -> bool,
) -> v1::GetInboxResponse {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let read = inbox(client, repos).await;
        if found(&read) {
            return read;
        }
        assert!(Instant::now() < deadline, "never {what}: {read:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn needs(read: &v1::GetInboxResponse) -> Vec<&v1::InboxItem> {
    read.sections
        .iter()
        .flat_map(|section| section.needs_you_now.iter())
        .collect()
}

fn item<'a>(read: &'a v1::GetInboxResponse, key: &str) -> Option<&'a v1::InboxItem> {
    read.sections
        .iter()
        .flat_map(|section| section.needs_you_now.iter().chain(section.watching.iter()))
        .find(|item| item.key == key)
}

const SIGNOFF: &str = r#"
version = 1

[owners]
identities = ["alice"]

[[signoff]]
name = "review"
identities = ["alice"]
"#;

const PERMISSIVE: &str = r#"
version = 1

[owners]
identities = ["alice"]

[landing]
checks = "proposal"
"#;

// A help request, a proposal waiting on a sign-off and a workspace needing
// attention, each in needs-you-now under its key; the sign-off's AUTOMATIC
// entry moves when the evidence lands, and the workspace's ATTENTION entry
// starts and ends with its attention; a second repository is read in the
// same call.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "every source in one repository, then its live entries"
)]
async fn each_source_in_its_bucket() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let state = daemon.handle.state();
    let main = setup(&daemon, &repo, SIGNOFF).await;
    let proposed = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &proposed).await;
    let draft = commit_on(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")]).await;
    let asked = client
        .create_comment(CreateCommentRequest {
            repo: repo.id.clone(),
            revision: draft.commit_id.clone(),
            kind: CommentKind::HelpRequest.into(),
            text: "Which config format?".to_owned(),
            help: Some(HelpInput {
                reason: HelpReason::Decision.into(),
                tried: "read docs/config.md".to_owned(),
                question: "TOML or YAML?".to_owned(),
                ..HelpInput::default()
            }),
            client_session: "agent-a".to_owned(),
            ..CreateCommentRequest::default()
        })
        .await
        .unwrap()
        .comment
        .unwrap();

    let proposal_key = format!("proposal:{}:{}", repo.id, proposed.change_id);
    let read = inbox_until(client, &[], "the sign-off is asked for", |read| {
        item(read, &proposal_key).is_some_and(|item| item.reason() == InboxReason::Signoff)
    })
    .await;
    let waiting = item(&read, &proposal_key).unwrap();
    assert_eq!(waiting.headline, "Needs a sign-off from alice");
    assert_eq!(waiting.commit_id, proposed.commit_id);
    let help = item(&read, &format!("help:{}:{}", repo.id, asked.id)).unwrap();
    assert_eq!(help.reason(), InboxReason::Help);
    assert_eq!(help.headline, "An agent asks for help with a decision");
    assert_eq!(help.change_id, draft.change_id);
    assert!(
        read.refresh_operations
            .contains(&i32::from(v1::OperationKind::Comment))
    );
    assert!(
        read.refresh_presence
            .contains(&i32::from(v1::PresenceKind::Automatic))
    );

    let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
    let keys = state.presence.automatic_keys(repo_id);
    assert_eq!(keys.len(), 1, "{keys:?}");
    let entry = state.presence.daemon_entry(keys[0]).unwrap();
    assert_eq!(entry.kind(), v1::PresenceKind::Automatic);
    assert_eq!(entry.change_id, proposed.change_id);
    let status = entry.automatic.unwrap();
    assert_eq!(status.state(), v1::AutomaticState::Needs);
    assert!(status.missing.is_empty(), "{status:?}");

    // Alice signs off: the entry moves with no operation (#362), and the
    // proposal leaves needs-you-now for watching.
    let key = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap()
        .signing_key;
    let signed = signoff(&proposed.tree_id, &key);
    client
        .add_evidence(v1::AddEvidenceRequest {
            repo: repo.id.clone(),
            envelope: signed.envelope,
            public_key: signed.public_key,
            ..v1::AddEvidenceRequest::default()
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state_now = state
            .presence
            .daemon_entry(keys[0])
            .and_then(|entry| entry.automatic)
            .map(|status| status.state());
        if state_now == Some(v1::AutomaticState::Ready) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the entry never moved: {state_now:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let read = inbox(client, &[&repo.id]).await;
    assert_eq!(
        item(&read, &proposal_key).map(v1::InboxItem::reason),
        Some(InboxReason::InReview)
    );

    let workspace = client
        .create_workspace(v1::CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "claude@inbox".to_owned(),
            ..v1::CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let id: reef_daemon::model::WorkspaceId = workspace.id.parse().unwrap();
    state
        .db
        .set_workspace_attention(id, Some(reef_daemon::model::Attention::WatchFailed), None)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while state
        .presence
        .daemon_entry(DaemonKey::Attention(id))
        .is_none()
    {
        assert!(Instant::now() < deadline, "no ATTENTION entry");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let entry = state
        .presence
        .daemon_entry(DaemonKey::Attention(id))
        .unwrap();
    assert_eq!(entry.attention(), v1::Attention::WatchFailed);
    let read = inbox(client, &[&repo.id]).await;
    let attention = item(&read, &format!("workspace:{}:{}", repo.id, workspace.id)).unwrap();
    assert_eq!(attention.reason(), InboxReason::Attention);
    assert_eq!(
        attention.headline,
        "Workspace claude@inbox: its file watch could not be attached"
    );
    assert_eq!(needs(&read).len(), 2, "{read:#?}");
    assert_eq!(read.sections[0].watching.len(), 1, "{read:#?}");

    state.db.set_workspace_attention(id, None, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while state
        .presence
        .daemon_entry(DaemonKey::Attention(id))
        .is_some()
    {
        assert!(Instant::now() < deadline, "the ATTENTION entry never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(needs(&inbox(client, &[&repo.id]).await).len(), 1);

    // A second repository is read in the same call, its items keyed by it.
    let other = daemon.tmp.path().join("other");
    let at = other.clone();
    tokio::task::spawn_blocking(move || {
        reef_daemon::testing::init_synthetic_repo(&at, Preset::Small);
    })
    .await
    .unwrap();
    let second = client
        .register_repository(&other, v1::IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap()
        .id;
    let base = commit_on(client, &second, "root()", &[("README.md", "other\n")]).await;
    let asked = client
        .create_comment(CreateCommentRequest {
            repo: second.clone(),
            revision: base.commit_id.clone(),
            kind: CommentKind::HelpRequest.into(),
            text: "Which branch?".to_owned(),
            help: Some(HelpInput {
                reason: HelpReason::Unclear.into(),
                tried: "read the task".to_owned(),
                ..HelpInput::default()
            }),
            client_session: "agent-b".to_owned(),
            ..CreateCommentRequest::default()
        })
        .await
        .unwrap()
        .comment
        .unwrap();
    let read = inbox(client, &[]).await;
    let repos: Vec<&str> = read.sections.iter().map(|s| s.repo_id.as_str()).collect();
    assert_eq!(repos, [repo.id.as_str(), second.as_str()]);
    assert_eq!(
        item(&read, &format!("help:{second}:{}", asked.id)).map(v1::InboxItem::reason),
        Some(InboxReason::Help)
    );
}

/// A passing `signoff` on `tree`, signed by `key`.
fn signoff(tree: &str, key: &SigningKey) -> v1::SignedEvidence {
    let started = UNIX_EPOCH
        + Duration::from_secs(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
    let claim = Claim {
        kind: "signoff".to_owned(),
        result: Verdict::Pass,
        target: None,
        started_on: started,
        finished_on: started + Duration::from_secs(1),
        artifacts: Vec::new(),
        annotations: serde_json::Map::new(),
    };
    v1::SignedEvidence {
        envelope: claim
            .sign(&TreeDigest::git(tree).unwrap(), key)
            .unwrap()
            .to_bytes(),
        public_key: key.public_key().spki().to_vec(),
    }
}

const OWNERS_ONLY: &str = r#"
version = 1

[owners]
identities = ["alice"]
"#;

// A change to the policy file waits on its owners' built-in sign-off, which
// is a person's to give, so it needs them now.
#[tokio::test(flavor = "multi_thread")]
async fn a_policy_change_waits_on_its_owners() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, OWNERS_ONLY).await;
    let change = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[(
            "reef-policy.toml",
            "version = 1\n\n[owners]\nidentities = [\"alice\", \"bob\"]\n",
        )],
    )
    .await;
    propose(client, &repo.id, &change).await;
    let key = format!("proposal:{}:{}", repo.id, change.change_id);
    let read = inbox_until(
        client,
        &[&repo.id],
        "the owners' sign-off is asked for",
        |read| item(read, &key).is_some_and(|item| item.reason() == InboxReason::Signoff),
    )
    .await;
    assert_eq!(
        item(&read, &key).unwrap().headline,
        "Needs a sign-off from alice"
    );
}

// #422: long commit messages, over a client's 4 MiB together, leave the inbox
// and the status readable.
#[tokio::test(flavor = "multi_thread")]
async fn long_descriptions_do_not_stop_the_inbox_or_status() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, OWNERS_ONLY).await;
    // 1,023 bytes, then two-byte characters: the bound falls inside one.
    let long = format!("{}{}", "a".repeat(1023), "é".repeat(1 << 20));
    let mut keys = Vec::new();
    for n in 0..3 {
        let change = commit_on(
            client,
            &repo.id,
            &main.commit_id,
            &[(&format!("f{n}"), "x\n")],
        )
        .await;
        let change = client
            .describe_change(DescribeChangeRequest {
                repo: repo.id.clone(),
                revision: change.commit_id.clone(),
                description: long.clone(),
                ..DescribeChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap();
        propose(client, &repo.id, &change).await;
        keys.push(format!("proposal:{}:{}", repo.id, change.change_id));
    }
    let read = inbox_until(client, &[&repo.id], "every proposal is shown", |read| {
        keys.iter().all(|key| item(read, key).is_some())
    })
    .await;
    for key in &keys {
        let Some(v1::inbox_item::Record::Proposal(entry)) = &item(&read, key).unwrap().record
        else {
            panic!("{key} is no proposal");
        };
        assert_eq!(entry.change.as_ref().unwrap().description, "a".repeat(1023));
    }
    let status = client
        .get_status(GetStatusRequest {
            repo: repo.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap();
    let heads = status.repository_status.unwrap().heads;
    let cut = |head: &&v1::Change| head.description == "a".repeat(1023);
    assert_eq!(heads.iter().filter(cut).count(), 3);
}

// The digest counts what landed after the mark; the mark moves forward
// only, never past the newest transition, and a remote token linked to no
// identity cannot set one.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one mark through the socket, the web's person and remote tokens"
)]
async fn the_mark_moves_forward_only() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo, PERMISSIVE).await;
    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;
    client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: change.commit_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap();

    let read = inbox_until(client, &[&repo.id], "the landing is counted", |read| {
        read.sections[0]
            .landed
            .as_ref()
            .is_some_and(|landed| landed.count == 1)
    })
    .await;
    let landed = read.sections[0].landed.clone().unwrap();
    assert!(!landed.from_mark);
    assert_eq!(landed.newest[0].change_id, change.change_id);
    assert_eq!(landed.newest[0].subject, "");
    let through = landed.through_seq;
    assert!(through > 0);

    let mark = |seq: u64| MarkInboxSeenRequest {
        marks: vec![InboxMark {
            repo: repo.id.clone(),
            seq,
        }],
    };
    let from_mark =
        |read: &v1::GetInboxResponse| read.sections[0].landed.as_ref().unwrap().from_mark;
    let one = GetInboxRequest {
        repos: vec![repo.id.clone()],
        ..GetInboxRequest::default()
    };

    // Over the remote listener a mark is the linked identity's, not the
    // default person's; a token linked to none reads but cannot mark.
    let linked = remote_client(&daemon, "linked", RemoteScope::Write, "alice").await;
    assert_eq!(
        linked.mark_inbox_seen(mark(through)).await.unwrap().marks[0].seq,
        through
    );
    assert!(from_mark(&linked.get_inbox(one.clone()).await.unwrap()));
    assert!(!from_mark(&client.get_inbox(one.clone()).await.unwrap()));
    let plain = remote_client(&daemon, "plain", RemoteScope::Write, "").await;
    let refused = plain.mark_inbox_seen(mark(through)).await.unwrap_err();
    assert_eq!(
        reason(&refused),
        (
            Code::PermissionDenied,
            "REMOTE_IDENTITY_REQUIRED".to_owned()
        )
    );
    assert!(!from_mark(&plain.get_inbox(one.clone()).await.unwrap()));

    let stands = client.mark_inbox_seen(mark(through)).await.unwrap();
    assert_eq!(stands.marks[0].seq, through);
    let read = client.get_inbox(one.clone()).await.unwrap();
    let landed = read.sections[0].landed.clone().unwrap();
    assert!(landed.from_mark);
    assert_eq!((landed.count, landed.from_seq), (0, through));

    assert_eq!(
        client.mark_inbox_seen(mark(0)).await.unwrap().marks[0].seq,
        through,
        "backward changes nothing"
    );
    assert_eq!(
        client
            .mark_inbox_seen(mark(through + 1000))
            .await
            .unwrap()
            .marks[0]
            .seq,
        through,
        "capped at the newest transition"
    );

    let kept = client
        .get_inbox(GetInboxRequest {
            digest_from: vec![InboxMark {
                repo: repo.id.clone(),
                seq: 0,
            }],
            ..one
        })
        .await
        .unwrap();
    assert_eq!(
        kept.sections[0].landed.as_ref().unwrap().count,
        1,
        "a page keeps its visit's base"
    );
    assert!(
        !kept.sections[0].landed.as_ref().unwrap().from_mark,
        "a kept base is not the mark"
    );

    // A base for a repository no longer registered is a page's stale
    // state: the read ignores it.
    let stale = client
        .get_inbox(GetInboxRequest {
            digest_from: vec![InboxMark {
                repo: "ffff000000000000".to_owned(),
                seq: 3,
            }],
            ..GetInboxRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(stale.sections.len(), 1);
    // One that is not a repository name at all (not hex) is the request's
    // error.
    let malformed = client
        .get_inbox(GetInboxRequest {
            digest_from: vec![InboxMark {
                repo: "zzzz".to_owned(),
                seq: 3,
            }],
            ..GetInboxRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&malformed).0, Code::InvalidArgument);
    assert!(malformed.to_string().contains("hex"), "{malformed}");
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

async fn remote_client(
    daemon: &TestDaemon,
    name: &str,
    scope: RemoteScope,
    identity: &str,
) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), identity)
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
