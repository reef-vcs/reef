//! Passkeys and approvals through the daemon (R3.43, the brief's tests 5 to
//! 9): registration from the web UI and its refusals, approvals requested
//! on the socket and approved with a passkey's assertion through the web
//! UI, the one-sign-off rules of `AddEvidence`, policy counting only pinned
//! passkeys at a landing, a reset, and the touch a web landing needs
//! (R3.31). Ceremonies use
//! `reef_evidence::webauthn::testing::Authenticator`, which writes the
//! bytes a browser and a platform authenticator would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use reef_api::v1::evidence_service_client::EvidenceServiceClient;
use reef_api::v1::identity_service_client::IdentityServiceClient;
use reef_api::v1::landing_service_client::LandingServiceClient;
use reef_api::v1::{
    self, AddEvidenceRequest, ApprovalState, CreateIdentityRequest, CreateWebSessionResponse,
    EditFilesRequest, EvaluatePolicyRequest, FileEdit, GetApprovalRequest, GetChangeRequest,
    IdentityKind as Kind, LandChangeRequest, LandingOutcome, LandingReason, LifecycleState,
    ListPasskeysRequest, PasskeyAssertion, RegisterPasskeyRequest, RemoteScope,
    RemovePasskeyRequest, RequestApprovalRequest, ResetPasskeysRequest, ResolveApprovalRequest,
    SessionPresence, StartPasskeyRegistrationRequest, TransitionActor, TransitionChangeRequest,
    edit_files_request, file_edit, resolve_approval_request,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_daemon::testing::{WebConnector, web_connector};
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

const RP: &str = "reef.localhost";

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
    GrpcWebClientService<HttpClient<WebConnector, tonic_web::GrpcWebCall<Body>>>,
    Bearer,
>;

/// A web UI session, as a page holds it.
struct Ui {
    session: CreateWebSessionResponse,
    token: String,
    /// `http://<run>.reef.localhost:<port>`, the page's origin.
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
    /// `http://reef.localhost:<port>`, what a policy pins for it.
    fn site(&self) -> String {
        let (_, under) = self.origin.split_once('.').unwrap();
        format!("http://{under}")
    }

    fn service(&self) -> (Web, Uri) {
        let client = HttpClient::builder(TokioExecutor::new()).build(web_connector());
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

    fn landing(&self) -> LandingServiceClient<Web> {
        let (service, origin) = self.service();
        LandingServiceClient::with_origin(service, origin)
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
    assert_eq!(started.rp_id, RP);
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

    // Another origin of the RP ID, at another port: a look-alike's.
    let mut fourth = Authenticator::new(Algorithm::EdDsa);
    let (_, request) = registration(
        &page,
        &mut fourth,
        "alice",
        "http://0123456789abcdef.reef.localhost:1",
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

// An approve whose call is dropped mid-way (a closed tab, a client's
// deadline), here just after its key is certified, still ends with the
// approval approved: a decline cannot end an approval whose key is live.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_approve_still_ends_approved() {
    use reef_daemon::identity::approvals::Resolution;
    use reef_daemon::identity::issue::Channel;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let identities = &daemon.handle.state().identity;
    let key = fresh_key();
    let key_id = key.public_key().id().to_string();
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        payload(&signoff(None), &change.tree_id),
        &key,
    )
    .await
    .unwrap();
    let asserted = assertion(&mut passkey, &page.origin, &approval.challenge);
    let web = Channel::Web {
        session: "0123456789abcdef".to_owned(),
    };
    let mut approving = Box::pin(identities.resolve_approval(
        &web,
        Some(&page.origin),
        &approval.id,
        Resolution::Approve(asserted),
    ));
    let give_up = Instant::now() + Duration::from_secs(10);
    while futures::poll!(approving.as_mut()).is_pending()
        && client.get_session_key(&key_id).await.is_err()
    {
        assert!(Instant::now() < give_up, "the key was never certified");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    drop(approving);

    let refused = client
        .resolve_approval(ResolveApprovalRequest {
            id: approval.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Decline(
                "not mine".to_owned(),
            )),
        })
        .await
        .unwrap_err();
    assert!(
        ["APPROVAL_RESOLVED", "APPROVAL_BUSY"].contains(&refused.reason()),
        "{refused:?}"
    );
    let ended = client
        .get_approval(GetApprovalRequest {
            id: approval.id.clone(),
            wait: Some(prost_types::Duration {
                seconds: 10,
                nanos: 0,
            }),
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(ended.state(), ApprovalState::Approved);
    assert_eq!(ended.key.unwrap().key_id, key_id);
    daemon.stop().await;
}

// A `GetApproval { wait }` begun while an approve is in flight ends at the
// approval's expiry when that approve fails, not at the end of its `wait`.
// On one thread the approve's task runs only once the test awaits, so the
// wait begins while it is in flight.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_approve_leaves_the_wait_bounded_by_the_expiry() {
    use reef_daemon::identity::IdentityError;
    use reef_daemon::identity::approvals::{Resolution, State};
    use reef_daemon::identity::issue::Channel;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let identities = &daemon.handle.state().identity;
    identities.set_approval_wait(Duration::from_millis(1500));
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        payload(&signoff(None), &change.tree_id),
        &fresh_key(),
    )
    .await
    .unwrap();
    let asserted = assertion(&mut passkey, &page.origin, &approval.challenge);
    let web = Channel::Web {
        session: "0123456789abcdef".to_owned(),
    };
    let mut approving = Box::pin(identities.resolve_approval(
        &web,
        Some("http://elsewhere.example"),
        &approval.id,
        Resolution::Approve(asserted),
    ));
    assert!(futures::poll!(approving.as_mut()).is_pending());
    let started = Instant::now();
    let mut waiting = Box::pin(identities.get_approval(&approval.id, Duration::from_secs(8)));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    let (refused, waited) = tokio::join!(approving, waiting);
    let took = started.elapsed();
    assert!(
        matches!(
            refused,
            Err(IdentityError::PasskeyInvalid {
                check: "origin",
                ..
            })
        ),
        "{refused:?}"
    );
    let waited = waited.unwrap();
    assert!(matches!(waited.state, State::Expired), "{:?}", waited.state);
    assert!(
        took < Duration::from_secs(4),
        "the wait held {took:?}, past the approval's 1.5 s expiry"
    );
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
/// presence, on a passkey pinned as `pinned` at `origin`, and so does a
/// change to the pins.
fn verified_policy(pinned: &str, origin: &str) -> String {
    format!(
        r#"
version = 1

[owners]
identities = ["alice"]
presence = "user_verified"

[passkeys]
alice = [{{ fingerprint = "{pinned}", origin = "{origin}" }}]

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

/// What `EvaluatePolicy` reports of `change`'s `user_verified` signers:
/// identity, passkey and whether `main`'s policy pins it.
async fn verified_signers(
    client: &Client,
    repo: &str,
    change: &v1::Change,
) -> Vec<(String, String, bool)> {
    client
        .evaluate_policy(EvaluatePolicyRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            skip_snapshot: true,
            ..EvaluatePolicyRequest::default()
        })
        .await
        .unwrap()
        .evaluation
        .unwrap()
        .verified
        .into_iter()
        .map(|signer| (signer.identity, signer.passkey, signer.pinned))
        .collect()
}

// 8. Policy end to end: a `[[signoff]]` that needs `user_verified` lands a
// change signed off with a pinned passkey, and blocks one signed off with
// an unpinned passkey of the same identity; `EvaluatePolicy` reports each
// signer's passkey and whether the policy pins it.
#[tokio::test(flavor = "multi_thread")]
async fn landing_counts_only_pinned_passkeys() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut pinned = Authenticator::new(Algorithm::EdDsa);
    let pinned_key = register(&page, &mut pinned, "alice", None).await.unwrap();
    let mut unpinned = Authenticator::new(Algorithm::Es256);
    let unpinned_key = register(&page, &mut unpinned, "alice", Some(&mut pinned))
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &verified_policy(&pinned_key.fingerprint, &page.site()),
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
    assert_eq!(
        verified_signers(client, &repo.id, &change).await,
        [("alice".to_owned(), pinned_key.fingerprint.clone(), true)]
    );
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
    assert_eq!(
        verified_signers(client, &repo.id, &next).await,
        [("alice".to_owned(), unpinned_key.fingerprint.clone(), false)]
    );
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
    // A removed passkey's credential id is never registered again, so no
    // registration can reuse the person's id with another key.
    let reused = register(&page, &mut passkey, "alice", None)
        .await
        .unwrap_err();
    assert_eq!(reason(&reused), "PASSKEY_EXISTS", "{reused:?}");
    let mut fresh = Authenticator::new(Algorithm::Es256);
    register(&page, &mut fresh, "alice", None).await.unwrap();
    daemon.stop().await;
}

// An approve that fails certifies no key: here the key's read fails, as a
// busy or full disk would make it, and the approval stays pending with
// nothing live, so a decline ends it. A failure after the insert would
// leave a `user_verified` key certified for an approval a decline ends.
#[tokio::test(flavor = "multi_thread")]
async fn an_approve_that_fails_certifies_no_key() {
    use reef_daemon::identity::approvals::Resolution;
    use reef_daemon::identity::issue::Channel;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let identities = &daemon.handle.state().identity;
    let key = fresh_key();
    let key_id = key.public_key().id().to_string();
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        payload(&signoff(None), &change.tree_id),
        &key,
    )
    .await
    .unwrap();
    let asserted = assertion(&mut passkey, &page.origin, &approval.challenge);
    let db =
        rusqlite::Connection::open(daemon.tmp.path().join("state").join("reef.sqlite3")).unwrap();
    db.execute_batch(
        "ALTER TABLE identity_authorities RENAME TO identity_authorities_real;
         CREATE VIEW identity_authorities AS SELECT id, algorithm, public_key, host,
           json_extract('not json', '$') AS created_at, retired_at, revoked_at,
           invalid_from, revoke_reason, kind FROM identity_authorities_real;",
    )
    .unwrap();
    let web = Channel::Web {
        session: "0123456789abcdef".to_owned(),
    };
    let failed = identities
        .resolve_approval(
            &web,
            Some(&page.origin),
            &approval.id,
            Resolution::Approve(asserted),
        )
        .await;
    db.execute_batch(
        "DROP VIEW identity_authorities;
         ALTER TABLE identity_authorities_real RENAME TO identity_authorities;",
    )
    .unwrap();
    assert!(failed.is_err(), "the key's read did not fail");
    assert_eq!(
        client.get_session_key(&key_id).await.unwrap_err().reason(),
        "SESSION_KEY_NOT_FOUND",
        "a failed approve certified key {key_id}"
    );
    let declined = client
        .resolve_approval(ResolveApprovalRequest {
            id: approval.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Decline(
                "not mine".to_owned(),
            )),
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(declined.state(), ApprovalState::Declined);
    daemon.stop().await;
}

// While an approve is being checked, a decline and a second approve are
// refused as busy, to retry, not as approved: the approve may yet fail.
// On one thread the approve's task runs only once the test awaits, so the
// resolutions meet it in flight.
#[tokio::test(flavor = "current_thread")]
async fn a_resolution_during_an_approve_is_busy() {
    use reef_daemon::identity::IdentityError;
    use reef_daemon::identity::approvals::{Resolution, State};
    use reef_daemon::identity::issue::Channel;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let identities = &daemon.handle.state().identity;
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        payload(&signoff(None), &change.tree_id),
        &fresh_key(),
    )
    .await
    .unwrap();
    let asserted = assertion(&mut passkey, &page.origin, &approval.challenge);
    let web = Channel::Web {
        session: "0123456789abcdef".to_owned(),
    };
    let mut approving = Box::pin(identities.resolve_approval(
        &web,
        Some("http://elsewhere.example"),
        &approval.id,
        Resolution::Approve(asserted.clone()),
    ));
    assert!(futures::poll!(approving.as_mut()).is_pending());
    let declined = identities
        .resolve_approval(
            &web,
            None,
            &approval.id,
            Resolution::Decline("not mine".to_owned()),
        )
        .await;
    assert!(
        matches!(declined, Err(IdentityError::ApprovalBusy { .. })),
        "{declined:?}"
    );
    let again = identities
        .resolve_approval(
            &web,
            Some(&page.origin),
            &approval.id,
            Resolution::Approve(asserted),
        )
        .await;
    assert!(
        matches!(again, Err(IdentityError::ApprovalBusy { .. })),
        "{again:?}"
    );
    assert!(
        approving.await.is_err(),
        "an approve from another origin passed"
    );
    let declined = identities
        .resolve_approval(
            &web,
            None,
            &approval.id,
            Resolution::Decline("not mine".to_owned()),
        )
        .await
        .unwrap();
    assert!(
        matches!(declined.state, State::Declined { .. }),
        "{:?}",
        declined.state
    );
    daemon.stop().await;
}

// 9. A reset that commits after an approve has recorded its passkey's use
// and before the approve's key is inserted: the insert is refused, so no
// live key is left certified by the reset passkey, and the approval is
// pending again. The approve is held between the two, the reset runs, and
// the approve goes on. Reviews of df9d1598, F1, 62dd8618, F1, and
// c64f1662, F2.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_during_an_approve_leaves_no_live_key() {
    use reef_daemon::identity::IdentityError;
    use reef_daemon::identity::approvals::Resolution;
    use reef_daemon::identity::issue::Channel;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let identities = Arc::clone(&daemon.handle.state().identity);
    let key = fresh_key();
    let key_id = key.public_key().id().to_string();
    let approval = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        payload(&signoff(None), &change.tree_id),
        &key,
    )
    .await
    .unwrap();
    let asserted = assertion(&mut passkey, &page.origin, &approval.challenge);
    let hold = identities.hold_approves().await;
    let approving = tokio::spawn({
        let identities = Arc::clone(&identities);
        let origin = page.origin.clone();
        let id = approval.id.clone();
        async move {
            let web = Channel::Web {
                session: "0123456789abcdef".to_owned(),
            };
            identities
                .resolve_approval(&web, Some(&origin), &id, Resolution::Approve(asserted))
                .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while identities.held_approves() == 0 {
        assert!(
            Instant::now() < deadline,
            "the approve never reached the hold"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let reset = identities
        .reset_passkeys("alice", "lost my laptop")
        .unwrap();
    drop(hold);
    let approved = approving.await.unwrap();
    assert_eq!(reset.removed.len(), 1, "the reset removed the passkey");
    assert!(
        reset.removed[0].last_used_at.is_some(),
        "the reset committed before the approve used the passkey"
    );
    assert_eq!(reset.revoked_key_ids, Vec::<String>::new());
    assert!(
        matches!(&approved, Err(IdentityError::PasskeyInvalid { check: "passkey", message })
            if message.contains("while the approval was being checked")),
        "the approve was not refused at its insert: {approved:?}"
    );
    assert_eq!(
        client.get_session_key(&key_id).await.unwrap_err().reason(),
        "SESSION_KEY_NOT_FOUND",
        "an approve refused at its insert certified key {key_id}"
    );
    let pending = client
        .get_approval(GetApprovalRequest {
            id: approval.id.clone(),
            wait: None,
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(pending.state(), ApprovalState::Pending);
    daemon.stop().await;
}

// A reset revokes the keys every passkey of the identity certified, one
// removed before it included: removing a passkey revokes nothing, and the
// reset is the way back from one the person did not add. Review of
// 62dd8618, F4.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_revokes_what_a_removed_passkey_certified() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let mut rogue = Authenticator::new(Algorithm::Es256);
    let rogue_key = register(&page, &mut rogue, "alice", Some(&mut passkey))
        .await
        .unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let (_, _, approved) = signed_off(client, &page, &repo.id, "alice", &change, &mut rogue).await;
    let key_id = approved.key.unwrap().key_id;
    let challenge = reef_evidence::webauthn::passkey_change_challenge(
        reef_evidence::webauthn::PasskeyChange::Remove {
            passkey: &rogue_key.id,
        },
    );
    page.identities()
        .remove_passkey(RemovePasskeyRequest {
            id: rogue_key.id.clone(),
            assertion: Some(assertion(&mut passkey, &page.origin, &challenge)),
            reason: "not mine".to_owned(),
        })
        .await
        .unwrap();
    let kept = client.get_session_key(&key_id).await.unwrap().key.unwrap();
    assert!(
        kept.revocation.is_none(),
        "the removal revoked key {key_id}"
    );

    let reset = client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "a passkey I did not add".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(reset.removed.len(), 1, "only the live passkey is removed");
    assert_eq!(reset.revoked_key_ids, std::slice::from_ref(&key_id));
    let revoked = client.get_session_key(&key_id).await.unwrap().key.unwrap();
    assert!(
        revoked.revocation.is_some(),
        "key {key_id} of a removed passkey is live after the reset"
    );
    daemon.stop().await;
}

// A reset is one transaction: one that fails part way leaves the passkeys
// live, so its retry still finds them and revokes their keys. Here the
// key's revocation fails, as a full disk would make it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reset_revokes_on_its_retry() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let (_, _, approved) =
        signed_off(client, &page, &repo.id, "alice", &change, &mut passkey).await;
    let key_id = approved.key.unwrap().key_id;
    let db =
        rusqlite::Connection::open(daemon.tmp.path().join("state").join("reef.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER failing_revocation BEFORE UPDATE OF revoked_at ON session_keys
         BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
    )
    .unwrap();
    let reset = ResetPasskeysRequest {
        identity: "alice".to_owned(),
        reason: "lost my laptop".to_owned(),
    };
    client.reset_passkeys(reset.clone()).await.unwrap_err();
    db.execute_batch("DROP TRIGGER failing_revocation;")
        .unwrap();
    let retried = client.reset_passkeys(reset).await.unwrap();
    assert_eq!(retried.removed.len(), 1, "the failed reset removed nothing");
    assert_eq!(retried.revoked_key_ids, std::slice::from_ref(&key_id));
    let key = client.get_session_key(&key_id).await.unwrap().key.unwrap();
    assert!(
        key.revocation.is_some(),
        "key {key_id} is live after the reset"
    );
    daemon.stop().await;
}

// A decline keeps its reason trimmed and names the web UI session that
// declined it; one on the socket names none. Review of df9d1598, F3.
#[tokio::test(flavor = "multi_thread")]
async fn a_decline_records_its_reason_and_decliner() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let page_session = registered.created_web_session.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let statement = payload(&signoff(None), &change.tree_id);
    let decline = |id: String| ResolveApprovalRequest {
        id,
        resolution: Some(resolve_approval_request::Resolution::Decline(
            "  not mine \n".to_owned(),
        )),
    };

    let from_page = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement.clone(),
        &fresh_key(),
    )
    .await
    .unwrap();
    let declined = page
        .identities()
        .resolve_approval(decline(from_page.id.clone()))
        .await
        .unwrap()
        .into_inner()
        .approval
        .unwrap();
    assert_eq!(declined.state(), ApprovalState::Declined);
    assert_eq!(declined.declined_reason, "not mine");
    assert_eq!(declined.resolved_web_session, Some(page_session.clone()));
    let read = client
        .get_approval(GetApprovalRequest {
            id: from_page.id.clone(),
            wait: None,
        })
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(read.declined_reason, "not mine");
    assert_eq!(read.resolved_web_session, Some(page_session));

    let from_socket = request_approval(
        client,
        "alice",
        &repo.id,
        &change.commit_id,
        statement,
        &fresh_key(),
    )
    .await
    .unwrap();
    let declined = client
        .resolve_approval(decline(from_socket.id.clone()))
        .await
        .unwrap()
        .approval
        .unwrap();
    assert_eq!(declined.declined_reason, "not mine");
    assert_eq!(declined.resolved_web_session, None);
    daemon.stop().await;
}

/// Sends a `reef-agent` beside the session token, as a page script could.
#[derive(Clone)]
struct AgentBearer(String);

impl Interceptor for AgentBearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.0).parse().unwrap(),
        );
        request
            .metadata_mut()
            .insert("reef-agent", "planner-1".parse().unwrap());
        Ok(request)
    }
}

// R3.31 (ADR 0086): a web write is a person's, so a `reef-agent` a page
// sends is not recorded on the evidence it adds.
#[tokio::test(flavor = "multi_thread")]
async fn web_evidence_records_no_page_agent() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();
    let change = commit_on(client, &repo.id, "root()", &[("a.txt", "a\n")]).await;
    let (key, statement, _) =
        signed_off(client, &page, &repo.id, "alice", &change, &mut passkey).await;
    let http = HttpClient::builder(TokioExecutor::new()).build(web_connector());
    let service = InterceptedService::new(
        GrpcWebClientLayer::new().layer(http),
        AgentBearer(page.token.clone()),
    );
    let origin: Uri = page.session.url.parse().unwrap();
    let added = EvidenceServiceClient::with_origin(service, origin)
        .add_evidence(add_request(
            &repo.id,
            &change.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap()
        .into_inner()
        .evidence
        .unwrap();
    assert_eq!(
        added.added_by_agent, "",
        "a reef-agent a page sends is not recorded"
    );
    daemon.stop().await;
}

/// `change` proposed by an agent over the socket.
async fn propose(client: &Client, repo: &str, change: &v1::Change) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            to: LifecycleState::Proposed.into(),
            actor: TransitionActor::Agent.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

/// Accept's sign-off of `change`: approved with `authenticator` through
/// `ui` and added through it, as the proposal page does. Returns the key's
/// id.
async fn accept_signoff(
    client: &Client,
    ui: &Ui,
    repo: &str,
    change: &v1::Change,
    authenticator: &mut Authenticator,
) -> String {
    let (key, statement, _) = signed_off(client, ui, repo, "alice", change, authenticator).await;
    ui.evidence()
        .add_evidence(add_request(
            repo,
            &change.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap();
    key.public_key().id().to_string()
}

/// The sign-off Accept makes, all through `ui` as a leaked session or a
/// script in the page could: the approval requested and resolved through
/// the web listener too.
async fn web_only_signoff(
    client: &Client,
    ui: &Ui,
    repo: &str,
    change: &v1::Change,
    authenticator: &mut Authenticator,
) {
    let key = fresh_key();
    let statement = payload(&signoff(None), &change.tree_id);
    let authority = reef_evidence::KeyId::parse(&current_authority(client).await).unwrap();
    let proof = sign_proof(
        &key,
        &ProofFor {
            authority: &authority,
            identity: "alice",
            actor: Actor::Human,
            session: "",
        },
    );
    let approval = ui
        .identities()
        .request_approval(RequestApprovalRequest {
            identity: "alice".to_owned(),
            repo: repo.to_owned(),
            workspace: String::new(),
            revision: change.commit_id.clone(),
            statement: statement.clone(),
            public_key: key.public_key().spki().to_vec(),
            proof,
        })
        .await
        .unwrap()
        .into_inner()
        .approval
        .unwrap();
    approve(ui, &approval, authenticator).await.unwrap();
    ui.evidence()
        .add_evidence(add_request(
            repo,
            &change.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap();
}

/// Accept's landing request: the change by its id, at the commit the page
/// showed.
fn web_land(repo: &str, change: &v1::Change) -> LandChangeRequest {
    LandChangeRequest {
        repo: repo.to_owned(),
        revision: change.change_id.clone(),
        expected_commit_id: change.commit_id.clone(),
        actor: TransitionActor::Human.into(),
        skip_snapshot: true,
        wait_ms: 60_000,
        ..LandChangeRequest::default()
    }
}

/// Asserts that `status` is the web landing's refusal for `change`, for
/// want of `want`: `touch`, `pinned_touch` or `pinned_origin`, with a
/// command that signs it off, or for want of a pin, the destination's policy state (`no_pins`,
/// `no_policy` or `invalid_policy`), with the command to start pinning and
/// `reef land` suggested only where a policy loads.
fn assert_untouched(status: &Status, change: &v1::Change, want: &str) {
    let (code, reason, metadata) = refusal(status);
    let (missing, policy) = match want {
        "touch" | "pinned_touch" | "pinned_origin" => (want, None),
        policy => ("pin", Some(policy)),
    };
    assert_eq!(
        (code, reason.as_str(), metadata["missing"].as_str()),
        (
            Code::FailedPrecondition,
            "VERIFIED_SIGNOFF_REQUIRED",
            missing
        ),
        "{status:?}"
    );
    assert_eq!(metadata.get("policy").map(String::as_str), policy);
    assert_eq!(metadata["tree_id"], change.tree_id);
    assert_eq!(metadata["commit_id"], change.commit_id);
    assert_eq!(metadata["change_id"], change.change_id);
    assert_eq!(metadata["destination"], "main");
    let detail = reef_daemon::error::detail_of(status).unwrap();
    let next = detail.next;
    match policy {
        None => {
            assert_eq!(next.first().map(String::as_str), Some("reef"));
            assert!(next.contains(&"--verified".to_owned()), "{next:?}");
        }
        Some("no_pins") => {
            assert_eq!(next, ["reef", "identity", "passkey", "list"]);
            assert!(
                detail
                    .hint
                    .contains(&format!("`reef land {}`", change.change_id)),
                "{}",
                detail.hint
            );
        }
        Some(policy) => {
            let expected: Vec<&str> = if policy == "no_policy" {
                vec!["reef", "policy", "init"]
            } else {
                vec![
                    "reef",
                    "policy",
                    "eval",
                    "--policy-revision",
                    "main",
                    "-r",
                    &change.commit_id,
                ]
            };
            assert_eq!(next, expected);
            assert!(detail.hint.contains("by hand"), "{}", detail.hint);
            assert!(!detail.hint.contains("`reef land "), "{}", detail.hint);
        }
    }
}

fn assert_needs_touch(status: &Status, change: &v1::Change) {
    assert_untouched(status, change, "touch");
}

/// Rewrites `commit` with another file, as the agent amending it does.
async fn amend(client: &Client, repo: &str, commit: &str) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::Revision(commit.to_owned())),
            edits: vec![FileEdit {
                path: "amended.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"more\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// `main`'s policy: nothing but a proposal, no sign-off, and no passkey
/// pinned.
const NO_SIGNOFF: &str = r#"
version = 1

[owners]
identities = ["alice"]

[landing]
checks = "proposal"
"#;

/// [`NO_SIGNOFF`] pinning alice's passkey `fingerprint`: a web landing can
/// count its touch.
fn pinning(fingerprint: &str, origin: &str) -> String {
    format!(
        "{NO_SIGNOFF}\n[passkeys]\nalice = [{{ fingerprint = \"{fingerprint}\", origin = \"{origin}\" }}]\n"
    )
}

/// Asks to land `change` through `ui`, as Accept does.
async fn land_from_page(ui: &Ui, repo: &str, change: &v1::Change) -> Result<v1::Landing, Status> {
    ui.landing()
        .land_change(web_land(repo, change))
        .await
        .map(|response| response.into_inner().landing.unwrap())
}

// R3.31 (ADR 0086 decision 2): through the web UI a landing needs a person's
// touch with a passkey `main` pins, under a policy that asks for no sign-off.
// The page's `LandChange` is refused until such a sign-off is on the tree of
// the commit it asks for: a
// declared `human` sign-off does not count, nor a touch on the change below
// it, nor one on the tree before an amend. Accept's touch on the head of a
// stack lands the members below it. The socket and the remote listener
// land without one.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario, in order")]
async fn a_web_landing_needs_a_touch() {
    let (daemon, repo) = fixture_with(remote_on).await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &pinning(&registered.fingerprint, &page.site()),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    let bottom = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let head = commit_on(client, &repo.id, &bottom.commit_id, &[("b.txt", "b\n")]).await;
    propose(client, &repo.id, &bottom).await;
    propose(client, &repo.id, &head).await;

    let refused = page
        .landing()
        .land_change(web_land(&repo.id, &head))
        .await
        .unwrap_err();
    assert_needs_touch(&refused, &head);

    // A declared `human` sign-off, which any process of the uid can make.
    let declared = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap()
        .signing_key;
    client
        .add_evidence(add_request(
            &repo.id,
            &head.commit_id,
            sign(&payload(&signoff(None), &head.tree_id), &declared),
            &declared,
        ))
        .await
        .unwrap();
    let refused = page
        .landing()
        .land_change(web_land(&repo.id, &head))
        .await
        .unwrap_err();
    assert_needs_touch(&refused, &head);

    // A touch on the member below does not cover the head's tree.
    accept_signoff(client, &page, &repo.id, &bottom, &mut passkey).await;
    let refused = page
        .landing()
        .land_change(web_land(&repo.id, &head))
        .await
        .unwrap_err();
    assert_needs_touch(&refused, &head);

    // Accept's touch on the head lands the stack.
    accept_signoff(client, &page, &repo.id, &head, &mut passkey).await;
    let response = page
        .landing()
        .land_change(web_land(&repo.id, &head))
        .await
        .unwrap()
        .into_inner();
    let landing = response.landing.unwrap();
    assert_eq!(landing.outcome(), LandingOutcome::Landed, "{landing:#?}");
    assert_eq!(
        landing.web, registered.created_web_session,
        "the landing names the page's session"
    );
    let below: Vec<_> = response
        .stack
        .iter()
        .map(|member| (member.changes[0].change_id.as_str(), member.outcome()))
        .collect();
    assert_eq!(
        below,
        [(bottom.change_id.as_str(), LandingOutcome::Landed)],
        "{:#?}",
        response.stack
    );

    // The agent amends after the touch: the sign-off is on the old tree.
    let shown = commit_on(client, &repo.id, "main", &[("c.txt", "c\n")]).await;
    propose(client, &repo.id, &shown).await;
    accept_signoff(client, &page, &repo.id, &shown, &mut passkey).await;
    let amended = amend(client, &repo.id, &shown.commit_id).await;
    assert_ne!(amended.tree_id, shown.tree_id);
    // The page's request at the commit it showed names both commits.
    let stale = page
        .landing()
        .land_change(web_land(&repo.id, &shown))
        .await
        .unwrap_err();
    assert_eq!(reason(&stale), "EXPECTED_COMMIT_MISMATCH", "{stale:?}");
    assert!(
        stale.message().contains(&amended.commit_id) && stale.message().contains(&shown.commit_id),
        "{stale:?}"
    );
    propose(client, &repo.id, &amended).await;
    let refused = page
        .landing()
        .land_change(web_land(&repo.id, &amended))
        .await
        .unwrap_err();
    assert_needs_touch(&refused, &amended);

    // The socket lands it without a touch, as before R3.31.
    let landed = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: amended.commit_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap();
    assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");

    // So does the remote listener.
    let remote = remote_client(&daemon, "alice").await;
    let last = commit_on(client, &repo.id, "main", &[("d.txt", "d\n")]).await;
    propose(client, &repo.id, &last).await;
    let landed = remote
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: last.commit_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap();
    assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");
    daemon.stop().await;
}

// Review of b6c2a860, N1: a leaked web session alone registers an identity's
// first passkey (R3.43 asks no assertion for it), signs off with it and asks
// to land, all through the web listener. Its touch counts only where `main`
// pins that passkey, which the session cannot make so: refused while `main`
// pins none, and while it pins another; again after a reset lets the same
// session register a first passkey anew. The socket lands it as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_passkey_a_web_session_registers_lands_nothing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let unpinned = commit_on(
        client,
        &repo.id,
        "root()",
        &[("reef-policy.toml", NO_SIGNOFF)],
    )
    .await;
    set_main(&repo, &unpinned.commit_id).await;
    let change = commit_on(client, &repo.id, "main", &[("evil.txt", "x\n")]).await;
    propose(client, &repo.id, &change).await;

    let mut attacker = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut attacker, "alice", None).await.unwrap();
    web_only_signoff(client, &page, &repo.id, &change, &mut attacker).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "no_pins");

    // `main` pins a passkey of alice's, not the session's.
    let pinned = commit_on(
        client,
        &repo.id,
        "root()",
        &[("reef-policy.toml", &pinning(&"ab".repeat(32), &page.site()))],
    )
    .await;
    set_main(&repo, &pinned.commit_id).await;
    let change = commit_on(client, &repo.id, "main", &[("evil.txt", "x\n")]).await;
    propose(client, &repo.id, &change).await;
    web_only_signoff(client, &page, &repo.id, &change, &mut attacker).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_touch");

    client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "a passkey I did not add".to_owned(),
        })
        .await
        .unwrap();
    let mut again = Authenticator::new(Algorithm::Es256);
    register(&page, &mut again, "alice", None).await.unwrap();
    web_only_signoff(client, &page, &repo.id, &change, &mut again).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_touch");

    let landed = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: change.commit_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap()
        .landing
        .unwrap();
    assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");
    daemon.stop().await;
}

/// A port nothing holds now, on either loopback family, below the ephemeral
/// ranges, where no other test's `:0` bind lands in between.
fn free_port() -> u16 {
    loop {
        let port = 20_000 + u16::from_le_bytes(rand::random()) % 12_000;
        let free = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            && std::net::TcpStream::connect(("::1", port)).is_err();
        if free {
            return port;
        }
    }
}

async fn web_ui_status(client: &Client) -> v1::WebUiStatus {
    client.daemon_status(true).await.unwrap().web_ui.unwrap()
}

// #346: with a fixed port, a daemon holds the web UI's origin from its start
// once a passkey is registered, so no other process serves the pinned origin
// while it runs; before that, and with port 0, it opens no TCP port. A port
// someone else holds is reported, is never replaced, and is bound by the next
// `reef ui` once it is free.
#[tokio::test(flavor = "multi_thread")]
async fn a_passkey_makes_the_daemon_hold_its_fixed_port() {
    use v1::WebUiState::{Bound, Unavailable, Unbound};
    let port = free_port();
    let (daemon, _repo) =
        fixture_with(|config| config.file.ui.listen = format!("127.0.0.1:{port}")).await;
    let site = format!("http://reef.localhost:{port}");
    let status = web_ui_status(&daemon.client).await;
    assert_eq!(
        (status.state(), status.site.as_str()),
        (Unbound, site.as_str())
    );
    let daemon = daemon.restart().await;
    create_identity(&daemon.client, "alice").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!daemon.handle.state().web.is_bound(), "no passkey, no port");
    let page = ui(&daemon.client).await;
    assert_eq!(page.site(), site);
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut passkey, "alice", None).await.unwrap();

    let held = async |daemon: &TestDaemon, want: v1::WebUiState| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = web_ui_status(&daemon.client).await;
            if status.state() == want {
                return status;
            }
            assert!(Instant::now() < deadline, "{status:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let daemon = daemon.restart().await;
    let status = held(&daemon, Bound).await;
    assert_eq!((status.site, status.problem), (site.clone(), String::new()));
    assert_eq!(ui(&daemon.client).await.origin, status.origin);

    // Another process takes the port while the daemon is down.
    let stopped = daemon.stop_keeping().await;
    let squatter = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let daemon = stopped.start().await;
    let status = held(&daemon, Unavailable).await;
    assert_eq!(status.site, site);
    assert!(status.problem.contains(&port.to_string()), "{status:?}");
    let refused = daemon
        .client
        .create_web_session(false, "")
        .await
        .unwrap_err();
    let ClientError::Rpc { status, .. } = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(reason(&status), "WEB_UI_UNAVAILABLE", "no other port");
    // Once it lets go, the daemon takes it with no `reef ui`.
    drop(squatter);
    let status = held(&daemon, Bound).await;
    assert_eq!((status.site, status.problem), (site.clone(), String::new()));
    assert_eq!(ui(&daemon.client).await.origin, status.origin);

    // A removed passkey is no reason to hold it.
    daemon
        .client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "lost".to_owned(),
        })
        .await
        .unwrap();
    let daemon = daemon.restart().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!daemon.handle.state().web.is_bound());
    daemon.stop().await;
}

