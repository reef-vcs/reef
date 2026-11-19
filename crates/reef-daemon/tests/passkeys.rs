//! Passkeys and approvals through the daemon (R3.43, the brief's tests 5 to
//! 9): registration from the web UI and its refusals, approvals requested
//! on the socket and approved with a passkey's assertion through the web
//! UI, the one-sign-off rules of `AddEvidence`, policy counting only pinned
//! passkeys at a landing, and a reset. Ceremonies use
//! `reef_evidence::webauthn::testing::Authenticator`, which writes the
//! bytes a browser and a platform authenticator would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant, UNIX_EPOCH};

use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use reef_api::v1::evidence_service_client::EvidenceServiceClient;
use reef_api::v1::identity_service_client::IdentityServiceClient;
use reef_api::v1::{
    self, AddEvidenceRequest, ApprovalState, CreateIdentityRequest, CreateWebSessionResponse,
    EditFilesRequest, FileEdit, GetApprovalRequest, GetChangeRequest, IdentityKind as Kind,
    LandChangeRequest, LandingOutcome, LandingReason, LifecycleState, ListPasskeysRequest,
    PasskeyAssertion, RegisterPasskeyRequest, RemoteScope, RemovePasskeyRequest,
    RequestApprovalRequest, ResetPasskeysRequest, ResolveApprovalRequest, SessionPresence,
    StartPasskeyRegistrationRequest, TransitionChangeRequest, edit_files_request, file_edit,
    resolve_approval_request,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::{
    Actor, Authority, IssuerKind, Presence, ProofFor, check_certificate, sign_proof,
};
use reef_evidence::webauthn::Algorithm;
use reef_evidence::webauthn::testing::Authenticator;
use reef_evidence::{Claim, Outcome, PublicKey, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;
use tonic::body::Body;
use tonic::codegen::http::Uri;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Code, Request, Status};
use tonic_web::{GrpcWebClientLayer, GrpcWebClientService};
use tower::Layer as _;

const RP: &str = "localhost";

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
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

// ---- the web UI, over gRPC-web ----

/// Adds `authorization: Bearer <token>`.
#[derive(Clone)]
struct Bearer(String);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.0).parse().unwrap(),
        );
        Ok(request)
    }
}

type Web = InterceptedService<
    GrpcWebClientService<HttpClient<HttpConnector, tonic_web::GrpcWebCall<Body>>>,
    Bearer,
>;

/// A web UI session, as a page holds it.
struct Ui {
    session: CreateWebSessionResponse,
    token: String,
    /// `http://localhost:<port>`, the page's origin.
    origin: String,
}

async fn ui(client: &Client) -> Ui {
    let session = client.create_web_session(false, "").await.unwrap();
    let token = session
        .session_url
        .split_once("#session=")
        .unwrap()
        .1
        .to_owned();
    let origin = session.url.trim_end_matches('/').to_owned();
    Ui {
        session,
        token,
        origin,
    }
}

impl Ui {
    fn service(&self) -> (Web, Uri) {
        let client = HttpClient::builder(TokioExecutor::new()).build_http();
        let service = GrpcWebClientLayer::new().layer(client);
        (
            InterceptedService::new(service, Bearer(self.token.clone())),
            self.session.url.parse().unwrap(),
        )
    }

    fn identities(&self) -> IdentityServiceClient<Web> {
        let (service, origin) = self.service();
        IdentityServiceClient::with_origin(service, origin)
    }

    fn evidence(&self) -> EvidenceServiceClient<Web> {
        let (service, origin) = self.service();
        EvidenceServiceClient::with_origin(service, origin)
    }
}

/// A refusal's code, reason and metadata.
fn refusal(status: &Status) -> (Code, String, BTreeMap<String, String>) {
    let detail = reef_daemon::error::detail_of(status).unwrap_or_default();
    (
        status.code(),
        detail.reason,
        detail.metadata.into_iter().collect(),
    )
}

fn reason(status: &Status) -> String {
    refusal(status).1
}

fn check(status: &Status) -> String {
    refusal(status).2.get("check").cloned().unwrap_or_default()
}

// ---- passkeys ----

