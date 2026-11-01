//! The remote listener (R2.08, tests 1 to 23): TLS and certificates,
//! tokens and scopes, the WRITE rules, identity on operations, limits,
//! the web UI over TLS, and the socket left as it was. Native gRPC goes
//! through `reef_client::Client::connect_remote`; HTTP/1.1, gRPC-web and
//! the assets through raw TLS and hyper.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use reef_api::v1::{
    self, CreateWorkspaceRequest, DescribeChangeRequest, GetDaemonStatusRequest,
    MoveWorkspaceRequest, PresenceKind, RemoteListenerState, RemoteScope, ReportPresenceRequest,
    SubscribeOperationsRequest,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, TlsFailure, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_lib::error::Classify as _;
use reef_testing::synth::Preset;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tonic::codegen::Bytes;
use tonic::codegen::http::{self, Method, StatusCode};

const SECOND: Duration = Duration::from_secs(1);

/// Turns the remote listener on, on a kernel-chosen loopback port, with
/// `localhost` as its name.
fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

/// A daemon with the listener on, once it listens: the daemon, its address
/// and its fingerprint.
async fn daemon_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SocketAddr, String) {
    let daemon = TestDaemon::start_with(|config| {
        remote_on(config);
        configure(config);
    })
    .await;
    let addr = listening(&daemon).await;
    let fingerprint = fingerprint_of(&daemon).await;
    (daemon, addr, fingerprint)
}

async fn listening(daemon: &TestDaemon) -> SocketAddr {
    let deadline = Instant::now() + 10 * SECOND;
    loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            return addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn remote_status(daemon: &TestDaemon) -> v1::RemoteListener {
    daemon
        .client
        .daemon_status(true)
        .await
        .unwrap()
        .remote
        .unwrap()
}

async fn fingerprint_of(daemon: &TestDaemon) -> String {
    remote_status(daemon).await.certificate.unwrap().fingerprint
}

/// A new token's bearer string.
async fn token(daemon: &TestDaemon, name: &str, scope: RemoteScope) -> String {
    daemon
        .client
        .create_remote_token(name, scope, Some(Duration::from_hours(1)))
        .await
        .unwrap()
        .secret
}

fn target(addr: SocketAddr, fingerprint: Option<&str>, token: Option<&str>) -> RemoteTarget {
    RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: fingerprint.map(str::to_owned),
        token: token.map(Token::new),
    }
}

async fn connect(addr: SocketAddr, fingerprint: &str, token: &str) -> Client {
    Client::connect_remote(&target(addr, Some(fingerprint), Some(token)))
        .await
        .unwrap()
}

fn reason(error: &ClientError) -> &str {
    error.reason()
}

fn meta<'a>(error: &'a ClientError, key: &str) -> &'a str {
    error.metadata().get(key).map_or("", String::as_str)
}

// ---- raw TLS and HTTP ----

/// Accepts any certificate: these tests check the daemon, not the client's
/// trust, which `Client::connect_remote` covers.
#[derive(Debug)]
struct AnyCert(rustls::crypto::WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

async fn tls(
    addr: SocketAddr,
    alpn: &[u8],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(
            provider.signature_verification_algorithms,
        )))
        .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    let tcp = TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

/// One request over TLS; HTTP/2 when `h2`, else HTTP/1.1. The whole body.
async fn send(
    addr: SocketAddr,
    h2: bool,
    request: http::Request<Full<Bytes>>,
) -> (http::response::Parts, Bytes, Option<http::HeaderMap>) {
    let response = open(addr, h2, request).await;
    let (parts, body) = response.into_parts();
    let collected = body.collect().await.unwrap();
    let trailers = collected.trailers().cloned();
    (parts, collected.to_bytes(), trailers)
}

/// One request over TLS; the response with its body unread.
async fn open(
    addr: SocketAddr,
    h2: bool,
    request: http::Request<Full<Bytes>>,
) -> http::Response<hyper::body::Incoming> {
    let io = TokioIo::new(
        tls(addr, if h2 { b"h2" } else { b"http/1.1" })
            .await
            .unwrap(),
    );
    if h2 {
        let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();
        tokio::spawn(conn);
        sender.send_request(request).await.unwrap()
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);
        sender.send_request(request).await.unwrap()
    }
}

fn request(
    method: Method,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> http::Request<Full<Bytes>> {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(format!("https://{host}{path}"));
    if !headers.iter().any(|(name, _)| *name == "host") {
        builder = builder.header("host", host);
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::from(body))).unwrap()
}

/// A gRPC message frame around an encoded request.
fn frame(message: &impl prost::Message) -> Vec<u8> {
    let encoded = message.encode_to_vec();
    let mut framed = vec![0];
    framed.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
    framed.extend(encoded);
    framed
}

/// The reason in a status's headers or trailers (`grpc-status-details-bin`).
fn reason_in(headers: &http::HeaderMap) -> String {
    tonic::Status::from_header_map(headers)
        .and_then(|status| reef_api::error::detail(&status))
        .map(|detail| detail.reason)
        .unwrap_or_default()
}

