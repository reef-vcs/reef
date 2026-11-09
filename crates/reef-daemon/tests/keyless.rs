//! Sigstore keyless session keys through the daemon (R3.42, tests 5 to 10
//! and 11's client path): OIDC matches, registration over the socket and the
//! remote listener with its rules and sessions, keyless identity in every
//! evidence read, expiry at Fulcio's window, revocation of a match, a key, a
//! CA authority, tampered rows, and `[sigstore] enabled = false`.
//!
//! Fulcio, Rekor and the OIDC issuer are `reef-testing`'s fake on loopback,
//! and the daemon's trusted root is the fake's, through `[sigstore]
//! trusted_root`: nothing here touches the network.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, CreateIdentityRequest, CreateWorkspaceRequest, GetChangeRequest,
    IdentityKind as Kind, IdentityStatus, IssuerKind, ListEvidenceRequest, OidcMatchSpec,
    RemoteScope, RevokeIdentityRequest, RevokeOidcMatchRequest, SessionActor, Workspace,
};
use reef_daemon::client::keyless::{KeylessError, KeylessOptions};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError, SessionKey};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome, TreeDigest};
use reef_testing::sigstore::San;
use reef_testing::sigstore::fake::FakeSigstore;
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNER: &str = "tester@host";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const SUBJECT: &str =
    "https://github.com/reef-vcs/reef/.github/workflows/checks.yml@refs/heads/main";

/// A daemon whose trusted root is `fake`'s, with the remote listener on,
/// and the directory holding the root (kept alive with it).
async fn fixture_with(
    fake: &FakeSigstore,
    configure: impl FnOnce(&mut Config),
) -> (TestDaemon, SyntheticRepo, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("trusted_root.json");
    std::fs::write(&root, fake.trusted_root()).unwrap();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        config.file.remote.enabled = true;
        "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
        config.file.remote.names = vec!["localhost".to_owned()];
        config.file.sigstore.trusted_root = root.display().to_string();
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo, dir)
}

fn fake() -> FakeSigstore {
    FakeSigstore::start(ISSUER, San::Uri(SUBJECT.to_owned()))
}

fn options(fake: &FakeSigstore) -> KeylessOptions {
    KeylessOptions {
        fulcio_url: fake.url().to_owned(),
        rekor_url: fake.url().to_owned(),
        ..KeylessOptions::default()
    }
}

/// A keyless key for `actor` in `session` (empty: a new one), with the
/// fake's token from `SIGSTORE_ID_TOKEN`.
async fn keyless(
    client: &Client,
    fake: &FakeSigstore,
    actor: Actor,
    session: &str,
) -> Result<SessionKey, KeylessError> {
    let token = fake.token();
    let env = move |name: &str| (name == "SIGSTORE_ID_TOKEN").then(|| token.clone());
    client
        .issue_keyless_session_key(actor, session, &options(fake), &env)
        .await
}

fn refusal(error: &KeylessError) -> (String, String) {
    let KeylessError::Client(error) = error else {
        panic!("not the daemon's refusal: {error}");
    };
    reason(error)
}

fn reason(error: &ClientError) -> (String, String) {
    let metadata = error.metadata();
    let first = [
        "rule", "step", "field", "identity", "problem", "issuer", "state",
    ]
    .iter()
    .find_map(|key| metadata.get(*key).cloned())
    .unwrap_or_default();
    (error.reason().to_owned(), first)
}

