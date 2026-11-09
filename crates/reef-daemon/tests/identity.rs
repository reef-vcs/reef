//! Operator identities and session keys through the daemon (R3.03, tests 4
//! to 12): the start sequence and the authority key, the ADMIN methods,
//! issuance over the socket and the remote listener and its rules, identity
//! in every evidence read, revocation, tampered rows, and GC, backup,
//! restore and forget.
//!
//! The web listener's refusal of every method that is not READ, the new
//! ones included, is `web_ui.rs`'s `only_read_rpcs`; the remote listener's
//! by scope is `remote.rs`'s `scopes_cover_every_method`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, CreateIdentityRequest, CreateWorkspaceRequest, GetChangeRequest,
    IdentityKind as Kind, IdentityStatus, IssueSessionKeyRequest, ListEvidenceRequest, RemoteScope,
    RevokeIdentityRequest, RevokeSessionKeysRequest, RotateAuthorityRequest, SessionActor,
    SignatureCheck, Workspace, revoke_session_keys_request,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError, SessionKey};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::{
    Actor, Authority, IssuerKind, ProofFor, check_certificate, sign_proof,
};
use reef_evidence::{Claim, KeyId, Outcome, PublicKey, SigningKey, TreeDigest};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::synth::Preset;
use serde_json::Map;

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

/// A workspace of `repo` and its tree.
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

fn claim(target: &str) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: "test".to_owned(),
        result: Outcome::Pass,
        target: Some(target.to_owned()),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
}