/// `path` called over native gRPC with `token` and a default request
/// body: the status's headers (a trailers-only answer, or the trailers).
async fn call_raw(addr: SocketAddr, path: &str, token: &str) -> http::HeaderMap {
    let bearer = format!("Bearer {token}");
    let response = open(
        addr,
        true,
        request(
            Method::POST,
            &format!("localhost:{}", addr.port()),
            path,
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("authorization", &bearer),
            ],
            vec![0, 0, 0, 0, 0],
        ),
    )
    .await;
    let (parts, mut body) = response.into_parts();
    if parts.headers.contains_key("grpc-status") {
        return parts.headers;
    }
    // A stream: its first frame or nothing for a moment means it was let
    // through.
    if reef_daemon::remote::gate_is_streaming(path) {
        return http::HeaderMap::new();
    }
    let mut trailers = http::HeaderMap::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { break };
        if let Ok(found) = frame.into_trailers() {
            trailers = found;
        }
    }
    trailers
}

// ---- 1 to 5: on, off, TLS, certificates ----

// R2.08 1.
#[tokio::test(flavor = "multi_thread")]
async fn off_by_default() {
    let daemon = TestDaemon::start().await;
    assert!(!daemon.handle.state().remote.is_bound());
    let status = remote_status(&daemon).await;
    assert_eq!(status.state, i32::from(RemoteListenerState::Off));
    assert!(status.certificate.is_none());
    // Tokens can be made before the listener is on.
    token(&daemon, "early", RemoteScope::Read).await;
    assert_eq!(remote_status(&daemon).await.live_token_count, 1);
}

// R2.08 2.
#[tokio::test(flavor = "multi_thread")]
async fn generated_certificate() {
    use std::os::unix::fs::PermissionsExt as _;
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    let dir = daemon.state_dir().join("remote");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("key.pem")), 0o600);
    assert_eq!(mode(&dir.join("cert.pem")), 0o600);
    let status = remote_status(&daemon).await;
    assert_eq!(status.state, i32::from(RemoteListenerState::Listening));
    assert_eq!(status.enabled_by, i32::from(v1::RemoteEnabledBy::Config));
    let certificate = status.certificate.unwrap();
    assert_eq!(
        certificate.source,
        i32::from(v1::RemoteCertificateSource::Generated)
    );
    assert!(certificate.subject_names.contains(&"localhost".to_owned()));
    assert!(certificate.subject_names.contains(&"127.0.0.1".to_owned()));
    let presented =
        reef_daemon::client::remote::presented_fingerprint(&target(addr, None, None).url)
            .await
            .unwrap();
    assert_eq!(presented, fingerprint);
    // A restart presents the same key.
    let daemon = daemon.restart().await;
    listening(&daemon).await;
    assert_eq!(fingerprint_of(&daemon).await, fingerprint);
}

/// A self-signed pair for `name` in `dir`: its certificate and key paths.
fn pair(dir: &std::path::Path, name: &str) -> (String, String) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let (cert_path, key_path) = (
        dir.join(format!("{name}.crt")),
        dir.join(format!("{name}.key")),
    );
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    (
        cert_path.display().to_string(),
        key_path.display().to_string(),
    )
}

// R2.08 3.
#[tokio::test(flavor = "multi_thread")]
async fn supplied_certificate() {
    let files = tempfile::tempdir().unwrap();
    let (cert, key) = pair(files.path(), "localhost");
    let live = (files.path().join("live.crt"), files.path().join("live.key"));
    std::fs::copy(&cert, &live.0).unwrap();
    std::fs::copy(&key, &live.1).unwrap();
    let (live_cert, live_key) = (live.0.display().to_string(), live.1.display().to_string());
    let (daemon, addr, first) = daemon_with(|config| {
        config.file.remote.tls_cert = live_cert;
        config.file.remote.tls_key = live_key;
        config.remote_limits.cert_reload_interval = Duration::from_millis(100);
    })
    .await;
    let status = remote_status(&daemon).await;
    assert_eq!(
        status.certificate.unwrap().source,
        i32::from(v1::RemoteCertificateSource::File)
    );
    // Pinned to the supplied key, a client connects.
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    connect(addr, &first, &secret)
        .await
        .daemon_status(true)
        .await
        .unwrap();
    // A new pair is presented without a restart.
    let (cert2, key2) = pair(files.path(), "second");
    std::fs::copy(&cert2, &live.0).unwrap();
    std::fs::copy(&key2, &live.1).unwrap();
    let url = target(addr, None, None).url;
    let deadline = Instant::now() + 5 * SECOND;
    let second = loop {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let presented = reef_daemon::client::remote::presented_fingerprint(&url)
            .await
            .unwrap();
        if presented != first {
            break presented;
        }
        assert!(
            Instant::now() < deadline,
            "the new pair was never presented"
        );
    };
    assert_eq!(fingerprint_of(&daemon).await, second);
    // A broken pair keeps the last one serving and says why.
    std::fs::write(&live.1, "not a key").unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let presented = reef_daemon::client::remote::presented_fingerprint(&url)
        .await
        .unwrap();
    assert_eq!(presented, second);
    let status = remote_status(&daemon).await;
    assert!(!status.error.is_empty(), "{status:?}");
    // Unreadable files at start: FAILED, and the socket serves.
    drop(daemon);
    let daemon = TestDaemon::start_with(|config| {
        remote_on(config);
        config.file.remote.tls_cert = "/nonexistent/c.pem".to_owned();
        config.file.remote.tls_key = "/nonexistent/k.pem".to_owned();
    })
    .await;
    let deadline = Instant::now() + 5 * SECOND;
    let status = loop {
        let status = remote_status(&daemon).await;
        if status.state == i32::from(RemoteListenerState::Failed) {
            break status;
        }
        assert!(Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.error.contains("/nonexistent/c.pem"), "{status:?}");
}

// R2.08 4.
#[tokio::test(flavor = "multi_thread")]
async fn no_plaintext() {
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    for clear in [
        &b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
        b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
    ] {
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        tcp.write_all(clear).await.unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(5 * SECOND, tcp.read_to_end(&mut answer)).await;
        assert!(!answer.starts_with(b"HTTP/"), "{answer:?}");
    }
    // HTTP/1.1 over TLS gets the assets; HTTP/2 native gRPC.
    let host = format!("localhost:{}", addr.port());
    let (parts, _, _) = send(
        addr,
        false,
        request(Method::GET, &host, "/", &[], Vec::new()),
    )
    .await;
    assert_eq!(parts.status, StatusCode::OK);
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    connect(addr, &fingerprint, &secret)
        .await
        .daemon_status(true)
        .await
        .unwrap();
}

// R2.08 5.
#[tokio::test(flavor = "multi_thread")]
async fn pin_is_checked_before_the_token() {
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    let wrong = format!("sha256:{}", "ab".repeat(32));
    let error = Client::connect_remote(&target(addr, Some(&wrong), Some(&secret)))
        .await
        .unwrap_err();
    let ClientError::RemoteTls {
        failure: TlsFailure::PinMismatch { presented, .. },
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*presented, fingerprint);
    assert!(error.advice().unwrap().hint.contains(&fingerprint));
    // No pin, and no root trusts a generated certificate.
    let error = Client::connect_remote(&target(addr, None, Some(&secret)))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ClientError::RemoteTls {
                failure: TlsFailure::Untrusted { presented, .. },
                ..
            } if *presented == fingerprint
        ),
        "{error:?}"
    );
    // Nothing reached the gate: the token is still unused.
    let tokens = daemon.client.list_remote_tokens_all(false).await.unwrap();
    assert!(tokens[0].last_used_at.is_none());
}