// #346: the attack. A look-alike page on another localhost port runs the
// ceremony with the pinned passkey, and code that can read the authority's
// key certifies the result: the daemon's own check of the origin is skipped,
// as `resolve_approval` does here when it is told the look-alike's origin.
// The certificate verifies and the sign-off is stored, and it counts for
// nothing: not in the policy, not for a landing, not through the web UI.
// Pinning that origin too, as the person does after moving the web UI's
// port, makes the same touch count.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario, in order")]
async fn a_ceremony_on_another_port_counts_for_nothing() {
    use reef_daemon::identity::approvals::Resolution;
    use reef_daemon::identity::issue::Channel;
    const ELSEWHERE: &str = "http://0123456789abcdef.reef.localhost:49953";
    const ELSEWHERE_SITE: &str = "http://reef.localhost:49953";
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::Es256);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &verified_policy(&registered.fingerprint, &page.site()),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;

    // The look-alike's touch, over the approval's own challenge.
    let key = fresh_key();
    let statement = payload(&signoff(None), &change.tree_id);
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
    let asserted = assertion(&mut passkey, ELSEWHERE, &approval.challenge);
    // Through Reef's own page it is refused.
    let refused = page
        .identities()
        .resolve_approval(ResolveApprovalRequest {
            id: approval.id.clone(),
            resolution: Some(resolve_approval_request::Resolution::Approve(
                asserted.clone(),
            )),
        })
        .await
        .unwrap_err();
    assert_eq!(check(&refused), "origin");
    daemon
        .handle
        .state()
        .identity
        .resolve_approval(
            &Channel::Web {
                session: "0123456789abcdef".to_owned(),
            },
            Some(ELSEWHERE),
            &approval.id,
            Resolution::Approve(asserted),
        )
        .await
        .unwrap();
    let added = client
        .add_evidence(add_request(
            &repo.id,
            &change.commit_id,
            sign(&statement, &key),
            &key,
        ))
        .await
        .unwrap()
        .evidence
        .unwrap();
    let signer = added.identity.unwrap();
    assert_eq!(signer.status(), v1::IdentityStatus::Bound);
    let signer = signer.key.unwrap();
    assert!(signer.certificate_valid);
    assert_eq!(signer.presence(), SessionPresence::UserVerified);
    assert_eq!(signer.passkey_origin, ELSEWHERE);

    let reported = |evaluation: v1::PolicyEvaluation| -> Vec<(String, bool, String)> {
        evaluation
            .verified
            .into_iter()
            .map(|signer| (signer.origin, signer.pinned, signer.unpinned))
            .collect()
    };
    let evaluate = async || {
        client
            .evaluate_policy(EvaluatePolicyRequest {
                repo: repo.id.clone(),
                revision: change.commit_id.clone(),
                skip_snapshot: true,
                ..EvaluatePolicyRequest::default()
            })
            .await
            .unwrap()
            .evaluation
            .unwrap()
    };
    assert_eq!(
        reported(evaluate().await),
        [(ELSEWHERE.to_owned(), false, "origin".to_owned())]
    );
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_origin");
    assert_eq!(refusal(&refused).2["origin"], ELSEWHERE);
    for origin in [ELSEWHERE, page.site().as_str()] {
        assert!(refused.message().contains(origin), "{}", refused.message());
    }
    let land = async || {
        client
            .land_change(LandChangeRequest {
                repo: repo.id.clone(),
                revision: change.commit_id.clone(),
                wait_ms: 60_000,
                ..LandChangeRequest::default()
            })
            .await
            .unwrap()
            .landing
            .unwrap()
    };
    let blocked = land().await;
    assert_eq!(blocked.outcome(), LandingOutcome::Blocked, "{blocked:#?}");
    assert_eq!(blocked.reason(), LandingReason::Policy, "{blocked:#?}");

    // The person pins that origin too.
    let policy = verified_policy(&registered.fingerprint, &page.site()).replace(
        "alice = [",
        &format!(
            "alice = [{{ fingerprint = \"{}\", origin = \"{ELSEWHERE_SITE}\" }}, ",
            registered.fingerprint
        ),
    );
    let main = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("reef-policy.toml", &policy)],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    assert_eq!(
        reported(evaluate().await),
        [(ELSEWHERE.to_owned(), true, String::new())]
    );
    let landed = land().await;
    assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");
    daemon.stop().await;
}