/// `authenticator`'s assertion over `challenge` from a page at `origin`.
fn assertion(
    authenticator: &mut Authenticator,
    origin: &str,
    challenge: &[u8],
) -> PasskeyAssertion {
    let challenge: [u8; 32] = challenge.try_into().unwrap();
    let asserted = authenticator.assert(RP, origin, &challenge);
    PasskeyAssertion {
        credential_id: authenticator.credential_id().to_vec(),
        authenticator_data: asserted.authenticator_data,
        client_data_json: asserted.client_data_json,
        signature: asserted.signature,
    }
}

/// Starts a registration for `identity` through `ui`, and the request that
/// completes it with `authenticator` from a page at `origin`, approved by
/// `approver` when given.
async fn registration(
    ui: &Ui,
    authenticator: &mut Authenticator,
    identity: &str,
    origin: &str,
    approver: Option<&mut Authenticator>,
) -> (v1::StartPasskeyRegistrationResponse, RegisterPasskeyRequest) {
    let started = ui
        .identities()
        .start_passkey_registration(StartPasskeyRegistrationRequest {
            identity: identity.to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    let challenge: [u8; 32] = started.challenge.clone().try_into().unwrap();
    let registered = authenticator.register(RP, origin, &challenge);
    let approval = approver.map(|approver| assertion(approver, origin, &started.add_challenge));
    let request = RegisterPasskeyRequest {
        challenge: started.challenge.clone(),
        label: "test passkey".to_owned(),
        client_data_json: registered.client_data_json,
        authenticator_data: registered.authenticator_data,
        public_key: registered.public_key,
        algorithm: i32::try_from(registered.algorithm).unwrap(),
        approval,
    };
    (started, request)
}

/// Registers `authenticator` for `identity` through `ui`.
async fn register(
    ui: &Ui,
    authenticator: &mut Authenticator,
    identity: &str,
    approver: Option<&mut Authenticator>,
) -> Result<v1::Passkey, Status> {
    let (_, request) = registration(ui, authenticator, identity, &ui.origin, approver).await;
    ui.identities()
        .register_passkey(request)
        .await
        .map(|response| response.into_inner().passkey.unwrap())
}

async fn create_identity(client: &Client, name: &str) -> v1::Identity {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap()
        .identity
        .unwrap()
}

// ---- changes and statements ----

/// A new commit on `on` writing each of `files`, outside any workspace.
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

fn signoff(target: Option<&str>) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: "signoff".to_owned(),
        result: Outcome::Pass,
        target: target.map(str::to_owned),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_801),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
}

/// The statement payload of `claim` about `tree`.
fn payload(claim: &Claim, tree: &str) -> Vec<u8> {
    claim.payload(&TreeDigest::git(tree).unwrap()).unwrap()
}

/// A fresh session key.
fn fresh_key() -> SigningKey {
    let (pem, _) = SigningKey::generate().unwrap();
    SigningKey::from_pkcs8_pem(&pem).unwrap()
}

async fn current_authority(client: &Client) -> String {
    client
        .list_authorities_all()
        .await
        .unwrap()
        .into_iter()
        .find(|authority| authority.current)
        .unwrap()
        .key_id
}

/// `RequestApproval` over the socket, as `reef evidence add --verified`
/// sends it: `key`'s proof for `identity`, the actor `human` and a new
/// session.
async fn request_approval(
    client: &Client,
    identity: &str,
    repo: &str,
    revision: &str,
    statement: Vec<u8>,
    key: &SigningKey,
) -> Result<v1::Approval, ClientError> {
    let authority = reef_evidence::KeyId::parse(&current_authority(client).await).unwrap();
    let proof = sign_proof(
        key,
        &ProofFor {
            authority: &authority,
            identity,
            actor: Actor::Human,
            session: "",
        },
    );
    client
        .request_approval(RequestApprovalRequest {
            identity: identity.to_owned(),
            repo: repo.to_owned(),
            workspace: String::new(),
            revision: revision.to_owned(),
            statement,
            public_key: key.public_key().spki().to_vec(),
            proof,
        })
        .await
        .map(|response| response.approval.unwrap())
}

/// Approves `approval` through `ui` with `authenticator`.
async fn approve(
    ui: &Ui,
    approval: &v1::Approval,
    authenticator: &mut Authenticator,
) -> Result<v1::Approval, Status> {
    let assertion = assertion(authenticator, &ui.origin, &approval.challenge);
    ui.identities()
        .resolve_approval(ResolveApprovalRequest {
            id: approval.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Approve(assertion)),
        })
        .await
        .map(|response| response.into_inner().approval.unwrap())
}