// ---- 6 to 10: tokens, scopes, rules, identity ----

// R2.08 6.
#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_required() {
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    let good = token(&daemon, "laptop", RemoteScope::Read).await;
    let (_, rest) = good.split_at(22);
    let unknown_id = format!("reef_0000000000000000_{rest}");
    let wrong_secret = format!("{}{}", &good[..22], "A".repeat(43));
    for bad in ["x", "reef_", unknown_id.as_str(), wrong_secret.as_str()] {
        let error = connect(addr, &fingerprint, bad)
            .await
            .daemon_status(true)
            .await
            .unwrap_err();
        assert_eq!(
            reason(&error),
            reef_api::error::REMOTE_TOKEN_INVALID,
            "{bad}"
        );
        assert_eq!(meta(&error, "state"), "unknown", "{bad}");
        assert_eq!(error.kind(), reef_lib::error::ErrorKind::PermissionDenied);
    }
    // No authorization at all.
    let none = Client::connect_remote(&target(addr, Some(&fingerprint), None))
        .await
        .unwrap();
    let error = none.daemon_status(true).await.unwrap_err();
    assert_eq!(meta(&error, "state"), "unknown");
    // Expired, then revoked: said only to the token's holder.
    let short = daemon
        .client
        .create_remote_token("short", RemoteScope::Read, Some(Duration::from_millis(200)))
        .await
        .unwrap()
        .secret;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let error = connect(addr, &fingerprint, &short)
        .await
        .daemon_status(true)
        .await
        .unwrap_err();
    assert_eq!(meta(&error, "state"), "expired");
    daemon.client.revoke_remote_token("laptop").await.unwrap();
    let error = connect(addr, &fingerprint, &good)
        .await
        .daemon_status(true)
        .await
        .unwrap_err();
    assert_eq!(meta(&error, "state"), "revoked");
    let fresh = token(&daemon, "fresh", RemoteScope::Read).await;
    connect(addr, &fingerprint, &fresh)
        .await
        .daemon_status(true)
        .await
        .unwrap();
}

// R2.08 7.
#[tokio::test(flavor = "multi_thread")]
async fn scopes_cover_every_method() {
    use reef_api::v1::Access;
    let (daemon, addr, _) = daemon_with(|config| {
        config.remote_limits.max_requests_per_token = 1_000;
        config.remote_limits.max_streams_per_token = 1_000;
    })
    .await;
    let read = token(&daemon, "reader", RemoteScope::Read).await;
    let write = token(&daemon, "writer", RemoteScope::Write).await;
    let mut methods: Vec<&str> = reef_api::methods().collect();
    methods.push("/reef.daemon.v0.Daemon/Status");
    for path in methods {
        let access = reef_api::access(path);
        for (scope, token) in [("read", &read), ("write", &write)] {
            let headers = call_raw(addr, path, token).await;
            let denied = reason_in(&headers) == reef_api::error::REMOTE_ACCESS_DENIED;
            let allowed = match access {
                Some(Access::Read) => true,
                Some(Access::Write) => scope == "write",
                _ => false,
            };
            assert_eq!(denied, !allowed, "{path} with a {scope} token: {headers:?}");
            if denied {
                let status = tonic::Status::from_header_map(&headers).unwrap();
                let detail = reef_api::error::detail(&status).unwrap();
                let expected = match access {
                    Some(Access::Write) => "write",
                    Some(Access::Admin) => "admin",
                    _ => "unknown",
                };
                assert_eq!(detail.metadata["access"], expected, "{path}");
                assert_eq!(detail.metadata["scope"], scope, "{path}");
            }
        }
    }
}