// Review of b6c2a860, N2: a touch counts only while its passkey is live. A
// pinned passkey's touch lands; another made before the passkey's removal,
// or before a reset, no longer does, though `main` still pins it and the
// key was bound when the sign-off was added.
#[tokio::test(flavor = "multi_thread")]
async fn a_touch_counts_only_while_its_passkey_is_live() {
    for end in ["remove", "reset"] {
        let (daemon, repo) = fixture().await;
        let client = &daemon.client;
        create_identity(client, "alice").await;
        let page = ui(client).await;
        let mut passkey = Authenticator::new(Algorithm::EdDsa);
        let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
        let main = commit_on(
            client,
            &repo.id,
            "root()",
            &[(
                "reef-policy.toml",
                &pinning(&registered.fingerprint, &page.site()),
            )],
        )
        .await;
        set_main(&repo, &main.commit_id).await;
        let first = commit_on(client, &repo.id, "main", &[("a.txt", "a\n")]).await;
        let second = commit_on(client, &repo.id, "main", &[("b.txt", "b\n")]).await;
        propose(client, &repo.id, &first).await;
        propose(client, &repo.id, &second).await;
        accept_signoff(client, &page, &repo.id, &first, &mut passkey).await;
        accept_signoff(client, &page, &repo.id, &second, &mut passkey).await;
        let landed = land_from_page(&page, &repo.id, &first).await.unwrap();
        assert_eq!(landed.outcome(), LandingOutcome::Landed, "{landed:#?}");

        if end == "remove" {
            // A second passkey approves the removal, and stays live.
            let mut other = Authenticator::new(Algorithm::Es256);
            register(&page, &mut other, "alice", Some(&mut passkey))
                .await
                .unwrap();
            let challenge = reef_evidence::webauthn::passkey_change_challenge(
                reef_evidence::webauthn::PasskeyChange::Remove {
                    passkey: &registered.id,
                },
            );
            page.identities()
                .remove_passkey(RemovePasskeyRequest {
                    id: registered.id.clone(),
                    assertion: Some(assertion(&mut other, &page.origin, &challenge)),
                    reason: "lost".to_owned(),
                })
                .await
                .unwrap();
        } else {
            client
                .reset_passkeys(ResetPasskeysRequest {
                    identity: "alice".to_owned(),
                    reason: "lost".to_owned(),
                })
                .await
                .unwrap();
        }
        let refused = land_from_page(&page, &repo.id, &second).await.unwrap_err();
        assert_untouched(&refused, &second, "pinned_touch");
        daemon.stop().await;
    }
}