async fn create_ci(client: &Client, name: &str, oidc: &[(&str, &str)]) -> v1::Identity {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: Kind::Ci.into(),
            oidc: oidc
                .iter()
                .map(|(issuer, subject)| OidcMatchSpec {
                    issuer: (*issuer).to_owned(),
                    subject: (*subject).to_owned(),
                })
                .collect(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap()
        .identity
        .unwrap()
}

async fn workspace(client: &Client, repo: &str) -> (Workspace, String) {
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let tree = client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .tree_id;
    (ws, tree)
}

async fn add(
    client: &Client,
    ws: &Workspace,
    tree: &str,
    target: &str,
    key: &SessionKey,
) -> v1::Evidence {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    let envelope = Claim {
        kind: "test".to_owned(),
        result: Outcome::Pass,
        target: Some(target.to_owned()),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
    .sign(&TreeDigest::git(tree).unwrap(), &key.signing_key)
    .unwrap()
    .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: String::new(),
            workspace: ws.id.clone(),
            envelope,
            public_key: key.signing_key.public_key().spki().to_vec(),
            revision: String::new(),
        })
        .await
        .unwrap()
        .evidence
        .unwrap()
}

async fn get(client: &Client, repo: &str, id: &str) -> v1::Evidence {
    client
        .get_evidence(repo, id)
        .await
        .unwrap()
        .evidence
        .unwrap()
}

fn status(evidence: &v1::Evidence) -> IdentityStatus {
    evidence.identity.as_ref().unwrap().status()
}

fn at_ms(timestamp: Option<&prost_types::Timestamp>) -> i64 {
    let t = timestamp.unwrap();
    t.seconds * 1000 + i64::from(t.nanos / 1_000_000)
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

async fn remote_client(daemon: &TestDaemon, name: &str, scope: RemoteScope) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), "")
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