// R2.08 7, continued: a v1 method this daemon does not know is
// `UNKNOWN_METHOD`, as on the socket, so a newer client names the version
// mismatch; without a token it is still a token refusal.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_methods_are_named() {
    let (daemon, addr, _) = daemon_with(|_| {}).await;
    let read = token(&daemon, "reader", RemoteScope::Read).await;
    let headers = call_raw(addr, "/reef.api.v1.DaemonService/Nope", &read).await;
    assert_eq!(
        reason_in(&headers),
        reef_api::error::UNKNOWN_METHOD,
        "{headers:?}"
    );
    let headers = call_raw(addr, "/reef.api.v1.DaemonService/Nope", "reef_x").await;
    assert_eq!(
        reason_in(&headers),
        reef_api::error::REMOTE_TOKEN_INVALID,
        "{headers:?}"
    );
}

// Review: an owner cannot carry a remote token's mark into the operations
// that fall back to it.
#[tokio::test(flavor = "multi_thread")]
async fn owners_cannot_forge_a_token() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let remote = connect(
        addr,
        &fingerprint,
        &token(&daemon, "w", RemoteScope::Write).await,
    )
    .await;
    for client in [&remote, &daemon.client] {
        let error = client
            .create_workspace(CreateWorkspaceRequest {
                repo: repo.id.clone(),
                owner: "x\u{1}0000000000000000\u{1}victim".to_owned(),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            reef_lib::error::ErrorKind::InvalidArgument,
            "{error:?}"
        );
    }
}

// Review: a connection that makes no authenticated request within
// `auth_timeout` is closed.
#[tokio::test(flavor = "multi_thread")]
async fn idle_connections_are_closed() {
    let (_daemon, addr, _) = daemon_with(|config| {
        config.remote_limits.auth_timeout = Duration::from_millis(300);
    })
    .await;
    let mut idle = tls(addr, b"http/1.1").await.unwrap();
    let started = Instant::now();
    let mut buffer = [0_u8; 64];
    let read = tokio::time::timeout(5 * SECOND, idle.read(&mut buffer))
        .await
        .expect("the idle connection was never closed");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert!(started.elapsed() >= Duration::from_millis(250));
}

async fn create(client: &Client, repo: &SyntheticRepo) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "remote-test".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn remote_repo() -> (TestDaemon, SyntheticRepo, SocketAddr, String) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        remote_on(config);
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let addr = listening(&daemon).await;
    let fingerprint = fingerprint_of(&daemon).await;
    (daemon, repo, addr, fingerprint)
}

// R2.08 9.
#[tokio::test(flavor = "multi_thread")]
async fn field_and_ownership_rules() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let mine = connect(
        addr,
        &fingerprint,
        &token(&daemon, "mine", RemoteScope::Write).await,
    )
    .await;
    let theirs = connect(
        addr,
        &fingerprint,
        &token(&daemon, "theirs", RemoteScope::Write).await,
    )
    .await;
    let error = mine
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "remote".to_owned(),
            path: "/tmp/anywhere".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REMOTE_ACCESS_DENIED);
    assert_eq!(meta(&error, "field"), "path");
    assert!(
        daemon
            .client
            .list_workspaces(v1::ListWorkspacesRequest::default())
            .await
            .unwrap()
            .is_empty(),
        "a refused create made a workspace"
    );

    let ws = create(&mine, &repo).await;
    let tokens = daemon.client.list_remote_tokens_all(false).await.unwrap();
    let mine_id = &tokens.iter().find(|t| t.name == "mine").unwrap().id;
    assert_eq!(&ws.remote_token_id, mine_id);
    let local = create(&daemon.client, &repo).await;
    assert!(local.remote_token_id.is_empty(), "{local:?}");
    let lease = ws.lease.as_ref().unwrap().id.clone();
    // Another token's, and the socket's: refused.
    for (client, workspace) in [(&theirs, &ws), (&mine, &local)] {
        let error = client
            .release_workspace("", &workspace.id, false, false)
            .await
            .unwrap_err();
        assert_eq!(reason(&error), reef_api::error::REMOTE_WORKSPACE_NOT_OWNED);
        assert_eq!(meta(&error, "workspace"), workspace.id);
    }
    let error = theirs.renew_lease("", &ws.id, &lease).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REMOTE_WORKSPACE_NOT_OWNED);
    let error = theirs
        .move_workspace(MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REMOTE_WORKSPACE_NOT_OWNED);
    // A snapshot of anyone's is a READ-like write.
    theirs.snapshot_workspace("", &local.id, "").await.unwrap();
    // Its own: renewed and released.
    mine.renew_lease("", &ws.id, &lease).await.unwrap();
    mine.release_workspace("", &ws.id, false, false)
        .await
        .unwrap();
    // The socket releases anything.
    let other = create(&mine, &repo).await;
    daemon
        .client
        .release_workspace("", &other.id, false, false)
        .await
        .unwrap();
}