// Review of b6c2a860, RM4: a touch whose key reads revoked at the sign-off
// is no touch, though its passkey is live and pinned.
#[tokio::test(flavor = "multi_thread")]
async fn a_touch_by_a_revoked_key_does_not_count() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &pinning(&registered.fingerprint, &page.site()),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    let change = commit_on(client, &repo.id, "main", &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;
    let key_id = accept_signoff(client, &page, &repo.id, &change, &mut passkey).await;
    client
        .revoke_session_keys(v1::RevokeSessionKeysRequest {
            target: Some(v1::revoke_session_keys_request::Target::KeyId(key_id)),
            since_issued: true,
            reason: "test".to_owned(),
            ..v1::RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_needs_touch(&refused, &change);
    daemon.stop().await;
}

/// Registers through `ui`, for `identity`, a passkey with `copied`'s public
/// key under `carrier`'s credential id, approved by `approver` when given.
/// Attestation `none` proves no possession, so the caller need hold no
/// private key: the public key is in every certificate it approved.
async fn register_copied_key(
    ui: &Ui,
    carrier: &mut Authenticator,
    identity: &str,
    copied: &Authenticator,
    approver: Option<&mut Authenticator>,
) -> Result<v1::Passkey, Status> {
    let (_, mut request) = registration(ui, carrier, identity, &ui.origin, approver).await;
    request.public_key = copied.passkey().spki().to_vec();
    request.algorithm = i32::try_from(copied.passkey().algorithm().cose()).unwrap();
    ui.identities()
        .register_passkey(request)
        .await
        .map(|response| response.into_inner().passkey.unwrap())
}

/// Removes `passkey` through `ui`, approved by `by`'s assertion.
async fn remove_with(ui: &Ui, passkey: &v1::Passkey, by: &mut Authenticator) {
    let challenge = reef_evidence::webauthn::passkey_change_challenge(
        reef_evidence::webauthn::PasskeyChange::Remove {
            passkey: &passkey.id,
        },
    );
    ui.identities()
        .remove_passkey(RemovePasskeyRequest {
            id: passkey.id.clone(),
            assertion: Some(assertion(by, &ui.origin, &challenge)),
            reason: "replaced".to_owned(),
        })
        .await
        .unwrap();
}

/// Asserts that `status` refuses a registration of `existing`'s credential
/// or public key.
fn assert_exists(status: &Status, existing: &v1::Passkey) {
    let (code, reason, metadata) = refusal(status);
    assert_eq!(
        (code, reason.as_str(), metadata["id"].as_str()),
        (Code::AlreadyExists, "PASSKEY_EXISTS", existing.id.as_str()),
        "{status:?}"
    );
}

// Review of b9bf8767, F1 (P1): alice touches with her pinned passkey and
// resets her passkeys before the landing is asked for, so the touch stops
// counting. A leaked web session then registers her first passkey anew with
// the old passkey's public key, copied from a certificate, and no private
// key: refused, since a public key is never registered again, and the old
// touch still lands nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_passkeys_public_key_never_comes_back() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &pinning(&registered.fingerprint, &page.site()),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    let change = commit_on(client, &repo.id, "main", &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;
    accept_signoff(client, &page, &repo.id, &change, &mut passkey).await;
    client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "lost".to_owned(),
        })
        .await
        .unwrap();
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_touch");

    let mut carrier = Authenticator::new(Algorithm::EdDsa);
    let copied = register_copied_key(&page, &mut carrier, "alice", &passkey, None)
        .await
        .unwrap_err();
    assert_exists(&copied, &registered);
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_touch");
    daemon.stop().await;
}