/// An approved sign-off by `identity` of `change`, approved through `ui`
/// with `authenticator`: the key, the statement, and the approval.
async fn signed_off(
    client: &Client,
    ui: &Ui,
    repo: &str,
    identity: &str,
    change: &v1::Change,
    authenticator: &mut Authenticator,
) -> (SigningKey, Vec<u8>, v1::Approval) {
    let key = fresh_key();
    let statement = payload(&signoff(None), &change.tree_id);
    let approval = request_approval(
        client,
        identity,
        repo,
        &change.commit_id,
        statement.clone(),
        &key,
    )
    .await
    .unwrap();
    let approved = approve(ui, &approval, authenticator).await.unwrap();
    (key, statement, approved)
}

fn add_request(
    repo: &str,
    revision: &str,
    envelope: Vec<u8>,
    key: &SigningKey,
) -> AddEvidenceRequest {
    AddEvidenceRequest {
        repo: repo.to_owned(),
        workspace: String::new(),
        envelope,
        public_key: key.public_key().spki().to_vec(),
        revision: revision.to_owned(),
    }
}

fn sign(statement: &[u8], key: &SigningKey) -> Vec<u8> {
    key.sign(reef_evidence::PAYLOAD_TYPE, statement).to_bytes()
}

// ---- the remote listener ----

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