// R2.08 10.
#[tokio::test(flavor = "multi_thread")]
async fn operations_name_the_token() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let secret = token(&daemon, "laptop", RemoteScope::Write).await;
    let remote = connect(addr, &fingerprint, &secret).await;
    let ws = create(&remote, &repo).await;
    let describe = |client: &Client, text: &str| {
        let request = DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: text.to_owned(),
            ..DescribeChangeRequest::default()
        };
        let client = client.clone();
        async move {
            client
                .describe_change(request)
                .await
                .unwrap()
                .operation
                .unwrap()
        }
    };
    let op = describe(&remote, "remote one").await;
    let token_id = daemon.client.list_remote_tokens_all(false).await.unwrap()[0]
        .id
        .clone();
    let caller = op.remote.clone().unwrap();
    assert_eq!(caller.token_id, token_id);
    assert_eq!(caller.token_name, "laptop");
    assert_eq!(op.agent, "laptop");
    assert_eq!(op.attributes["reef.remote-token"], token_id);
    assert_eq!(op.attributes["reef.remote-token-name"], "laptop");
    assert_eq!(op.attributes["reef.agent"], "laptop");
    // With an agent: the agent is named, the token still recorded.
    let planner = remote.clone().with_agent("planner").unwrap();
    let op = describe(&planner, "remote two").await;
    assert_eq!(op.agent, "planner");
    assert_eq!(op.remote.unwrap().token_id, token_id);
    // Over the socket: no token.
    let op = describe(&daemon.client, "local").await;
    assert!(op.remote.is_none());
    assert!(!op.attributes.contains_key("reef.remote-token"));
    // A forged header changes nothing.
    let op = describe(&remote, "remote three").await;
    assert_eq!(op.remote.unwrap().token_name, "laptop");
    // The create's own operation names it too.
    let ops = daemon
        .client
        .list_operations_limited(
            v1::ListOperationsRequest {
                repo: repo.id.clone(),
                ..v1::ListOperationsRequest::default()
            },
            50,
        )
        .await
        .unwrap();
    let created = ops
        .items
        .iter()
        .find(|op| op.kind == i32::from(v1::OperationKind::CreateWorkspace))
        .unwrap();
    assert_eq!(created.remote.as_ref().unwrap().token_id, token_id);
}

// ---- 12 to 18: tokens' life and limits ----

// R2.08 12.
#[tokio::test(flavor = "multi_thread")]
async fn rotation() {
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    let old = token(&daemon, "laptop", RemoteScope::Read).await;
    let rotated = daemon
        .client
        .rotate_remote_token("laptop", Duration::ZERO)
        .await
        .unwrap();
    let call = |secret: String| {
        let fingerprint = fingerprint.clone();
        async move {
            connect(addr, &fingerprint, &secret)
                .await
                .daemon_status(true)
                .await
        }
    };
    call(rotated.secret.clone()).await.unwrap();
    call(old).await.unwrap_err();
    let again = daemon
        .client
        .rotate_remote_token("laptop", Duration::from_millis(300))
        .await
        .unwrap();
    call(rotated.secret.clone()).await.unwrap();
    call(again.secret.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    call(rotated.secret).await.unwrap_err();
    call(again.secret).await.unwrap();
    let token = again.token.unwrap();
    assert_eq!(token.name, "laptop");
    assert_eq!(token.scope, i32::from(RemoteScope::Read));
    assert!(token.rotated_at.is_some());
}

// R2.08 13.
#[tokio::test(flavor = "multi_thread")]
async fn revocation_ends_streams() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let doomed = connect(
        addr,
        &fingerprint,
        &token(&daemon, "doomed", RemoteScope::Read).await,
    )
    .await;
    let kept = connect(
        addr,
        &fingerprint,
        &token(&daemon, "kept", RemoteScope::Read).await,
    )
    .await;
    let subscribe = |client: &Client| {
        let client = client.clone();
        let repo = repo.id.clone();
        async move {
            client
                .subscribe_operations(SubscribeOperationsRequest {
                    repos: vec![repo],
                    ..SubscribeOperationsRequest::default()
                })
                .await
                .unwrap()
        }
    };
    let mut doomed_stream = subscribe(&doomed).await;
    let mut kept_stream = subscribe(&kept).await;
    let revoked_at = Instant::now();
    daemon.client.revoke_remote_token("doomed").await.unwrap();
    let ended = tokio::time::timeout(3 * SECOND, async {
        loop {
            match doomed_stream.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without a status"),
                Err(status) => return status,
            }
        }
    })
    .await
    .expect("the revoked stream never ended");
    assert!(revoked_at.elapsed() < 2 * SECOND);
    let detail = reef_api::error::detail(&ended).unwrap();
    assert_eq!(detail.reason, reef_api::error::REMOTE_TOKEN_INVALID);
    assert_eq!(detail.metadata["state"], "revoked");
    // The other token's stream still delivers.
    let ws = create(&daemon.client, &repo).await;
    let frame = tokio::time::timeout(5 * SECOND, kept_stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !frame.operations.is_empty() || !frame.head_operation_ids.is_empty(),
        "{ws:?}"
    );
    let error = doomed.daemon_status(true).await.unwrap_err();
    assert_eq!(meta(&error, "state"), "revoked");
}

// Review: a rotation ends the streams opened with the old secret, which
// resume with whichever secret their client holds.
#[tokio::test(flavor = "multi_thread")]
async fn rotation_ends_streams() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let old = token(&daemon, "laptop", RemoteScope::Read).await;
    let client = connect(addr, &fingerprint, &old).await;
    let mut stream = client
        .subscribe_operations(SubscribeOperationsRequest {
            repos: vec![repo.id.clone()],
            ..SubscribeOperationsRequest::default()
        })
        .await
        .unwrap();
    daemon
        .client
        .rotate_remote_token("laptop", Duration::ZERO)
        .await
        .unwrap();
    let ended = tokio::time::timeout(3 * SECOND, async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("ended without a status"),
                Err(status) => return status,
            }
        }
    })
    .await
    .expect("the old secret's stream outlived the rotation");
    let detail = reef_api::error::detail(&ended).unwrap();
    assert_eq!(detail.metadata["state"], "unknown");
}