// Review of b9bf8767, F1 (P2): the pinned passkey's private key is stolen,
// which is why alice resets. The thief cannot register that key again as
// her first passkey, so it cannot approve a sign-off with it; a passkey of
// its own registers, but `main` does not pin it, and the web lands nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_stolen_pinned_key_lands_nothing_after_a_reset() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut stolen = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut stolen, "alice", None).await.unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[(
            "reef-policy.toml",
            &pinning(&registered.fingerprint, &page.site()),
        )],
    )
    .await;
    set_main(&repo, &main.commit_id).await;
    client
        .reset_passkeys(ResetPasskeysRequest {
            identity: "alice".to_owned(),
            reason: "stolen".to_owned(),
        })
        .await
        .unwrap();
    let change = commit_on(client, &repo.id, "main", &[("evil.txt", "x\n")]).await;
    propose(client, &repo.id, &change).await;

    let mut carrier = Authenticator::new(Algorithm::EdDsa);
    let copied = register_copied_key(&page, &mut carrier, "alice", &stolen, None)
        .await
        .unwrap_err();
    assert_exists(&copied, &registered);
    let mut own = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut own, "alice", None).await.unwrap();
    web_only_signoff(client, &page, &repo.id, &change, &mut own).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "pinned_touch");
    daemon.stop().await;
}