/// A client through the remote listener with a write token linked to
/// `identity`.
async fn remote_client(daemon: &TestDaemon, identity: &str) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(
            "remote",
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

// ---- tests ----

// 5. Registration.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every registration rule, in order")]
async fn registration_rules() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    let alice = create_identity(client, "alice").await;
    let page = ui(client).await;

    // The first passkey needs nothing but a web UI session.
    let mut first = Authenticator::new(Algorithm::EdDsa);
    let (started, request) = registration(&page, &mut first, "alice", &page.origin, None).await;
    assert_eq!(started.rp_id, "localhost");
    assert_eq!(started.user_name, "Reef: alice");
    assert_eq!(started.identity_id, alice.id);
    assert_eq!(started.algorithms, [-8, -7, -257]);
    assert!(!started.needs_approval && started.credential_ids.is_empty());
    let added = page
        .identities()
        .register_passkey(request.clone())
        .await
        .unwrap()
        .into_inner()
        .passkey
        .unwrap();
    assert!(added.id.starts_with("pk_") && added.id.len() == 19);
    assert_eq!(added.identity, "alice");
    assert_eq!(added.fingerprint, first.passkey().fingerprint().as_str());
    assert_eq!(added.algorithm, -8);
    assert_eq!(added.label, "test passkey");
    assert_eq!(added.sign_count, 1);
    assert_eq!(added.created_web_session.unwrap().session.len(), 16);
    // Its challenge was single use.
    let used_twice = page
        .identities()
        .register_passkey(request)
        .await
        .unwrap_err();
    assert_eq!(
        (reason(&used_twice), check(&used_twice)),
        (
            "PASSKEY_ASSERTION_INVALID".to_owned(),
            "challenge".to_owned()
        )
    );

    // A second one needs an assertion by the first over the add.
    let mut second = Authenticator::new(Algorithm::Es256);
    let refused = register(&page, &mut second, "alice", None)
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused).0, Code::FailedPrecondition);
    assert_eq!(reason(&refused), "PASSKEY_REQUIRED");
    let (started, _) = registration(&page, &mut second, "alice", &page.origin, None).await;
    assert!(started.needs_approval);
    assert_eq!(started.credential_ids, [first.credential_id().to_vec()]);
    let mut stranger = Authenticator::new(Algorithm::EdDsa);
    let refused = register(&page, &mut second, "alice", Some(&mut stranger))
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "passkey");
    let second_key = register(&page, &mut second, "alice", Some(&mut first))
        .await
        .unwrap();
    assert_eq!(second_key.algorithm, -7);
    // The same credential again.
    let again = register(&page, &mut second, "alice", Some(&mut first))
        .await
        .unwrap_err();
    assert_eq!(refusal(&again).0, Code::AlreadyExists);
    assert_eq!(reason(&again), "PASSKEY_EXISTS");

    // A challenge another web UI session started.
    let other = ui(client).await;
    let mut rsa = Authenticator::new(Algorithm::Rs256);
    let (_, request) = registration(&page, &mut rsa, "alice", &page.origin, Some(&mut first)).await;
    let refused = other
        .identities()
        .register_passkey(request.clone())
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "challenge");
    // Left for its own session, which uses it.
    page.identities().register_passkey(request).await.unwrap();

    // Another origin, the RP ID's but another port: a dev server's.
    let mut fourth = Authenticator::new(Algorithm::EdDsa);
    let (_, request) = registration(
        &page,
        &mut fourth,
        "alice",
        "http://localhost:1",
        Some(&mut first),
    )
    .await;
    let refused = page
        .identities()
        .register_passkey(request)
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "origin");
    // An algorithm Reef does not verify, and a key not of the one named.
    for algorithm in [-35, -7] {
        let (_, mut request) =
            registration(&page, &mut fourth, "alice", &page.origin, Some(&mut first)).await;
        request.algorithm = algorithm;
        let refused = page
            .identities()
            .register_passkey(request)
            .await
            .unwrap_err();
        assert_eq!(reason(&refused), "PASSKEY_ALGORITHM", "{algorithm}");
    }
    // No user verification.
    fourth.set_user_verified(false);
    let refused = register(&page, &mut fourth, "alice", Some(&mut first))
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "flags");

    // Listed, then one removed with an assertion over its removal.
    let listed = client
        .list_passkeys(ListPasskeysRequest {
            identity: "alice".to_owned(),
            ..ListPasskeysRequest::default()
        })
        .await
        .unwrap()
        .passkeys;
    assert_eq!(listed.len(), 3);
    let removal = RemovePasskeyRequest {
        id: second_key.id.clone(),
        assertion: None,
        reason: "lost".to_owned(),
    };
    let refused = page
        .identities()
        .remove_passkey(removal.clone())
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "PASSKEY_REQUIRED");
    let challenge = reef_evidence::webauthn::passkey_change_challenge(
        reef_evidence::webauthn::PasskeyChange::Remove {
            passkey: &second_key.id,
        },
    );
    let removed = page
        .identities()
        .remove_passkey(RemovePasskeyRequest {
            assertion: Some(assertion(&mut first, &page.origin, &challenge)),
            ..removal.clone()
        })
        .await
        .unwrap()
        .into_inner()
        .passkey
        .unwrap();
    assert!(removed.removed_at.is_some());
    assert_eq!(removed.removed_reason, "removed: lost");
    let refused = page
        .identities()
        .remove_passkey(RemovePasskeyRequest {
            assertion: Some(assertion(&mut first, &page.origin, &challenge)),
            ..removal
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "PASSKEY_NOT_FOUND");
    let all = client
        .list_passkeys(ListPasskeysRequest {
            identity: String::new(),
            include_removed: true,
            ..ListPasskeysRequest::default()
        })
        .await
        .unwrap()
        .passkeys;
    assert_eq!(all.len(), 3);

    // A ci identity has no passkeys.
    client
        .create_identity(CreateIdentityRequest {
            name: "ci-main".to_owned(),
            kind: Kind::Ci.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let refused = page
        .identities()
        .start_passkey_registration(StartPasskeyRegistrationRequest {
            identity: "ci-main".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "SESSION_KEY_REFUSED");
    daemon.stop().await;
}

// 5. Off the web UI, passkey and approval writes are refused: on the
// socket for a ceremony, and every one through the remote listener.
#[tokio::test(flavor = "multi_thread")]
async fn passkey_writes_need_the_web_ui() {
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let socket = client
        .start_passkey_registration(StartPasskeyRegistrationRequest::default())
        .await
        .unwrap_err();
    assert_eq!(socket.reason(), "PASSKEY_NEEDS_WEB");
    for error in [
        client
            .register_passkey(RegisterPasskeyRequest::default())
            .await
            .unwrap_err(),
        client
            .remove_passkey(RemovePasskeyRequest::default())
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.reason(), "PASSKEY_NEEDS_WEB");
    }

    let remote = remote_client(&daemon, "alice").await;
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let key = fresh_key();
    let statement = payload(&signoff(None), &change.tree_id);
    for error in [
        remote
            .start_passkey_registration(StartPasskeyRegistrationRequest::default())
            .await
            .unwrap_err(),
        remote
            .register_passkey(RegisterPasskeyRequest::default())
            .await
            .unwrap_err(),
        remote
            .remove_passkey(RemovePasskeyRequest::default())
            .await
            .unwrap_err(),
        request_approval(&remote, "", &repo.id, &change.commit_id, statement, &key)
            .await
            .unwrap_err(),
        remote
            .resolve_approval(ResolveApprovalRequest {
                id: "apr_0000000000000000".to_owned(),
                resolution: Some(resolve_approval_request::Resolution::Decline(String::new())),
            })
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.reason(), "PASSKEY_OVER_REMOTE", "{error:?}");
    }
    daemon.stop().await;
}

// 6. Approvals.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the approval and each refusal")]
async fn approvals() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let alice = create_identity(client, "alice").await;
    create_identity(client, "bob").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::Es256);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let other = commit_on(client, &repo.id, "root()", &[("b.txt", "b\n")]).await;

    // A CLI-style request over the socket: pending, with its challenge.
    let key = fresh_key();
    let statement = payload(&signoff(Some("review")), &change.tree_id);
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &key,
    )
    .await
    .unwrap();
    assert_eq!(approval.state(), ApprovalState::Pending);
    assert!(approval.id.starts_with("apr_"));
    assert_eq!(approval.identity, "alice");
    assert_eq!(approval.key_id, key.public_key().id().as_str());
    assert_eq!(
        (
            approval.kind.as_str(),
            approval.result.as_str(),
            approval.target.as_str()
        ),
        ("signoff", "pass", "review")
    );
    assert_eq!(approval.tree_id, change.tree_id);
    assert_eq!(approval.change_id, change.change_id);
    assert_eq!(approval.requested_through, "socket");
    assert_eq!(approval.credential_ids, [passkey.credential_id().to_vec()]);
    assert_eq!(
        approval.approves.as_ref().unwrap().statement_sha256,
        reef_evidence::sha256_hex(&statement)
    );
    let expires = approval.expires_at.unwrap().seconds - approval.created_at.unwrap().seconds;
    assert_eq!(expires, 120);

    // The socket cannot approve.
    let socket = client
        .resolve_approval(ResolveApprovalRequest {
            id: approval.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Approve(assertion(
                &mut passkey,
                &page.origin,
                &approval.challenge,
            ))),
        })
        .await
        .unwrap_err();
    assert_eq!(socket.reason(), "APPROVAL_NEEDS_WEB");

    // Another identity's passkey, and a stale sign count.
    let mut bobs = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut bobs, "bob", None).await.unwrap();
    let refused = approve(&page, &approval, &mut bobs).await.unwrap_err();
    assert_eq!(check(&refused), "passkey");
    passkey.set_sign_count(Some(0));
    let refused = approve(&page, &approval, &mut passkey).await.unwrap_err();
    assert_eq!(check(&refused), "counter");
    passkey.set_sign_count(Some(100));

    // Approved through the web UI: a 2-minute user_verified key naming the
    // passkey, the approval and what it approves.
    let approved = approve(&page, &approval, &mut passkey).await.unwrap();
    assert_eq!(approved.state(), ApprovalState::Approved);
    let issued = approved.key.clone().unwrap();
    assert_eq!(issued.presence(), SessionPresence::UserVerified);
    assert_eq!(issued.actor(), v1::SessionActor::Human);
    assert_eq!(issued.identity_id, alice.id);
    assert_eq!(issued.session_id, approval.session_id);
    assert_eq!(issued.approval_id, approval.id);
    let named = issued.passkey.clone().unwrap();
    assert_eq!(named.id, registered.id);
    assert_eq!(named.fingerprint, registered.fingerprint);
    assert_eq!(issued.approves, approval.approves);
    assert_eq!(issued.web_session.as_ref().unwrap().session.len(), 16);
    assert_eq!(
        issued.not_after.unwrap().seconds - issued.not_before.unwrap().seconds,
        120
    );
    // Any verifier checks the assertion again from the certificate alone.
    let authorities = client.list_authorities_all().await.unwrap();
    let lookup = |id: &reef_evidence::KeyId| {
        authorities
            .iter()
            .find(|authority| authority.key_id == id.as_str())
            .map(|authority| Authority {
                key: PublicKey::from_spki(&authority.public_key).unwrap(),
                kind: IssuerKind::ReefDaemon,
                revocation: None,
            })
    };
    let certified = check_certificate(&issued.certificate, &lookup).unwrap();
    assert_eq!(certified.binding.presence.kind(), Presence::UserVerified);
    assert_eq!(
        certified
            .binding
            .presence
            .evidence()
            .unwrap()
            .approves
            .change,
        change.change_id
    );
    // Read back, and never approved twice.
    let read = client
        .get_approval(GetApprovalRequest {
            id: approval.id.clone(),
            wait: None,
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(read.key.unwrap().certificate, issued.certificate);
    let stored = client
        .get_session_key(&issued.key_id)
        .await
        .unwrap()
        .key
        .unwrap();
    assert_eq!(stored.presence(), SessionPresence::UserVerified);
    assert_eq!(stored.passkey.unwrap().label, "test passkey");
    let twice = approve(&page, &approval, &mut passkey).await.unwrap_err();
    assert_eq!(reason(&twice), "APPROVAL_RESOLVED");

    // A statement whose subject is not the revision's tree.
    let error = request_approval(
        client,
        "alice",
        &repo.id,
        &other.commit_id,
        statement.clone(),
        &fresh_key(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), "EVIDENCE_TREE_MISMATCH");
    // A key already certified.
    let error = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &key,
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), "SESSION_KEY_EXISTS");

    // A removed passkey approves nothing.
    let mut spare = Authenticator::new(Algorithm::EdDsa);
    let spare_key = register(&page, &mut spare, "alice", Some(&mut passkey))
        .await
        .unwrap();
    let challenge = reef_evidence::webauthn::passkey_change_challenge(
        reef_evidence::webauthn::PasskeyChange::Remove {
            passkey: &spare_key.id,
        },
    );
    page.identities()
        .remove_passkey(RemovePasskeyRequest {
            id: spare_key.id.clone(),
            assertion: Some(assertion(&mut spare, &page.origin, &challenge)),
            reason: String::new(),
        })
        .await
        .unwrap();
    let pending = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &fresh_key(),
    )
    .await
    .unwrap();
    let refused = approve(&page, &pending, &mut spare).await.unwrap_err();
    assert_eq!(check(&refused), "passkey");
    // Nor does an assertion over another approval's challenge.
    let replayed = assertion(&mut passkey, &page.origin, &approval.challenge);
    let refused = page
        .identities()
        .resolve_approval(ResolveApprovalRequest {
            id: pending.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Approve(replayed)),
        })
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "challenge");

    // An expired approval.
    daemon
        .handle
        .state()
        .identity
        .set_approval_wait(Duration::from_millis(200));
    let short = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement,
        &fresh_key(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let refused = approve(&page, &short, &mut passkey).await.unwrap_err();
    assert_eq!(reason(&refused), "APPROVAL_EXPIRED");
    let read = client
        .get_approval(GetApprovalRequest {
            id: short.id.clone(),
            wait: None,
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(read.state(), ApprovalState::Expired);
    let missing = client
        .get_approval(GetApprovalRequest {
            id: "apr_0000000000000000".to_owned(),
            wait: None,
        })
        .await
        .unwrap_err();
    assert_eq!(missing.reason(), "APPROVAL_NOT_FOUND");
    daemon.stop().await;
}

// 6. `GetApproval { wait }` returns on a resolution and on the expiry; a
// decline ends an approval.
#[tokio::test(flavor = "multi_thread")]
async fn approval_waits() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let statement = payload(&signoff(None), &change.tree_id);
    let wait = |id: String| {
        let client = client.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let approval = client
                .get_approval(GetApprovalRequest {
                    id,
                    wait: Some(prost_types::Duration {
                        seconds: 60,
                        nanos: 0,
                    }),
                })
                .await
                .unwrap()
                .approval
                .unwrap();
            (approval, started.elapsed())
        })
    };

    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &fresh_key(),
    )
    .await
    .unwrap();
    let waiting = wait(approval.id.clone());
    tokio::time::sleep(Duration::from_millis(200)).await;
    approve(&page, &approval, &mut passkey).await.unwrap();
    let (approved, took) = waiting.await.unwrap();
    assert_eq!(approved.state(), ApprovalState::Approved);
    assert!(approved.key.is_some());
    assert!(took < Duration::from_secs(10), "{took:?}");

    // A decline, from the socket.
    let declined = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &fresh_key(),
    )
    .await
    .unwrap();
    let waiting = wait(declined.id.clone());
    let response = client
        .resolve_approval(ResolveApprovalRequest {
            id: declined.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Decline(
                "not now".to_owned(),
            )),
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(response.state(), ApprovalState::Declined);
    assert_eq!(response.declined_reason, "not now");
    assert_eq!(waiting.await.unwrap().0.state(), ApprovalState::Declined);
    let refused = approve(&page, &declined, &mut passkey).await.unwrap_err();
    assert_eq!(reason(&refused), "APPROVAL_DECLINED");

    // The expiry ends the wait.
    daemon
        .handle
        .state()
        .identity
        .set_approval_wait(Duration::from_millis(300));
    let expiring = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement,
        &fresh_key(),
    )
    .await
    .unwrap();
    let (expired, took) = wait(expiring.id.clone()).await.unwrap();
    assert_eq!(expired.state(), ApprovalState::Expired);
    assert!(took < Duration::from_secs(10), "{took:?}");
    daemon.stop().await;
}