// R2.08 14.
#[tokio::test(flavor = "multi_thread")]
async fn token_lifecycle_rpcs() {
    let (daemon, addr, fingerprint) = daemon_with(|config| {
        config.remote_limits.max_tokens = 2;
        config.remote_limits.last_used_flush = Duration::from_millis(50);
    })
    .await;
    let created = daemon
        .client
        .create_remote_token("laptop", RemoteScope::Write, None)
        .await
        .unwrap();
    let secret = &created.secret;
    let parts: Vec<&str> = secret.splitn(3, '_').collect();
    assert_eq!(parts[0], "reef");
    assert_eq!(parts[1].len(), 16);
    assert_eq!(secret.len(), 65);
    let token = created.token.unwrap();
    assert!(token.expires_at.is_none(), "never");
    // Only the digest is stored.
    let db = std::fs::read(daemon.state_dir().join("reef.sqlite3")).unwrap();
    let raw = &secret[22..];
    assert!(!db.windows(raw.len()).any(|window| window == raw.as_bytes()));
    for (name, expected) in [
        ("Laptop", reef_api::error::INVALID_REMOTE_TOKEN_NAME),
        ("laptop", reef_api::error::REMOTE_TOKEN_EXISTS),
    ] {
        let error = daemon
            .client
            .create_remote_token(name, RemoteScope::Read, None)
            .await
            .unwrap_err();
        assert_eq!(reason(&error), expected, "{name}");
    }
    let defaults = daemon
        .client
        .create_remote_token(
            "phone",
            RemoteScope::Unspecified,
            Some(Duration::from_hours(720)),
        )
        .await
        .unwrap()
        .token
        .unwrap();
    assert_eq!(defaults.scope, i32::from(RemoteScope::Read));
    let error = daemon
        .client
        .create_remote_token("third", RemoteScope::Read, None)
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::TOO_MANY_REMOTE_TOKENS);
    for error in [
        daemon.client.revoke_remote_token("nope").await.unwrap_err(),
        daemon
            .client
            .rotate_remote_token("nope", Duration::ZERO)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(reason(&error), reef_api::error::REMOTE_TOKEN_NOT_FOUND);
    }
    // Last use.
    connect(addr, &fingerprint, secret)
        .await
        .daemon_status(true)
        .await
        .unwrap();
    let listed = daemon.client.list_remote_tokens_all(false).await.unwrap();
    assert_eq!(listed.len(), 2);
    let laptop = listed.iter().find(|t| t.name == "laptop").unwrap();
    assert!(laptop.last_used_at.is_some());
    assert!(laptop.last_used_from.starts_with("127.0.0.1:"));
    daemon.client.revoke_remote_token("phone").await.unwrap();
    assert_eq!(
        daemon
            .client
            .list_remote_tokens_all(false)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        daemon
            .client
            .list_remote_tokens_all(true)
            .await
            .unwrap()
            .len(),
        2
    );
    // Revoked: the name is free and counts no more.
    daemon
        .client
        .create_remote_token("phone", RemoteScope::Read, None)
        .await
        .unwrap();
}

// R2.08 15.
#[tokio::test(flavor = "multi_thread")]
async fn backoff() {
    let (daemon, addr, fingerprint) = daemon_with(|config| {
        config.remote_limits.failures_before_backoff = 3;
        config.remote_limits.backoff = Duration::from_millis(400);
    })
    .await;
    let good = token(&daemon, "laptop", RemoteScope::Read).await;
    let bad = connect(addr, &fingerprint, "reef_bad").await;
    for _ in 0..3 {
        bad.daemon_status(true).await.unwrap_err();
    }
    // In backoff: closed before TLS, whatever the token.
    let error = Client::connect_remote(&target(addr, Some(&fingerprint), Some(&good)))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ClientError::RemoteConnect { .. } | ClientError::RemoteTls { .. }
        ),
        "{error:?}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    connect(addr, &fingerprint, &good)
        .await
        .daemon_status(true)
        .await
        .unwrap();
}

// R2.08 16.
#[tokio::test(flavor = "multi_thread")]
async fn connection_limits() {
    let (_daemon, addr, _) = daemon_with(|config| {
        config.remote_limits.max_connections_per_address = 2;
        config.remote_limits.max_handshakes = 1;
        config.remote_limits.handshake_timeout = Duration::from_millis(300);
    })
    .await;
    // A silent connection holds the only handshake slot until its timeout.
    let _silent = TcpStream::connect(addr).await.unwrap();
    let started = Instant::now();
    let first = tls(addr, b"http/1.1").await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    // The silent one is gone; one more fits, and a third is closed.
    let _second = tls(addr, b"http/1.1").await.unwrap();
    let third = tokio::time::timeout(5 * SECOND, tls(addr, b"http/1.1"))
        .await
        .unwrap();
    assert!(third.is_err());
    drop(first);
}