// A removed passkey's public key is never registered again either, under a
// new credential id, approved by a live passkey or, once the identity has
// none left, with no approval at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_passkeys_public_key_is_never_registered_again() {
    let (daemon, _repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut first = Authenticator::new(Algorithm::EdDsa);
    let kept = register(&page, &mut first, "alice", None).await.unwrap();
    let mut second = Authenticator::new(Algorithm::Es256);
    let removed = register(&page, &mut second, "alice", Some(&mut first))
        .await
        .unwrap();
    remove_with(&page, &removed, &mut second).await;
    let mut carrier = Authenticator::new(Algorithm::Es256);
    let copied = register_copied_key(&page, &mut carrier, "alice", &second, Some(&mut first))
        .await
        .unwrap_err();
    assert_exists(&copied, &removed);
    remove_with(&page, &kept, &mut first).await;
    for (copied, passkey) in [(&first, &kept), (&second, &removed)] {
        let mut carrier = Authenticator::new(copied.passkey().algorithm());
        let refused = register_copied_key(&page, &mut carrier, "alice", copied, None)
            .await
            .unwrap_err();
        assert_exists(&refused, passkey);
    }
    let mut fresh = Authenticator::new(Algorithm::EdDsa);
    register(&page, &mut fresh, "alice", None).await.unwrap();
    daemon.stop().await;
}