// 7. `AddEvidence` with a presence key: the approved statement for the
// approved change, once.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "each rule, then the read")]
async fn presence_keys_sign_one_sign_off() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    // Another change with the same tree.
    let twin = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    assert_eq!(twin.tree_id, change.tree_id);
    assert_ne!(twin.change_id, change.change_id);
    let (key, statement, _) =
        signed_off(client, &page, &repo.id, "alice", &change, &mut passkey).await;
    let envelope = sign(&statement, &key);

    // Without the approved change, or another change.
    for revision in ["", twin.commit_id.as_str()] {
        let error = client
            .add_evidence(add_request(&repo.id, revision, envelope.clone(), &key))
            .await
            .unwrap_err();
        assert_eq!(
            error.reason(),
            "PRESENCE_STATEMENT_MISMATCH",
            "{revision:?}"
        );
        assert_eq!(error.metadata()["field"], "change");
    }
    // Another statement.
    let other = sign(&payload(&signoff(Some("other")), &change.tree_id), &key);
    let error = client
        .add_evidence(add_request(&repo.id, &change.commit_id, other, &key))
        .await
        .unwrap_err();
    assert_eq!(error.reason(), "PRESENCE_STATEMENT_MISMATCH");
    assert_eq!(error.metadata()["field"], "statement");

    // The approved one, once; the same object again changes nothing.
    let added = client
        .add_evidence(add_request(
            &repo.id,
            &change.commit_id,
            envelope.clone(),
            &key,
        ))
        .await
        .unwrap();
    assert!(added.created);
    let evidence = added.evidence.unwrap();
    let signer = evidence.identity.as_ref().unwrap().key.as_ref().unwrap();
    assert_eq!(signer.presence(), SessionPresence::UserVerified);
    assert_eq!(evidence.change_ids, std::slice::from_ref(&change.change_id));
    let again = client
        .add_evidence(add_request(&repo.id, &change.commit_id, envelope, &key))
        .await
        .unwrap();
    assert!(!again.created);

    // Through the web UI: a presence key's sign-off, and nothing else.
    let none = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap()
        .signing_key;
    let refused = page
        .evidence()
        .add_evidence(add_request(
            &repo.id,
            &change.commit_id,
            sign(&statement, &none),
            &none,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        (refusal(&refused).0, reason(&refused)),
        (
            Code::PermissionDenied,
            "PRESENCE_STATEMENT_MISMATCH".to_owned()
        )
    );
    let (web_key, web_statement, _) =
        signed_off(client, &page, &repo.id, "alice", &twin, &mut passkey).await;
    let from_web = page
        .evidence()
        .add_evidence(add_request(
            &repo.id,
            &twin.commit_id,
            sign(&web_statement, &web_key),
            &web_key,
        ))
        .await
        .unwrap()
        .into_inner()
        .evidence
        .unwrap();
    assert_eq!(from_web.web_session.unwrap().session.len(), 16);

    // A read that finds the link gone (a row changed behind the daemon)
    // reports the key's presence as none.
    let path = daemon.state_dir().join("reef.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "DELETE FROM evidence_links WHERE evidence_id = ?1",
        [&evidence.evidence_id],
    )
    .unwrap();
    drop(conn);
    let read = client
        .get_evidence(&repo.id, &evidence.evidence_id)
        .await
        .unwrap()
        .evidence
        .unwrap();
    let signer = read.identity.unwrap().key.unwrap();
    assert_eq!(signer.presence(), SessionPresence::None);
    daemon.stop().await;
}