// R2.08 17.
#[tokio::test(flavor = "multi_thread")]
async fn per_token_limits() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    daemon.handle.state().remote.tokens();
    let limited = daemon_limits_token(&daemon, "limited").await;
    let other = daemon_limits_token(&daemon, "other").await;
    let client = connect(addr, &fingerprint, &limited).await;
    let subscribe = |client: Client| {
        let repo = repo.id.clone();
        async move {
            client
                .subscribe_operations(SubscribeOperationsRequest {
                    repos: vec![repo],
                    ..SubscribeOperationsRequest::default()
                })
                .await
        }
    };
    let mut streams = Vec::new();
    for _ in 0..16 {
        streams.push(subscribe(client.clone()).await.unwrap());
    }
    let error = subscribe(client.clone()).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REMOTE_BUSY);
    assert_eq!(meta(&error, "limit"), "streams");
    let other = connect(addr, &fingerprint, &other).await;
    subscribe(other).await.unwrap();
    drop(streams);
    tokio::time::sleep(Duration::from_millis(200)).await;
    subscribe(client).await.unwrap();
}

async fn daemon_limits_token(daemon: &TestDaemon, name: &str) -> String {
    token(daemon, name, RemoteScope::Read).await
}

// ---- 19 to 22: the web UI, the socket, binding, shutdown ----

const HEADERS: &[&str] = &[
    "content-security-policy",
    "x-content-type-options",
    "referrer-policy",
    "x-frame-options",
    "cross-origin-opener-policy",
    "cross-origin-resource-policy",
    "permissions-policy",
];

// R2.08 19.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario, in its order")]
async fn web_ui_over_remote() {
    let (daemon, addr, _) = daemon_with(|_| {}).await;
    let secret = token(&daemon, "browser", RemoteScope::Read).await;
    let port = addr.port();
    let host = format!("localhost:{port}");
    for h2 in [false, true] {
        let (parts, _, _) = send(addr, h2, request(Method::GET, &host, "/", &[], Vec::new())).await;
        assert_eq!(parts.status, StatusCode::OK, "h2 {h2}");
        for name in HEADERS {
            assert!(parts.headers.contains_key(*name), "{name}");
        }
        assert!(!parts.headers.contains_key("strict-transport-security"));
        assert!(
            !parts
                .headers
                .keys()
                .any(|name| name.as_str().starts_with("access-control-"))
        );
    }
    for (authority, status) in [
        (format!("127.0.0.1:{port}"), StatusCode::OK),
        (format!("LOCALHOST:{port}"), StatusCode::OK),
        (
            format!("evil.example:{port}"),
            StatusCode::MISDIRECTED_REQUEST,
        ),
    ] {
        let (parts, _, _) = send(
            addr,
            false,
            request(Method::GET, &authority, "/", &[], Vec::new()),
        )
        .await;
        assert_eq!(parts.status, status, "{authority}");
    }
    let call = |origin: Option<String>| {
        let secret = secret.clone();
        let host = host.clone();
        async move {
            let bearer = format!("Bearer {secret}");
            let mut headers = vec![
                ("content-type", "application/grpc-web+proto"),
                ("authorization", bearer.as_str()),
            ];
            let origin = origin.unwrap_or_default();
            if !origin.is_empty() {
                headers.push(("origin", origin.as_str()));
            }
            send(
                addr,
                false,
                request(
                    Method::POST,
                    &host,
                    "/reef.api.v1.DaemonService/GetDaemonStatus",
                    &headers,
                    frame(&GetDaemonStatusRequest {
                        skip_backends: true,
                    }),
                ),
            )
            .await
        }
    };
    let (parts, body, _) = call(Some(format!("https://localhost:{port}"))).await;
    assert_eq!(parts.status, StatusCode::OK);
    assert!(!body.is_empty());
    for origin in [
        format!("http://localhost:{port}"),
        "https://evil.example".to_owned(),
    ] {
        let (parts, _, _) = call(Some(origin.clone())).await;
        assert_eq!(parts.status, StatusCode::FORBIDDEN, "{origin}");
    }
    // A refusal over gRPC-web is a trailers-only gRPC-web answer.
    let (parts, _, _) = send(
        addr,
        false,
        request(
            Method::POST,
            &host,
            "/reef.api.v1.DaemonService/GetDaemonStatus",
            &[("content-type", "application/grpc-web+proto")],
            frame(&GetDaemonStatusRequest::default()),
        ),
    )
    .await;
    assert_eq!(parts.headers["content-type"], "application/grpc-web+proto");
    assert_eq!(
        reason_in(&parts.headers),
        reef_api::error::REMOTE_TOKEN_INVALID
    );
    // Text mode is refused.
    let (parts, _, _) = send(
        addr,
        false,
        request(
            Method::POST,
            &host,
            "/reef.api.v1.DaemonService/GetDaemonStatus",
            &[("content-type", "application/grpc-web-text")],
            Vec::new(),
        ),
    )
    .await;
    assert_eq!(parts.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// R2.08 20.
#[tokio::test(flavor = "multi_thread")]
async fn socket_unaffected() {
    let (daemon, addr, fingerprint) = daemon_with(|_| {}).await;
    // The socket needs no token, and still mints web sessions.
    daemon.client.daemon_status(true).await.unwrap();
    let session = daemon.client.create_web_session(false).await.unwrap();
    let session_token = session.session_url.split_once("#session=").unwrap().1;
    // A web session's token is not a remote token.
    let error = connect(addr, &fingerprint, session_token)
        .await
        .daemon_status(true)
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REMOTE_TOKEN_INVALID);
    // And a remote token is no web session.
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    let web = session
        .url
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let response = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        .build_http::<Full<Bytes>>()
        .request(
            http::Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{web}/reef.api.v1.DaemonService/GetDaemonStatus"
                ))
                .header("content-type", "application/grpc-web+proto")
                .header("authorization", format!("Bearer {secret}"))
                .body(Full::new(Bytes::from(frame(
                    &GetDaemonStatusRequest::default(),
                ))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        reason_in(response.headers()),
        reef_api::error::WEB_SESSION_INVALID
    );
}

// R2.08 21.
#[tokio::test(flavor = "multi_thread")]
async fn bind_failure_retries() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let daemon = TestDaemon::start_with(|config| {
        remote_on(config);
        config.file.remote.listen = format!("127.0.0.1:{port}");
        config.remote_limits.bind_retry = Duration::from_millis(50);
        config.remote_limits.bind_retry_max = Duration::from_millis(100);
    })
    .await;
    let deadline = Instant::now() + 5 * SECOND;
    let status = loop {
        let status = remote_status(&daemon).await;
        if status.state == i32::from(RemoteListenerState::Failed) {
            break status;
        }
        assert!(Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.error.contains(&port.to_string()), "{status:?}");
    // The socket serves meanwhile.
    daemon.client.list_repositories().await.unwrap();
    drop(held);
    let addr = listening(&daemon).await;
    assert_eq!(addr.port(), port);
    let status = remote_status(&daemon).await;
    assert!(status.error.is_empty(), "{status:?}");
}

// R2.08 22.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let remote = connect(
        addr,
        &fingerprint,
        &token(&daemon, "laptop", RemoteScope::Read).await,
    )
    .await;
    let mut stream = remote
        .subscribe_operations(SubscribeOperationsRequest {
            repos: vec![repo.id.clone()],
            ..SubscribeOperationsRequest::default()
        })
        .await
        .unwrap();
    let TestDaemon { handle, .. } = daemon;
    handle.shutdown();
    let ended = tokio::time::timeout(5 * SECOND, async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                other => return other,
            }
        }
    })
    .await
    .expect("the stream outlived the daemon");
    assert!(ended.is_err() || matches!(ended, Ok(None)), "{ended:?}");
    handle.join().await.unwrap();
    assert!(TcpStream::connect(addr).await.is_err());
}