// Review of b9bf8767, F3 (P3): the change itself pins the session's own
// passkey in reef-policy.toml, where `main` pins none. The pin is read at
// `main`'s head, never from the change, so the web landing is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_that_pins_its_own_passkey_lands_nothing_from_the_web() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let unpinned = commit_on(
        client,
        &repo.id,
        "root()",
        &[("reef-policy.toml", NO_SIGNOFF)],
    )
    .await;
    set_main(&repo, &unpinned.commit_id).await;
    let mut attacker = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut attacker, "alice", None).await.unwrap();
    let change = commit_on(
        client,
        &repo.id,
        "main",
        &[
            (
                "reef-policy.toml",
                &pinning(&registered.fingerprint, &page.site()),
            ),
            ("evil.txt", "x\n"),
        ],
    )
    .await;
    propose(client, &repo.id, &change).await;
    web_only_signoff(client, &page, &repo.id, &change, &mut attacker).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "no_pins");
    daemon.stop().await;
}

// Review of b9bf8767, F3 (P4): `main`'s reef-policy.toml pins alice's
// passkey but does not load, so it pins nothing, and nothing lands from a
// terminal either: the refusal says to mend it by hand.
#[tokio::test(flavor = "multi_thread")]
async fn a_policy_that_does_not_load_pins_nothing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    create_identity(client, "alice").await;
    let page = ui(client).await;
    let mut passkey = Authenticator::new(Algorithm::EdDsa);
    let registered = register(&page, &mut passkey, "alice", None).await.unwrap();
    let broken = format!(
        "{}\n[[[ not toml",
        pinning(&registered.fingerprint, &page.site())
    );
    let main = commit_on(client, &repo.id, "root()", &[("reef-policy.toml", &broken)]).await;
    set_main(&repo, &main.commit_id).await;
    let change = commit_on(client, &repo.id, "main", &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;
    accept_signoff(client, &page, &repo.id, &change, &mut passkey).await;
    let refused = land_from_page(&page, &repo.id, &change).await.unwrap_err();
    assert_untouched(&refused, &change, "invalid_policy");
    daemon.stop().await;
}