/// `main`'s policy: every change needs a sign-off by alice with a verified
/// presence, on a passkey pinned as `pinned`.
fn verified_policy(pinned: &str) -> String {
    format!(
        r#"
version = 1

[owners]
identities = ["alice"]

[passkeys]
alice = ["{pinned}"]

[[signoff]]
name = "verified"
identities = ["alice"]
presence = "user_verified"

[landing]
checks = "proposal"
"#
    )
}

/// Sets `main` from outside the daemon, as `jj bookmark set` would.
async fn set_main(repo: &SyntheticRepo, commit: &str) {
    let (root, commit) = (repo.root.clone(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
            default_workspace_loader_factory,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let workspace = default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &test_settings(),
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(
            RefName::new("main"),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

async fn propose_and_land(client: &Client, repo: &str, change: &v1::Change) -> v1::Landing {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            to: LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    client
        .land_change(LandChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap()
}

// 8. Policy end to end: a `[[signoff]]` that needs `user_verified` lands a
// change signed off with a pinned passkey, and blocks one signed off with
// an unpinned passkey of the same identity.
#[tokio::test(flavor = "multi_thread")]
async fn landing_counts_only_pinned_passkeys() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut pinned = Authenticator::new(Algorithm::EdDsa);
    let pinned_key = register(&page, &mut pinned, "alice", None).await.unwrap();
    let mut unpinned = Authenticator::new(Algorithm::Es256);
    register(&page, &mut unpinned, "alice", Some(&mut pinned))
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &verified_policy(&pinned_key.fingerprint),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;

    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let (key, statement, _) =
        signed_off(client, &page, &repo.id, "alice", &change, &mut pinned).await;
    client
        .add_evidence(add_request(
            &repo.id,
            &change.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap();
    let landed = propose_and_land(client, &repo.id, &change).await;
    assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");

    let next = commit_on(client, &repo.id, &change.commit_id, &[("b.txt", "b\n")]).await;
    let (key, statement, _) =
        signed_off(client, &page, &repo.id, "alice", &next, &mut unpinned).await;
    client
        .add_evidence(add_request(
            &repo.id,
            &next.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap();
    let blocked = propose_and_land(client, &repo.id, &next).await;
    assert_eq!(blocked.outcome(), LandingOutcome::Blocked, "{blocked:#?}");
    assert_eq!(blocked.reason(), LandingReason::Policy);
    let current = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: "main".to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(current.commit_id, change.commit_id);
    daemon.stop().await;
}

// 9. A reset removes an identity's passkeys and revokes the keys they
// certified.
#[tokio::test(flavor = "multi_thread")]
async fn reset_revokes_what_the_passkeys_certified() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let (_, _, approved) =
        signed_off(client, &page, &repo.id, "alice", &change, &mut passkey).await;
    let key_id = approved.key.unwrap().key_id;

    let reset = client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "lost my laptop".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(reset.removed.len(), 1);
    assert_eq!(reset.removed[0].id, registered.id);
    assert_eq!(reset.removed[0].removed_reason, "reset: lost my laptop");
    assert_eq!(reset.revoked_key_ids, std::slice::from_ref(&key_id));
    let revoked = client.get_session_key(&key_id).await.unwrap().key.unwrap();
    assert!(revoked.revocation.is_some());
    let live = client
        .list_passkeys(ListPasskeysRequest {
            identity: "alice".to_owned(),
            ..ListPasskeysRequest::default()
        })
        .await
        .unwrap()
        .passkeys;
    assert_eq!(live, []);
    // Idempotent, and the identity starts over with a first passkey.
    let again = client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: String::new(),
        })
        .await
        .unwrap();
    assert!(again.removed.is_empty() && again.revoked_key_ids.is_empty());
    let mut fresh = Authenticator::new(Algorithm::Es256);
    register(&page, &mut fresh, "alice", None).await.unwrap();
    daemon.stop().await;
}