// Presence reported remotely names the token's agent.
#[tokio::test(flavor = "multi_thread")]
async fn presence_over_remote() {
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let remote = connect(
        addr,
        &fingerprint,
        &token(&daemon, "laptop", RemoteScope::Write).await,
    )
    .await
    .with_agent("agent-1")
    .unwrap();
    let reported = remote
        .report_presence(ReportPresenceRequest {
            kind: PresenceKind::Activity.into(),
            agent: "agent-1".to_owned(),
            repo: repo.id.clone(),
            ..ReportPresenceRequest::default()
        })
        .await;
    assert!(reported.is_ok(), "{reported:?}");
}

/// A log sink for [`audit_log_and_no_secrets`].
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

// R2.08 11 and 18: the audit lines, and no secret in any log line.
#[tokio::test(flavor = "multi_thread")]
async fn audit_log_and_no_secrets() {
    let sink = Sink::default();
    tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .init();
    let (daemon, repo, addr, fingerprint) = remote_repo().await;
    let read = token(&daemon, "reader", RemoteScope::Read).await;
    let write = token(&daemon, "writer", RemoteScope::Write).await;
    let rotated = daemon
        .client
        .rotate_remote_token("writer", Duration::from_secs(60))
        .await
        .unwrap()
        .secret;
    let writer = connect(addr, &fingerprint, &rotated).await;
    let ws = create(&writer, &repo).await;
    writer
        .report_presence(ReportPresenceRequest {
            kind: PresenceKind::Activity.into(),
            agent: "writer".to_owned(),
            repo: repo.id.clone(),
            activity: "testing".to_owned(),
            ..ReportPresenceRequest::default()
        })
        .await
        .unwrap();
    // A refused write, and token refusals from one address.
    let reader = connect(addr, &fingerprint, &read).await;
    reader.snapshot_workspace("", &ws.id, "").await.unwrap_err();
    let bad = connect(addr, &fingerprint, "reef_0000000000000000_x").await;
    for _ in 0..20 {
        bad.daemon_status(true).await.unwrap_err();
    }
    daemon.client.revoke_remote_token("reader").await.unwrap();
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let audit: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("reef_daemon::remote::audit"))
        .collect();
    let has = |needle: &str| audit.iter().any(|line| line.contains(needle));
    assert!(has("remote write") && has("CreateWorkspace"), "{audit:#?}");
    assert!(has("ReportPresence"), "{audit:#?}");
    assert!(
        has("remote call refused") && has("SnapshotWorkspace"),
        "{audit:#?}"
    );
    for action in ["create", "rotate", "revoke"] {
        assert!(
            has(&format!("action=\"{action}\"")) || has(&format!("action={action}")),
            "{action}: {audit:#?}"
        );
    }
    assert!(has("code=Ok"), "{audit:#?}");
    // Twenty refusals from one address: the first logged, the tenth with
    // the backoff it starts (and the nine between counted), the rest closed
    // before TLS.
    let refused = audit
        .iter()
        .filter(|line| line.contains("remote token refused"))
        .count();
    assert_eq!(refused, 2, "{audit:#?}");
    assert!(has("refusal_count=9"), "{audit:#?}");
    assert!(has("remote address in backoff"), "{audit:#?}");
    for secret in [&read, &write, &rotated] {
        let raw = &secret[22..];
        assert!(!text.contains(raw), "a secret reached the log");
    }
}