/// Adds a claim about `tree` with `target`, signed by `key`.
async fn add(
    client: &Client,
    ws: &Workspace,
    tree: &str,
    target: &str,
    key: &SigningKey,
) -> v1::Evidence {
    let envelope = claim(target)
        .sign(&TreeDigest::git(tree).unwrap(), key)
        .unwrap()
        .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: String::new(),
            workspace: ws.id.clone(),
            envelope,
            public_key: key.public_key().spki().to_vec(),
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

fn status_of(evidence: &v1::Evidence) -> IdentityStatus {
    evidence.identity.as_ref().unwrap().status()
}

async fn issue(client: &Client, identity: &str, actor: Actor) -> SessionKey {
    client
        .issue_session_key(identity, actor, None, "")
        .await
        .unwrap()
}

/// The daemon's current authority's key id: a proof's audience.
async fn audience(daemon: &TestDaemon) -> String {
    daemon
        .client
        .list_authorities_all()
        .await
        .unwrap()
        .into_iter()
        .find(|authority| authority.current)
        .unwrap()
        .key_id
}

/// A request for a fresh key with a valid proof, addressed to `daemon`'s
/// current authority.
async fn request(
    daemon: &TestDaemon,
    identity: &str,
    actor: Actor,
    session: &str,
) -> (SigningKey, IssueSessionKeyRequest) {
    let authority = KeyId::parse(&audience(daemon).await).unwrap();
    request_for(&authority, identity, actor, session)
}

/// A request for a fresh key with a proof addressed to `authority`.
fn request_for(
    authority: &KeyId,
    identity: &str,
    actor: Actor,
    session: &str,
) -> (SigningKey, IssueSessionKeyRequest) {
    let (pem, _) = SigningKey::generate().unwrap();
    let key = SigningKey::from_pkcs8_pem(&pem).unwrap();
    let request = IssueSessionKeyRequest {
        public_key: key.public_key().spki().to_vec(),
        proof: sign_proof(
            &key,
            &ProofFor {
                authority,
                identity,
                actor,
                session,
            },
        ),
        identity: identity.to_owned(),
        actor: reef_daemon::client::session_actor(actor).into(),
        lifetime: None,
        session_id: session.to_owned(),
    };
    (key, request)
}

/// The reason and one metadata value of a refusal.
fn refusal(error: &ClientError, key: &str) -> (String, String) {
    (
        error.reason().to_owned(),
        error.metadata().get(key).cloned().unwrap_or_default(),
    )
}

async fn create_identity(client: &Client, name: &str, kind: Kind) -> v1::Identity {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: kind.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap()
        .identity
        .unwrap()
}

fn authority_file(daemon: &TestDaemon) -> std::path::PathBuf {
    daemon.state_dir().join("identity").join("authority.pem")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A log sink, for the lines the daemon writes.
#[derive(Clone, Default)]
struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;

    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

impl Sink {
    /// Captures this process's log; nextest runs each test in its own.
    fn capture() -> Sink {
        let sink = Sink::default();
        let _ = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .try_init();
        sink
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

// 4.
#[tokio::test(flavor = "multi_thread")]
async fn migration_9_and_start() {
    let log = Sink::capture();
    let daemon = TestDaemon::start().await;
    let identities = daemon.client.list_identities_all(true).await.unwrap();
    assert_eq!(identities.len(), 1);
    let default = &identities[0];
    assert_eq!(default.kind(), Kind::Person);
    assert!(default.is_default);
    assert_eq!(
        default.name,
        reef_daemon::identity::default_name(&reef_daemon::client::os_username())
    );
    let path = authority_file(&daemon);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    let first = daemon.client.list_authorities_all().await.unwrap();
    assert_eq!(first.len(), 1);
    assert!(first[0].current);

    // A second start makes neither.
    let daemon = daemon.restart().await;
    assert_eq!(
        daemon.client.list_identities_all(true).await.unwrap().len(),
        1
    );
    assert_eq!(daemon.client.list_authorities_all().await.unwrap(), first);

    // A loosened mode is fixed, with a warning, and the key kept.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let daemon = daemon.restart().await;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(daemon.client.list_authorities_all().await.unwrap(), first);
    assert!(log.text().contains("changed it to 0600"), "{}", log.text());

    // A deleted file retires the authority and makes a new one, logging
    // both ids.
    std::fs::remove_file(&path).unwrap();
    let daemon = daemon.restart().await;
    let second = daemon.client.list_authorities_all().await.unwrap();
    assert_eq!(second.len(), 2);
    assert!(second[0].current && !second[1].current);
    assert_eq!(second[1].key_id, first[0].key_id);
    assert!(second[1].retired_at.is_some());
    let text = log.text();
    let line = text
        .lines()
        .find(|line| line.contains("retired_key_id") && line.contains(&second[0].key_id))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains(&first[0].key_id), "{line}");

    // So does a file with another key.
    let (other, _) = SigningKey::generate().unwrap();
    std::fs::write(&path, other).unwrap();
    let daemon = daemon.restart().await;
    let third = daemon.client.list_authorities_all().await.unwrap();
    assert_eq!(third.len(), 3);
    assert_ne!(third[0].key_id, second[0].key_id);
    assert_eq!(mode(&path), 0o600);
    daemon.stop().await;
}

// 5.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "every ADMIN method and refusal, in order"
)]
async fn identities_admin() {
    let daemon = TestDaemon::start().await;
    let client = &daemon.client;
    // Listings are newest first by creation time, to the millisecond; ids
    // break ties at random, so each identity here gets its own millisecond.
    let tick = || tokio::time::sleep(Duration::from_millis(3));
    tick().await;
    let alice = create_identity(client, "alice", Kind::Person).await;
    tick().await;
    assert!(alice.id.starts_with("idn_") && alice.id.len() == 20);
    assert!(!alice.is_default);
    let ci = create_identity(client, "ci-main", Kind::Ci).await;
    assert_eq!(ci.kind(), Kind::Ci);
    tick().await;
    let detailed = client
        .create_identity(CreateIdentityRequest {
            name: "carol".into(),
            kind: Kind::Person.into(),
            display_name: "Carol Example".into(),
            email: "carol@example.com".into(),
            oidc: Vec::new(),
        })
        .await
        .unwrap()
        .identity
        .unwrap();
    assert_eq!(detailed.email, "carol@example.com");

    for (name, kind, display_name, email, expected) in [
        (
            "Bad Name",
            Kind::Person,
            "",
            "",
            ("INVALID_IDENTITY_NAME", ""),
        ),
        ("x", Kind::Unspecified, "", "", ("INVALID_IDENTITY", "kind")),
        (
            "y",
            Kind::Person,
            "a\nb",
            "",
            ("INVALID_IDENTITY", "display_name"),
        ),
        ("z", Kind::Person, "", "nope", ("INVALID_IDENTITY", "email")),
    ] {
        let error = client
            .create_identity(CreateIdentityRequest {
                name: name.into(),
                kind: kind.into(),
                display_name: display_name.into(),
                email: email.into(),
                oidc: Vec::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
        assert_eq!(error.reason(), expected.0, "{error}");
        if !expected.1.is_empty() {
            assert_eq!(error.metadata()["field"], expected.1);
        }
    }
    let error = client
        .create_identity(CreateIdentityRequest {
            name: "alice".into(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "revoked"),
        ("IDENTITY_EXISTS".into(), "false".into())
    );
    client
        .revoke_identity(RevokeIdentityRequest {
            name: "alice".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let error = client
        .create_identity(CreateIdentityRequest {
            name: "alice".into(),
            kind: Kind::Ci.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "revoked"),
        ("IDENTITY_EXISTS".into(), "true".into())
    );

    // Newest first; revoked ones only when asked.
    let names = |list: Vec<v1::Identity>| list.into_iter().map(|i| i.name).collect::<Vec<_>>();
    let live = names(client.list_identities_all(false).await.unwrap());
    assert_eq!(live[..2], ["carol", "ci-main"]);
    assert!(!live.contains(&"alice".to_owned()));
    let all = names(client.list_identities_all(true).await.unwrap());
    assert_eq!(all[..3], ["carol", "ci-main", "alice"]);
    assert_eq!(all.len(), 4);

    // The default; then `[session_keys] default_identity` overrides it.
    let default = client.get_identity("").await.unwrap().identity.unwrap();
    assert!(default.is_default);
    assert_eq!(default.name, *all.last().unwrap());
    let error = client.get_identity("nobody").await.unwrap_err();
    assert_eq!(
        refusal(&error, "name"),
        ("IDENTITY_NOT_FOUND".into(), "nobody".into())
    );
    let mut stopped = daemon.stop_keeping().await;
    stopped.config.file.session_keys.default_identity = "carol".into();
    let daemon = stopped.start().await;
    let default = daemon
        .client
        .get_identity("")
        .await
        .unwrap()
        .identity
        .unwrap();
    assert_eq!(default.name, "carol");
    assert!(default.is_default);

    // The default cannot be revoked while it is the default (review
    // finding 3); the implicit one neither.
    let error = daemon
        .client
        .revoke_identity(RevokeIdentityRequest {
            name: "carol".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "name"),
        ("IDENTITY_IS_DEFAULT".into(), "carol".into())
    );
    assert_eq!(error.kind(), ErrorKind::Conflict);

    // A default_identity that names a ci, a revoked or an unknown identity
    // no longer stops the daemon (test 13's other half, review finding 3):
    // it starts with no default, and requests for one are refused.
    let mut stopped = daemon.stop_keeping().await;
    for (name, problem) in [
        ("ci-main", "names a ci identity; the default is a person"),
        ("alice", "names a revoked identity"),
        ("nobody", "names no identity"),
    ] {
        stopped.config.file.session_keys.default_identity = name.into();
        let daemon = stopped.start().await;
        let error = daemon.client.get_identity("").await.unwrap_err();
        assert_eq!(error.reason(), "DEFAULT_IDENTITY_UNAVAILABLE", "{name}");
        assert_eq!(error.metadata()["configured"], name);
        assert_eq!(error.metadata()["problem"], problem);
        let (_, defaulted) = request(&daemon, "", Actor::Agent, "").await;
        let error = daemon
            .client
            .issue_session_key_request(defaulted)
            .await
            .unwrap_err();
        assert_eq!(error.reason(), "DEFAULT_IDENTITY_UNAVAILABLE", "{name}");
        // A named identity is still issued.
        issue(&daemon.client, "carol", Actor::Agent).await;
        stopped = daemon.stop_keeping().await;
    }
    stopped.config.file.session_keys.default_identity = String::new();
    stopped.start().await.stop().await;
}

// 6.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every issuance check, in order")]
async fn issue_over_the_socket() {
    let (daemon, repo) = fixture().await;
    let client = daemon.client.clone().with_agent("builder-3").unwrap();
    let default = client.get_identity("").await.unwrap().identity.unwrap();
    let key = issue(&client, "", Actor::Agent).await;
    let record = &key.record;
    assert_eq!(record.identity_id, default.id);
    assert_eq!(record.identity, default.name);
    assert_eq!(record.actor(), SessionActor::Agent);
    assert_eq!(record.presence(), v1::SessionPresence::None);
    assert_eq!(record.custody(), v1::KeyCustody::Client);
    assert_eq!(record.issuer_kind(), v1::IssuerKind::ReefDaemon);
    assert_eq!(record.agent, "builder-3");
    assert_eq!(record.issued_by_agent, "builder-3");
    assert!(record.certificate_valid);
    assert!(record.session_id.starts_with("ses_"));
    let window = record.not_after.unwrap().seconds - record.not_before.unwrap().seconds;
    assert_eq!(window, 3600);
    assert_eq!(record.key_id, key.signing_key.public_key().id().as_str());
    // The certificate checks with the daemon's authority, and binds what
    // the record says.
    let authorities = client.list_authorities_all().await.unwrap();
    let lookup = |id: &KeyId| {
        authorities
            .iter()
            .find(|a| a.key_id == id.as_str())
            .map(|a| Authority {
                key: PublicKey::from_spki(&a.public_key).unwrap(),
                kind: IssuerKind::ReefDaemon,
                revocation: None,
            })
    };
    let certified = check_certificate(&key.certificate, &lookup).unwrap();
    assert_eq!(certified.binding.identity.name, default.name);
    assert_eq!(certified.binding.actor, Actor::Agent);
    assert_eq!(certified.binding.agent.as_deref(), Some("builder-3"));
    assert_eq!(
        certified.binding.not_after_ms - certified.binding.not_before_ms,
        3_600_000
    );
    assert_eq!(
        certified.binding.issuer.key_id.as_str(),
        authorities[0].key_id
    );
    // GetSessionKey has the certificate too.
    let got = client
        .get_session_key(&record.key_id[..8])
        .await
        .unwrap()
        .key
        .unwrap();
    assert_eq!(got.certificate, key.certificate);
    assert_eq!(got.public_key, key.signing_key.public_key().spki());

    // Lifetimes.
    let two = client
        .issue_session_key("", Actor::Agent, Some(Duration::from_hours(2)), "")
        .await
        .unwrap();
    let window = two.record.not_after.unwrap().seconds - two.record.not_before.unwrap().seconds;
    assert_eq!(window, 7200);
    let error = client
        .issue_session_key("", Actor::Agent, Some(Duration::from_hours(25)), "")
        .await
        .unwrap_err();
    let reef_daemon::client::IssueError::Client(error) = error else {
        panic!("{error:?}");
    };
    assert_eq!(
        refusal(&error, "field"),
        ("INVALID_SESSION_KEY_REQUEST".into(), "lifetime".into())
    );

    // Proofs: a bad one, and one made for another identity or actor.
    let (_, mut bad) = request(&daemon, "", Actor::Agent, "").await;
    bad.proof[0] ^= 1;
    let (_, mut replayed_identity) = request(&daemon, "", Actor::Agent, "").await;
    replayed_identity.identity = default.name.clone();
    let (_, mut replayed_actor) = request(&daemon, "", Actor::Agent, "").await;
    replayed_actor.actor = SessionActor::Executor.into();
    for request in [bad, replayed_identity, replayed_actor] {
        let error = client.issue_session_key_request(request).await.unwrap_err();
        assert_eq!(
            refusal(&error, "field"),
            ("INVALID_SESSION_KEY_REQUEST".into(), "proof".into())
        );
    }

    // A small-order or non-canonical public key is refused at the RPC.
    let mut small_order = reef_evidence::keys::SPKI_PREFIX.to_vec();
    small_order.extend_from_slice(&reef_evidence::keys::SMALL_ORDER[1]);
    let mut above_p = [0xff_u8; 32];
    above_p[0] = 0xf0;
    above_p[31] = 0x7f;
    let mut non_canonical = reef_evidence::keys::SPKI_PREFIX.to_vec();
    non_canonical.extend_from_slice(&above_p);
    for der in [small_order, non_canonical] {
        let (_, mut bad_key) = request(&daemon, "", Actor::Agent, "").await;
        bad_key.public_key = der;
        let error = client.issue_session_key_request(bad_key).await.unwrap_err();
        assert_eq!(
            refusal(&error, "field"),
            ("INVALID_SESSION_KEY_REQUEST".into(), "public_key".into())
        );
    }

    // A proof addressed to another authority (another daemon's, or the one
    // before a rotation) is refused, naming the current one (review
    // finding 6).
    let current = audience(&daemon).await;
    let elsewhere = SigningKey::from_seed(&[42; 32]).unwrap();
    let (_, misaddressed) = request_for(elsewhere.public_key().id(), "", Actor::Agent, "");
    let error = client
        .issue_session_key_request(misaddressed)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "field"),
        ("INVALID_SESSION_KEY_REQUEST".into(), "proof".into())
    );
    assert_eq!(error.metadata()["authority_key_id"], current);

    // The same key twice; a key that signed evidence as a bare key.
    let (bare, again) = request(&daemon, "", Actor::Agent, "").await;
    client
        .issue_session_key_request(again.clone())
        .await
        .unwrap();
    let error = client.issue_session_key_request(again).await.unwrap_err();
    assert_eq!(
        refusal(&error, "state"),
        ("SESSION_KEY_EXISTS".into(), "certified".into())
    );
    drop(bare);
    let (ws, tree) = workspace(&client, &repo.id).await;
    let (used, used_request) = request(&daemon, "", Actor::Agent, "").await;
    add(&client, &ws, &tree, "bare", &used).await;
    let error = client
        .issue_session_key_request(used_request)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "state"),
        ("SESSION_KEY_EXISTS".into(), "used".into())
    );

    // Renewal keeps the session; another identity or actor in it is refused.
    let session = record.session_id.clone();
    let renewed = client
        .issue_session_key("", Actor::Agent, None, &session)
        .await
        .unwrap();
    assert_eq!(renewed.record.session_id, session);
    create_identity(&client, "bob", Kind::Person).await;
    for (identity, actor) in [("bob", Actor::Agent), ("", Actor::Executor)] {
        let (_, request) = request(&daemon, identity, actor, &session).await;
        let error = client.issue_session_key_request(request).await.unwrap_err();
        assert_eq!(
            refusal(&error, "rule"),
            ("SESSION_KEY_REFUSED".into(), "session_mismatch".into())
        );
    }
    let (_, unknown) = request(&daemon, "", Actor::Agent, "ses_0000000000000000").await;
    let error = client.issue_session_key_request(unknown).await.unwrap_err();
    assert_eq!(
        refusal(&error, "field"),
        ("INVALID_SESSION_KEY_REQUEST".into(), "session_id".into())
    );

    // The limit, lowered: bob has none, then two, then is refused.
    daemon.handle.state().identity.set_limit(2);
    issue(&client, "bob", Actor::Agent).await;
    issue(&client, "bob", Actor::Executor).await;
    let (_, third) = request(&daemon, "bob", Actor::Agent, "").await;
    let error = client.issue_session_key_request(third).await.unwrap_err();
    assert_eq!(
        refusal(&error, "limit"),
        ("TOO_MANY_SESSION_KEYS".into(), "2".into())
    );
    daemon.handle.state().identity.set_limit(10_000);

    // The issue median (Performance expectations).
    let mut times = Vec::new();
    for _ in 0..50 {
        let begun = Instant::now();
        issue(&client, "", Actor::Executor).await;
        times.push(begun.elapsed());
    }
    times.sort();
    println!("issue median: {:?}", times[times.len() / 2]);
    daemon.stop().await;
}

// 7.
#[tokio::test(flavor = "multi_thread")]
async fn issuance_rules() {
    let daemon = TestDaemon::start().await;
    let client = &daemon.client;
    create_identity(client, "ci-main", Kind::Ci).await;
    create_identity(client, "dana", Kind::Person).await;
    for (identity, actor, rule) in [
        ("ci-main", Actor::Agent, "ci_over_socket"),
        ("ci-main", Actor::Executor, "ci_over_socket"),
        ("ci-main", Actor::Human, "human_for_ci"),
    ] {
        let (_, request) = request(&daemon, identity, actor, "").await;
        let error = client.issue_session_key_request(request).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            refusal(&error, "rule"),
            ("SESSION_KEY_REFUSED".into(), rule.into())
        );
    }
    let human = issue(client, "dana", Actor::Human).await;
    assert_eq!(human.record.actor(), SessionActor::Human);
    assert_eq!(human.record.presence(), v1::SessionPresence::None);
    client
        .revoke_identity(RevokeIdentityRequest {
            name: "dana".into(),
            reason: "left".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let (_, request) = request(&daemon, "dana", Actor::Agent, "").await;
    let error = client.issue_session_key_request(request).await.unwrap_err();
    assert_eq!(
        refusal(&error, "name"),
        ("IDENTITY_REVOKED".into(), "dana".into())
    );
    assert!(error.metadata().contains_key("revoked_at"));
    daemon.stop().await;
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

/// A client through the remote listener with a new token named `name`,
/// linked to `identity` (empty for none), and the token's id.
async fn remote_client(
    daemon: &TestDaemon,
    name: &str,
    scope: RemoteScope,
    identity: &str,
) -> (Client, v1::RemoteToken) {
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
    (
        Client::connect_remote(&target).await.unwrap(),
        created.token.unwrap(),
    )
}

// 8.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every remote rule, in order")]
async fn remote_issuance() {
    let log = Sink::capture();
    let (daemon, _repo) = fixture_with(remote_on).await;
    create_identity(&daemon.client, "ci-main", Kind::Ci).await;
    create_identity(&daemon.client, "alice", Kind::Person).await;
    let (ci, token) = remote_client(&daemon, "ci", RemoteScope::Write, "ci-main").await;
    assert_eq!(token.identity, "ci-main");
    for named in ["", "ci-main"] {
        let key = ci
            .issue_session_key(named, Actor::Executor, None, "")
            .await
            .unwrap();
        assert_eq!(key.record.identity, "ci-main");
        assert_eq!(key.record.identity_kind(), Kind::Ci);
        assert_eq!(key.record.remote_token_id, token.id);
    }
    for (named, actor, rule) in [
        ("alice", Actor::Agent, "token_identity_mismatch"),
        ("", Actor::Human, "human_over_remote"),
        ("ci-main", Actor::Human, "human_over_remote"),
    ] {
        let (_, request) = request(&daemon, named, actor, "").await;
        let error = ci.issue_session_key_request(request).await.unwrap_err();
        assert_eq!(
            refusal(&error, "rule"),
            ("SESSION_KEY_REFUSED".into(), rule.into()),
            "{named} {actor}"
        );
    }
    let (unlinked, _) = remote_client(&daemon, "plain", RemoteScope::Write, "").await;
    let (_, request_any) = request(&daemon, "", Actor::Agent, "").await;
    let error = unlinked
        .issue_session_key_request(request_any)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "rule"),
        ("SESSION_KEY_REFUSED".into(), "token_not_linked".into())
    );
    let (reader, _) = remote_client(&daemon, "reader", RemoteScope::Read, "ci-main").await;
    let (_, request_read) = request(&daemon, "", Actor::Agent, "").await;
    let error = reader
        .issue_session_key_request(request_read)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(error.reason(), "REMOTE_ACCESS_DENIED");
    // READ methods are served to a read token; ADMIN ones never.
    let readable = reader.list_identities_all(false).await.unwrap();
    assert!(readable.iter().any(|identity| identity.name == "ci-main"));
    let error = ci
        .create_identity(CreateIdentityRequest {
            name: "mallory".into(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "REMOTE_ACCESS_DENIED");
    for request in [
        ci.revoke_identity(RevokeIdentityRequest {
            name: "alice".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .map(drop),
        ci.revoke_session_keys(RevokeSessionKeysRequest::default())
            .await
            .map(drop),
        ci.rotate_authority(RotateAuthorityRequest::default())
            .await
            .map(drop),
    ] {
        assert_eq!(request.unwrap_err().reason(), "REMOTE_ACCESS_DENIED");
    }

    // A session is renewed only through the channel that started it
    // (review finding 7): alice's socket session, not through a token
    // linked to alice, and the token's own session, not over the socket.
    let socket_key = issue(&daemon.client, "alice", Actor::Agent).await;
    let (as_alice, alice_token) =
        remote_client(&daemon, "for-alice", RemoteScope::Write, "alice").await;
    let (_, across) = request(&daemon, "", Actor::Agent, &socket_key.record.session_id).await;
    let error = as_alice
        .issue_session_key_request(across)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "rule"),
        (
            "SESSION_KEY_REFUSED".into(),
            "session_channel_mismatch".into()
        )
    );
    let remote_key = as_alice
        .issue_session_key("", Actor::Agent, None, "")
        .await
        .unwrap();
    assert_eq!(remote_key.record.remote_token_id, alice_token.id);
    let (_, back) = request(
        &daemon,
        "alice",
        Actor::Agent,
        &remote_key.record.session_id,
    )
    .await;
    let error = daemon
        .client
        .issue_session_key_request(back)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "rule"),
        (
            "SESSION_KEY_REFUSED".into(),
            "session_channel_mismatch".into()
        )
    );
    as_alice
        .issue_session_key("", Actor::Agent, None, &remote_key.record.session_id)
        .await
        .unwrap();

    // Links: an unknown or revoked identity is refused; rotation keeps one.
    for (identity, reason) in [
        ("nobody", "IDENTITY_NOT_FOUND"),
        ("Bad", "INVALID_IDENTITY_NAME"),
    ] {
        let error = daemon
            .client
            .create_remote_token_for("t2", RemoteScope::Write, None, identity)
            .await
            .unwrap_err();
        assert_eq!(error.reason(), reason);
    }
    daemon
        .client
        .revoke_identity(RevokeIdentityRequest {
            name: "alice".into(),
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let error = daemon
        .client
        .create_remote_token_for("t3", RemoteScope::Write, None, "alice")
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "IDENTITY_REVOKED");
    let rotated = daemon
        .client
        .rotate_remote_token("ci", Duration::ZERO)
        .await
        .unwrap()
        .token
        .unwrap();
    assert_eq!(rotated.identity, "ci-main");
    let linked = daemon
        .client
        .get_identity("ci-main")
        .await
        .unwrap()
        .identity
        .unwrap();
    assert_eq!(linked.remote_token_names, ["ci", "reader"]);
    let alice = daemon
        .client
        .get_identity("alice")
        .await
        .unwrap()
        .identity
        .unwrap();
    assert_eq!(alice.remote_token_names, ["for-alice"]);
    daemon.stop().await;

    let text = log.text();
    assert!(
        text.lines()
            .any(|line| line.contains("reef_daemon::remote::audit")
                && line.contains("remote write")
                && line.contains("IssueSessionKey")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("reef_daemon::identity::audit")
                && line.contains("session key issued")
                && line.contains(&token.id)),
        "{text}"
    );
    assert!(!text.contains("PRIVATE KEY"), "{text}");
}

// 9.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "each status, the filter and the page timing"
)]
async fn evidence_reports_identity() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (ws, tree) = workspace(client, &repo.id).await;
    let key = issue(client, "", Actor::Agent).await;
    let bound = add(client, &ws, &tree, "bound", &key.signing_key).await;
    let identity = bound.identity.clone().unwrap();
    assert_eq!(identity.status(), IdentityStatus::Bound);
    assert_eq!(identity.witnessed_at, bound.added_at);
    let bound_key = identity.key.unwrap();
    assert_eq!(bound_key.identity, key.record.identity);
    assert_eq!(bound_key.actor(), SessionActor::Agent);
    assert_eq!(bound_key.session_id, key.record.session_id);
    assert!(bound_key.certificate.is_empty(), "no certificate bytes");
    assert_eq!(bound.signature(), SignatureCheck::Valid);
    let got = get(client, &repo.id, &bound.evidence_id).await;
    assert_eq!(got.identity, bound.identity);

    // A bare key is unbound.
    let bare_key = SigningKey::from_seed(&[9; 32]).unwrap();
    let bare = add(client, &ws, &tree, "bare", &bare_key).await;
    assert_eq!(status_of(&bare), IdentityStatus::Unbound);
    assert!(bare.identity.as_ref().unwrap().key.is_none());
    assert_eq!(bare.signature(), SignatureCheck::Valid);

    // Evidence first witnessed outside its key's window: expired. The
    // witness time is moved, as a clock would; one millisecond after
    // `notAfter`, then one before `notBefore` (which keeps it off the
    // newest page below).
    let late = add(client, &ws, &tree, "late", &key.signing_key).await;
    let not_after = at_ms(key.record.not_after.as_ref());
    let not_before = at_ms(key.record.not_before.as_ref());
    for witnessed in [not_after + 1, not_before - 1] {
        daemon
            .handle
            .state()
            .db
            .age_evidence(
                repo.id.parse().unwrap(),
                &late.evidence_id,
                reef_daemon::model::Timestamp(witnessed),
            )
            .unwrap();
        let read = get(client, &repo.id, &late.evidence_id).await;
        assert_eq!(status_of(&read), IdentityStatus::Expired);
        assert_eq!(read.signature(), SignatureCheck::Valid);
        assert_eq!(
            at_ms(read.identity.unwrap().witnessed_at.as_ref()),
            witnessed
        );
    }

    // The identities filter, across pages.
    create_identity(client, "bob", Kind::Person).await;
    let bob = issue(client, "bob", Actor::Executor).await;
    let mut bobs = Vec::new();
    for n in 0..3 {
        bobs.push(
            add(client, &ws, &tree, &format!("bob-{n}"), &bob.signing_key)
                .await
                .evidence_id,
        );
    }
    let mut seen = Vec::new();
    let mut request = ListEvidenceRequest {
        repo: repo.id.clone(),
        identities: vec!["bob".into()],
        page_size: 1,
        ..ListEvidenceRequest::default()
    };
    loop {
        let page = client.list_evidence_page(request.clone()).await.unwrap();
        for evidence in &page.evidence {
            assert_eq!(
                evidence
                    .identity
                    .as_ref()
                    .unwrap()
                    .key
                    .as_ref()
                    .unwrap()
                    .identity,
                "bob"
            );
            seen.push(evidence.evidence_id.clone());
        }
        if page.next_page_token.is_empty() {
            break;
        }
        request.page_token = page.next_page_token;
    }
    seen.sort();
    bobs.sort();
    assert_eq!(seen, bobs);
    let listed = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 6);
    assert!(
        listed
            .items
            .iter()
            .all(|e| e.signature() == SignatureCheck::Valid)
    );

    // The page median: 50 objects, each by its own session key.
    for n in 0..50 {
        let key = issue(client, "", Actor::Executor).await;
        add(client, &ws, &tree, &format!("page-{n}"), &key.signing_key).await;
    }
    let mut times = Vec::new();
    for _ in 0..20 {
        let begun = Instant::now();
        let page = client
            .list_evidence_page(ListEvidenceRequest {
                repo: repo.id.clone(),
                page_size: 50,
                ..ListEvidenceRequest::default()
            })
            .await
            .unwrap();
        times.push(begun.elapsed());
        assert_eq!(page.evidence.len(), 50);
        assert!(
            page.evidence
                .iter()
                .all(|e| status_of(e) == IdentityStatus::Bound)
        );
    }
    times.sort();
    println!("list page median (50 bound): {:?}", times[times.len() / 2]);
    daemon.stop().await;
}