// 6, 7 and 8: matches, registration and its rules, sessions, and keyless
// identity in evidence reads.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the registration rules, in order")]
async fn register_and_read() {
    let fake = fake();
    let (daemon, repo, _root) = fixture_with(&fake, |_| {}).await;
    let client = daemon.client.clone().with_agent("ci-runner-7").unwrap();

    // No match: refused, naming what to add.
    let error = keyless(&client, &fake, Actor::Executor, "")
        .await
        .unwrap_err();
    let KeylessError::Client(client_error) = &error else {
        panic!("{error}")
    };
    assert_eq!(client_error.reason(), "SIGSTORE_IDENTITY_UNMATCHED");
    assert_eq!(client_error.metadata()["issuer"], ISSUER);
    assert_eq!(client_error.metadata()["subject"], SUBJECT);

    let ci = create_ci(&client, "ci-main", &[(ISSUER, SUBJECT)]).await;
    assert_eq!(ci.oidc_matches.len(), 1);
    assert_eq!(ci.oidc_matches[0].issuer, ISSUER);
    assert_eq!(ci.oidc_matches[0].subject, SUBJECT);

    // A ci identity's keyless key over the socket: its OIDC token decides.
    let key = keyless(&client, &fake, Actor::Executor, "").await.unwrap();
    let record = &key.record;
    assert_eq!(record.issuer_kind(), IssuerKind::Sigstore);
    assert_eq!(record.identity, "ci-main");
    assert_eq!(record.identity_kind(), Kind::Ci);
    assert_eq!(record.actor(), SessionActor::Executor);
    assert_eq!(record.agent, "ci-runner-7");
    assert!(record.certificate_valid);
    assert_eq!(record.remote_token_id, "");
    let sigstore = record.sigstore.as_ref().unwrap();
    assert_eq!(sigstore.oidc_issuer, ISSUER);
    assert_eq!(sigstore.oidc_subject, SUBJECT);
    assert_eq!(sigstore.oidc_match_id, ci.oidc_matches[0].id);
    assert_eq!(
        sigstore
            .ci_claims
            .get("source_repository_uri")
            .map(String::as_str),
        Some("https://github.com/reef-vcs/reef")
    );
    assert_eq!(
        at_ms(record.not_after.as_ref()) - at_ms(record.not_before.as_ref()),
        630_000
    );

    // Its evidence reads bound, with the keyless certificate.
    let (ws, tree) = workspace(&client, &repo.id).await;
    let evidence = add(&client, &ws, &tree, "keyless", &key).await;
    assert_eq!(status(&evidence), IdentityStatus::Bound);
    let read = get(&client, &repo.id, &evidence.evidence_id).await;
    let identity = read.identity.unwrap();
    assert_eq!(identity.status(), IdentityStatus::Bound);
    let signer = identity.key.unwrap();
    assert_eq!(signer.issuer_kind(), IssuerKind::Sigstore);
    assert_eq!(signer.identity, "ci-main");
    assert_eq!(signer.sigstore.unwrap().oidc_subject, SUBJECT);
    let listed = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                identities: vec!["ci-main".into()],
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);

    // The same bundle again: the record, not created.
    let again = client
        .register_sigstore_key(key.certificate.clone())
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.key.unwrap().key_id, record.key_id);

    // Renewal in the same session; another actor in it is refused.
    let renewed = keyless(&client, &fake, Actor::Executor, &record.session_id)
        .await
        .unwrap();
    assert_eq!(renewed.record.session_id, record.session_id);
    assert_ne!(renewed.record.key_id, record.key_id);
    let error = keyless(&client, &fake, Actor::Agent, &record.session_id)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error),
        ("SESSION_KEY_REFUSED".into(), "session_mismatch".into())
    );
    // A daemon-issued session is not a keyless one.
    let daemon_key = client
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    let error = keyless(
        &client,
        &fake,
        Actor::Executor,
        &daemon_key.record.session_id,
    )
    .await
    .unwrap_err();
    assert_eq!(
        refusal(&error),
        (
            "SESSION_KEY_REFUSED".into(),
            "session_channel_mismatch".into()
        )
    );
    // Never human for a ci identity.
    let error = keyless(&client, &fake, Actor::Human, "").await.unwrap_err();
    assert_eq!(
        refusal(&error),
        ("SESSION_KEY_REFUSED".into(), "human_for_ci".into())
    );

    // Through the remote listener with a token linked to no identity.
    let remote = remote_client(&daemon, "unlinked", RemoteScope::Write).await;
    let remote_key = keyless(&remote, &fake, Actor::Executor, "").await.unwrap();
    assert_eq!(remote_key.record.identity, "ci-main");
    assert_ne!(remote_key.record.remote_token_id, "");
    // A read token is refused by scope.
    let reader = remote_client(&daemon, "reader", RemoteScope::Read).await;
    let error = keyless(&reader, &fake, Actor::Executor, "")
        .await
        .unwrap_err();
    let KeylessError::Client(error) = error else {
        panic!("{error}")
    };
    assert_eq!(
        reef_lib::error::Classify::kind(&error),
        reef_lib::error::ErrorKind::PermissionDenied
    );

    // The session-key listing filters by issuer, and the Fulcio CA is an
    // authority that never issues.
    let keyless_keys = client
        .list_session_keys_all(v1::ListSessionKeysRequest {
            issuer_kind: IssuerKind::Sigstore.into(),
            ..v1::ListSessionKeysRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(keyless_keys.len(), 3);
    assert!(
        keyless_keys
            .iter()
            .all(|key| key.issuer_kind() == IssuerKind::Sigstore)
    );
    let authorities = client.list_authorities_all().await.unwrap();
    let ca = authorities
        .iter()
        .find(|authority| authority.kind() == IssuerKind::Sigstore)
        .unwrap();
    assert!(!ca.current);
    assert_eq!(ca.key_id, record.authority_key_id);
    assert_eq!(ca.host, "fulcio.test");
    assert_eq!(
        authorities
            .iter()
            .filter(|authority| authority.current)
            .count(),
        1
    );
    // Revoking the CA authority locally revokes everything it issued.
    client
        .revoke_authority(v1::RevokeAuthorityRequest {
            key_id: ca.key_id.clone(),
            since_created: true,
            ..v1::RevokeAuthorityRequest::default()
        })
        .await
        .unwrap();
    let read = get(&client, &repo.id, &evidence.evidence_id).await;
    assert_eq!(status(&read), IdentityStatus::Revoked);
}

// 9: expiry at Fulcio's window, a match's revocation, and tampered rows.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "each status, in order")]
async fn expiry_revocation_and_tampering() {
    let fake = fake();
    let (daemon, repo, _root) = fixture_with(&fake, |_| {}).await;
    let client = &daemon.client;
    let ci = create_ci(client, "ci-main", &[(ISSUER, SUBJECT)]).await;
    let (ws, tree) = workspace(client, &repo.id).await;
    let key = keyless(client, &fake, Actor::Executor, "").await.unwrap();
    let early = add(client, &ws, &tree, "early", &key).await;

    // Witnessed after the leaf's notAfter: expired.
    let late = add(client, &ws, &tree, "late", &key).await;
    let not_after = at_ms(key.record.not_after.as_ref());
    daemon
        .handle
        .state()
        .db
        .age_evidence(
            repo.id.parse().unwrap(),
            &late.evidence_id,
            reef_daemon::model::Timestamp(not_after + 1),
        )
        .unwrap();
    assert_eq!(
        status(&get(client, &repo.id, &late.evidence_id).await),
        IdentityStatus::Expired
    );

    // Revoking the match from now: earlier evidence stays bound with the
    // revocation reported; the match binds no new key.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let revoked = client
        .revoke_oidc_match(RevokeOidcMatchRequest {
            id: ci.oidc_matches[0].id.clone(),
            reason: "workflow moved".into(),
            ..RevokeOidcMatchRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(revoked.key_ids, vec![key.record.key_id.clone()]);
    assert!(revoked.r#match.unwrap().revocation.is_some());
    let read = get(client, &repo.id, &early.evidence_id).await;
    assert_eq!(status(&read), IdentityStatus::Bound);
    assert_eq!(
        read.identity
            .unwrap()
            .key
            .unwrap()
            .revocation
            .unwrap()
            .reason,
        "workflow moved"
    );
    let after = add(client, &ws, &tree, "after", &key).await;
    assert_eq!(status(&after), IdentityStatus::Revoked);
    let error = keyless(client, &fake, Actor::Executor, "")
        .await
        .unwrap_err();
    assert_eq!(refusal(&error).0, "SIGSTORE_IDENTITY_UNMATCHED");
    // Backdated to the match's creation: everything reads revoked.
    client
        .revoke_oidc_match(RevokeOidcMatchRequest {
            id: ci.oidc_matches[0].id.clone(),
            since_created: true,
            ..RevokeOidcMatchRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        status(&get(client, &repo.id, &early.evidence_id).await),
        IdentityStatus::Revoked
    );
    // The pair is free again: a new match binds new keys, not the old one.
    let added = client
        .add_oidc_match("ci-main", ISSUER, SUBJECT)
        .await
        .unwrap()
        .r#match
        .unwrap();
    assert_ne!(added.id, ci.oidc_matches[0].id);
    let fresh = keyless(client, &fake, Actor::Executor, "").await.unwrap();
    let by_fresh = add(client, &ws, &tree, "fresh", &fresh).await;
    assert_eq!(status(&by_fresh), IdentityStatus::Bound);
    assert_eq!(
        status(&get(client, &repo.id, &early.evidence_id).await),
        IdentityStatus::Revoked
    );

    // Tampered rows read invalid: the match id moved to another subject's
    // match, or a byte of the bundle. (Moved to the revoked match of the
    // same subject, the key reads revoked: that match's revocation applies.)
    let other = client
        .add_oidc_match(
            "ci-main",
            ISSUER,
            "https://github.com/reef-vcs/reef/.github/workflows/other.yml@refs/heads/main",
        )
        .await
        .unwrap()
        .r#match
        .unwrap();
    let conn = rusqlite::Connection::open(daemon.state_dir().join("reef.sqlite3")).unwrap();
    conn.execute(
        "UPDATE session_keys SET oidc_id = ?1 WHERE key_id = ?2",
        rusqlite::params![other.id, fresh.record.key_id],
    )
    .unwrap();
    assert_eq!(
        status(&get(client, &repo.id, &by_fresh.evidence_id).await),
        IdentityStatus::Invalid
    );
    conn.execute(
        "UPDATE session_keys SET oidc_id = ?1 WHERE key_id = ?2",
        rusqlite::params![added.id, fresh.record.key_id],
    )
    .unwrap();
    assert_eq!(
        status(&get(client, &repo.id, &by_fresh.evidence_id).await),
        IdentityStatus::Bound
    );
    let mut bundle = fresh.certificate.clone();
    let at = bundle.len() / 3;
    bundle[at] = if bundle[at] == b'A' { b'B' } else { b'A' };
    conn.execute(
        "UPDATE session_keys SET certificate = ?1 WHERE key_id = ?2",
        rusqlite::params![bundle, fresh.record.key_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_fresh.evidence_id).await;
    assert_eq!(status(&read), IdentityStatus::Invalid);
    let key = client
        .get_session_key(&fresh.record.key_id)
        .await
        .unwrap()
        .key
        .unwrap();
    assert!(!key.certificate_valid);
    assert!(key.sigstore.is_none());
}

// 6: the ADMIN methods for matches.
#[tokio::test(flavor = "multi_thread")]
async fn matches_admin() {
    let fake = fake();
    let (daemon, _repo, _root) = fixture_with(&fake, |_| {}).await;
    let client = &daemon.client;
    create_ci(client, "ci-main", &[(ISSUER, SUBJECT)]).await;
    for (issuer, subject, field) in [
        ("http://insecure.example", SUBJECT, "issuer"),
        (ISSUER, "has space", "subject"),
    ] {
        let error = client
            .add_oidc_match("ci-main", issuer, subject)
            .await
            .unwrap_err();
        assert_eq!(reason(&error), ("INVALID_OIDC_MATCH".into(), field.into()));
    }
    // One live match per pair, across identities.
    create_ci(client, "ci-other", &[]).await;
    let error = client
        .add_oidc_match("ci-other", ISSUER, SUBJECT)
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        ("OIDC_MATCH_EXISTS".into(), "ci-main".into())
    );
    // A create whose match is taken creates nothing.
    let error = client
        .create_identity(CreateIdentityRequest {
            name: "ci-third".into(),
            kind: Kind::Ci.into(),
            oidc: vec![OidcMatchSpec {
                issuer: ISSUER.into(),
                subject: SUBJECT.into(),
            }],
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).0, "OIDC_MATCH_EXISTS");
    assert_eq!(
        client.get_identity("ci-third").await.unwrap_err().reason(),
        "IDENTITY_NOT_FOUND"
    );
    // A revoked identity takes no match, and an unknown match id is refused.
    client
        .revoke_identity(RevokeIdentityRequest {
            name: "ci-other".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let error = client
        .add_oidc_match("ci-other", ISSUER, "https://example.com/other")
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "IDENTITY_REVOKED");
    let error = client
        .revoke_oidc_match(RevokeOidcMatchRequest {
            id: "oidc_0000000000000000".into(),
            ..RevokeOidcMatchRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "OIDC_MATCH_NOT_FOUND");
    // Never through the remote listener.
    let remote = remote_client(&daemon, "writer", RemoteScope::Write).await;
    let error = remote
        .add_oidc_match("ci-main", ISSUER, "https://example.com/x")
        .await
        .unwrap_err();
    assert_eq!(
        reef_lib::error::Classify::kind(&error),
        reef_lib::error::ErrorKind::PermissionDenied
    );
}

// `[sigstore] enabled = false`: registration is refused.
#[tokio::test(flavor = "multi_thread")]
async fn disabled() {
    let fake = fake();
    let (daemon, _repo, _root) = fixture_with(&fake, |config| {
        config.file.sigstore.enabled = false;
    })
    .await;
    create_ci(&daemon.client, "ci-main", &[(ISSUER, SUBJECT)]).await;
    let error = keyless(&daemon.client, &fake, Actor::Executor, "")
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error),
        ("SIGSTORE_UNAVAILABLE".into(), "disabled".into())
    );
}

// 13: the whole flow against Sigstore's staging instance, with GitHub
// Actions' ambient token. Ignored: it needs the network and a workflow with
// `permissions: id-token: write`. Run it by hand there:
// `cargo nextest run -p reef-daemon --test keyless --run-ignored only live_staging`.
// The identity's match is the token's own, read from the certificate the
// first registration is refused with.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the network and a GitHub Actions OIDC token"]
async fn live_staging() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("trusted_root.json");
    std::fs::write(
        &root,
        include_str!("../../reef-evidence/tests/fixtures/sigstore/staging-trusted-root.json"),
    )
    .unwrap();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.sigstore.trusted_root = root.display().to_string();
    })
    .await;
    let client = &daemon.client;
    let options = KeylessOptions {
        fulcio_url: "https://fulcio.sigstage.dev".into(),
        rekor_url: "https://rekor.sigstage.dev".into(),
        ..KeylessOptions::default()
    };
    let env = |name: &str| std::env::var(name).ok();
    let error = client
        .issue_keyless_session_key(Actor::Executor, "", &options, &env)
        .await
        .unwrap_err();
    let KeylessError::Client(error) = error else {
        panic!("{error}")
    };
    assert_eq!(error.reason(), "SIGSTORE_IDENTITY_UNMATCHED");
    let (issuer, subject) = (
        error.metadata()["issuer"].clone(),
        error.metadata()["subject"].clone(),
    );
    create_ci(client, "ci-staging", &[(&issuer, &subject)]).await;
    let key = client
        .issue_keyless_session_key(Actor::Executor, "", &options, &env)
        .await
        .unwrap();
    assert_eq!(key.record.issuer_kind(), IssuerKind::Sigstore);
    let (ws, tree) = workspace(client, &repo.id).await;
    let evidence = add(client, &ws, &tree, "live", &key).await;
    assert_eq!(status(&evidence), IdentityStatus::Bound);
    eprintln!(
        "registered {} for {issuer} {subject}, log index {}",
        key.record.key_id,
        key.record.sigstore.unwrap().log_index
    );
}

// 7, 9 and 11 continued: a person's login (an email token), a key in a
// second bundle and a key that already signed as a bare key, a session's
// revocation, the GitHub Actions token source, and a Fulcio error.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "each case, in order")]
async fn person_keys_sources_and_refusals() {
    use reef_evidence::sigstore::binding_payload;
    use reef_testing::sigstore::{Entry, Leaf};
    const GOOGLE: &str = "https://accounts.google.com";
    const EMAIL: &str = "alice@example.com";
    let person_fake = FakeSigstore::start(GOOGLE, San::Email(EMAIL.to_owned()));
    let (daemon, repo, _root) = fixture_with(&person_fake, |_| {}).await;
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".into(),
            kind: Kind::Person.into(),
            oidc: vec![OidcMatchSpec {
                issuer: GOOGLE.into(),
                subject: EMAIL.into(),
            }],
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    // The fake checks the proof over the email, as Fulcio does.
    let key = keyless(client, &person_fake, Actor::Human, "")
        .await
        .unwrap();
    assert_eq!(key.record.identity, "alice");
    assert_eq!(key.record.identity_kind(), Kind::Person);
    assert_eq!(key.record.actor(), SessionActor::Human);
    assert_eq!(key.record.presence(), v1::SessionPresence::None);
    assert_eq!(key.record.sigstore.as_ref().unwrap().oidc_subject, EMAIL);
    let (ws, tree) = workspace(client, &repo.id).await;
    let evidence = add(client, &ws, &tree, "signoff", &key).await;
    assert_eq!(status(&evidence), IdentityStatus::Bound);

    // The same key in a second bundle: certified already.
    let kit = person_fake.kit();
    let spki = key.signing_key.public_key().spki().to_vec();
    let payload = binding_payload(
        key.signing_key.public_key().id(),
        Actor::Human,
        None,
        &key.record.session_id,
    );
    let leaf = Leaf::new(GOOGLE, San::Email(EMAIL.to_owned()));
    let second = kit.keyless_bundle(
        &spki,
        &|message| key.signing_key.sign_bytes(message),
        &payload,
        &leaf,
        Entry::default(),
    );
    let error = client
        .register_sigstore_key(second.into_bytes())
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        ("SESSION_KEY_EXISTS".into(), "certified".into())
    );
    // A key that already signed evidence as a bare key: used.
    let (pem, _) = reef_evidence::SigningKey::generate().unwrap();
    let bare = reef_evidence::SigningKey::from_pkcs8_pem(&pem).unwrap();
    let envelope = Claim {
        kind: "test".to_owned(),
        result: Outcome::Pass,
        target: None,
        started_on: UNIX_EPOCH + Duration::from_hours(498_298),
        finished_on: UNIX_EPOCH + Duration::from_hours(498_298) + Duration::from_mins(1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
    .sign(&TreeDigest::git(&tree).unwrap(), &bare)
    .unwrap()
    .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: String::new(),
            workspace: ws.id.clone(),
            envelope,
            public_key: bare.public_key().spki().to_vec(),
            revision: String::new(),
        })
        .await
        .unwrap();
    let used = kit.keyless_bundle(
        bare.public_key().spki(),
        &|message| bare.sign_bytes(message),
        &binding_payload(
            bare.public_key().id(),
            Actor::Human,
            None,
            "ses_00000000000000aa",
        ),
        &leaf,
        Entry::default(),
    );
    let error = client
        .register_sigstore_key(used.into_bytes())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), ("SESSION_KEY_EXISTS".into(), "used".into()));

    // Revoking the session revokes its keyless keys, from now.
    tokio::time::sleep(Duration::from_millis(5)).await;
    client
        .revoke_session_keys(v1::RevokeSessionKeysRequest {
            target: Some(v1::revoke_session_keys_request::Target::SessionId(
                key.record.session_id.clone(),
            )),
            ..v1::RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    let after = add(client, &ws, &tree, "after", &key).await;
    assert_eq!(status(&after), IdentityStatus::Revoked);
    assert_eq!(
        status(&get(client, &repo.id, &evidence.evidence_id).await),
        IdentityStatus::Bound
    );
    // The revoked session takes no renewal.
    let error = keyless(client, &person_fake, Actor::Human, &key.record.session_id)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error),
        ("SESSION_KEY_REFUSED".into(), "session_revoked".into())
    );

    // GitHub Actions' token endpoint, chunked, as a source.
    let ci_fake = fake();
    let (ci_daemon, _ci_repo, _ci_root) = fixture_with(&ci_fake, |_| {}).await;
    create_ci(&ci_daemon.client, "ci-main", &[(ISSUER, SUBJECT)]).await;
    let github = ci_fake.github_env();
    let env = move |name: &str| {
        github
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };
    let from_github = ci_daemon
        .client
        .issue_keyless_session_key(Actor::Executor, "", &options(&ci_fake), &env)
        .await
        .unwrap();
    assert_eq!(from_github.record.identity, "ci-main");
    assert!(
        ci_fake
            .requests()
            .iter()
            .any(|request| request.starts_with("GET /token?api-version=2.0&audience=sigstore"))
    );
    // Fulcio down: the error says which service and its status.
    ci_fake.fail_fulcio(true);
    let error = keyless(&ci_daemon.client, &ci_fake, Actor::Executor, "")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, KeylessError::Service { service: "fulcio", problem } if problem.contains("HTTP 500")),
        "{error}"
    );
}