fn at_ms(timestamp: Option<&prost_types::Timestamp>) -> i64 {
    let t = timestamp.unwrap();
    t.seconds * 1000 + i64::from(t.nanos / 1_000_000)
}

fn proto_at(ms: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: i32::try_from(ms.rem_euclid(1000) * 1_000_000).unwrap(),
    }
}

async fn revoke_key(
    client: &Client,
    key_id: &str,
    invalid_from: Option<i64>,
    since_issued: bool,
) -> Result<Vec<v1::SessionKey>, ClientError> {
    client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::KeyId(
                key_id.to_owned(),
            )),
            invalid_from: invalid_from.map(proto_at),
            since_issued,
            reason: "test".into(),
        })
        .await
        .map(|response| response.keys)
}

// 10.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario per revocation kind")]
async fn revocation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (ws, tree) = workspace(client, &repo.id).await;

    // A key revoked at T: before T bound (and revoked later), after it
    // revoked.
    let key = issue(client, "", Actor::Agent).await;
    let before = add(client, &ws, &tree, "before", &key.signing_key).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let revoked = revoke_key(client, &key.record.key_id, None, false)
        .await
        .unwrap();
    assert_eq!(revoked.len(), 1);
    let invalid_from = at_ms(
        revoked[0]
            .revocation
            .as_ref()
            .unwrap()
            .invalid_from
            .as_ref(),
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    let after = add(client, &ws, &tree, "after", &key.signing_key).await;
    let before = get(client, &repo.id, &before.evidence_id).await;
    assert_eq!(status_of(&before), IdentityStatus::Bound);
    let reported = before.identity.unwrap().key.unwrap().revocation.unwrap();
    assert_eq!(at_ms(reported.invalid_from.as_ref()), invalid_from);
    assert_eq!(reported.reason, "test");
    assert_eq!(status_of(&after), IdentityStatus::Revoked);

    // A later revocation moves invalid_from earlier, never later; a future
    // one is refused.
    let later = revoke_key(client, &key.record.key_id, Some(invalid_from - 1), false)
        .await
        .unwrap();
    assert_eq!(
        at_ms(later[0].revocation.as_ref().unwrap().invalid_from.as_ref()),
        invalid_from - 1
    );
    let kept = revoke_key(client, &key.record.key_id, Some(invalid_from), false)
        .await
        .unwrap();
    assert_eq!(
        at_ms(kept[0].revocation.as_ref().unwrap().invalid_from.as_ref()),
        invalid_from - 1
    );
    let future = reef_daemon::model::Timestamp::now().0 + 60_000;
    let error = revoke_key(client, &key.record.key_id, Some(future), false)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "field"),
        ("INVALID_REVOCATION".into(), "invalid_from".into())
    );
    let error = revoke_key(client, &key.record.key_id, Some(invalid_from), true)
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "INVALID_REVOCATION");
    let error = client
        .revoke_session_keys(RevokeSessionKeysRequest::default())
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "field"),
        ("INVALID_REVOCATION".into(), "target".into())
    );

    // --since-issued: everything it signed reads revoked.
    let leaked = issue(client, "", Actor::Agent).await;
    let signed = add(client, &ws, &tree, "leaked", &leaked.signing_key).await;
    assert_eq!(status_of(&signed), IdentityStatus::Bound);
    revoke_key(client, &leaked.record.key_id, None, true)
        .await
        .unwrap();
    let signed = get(client, &repo.id, &signed.evidence_id).await;
    assert_eq!(status_of(&signed), IdentityStatus::Revoked);

    // A session: every key, and no renewal.
    let first = issue(client, "", Actor::Executor).await;
    let session = first.record.session_id.clone();
    let second = client
        .issue_session_key("", Actor::Executor, None, &session)
        .await
        .unwrap();
    let covered = client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::SessionId(
                session.clone(),
            )),
            since_issued: true,
            ..RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap()
        .keys;
    let mut ids: Vec<&str> = covered.iter().map(|k| k.key_id.as_str()).collect();
    ids.sort_unstable();
    let mut expected = [first.record.key_id.as_str(), second.record.key_id.as_str()];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    assert!(covered.iter().all(|k| k.revocation.is_some()));
    let (_, renewal) = request(&daemon, "", Actor::Executor, &session).await;
    let error = client.issue_session_key_request(renewal).await.unwrap_err();
    assert_eq!(
        refusal(&error, "rule"),
        ("SESSION_KEY_REFUSED".into(), "session_revoked".into())
    );
    let error = client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::SessionId(
                "ses_0000000000000000".into(),
            )),
            ..RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "session_id"),
        (
            "SESSION_KEY_NOT_FOUND".into(),
            "ses_0000000000000000".into()
        )
    );

    // An identity: no issuance, and its keys revoked from its creation.
    create_identity(client, "erin", Kind::Person).await;
    let erins = issue(client, "erin", Actor::Agent).await;
    let by_erin = add(client, &ws, &tree, "erin", &erins.signing_key).await;
    let revoked = client
        .revoke_identity(RevokeIdentityRequest {
            name: "erin".into(),
            since_created: true,
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap()
        .identity
        .unwrap();
    assert!(revoked.revocation.is_some());
    let by_erin = get(client, &repo.id, &by_erin.evidence_id).await;
    assert_eq!(status_of(&by_erin), IdentityStatus::Revoked);
    let (_, request_erin) = request(&daemon, "erin", Actor::Agent, "").await;
    let error = client
        .issue_session_key_request(request_erin)
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "IDENTITY_REVOKED");
    // A revoked identity's keys are no longer live (review finding 5).
    let live = |identity: &str, include_ended: bool| {
        client.list_session_keys_all(v1::ListSessionKeysRequest {
            identity: identity.to_owned(),
            include_ended,
            ..v1::ListSessionKeysRequest::default()
        })
    };
    assert_eq!(live("erin", false).await.unwrap().len(), 0);
    assert_eq!(live("erin", true).await.unwrap().len(), 1);

    // Rotation: old certificates stay bound; new keys come from the new
    // authority; with revoke, the old one's read revoked.
    let old_key = issue(client, "", Actor::Agent).await;
    let by_old = add(client, &ws, &tree, "old-authority", &old_key.signing_key).await;
    let rotated = client
        .rotate_authority(RotateAuthorityRequest::default())
        .await
        .unwrap();
    let (retired, current) = (rotated.retired.unwrap(), rotated.current.unwrap());
    assert_eq!(retired.key_id, old_key.record.authority_key_id);
    let retired_first = retired.key_id.clone();
    assert!(retired.retired_at.is_some() && retired.revocation.is_none());
    assert!(current.current);
    assert_eq!(
        status_of(&get(client, &repo.id, &by_old.evidence_id).await),
        IdentityStatus::Bound
    );
    let new_key = issue(client, "", Actor::Agent).await;
    assert_eq!(new_key.record.authority_key_id, current.key_id);
    let by_new = add(client, &ws, &tree, "new-authority", &new_key.signing_key).await;
    let error = client
        .rotate_authority(RotateAuthorityRequest {
            revoke: false,
            invalid_from: Some(proto_at(0)),
            reason: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "INVALID_REVOCATION");
    let rotated = client
        .rotate_authority(RotateAuthorityRequest {
            revoke: true,
            invalid_from: None,
            reason: "compromised".into(),
        })
        .await
        .unwrap();
    let retired = rotated.retired.unwrap();
    assert_eq!(retired.key_id, current.key_id);
    assert_eq!(retired.revocation.unwrap().reason, "compromised");
    assert_eq!(
        status_of(&get(client, &repo.id, &by_new.evidence_id).await),
        IdentityStatus::Revoked
    );
    assert_eq!(
        status_of(&get(client, &repo.id, &by_old.evidence_id).await),
        IdentityStatus::Bound
    );
    assert_eq!(client.list_authorities_all().await.unwrap().len(), 3);
    // A revoked authority's keys are no longer live (review finding 5).
    let keys = live("", false).await.unwrap();
    assert!(keys.iter().all(|key| key.key_id != new_key.record.key_id));
    assert!(keys.iter().any(|key| key.key_id == old_key.record.key_id));

    // A retired authority is revoked by RevokeAuthority (review finding 4);
    // the current one is refused, and so is an unknown one.
    let error = client
        .revoke_authority(v1::RevokeAuthorityRequest {
            key_id: rotated.current.unwrap().key_id,
            ..v1::RevokeAuthorityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "AUTHORITY_IS_CURRENT");
    let error = client
        .revoke_authority(v1::RevokeAuthorityRequest {
            key_id: "0000".into(),
            ..v1::RevokeAuthorityRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&error, "key_id"),
        ("AUTHORITY_NOT_FOUND".into(), "0000".into())
    );
    let revoked = client
        .revoke_authority(v1::RevokeAuthorityRequest {
            key_id: retired_first[..8].to_owned(),
            since_created: true,
            reason: "restored elsewhere".into(),
            ..v1::RevokeAuthorityRequest::default()
        })
        .await
        .unwrap()
        .authority
        .unwrap();
    assert_eq!(revoked.key_id, retired_first);
    assert_eq!(revoked.revocation.unwrap().reason, "restored elsewhere");
    assert_eq!(
        status_of(&get(client, &repo.id, &by_old.evidence_id).await),
        IdentityStatus::Revoked
    );
    assert!(
        live("", false)
            .await
            .unwrap()
            .iter()
            .all(|key| key.key_id != old_key.record.key_id)
    );
    daemon.stop().await;
}

// 11.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one tampered row per case")]
async fn tampering() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (ws, tree) = workspace(client, &repo.id).await;
    create_identity(client, "frank", Kind::Person).await;
    let flipped = issue(client, "", Actor::Agent).await;
    let moved = issue(client, "", Actor::Agent).await;
    let orphan = issue(client, "", Actor::Agent).await;
    let by_flipped = add(client, &ws, &tree, "flipped", &flipped.signing_key).await;
    let by_moved = add(client, &ws, &tree, "moved", &moved.signing_key).await;
    let by_orphan = add(client, &ws, &tree, "orphan", &orphan.signing_key).await;
    let frank = client
        .get_identity("frank")
        .await
        .unwrap()
        .identity
        .unwrap();
    let path = daemon.state_dir().join("reef.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();

    // A flipped byte in the certificate: invalid.
    let mut certificate = flipped.certificate.clone();
    let middle = certificate.len() / 2;
    certificate[middle] ^= 0x01;
    conn.execute(
        "UPDATE session_keys SET certificate = ?1 WHERE key_id = ?2",
        rusqlite::params![certificate, flipped.record.key_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_flipped.evidence_id).await;
    assert_eq!(status_of(&read), IdentityStatus::Invalid);
    assert_eq!(read.signature(), SignatureCheck::Valid);
    let key = client
        .get_session_key(&flipped.record.key_id)
        .await
        .unwrap()
        .key
        .unwrap();
    assert!(!key.certificate_valid);
    // Only the daemon's own records of it (review finding 2).
    assert_eq!(key.key_id, flipped.record.key_id);
    assert!(key.issued_at.is_some());
    assert_eq!(key.identity, "");
    assert_eq!(key.identity_id, "");
    assert_eq!(key.session_id, "");
    assert_eq!(key.actor(), SessionActor::Unspecified);
    assert!(key.not_after.is_none() && key.not_before.is_none());
    let invalid = read.identity.as_ref().unwrap().key.as_ref().unwrap();
    assert_eq!(invalid.identity, "");
    // And never in an identity filter.
    let default_name = flipped.record.identity.clone();
    let filtered = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                identities: vec![default_name.clone()],
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert!(
        filtered
            .items
            .iter()
            .all(|e| e.evidence_id != by_flipped.evidence_id)
    );

    // An `evidence.key_id` column pointed at another, certified key: the
    // signature no longer verifies, and no identity is borrowed from that
    // key (review finding 1).
    let bare = SigningKey::from_seed(&[11; 32]).unwrap();
    let by_bare = add(client, &ws, &tree, "bare", &bare).await;
    let certified = issue(client, "frank", Actor::Executor).await;
    add(client, &ws, &tree, "frank", &certified.signing_key).await;
    conn.execute(
        "UPDATE evidence SET key_id = ?1 WHERE id = ?2",
        rusqlite::params![certified.record.key_id, by_bare.evidence_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_bare.evidence_id).await;
    assert_eq!(read.signature(), SignatureCheck::Invalid);
    let identity = read.identity.unwrap();
    assert_eq!(identity.status(), IdentityStatus::Invalid);
    assert!(identity.key.is_none(), "{identity:?}");
    let franks = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                identities: vec!["frank".into()],
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert!(
        franks
            .items
            .iter()
            .all(|e| e.evidence_id != by_bare.evidence_id)
    );

    // A revoked session and identity, and the key's columns moved to a live
    // session and identity: revocations follow the certificate, so it reads
    // revoked (review finding 15).
    create_identity(client, "gina", Kind::Person).await;
    let gina = issue(client, "gina", Actor::Agent).await;
    let by_gina = add(client, &ws, &tree, "gina", &gina.signing_key).await;
    client
        .revoke_session_keys(RevokeSessionKeysRequest {
            target: Some(revoke_session_keys_request::Target::SessionId(
                gina.record.session_id.clone(),
            )),
            since_issued: true,
            ..RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    client
        .revoke_identity(RevokeIdentityRequest {
            name: "gina".into(),
            since_created: true,
            ..RevokeIdentityRequest::default()
        })
        .await
        .unwrap();
    let live = issue(client, "frank", Actor::Agent).await;
    conn.execute(
        "UPDATE session_keys SET identity_id = ?1, session_id = ?2 WHERE key_id = ?3",
        rusqlite::params![frank.id, live.record.session_id, gina.record.key_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_gina.evidence_id).await;
    assert_eq!(status_of(&read), IdentityStatus::Revoked);
    assert_eq!(read.identity.unwrap().key.unwrap().identity, "gina");

    // The identity column moved to frank: the certificate decides.
    conn.execute(
        "UPDATE session_keys SET identity_id = ?1 WHERE key_id = ?2",
        rusqlite::params![frank.id, moved.record.key_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_moved.evidence_id).await;
    assert_eq!(status_of(&read), IdentityStatus::Bound);
    assert_eq!(
        read.identity.unwrap().key.unwrap().identity,
        moved.record.identity
    );
    let filtered = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                identities: vec!["frank".into()],
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert!(
        filtered
            .items
            .iter()
            .all(|e| e.evidence_id != by_moved.evidence_id),
        "{:?}",
        filtered.items
    );

    // The authority row deleted: invalid.
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "DELETE FROM identity_authorities WHERE id = ?1",
        [&orphan.record.authority_key_id],
    )
    .unwrap();
    let read = get(client, &repo.id, &by_orphan.evidence_id).await;
    assert_eq!(status_of(&read), IdentityStatus::Invalid);
    drop(conn);
    daemon.stop().await;
}

// 12.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "GC, backup, restore and forget in order"
)]
async fn gc_backup_restore() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (ws, tree) = workspace(client, &repo.id).await;
    let db = Arc::clone(&daemon.handle.state().db);
    let named = issue(client, "", Actor::Agent).await;
    let evidence = add(client, &ws, &tree, "named", &named.signing_key).await;
    let expired = issue(client, "", Actor::Agent).await;
    let recent = issue(client, "", Actor::Agent).await;
    let now = reef_daemon::model::Timestamp::now().0;
    let old = |days: i64| reef_daemon::model::Timestamp(now - days * 86_400_000);
    for key in [&named, &expired] {
        db.set_session_key_window(&key.record.key_id, old(9), old(8))
            .unwrap();
    }
    db.set_session_key_window(&recent.record.key_id, old(6), old(5))
        .unwrap();
    let (_, _, sessions_before, keys_before) = db.identity_counts().unwrap();
    assert_eq!(keys_before, 3);

    let dry = client.collect_garbage(&repo.id, true).await.unwrap();
    assert_eq!(dry.reports[0].session_keys_removed, 1);
    assert_eq!(
        db.identity_counts().unwrap().3,
        3,
        "a dry run deletes nothing"
    );
    let ran = client.collect_garbage(&repo.id, false).await.unwrap();
    assert_eq!(ran.reports[0].session_keys_removed, 1);
    let (_, _, sessions_after, keys_after) = db.identity_counts().unwrap();
    assert_eq!(keys_after, 2);
    assert_eq!(
        sessions_after,
        sessions_before - 1,
        "its empty session went"
    );
    let error = client
        .get_session_key(&expired.record.key_id)
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "SESSION_KEY_NOT_FOUND");
    client.get_session_key(&named.record.key_id).await.unwrap();
    client.get_session_key(&recent.record.key_id).await.unwrap();
    db.set_session_key_window(
        &named.record.key_id,
        reef_daemon::model::Timestamp(now - 60_000),
        reef_daemon::model::Timestamp(now + 3_600_000),
    )
    .unwrap();

    // A backup has the four tables and not the authority's key.
    let dest = daemon.tmp.path().join("backup");
    let mut stream = client.backup(&dest, "").await.unwrap();
    while stream.message().await.unwrap().is_some() {}
    let files: Vec<String> = walkdir::WalkDir::new(&dest)
        .into_iter()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !files.iter().any(|name| name.contains("authority")),
        "{files:?}"
    );
    let copy = reef_daemon::db::Db::open(&dest.join("reef.sqlite3")).unwrap();
    assert_eq!(
        copy.identity_counts().unwrap(),
        db.identity_counts().unwrap()
    );
    drop(copy);

    // Restored into a second state directory: the restored authority is
    // retired, the next start makes a new one, and evidence keeps its
    // identity.
    let into = daemon.tmp.path().join("restored");
    let root = daemon.tmp.path().join("restored-root");
    std::fs::create_dir_all(&root).unwrap();
    let options = reef_daemon::maintenance::restore::RestoreOptions {
        into: into.clone(),
        repo: Some(repo.id.parse().unwrap()),
        root: Some(root),
        force: false,
        lease: daemon.config.file.lease.clone(),
        settings: test_settings(),
    };
    let from = dest.clone();
    tokio::task::spawn_blocking(move || {
        reef_daemon::maintenance::restore::restore(&from, &options)
    })
    .await
    .unwrap()
    .unwrap();
    let restored = Arc::new(reef_daemon::db::Db::open(&into.join("reef.sqlite3")).unwrap());
    let authorities = restored.authorities().unwrap();
    assert!(authorities.iter().all(|row| row.retired_at.is_some()));
    let mut config = daemon.config.clone();
    config.paths = reef_daemon::config::Paths::in_state_dir(&into).unwrap();
    let started = {
        let (config, restored) = (config.clone(), Arc::clone(&restored));
        tokio::task::spawn_blocking(move || {
            reef_daemon::identity::Identities::start(&config, restored)
        })
        .await
        .unwrap()
        .unwrap()
    };
    let current = started.current_authority();
    assert!(!authorities.iter().any(|row| row.id == current.id));
    assert!(into.join("identity").join("authority.pem").is_file());
    let rows = restored
        .session_keys_by_id(&[named.record.key_id.as_str()])
        .unwrap();
    let reads = reef_daemon::identity::read::read_keys(&restored, rows, None).unwrap();
    let added = at_ms(evidence.added_at.as_ref());
    assert_eq!(
        reads[0].status_at(reef_daemon::model::Timestamp(added)),
        reef_evidence::session::IdentityStatus::Bound
    );

    // Forget leaves every identity table alone.
    let counts = db.identity_counts().unwrap();
    client
        .release_workspace(&repo.id, &ws.id, false, true)
        .await
        .unwrap();
    client.forget_repository(&repo.id).await.unwrap();
    assert_eq!(db.identity_counts().unwrap(), counts);
    daemon.stop().await;
}

// Review finding 15: an in-place restore, whose state directory still holds
// the old authority key file, which matches the restored (now retired) row,
// moves issuance to a new authority all the same.
#[tokio::test(flavor = "multi_thread")]
async fn in_place_restore_retires_the_authority() {
    let (daemon, repo) = fixture().await;
    let before = daemon.client.list_authorities_all().await.unwrap();
    let old_file = std::fs::read(authority_file(&daemon)).unwrap();
    let dest = daemon.tmp.path().join("backup");
    let mut stream = daemon.client.backup(&dest, "").await.unwrap();
    while stream.message().await.unwrap().is_some() {}
    let stopped = daemon.stop_keeping().await;
    let options = reef_daemon::maintenance::restore::RestoreOptions {
        into: stopped.config.paths.state_dir.clone(),
        repo: Some(repo.id.parse().unwrap()),
        root: None,
        force: true,
        lease: stopped.config.file.lease.clone(),
        settings: test_settings(),
    };
    tokio::task::spawn_blocking(move || {
        reef_daemon::maintenance::restore::restore(&dest, &options)
    })
    .await
    .unwrap()
    .unwrap();
    let path = stopped.config.paths.identity.join("authority.pem");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        old_file,
        "the old key is still there"
    );
    let daemon = stopped.start().await;
    let after = daemon.client.list_authorities_all().await.unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert!(after[0].current);
    assert_ne!(after[0].key_id, before[0].key_id);
    assert!(
        after
            .iter()
            .any(|a| a.key_id == before[0].key_id && !a.current)
    );
    assert_ne!(
        std::fs::read(&path).unwrap(),
        old_file,
        "a new key replaced it"
    );
    daemon.stop().await;
}

// Review finding 15: with `keep_newer` more than 7 days back (a 10-day undo
// window), GC keeps a key that ended 8 days ago and sweeps one that ended
// 11 days ago: the floor is the earlier of the two.
#[tokio::test(flavor = "multi_thread")]
async fn gc_floor_follows_an_older_keep_newer() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = Duration::from_hours(10 * 24);
    })
    .await;
    let client = &daemon.client;
    let db = Arc::clone(&daemon.handle.state().db);
    let kept = issue(client, "", Actor::Agent).await;
    let swept = issue(client, "", Actor::Agent).await;
    let now = reef_daemon::model::Timestamp::now().0;
    let ago = |days: i64| reef_daemon::model::Timestamp(now - days * 86_400_000);
    db.set_session_key_window(&kept.record.key_id, ago(9), ago(8))
        .unwrap();
    db.set_session_key_window(&swept.record.key_id, ago(12), ago(11))
        .unwrap();
    let ran = client.collect_garbage(&repo.id, false).await.unwrap();
    assert_eq!(ran.reports[0].session_keys_removed, 1);
    client.get_session_key(&kept.record.key_id).await.unwrap();
    let error = client
        .get_session_key(&swept.record.key_id)
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "SESSION_KEY_NOT_FOUND");
    daemon.stop().await;
}
