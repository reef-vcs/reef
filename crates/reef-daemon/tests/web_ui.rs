//! The web UI's loopback listener (R2.18, tests 1 to 14): sessions, the
//! gate's rules and headers, assets, gRPC-web streams, launch files, and
//! the listener's limits. Test daemons serve the placeholder assets through
//! the full gate (`web::Limits::assets_built`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use reef_api::v1::daemon_service_client::DaemonServiceClient;
use reef_api::v1::operation_service_client::OperationServiceClient;
use reef_api::v1::presence_service_client::PresenceServiceClient;
use reef_api::v1::{
    self, CreateWebSessionResponse, CreateWorkspaceRequest, DescribeChangeRequest,
    GetDaemonStatusRequest, PresenceKind, ReportPresenceRequest, SubscribeOperationsRequest,
    SubscribePresenceRequest,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use tonic::body::Body;
use tonic::codegen::Bytes;
use tonic::codegen::http::{self, Method, StatusCode, Uri, header};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Code, Request, Status};
use tonic_web::{GrpcWebClientLayer, GrpcWebClientService};
use tower::Layer as _;

const SECOND: Duration = Duration::from_secs(1);

/// The security headers every response carries (the gate's list).
const HEADERS: &[&str] = &[
    "content-security-policy",
    "x-content-type-options",
    "referrer-policy",
    "x-frame-options",
    "cross-origin-opener-policy",
    "cross-origin-resource-policy",
    "permissions-policy",
];

async fn session(client: &Client, launch_file: bool) -> CreateWebSessionResponse {
    client.create_web_session(launch_file).await.unwrap()
}

fn token_of(session: &CreateWebSessionResponse) -> String {
    session
        .session_url
        .split_once("#session=")
        .unwrap()
        .1
        .to_owned()
}

/// `127.0.0.1:<port>` of a session's URL.
fn authority_of(session: &CreateWebSessionResponse) -> String {
    session
        .url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}

fn reason(status: &Status) -> String {
    reef_daemon::error::detail_of(status)
        .map(|detail| detail.reason)
        .unwrap_or_default()
}

fn metadata(status: &Status, key: &str) -> String {
    reef_daemon::error::detail_of(status)
        .and_then(|detail| detail.metadata.get(key).cloned())
        .unwrap_or_default()
}

/// The status of a refused socket call.
fn status_of(error: &ClientError) -> &Status {
    match error {
        ClientError::Rpc { status, .. } => status,
        _ => panic!("not an RPC error: {error:?}"),
    }
}

// ---- a raw HTTP/1.1 client, for the gate's plain-HTTP rules ----

fn http() -> HttpClient<HttpConnector, Full<Bytes>> {
    HttpClient::builder(TokioExecutor::new()).build_http()
}

/// A request to `authority` (its `Host`, unless `headers` sets one).
async fn send(
    authority: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> http::Response<Bytes> {
    let response = request(authority, method, path, headers, body)
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    http::Response::from_parts(parts, bytes)
}

/// [`send`]'s response head, its body never read: a stream's body never
/// ends, and a refusal is trailers-only, all in the headers.
async fn send_head(
    authority: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> http::Response<()> {
    let response = request(authority, method, path, headers, body)
        .await
        .unwrap();
    response.map(drop)
}

fn request(
    authority: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> hyper_util::client::legacy::ResponseFuture {
    let mut request = http::Request::builder()
        .method(method)
        .uri(format!("http://{authority}{path}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    http().request(request.body(Full::new(Bytes::from(body))).unwrap())
}

async fn get(authority: &str, path: &str) -> http::Response<Bytes> {
    send(authority, Method::GET, path, &[], Vec::new()).await
}

/// A gRPC-web message frame around an encoded request.
fn frame(message: &impl prost::Message) -> Vec<u8> {
    let encoded = message.encode_to_vec();
    let mut framed = vec![0];
    framed.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
    framed.extend(encoded);
    framed
}

/// The trailers of a gRPC-web body: its frame flagged `0x80`, as text. A
/// response that is not trailers-only carries `grpc-status` there.
fn trailers(body: &[u8]) -> String {
    let mut rest = body;
    while rest.len() >= 5 {
        let len = usize::try_from(u32::from_be_bytes(rest[1..5].try_into().unwrap())).unwrap();
        let (flag, data) = (rest[0], &rest[5..5 + len]);
        if flag & 0x80 != 0 {
            return String::from_utf8(data.to_vec()).unwrap();
        }
        rest = &rest[5 + len..];
    }
    panic!("no trailers frame in {body:?}");
}

/// `GetDaemonStatus` as raw gRPC-web with `headers` added.
async fn raw_status_call(authority: &str, headers: &[(&str, &str)]) -> http::Response<Bytes> {
    let mut all = vec![("content-type", "application/grpc-web+proto")];
    all.extend_from_slice(headers);
    send(
        authority,
        Method::POST,
        "/reef.api.v1.DaemonService/GetDaemonStatus",
        &all,
        frame(&GetDaemonStatusRequest {
            skip_backends: true,
        }),
    )
    .await
}

// ---- gRPC-web through tonic, for the API ----

/// Adds `authorization: Bearer <token>`.
#[derive(Clone)]
struct Bearer(Option<String>);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = &self.0 {
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        Ok(request)
    }
}

type Web = InterceptedService<
    GrpcWebClientService<HttpClient<HttpConnector, tonic_web::GrpcWebCall<Body>>>,
    Bearer,
>;

fn web(session: &CreateWebSessionResponse, token: Option<&str>) -> (Web, Uri) {
    let client = HttpClient::builder(TokioExecutor::new()).build_http();
    let service = GrpcWebClientLayer::new().layer(client);
    (
        InterceptedService::new(service, Bearer(token.map(str::to_owned))),
        session.url.parse().unwrap(),
    )
}

fn daemon_service(
    session: &CreateWebSessionResponse,
    token: Option<&str>,
) -> DaemonServiceClient<Web> {
    let (service, origin) = web(session, token);
    DaemonServiceClient::with_origin(service, origin)
}

async fn status_over_web(
    session: &CreateWebSessionResponse,
    token: Option<&str>,
) -> Result<v1::GetDaemonStatusResponse, Status> {
    daemon_service(session, token)
        .get_daemon_status(GetDaemonStatusRequest {
            skip_backends: true,
        })
        .await
        .map(tonic::Response::into_inner)
}

// ---- tests ----

#[tokio::test(flavor = "multi_thread")]
async fn session_binds_lazily() {
    let daemon = TestDaemon::start().await;
    assert!(!daemon.handle.state().web.is_bound());
    let first = session(&daemon.client, true).await;
    assert!(daemon.handle.state().web.is_bound());
    let port: u16 = first
        .url
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .strip_suffix('/')
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(port, 0);
    let token = token_of(&first);
    assert_eq!(first.session_url, format!("{}#session={token}", first.url));
    assert_eq!(token.len(), 43);
    assert!(
        token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    );
    let launch = Path::new(&first.launch_path);
    let ui = daemon.state_dir().join("ui");
    assert_eq!(launch.parent().unwrap(), ui);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&ui), 0o700);
    assert_eq!(mode(launch), 0o600);
    let html = std::fs::read_to_string(launch).unwrap();
    assert!(html.contains(&format!("url={}\"", first.session_url)));
    assert!(!html.contains("<script"));
    let second = session(&daemon.client, false).await;
    assert_eq!(second.url, first.url);
    assert_ne!(token_of(&second), token);
    assert_eq!(second.launch_path, "");

    // Off, and not built: refused, and nothing is bound. Off wins.
    for (enabled, built, expected) in [
        (false, true, "WEB_UI_DISABLED"),
        (true, false, "WEB_UI_NOT_BUILT"),
        (false, false, "WEB_UI_DISABLED"),
    ] {
        let daemon = TestDaemon::start_with(|config: &mut Config| {
            config.file.ui.enabled = enabled;
            config.web.assets_built = built;
        })
        .await;
        let error = daemon.client.create_web_session(true).await.unwrap_err();
        let status = status_of(&error);
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(reason(status), expected);
        assert!(!daemon.handle.state().web.is_bound());
        let ui = daemon.state_dir().join("ui");
        assert!(!ui.exists() || std::fs::read_dir(&ui).unwrap().next().is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn assets_are_public_and_strict() {
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, false).await;
    let at = authority_of(&web);
    let index = get(&at, "/").await;
    assert_eq!(index.status(), StatusCode::OK);
    assert!(
        index.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(index.body().as_ref(), reef_web::index().bytes);
    assert_eq!(index.headers()[header::CACHE_CONTROL], "no-store");
    for name in HEADERS {
        assert!(index.headers().contains_key(*name), "{name}");
    }
    let csp = index.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe"));
    let hashed = reef_web::all().iter().find(|a| a.immutable).unwrap();
    let asset = get(&at, &format!("/{}", hashed.path)).await;
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(asset.headers()[header::CONTENT_TYPE], hashed.content_type);
    assert!(
        asset.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let route = get(&at, "/repos/abc/smartlog").await;
    assert_eq!(route.status(), StatusCode::OK);
    assert_eq!(route.body().as_ref(), reef_web::index().bytes);
    assert_eq!(get(&at, "/nope.js").await.status(), StatusCode::NOT_FOUND);
    let head = send(&at, Method::HEAD, "/", &[], Vec::new()).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
    assert!(head.headers().contains_key("content-security-policy"));
    let post = send(&at, Method::POST, "/", &[], Vec::new()).await;
    assert_eq!(post.status(), StatusCode::NOT_FOUND);
    assert!(post.headers().contains_key("content-security-policy"));
}

#[tokio::test(flavor = "multi_thread")]
async fn host_must_match() {
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, false).await;
    let at = authority_of(&web);
    let port: u16 = at.rsplit(':').next().unwrap().parse().unwrap();
    let other = format!("127.0.0.1:{}", port.wrapping_add(1));
    for host in [
        format!("localhost:{port}"),
        format!("attacker.example:{port}"),
        other,
    ] {
        for path in ["/", "/reef.api.v1.DaemonService/GetDaemonStatus"] {
            let response = send(&at, Method::GET, path, &[("host", &host)], Vec::new()).await;
            assert_eq!(
                response.status(),
                StatusCode::MISDIRECTED_REQUEST,
                "{host} {path}"
            );
        }
        let response = raw_status_call(&at, &[("host", &host)]).await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{host}");
    }
    // HTTP/1.1 requires Host; hyper refuses a request without one before
    // the gate, and an HTTP/1.0 request without one is refused by it.
    let response = send(&at, Method::GET, "/", &[("host", "")], Vec::new()).await;
    assert_ne!(response.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_needs_a_session() {
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, false).await;
    let other = TestDaemon::start().await;
    let foreign = token_of(&session(&other.client, false).await);
    for token in [None, Some("x"), Some(foreign.as_str())] {
        let status = status_over_web(&web, token).await.unwrap_err();
        assert_eq!(status.code(), Code::PermissionDenied, "{token:?}");
        assert_eq!(reason(&status), "WEB_SESSION_INVALID");
    }
    // A trailers-only answer: status and details in the headers.
    let response = raw_status_call(&authority_of(&web), &[("authorization", "Bearer x")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/grpc-web+proto"
    );
    assert_eq!(response.headers()["grpc-status"], "7");
    assert!(response.headers().contains_key("grpc-status-details-bin"));
    assert!(response.body().is_empty());
    let status = status_over_web(&web, Some(&token_of(&web))).await.unwrap();
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_read_rpcs() {
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, false).await;
    let at = authority_of(&web);
    let token = token_of(&web);
    let bearer = format!("Bearer {token}");
    let mut paths: Vec<&str> = reef_api::methods().collect();
    paths.extend(["/reef.daemon.v0.Daemon/Status", "/reef.api.v1.Nope/Nope"]);
    for path in paths {
        // The head only: `SubscribeOperations` and `SubscribePresence` are
        // READ and never end.
        let response = send_head(
            &at,
            Method::POST,
            path,
            &[
                ("content-type", "application/grpc-web+proto"),
                ("authorization", &bearer),
            ],
            vec![0, 0, 0, 0, 0],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let details = response
            .headers()
            .get("grpc-status-details-bin")
            .map(|value| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(value.as_bytes())
                    .unwrap()
            });
        let detail = details.and_then(|bytes| {
            let status = Status::with_details(Code::Unknown, "", bytes.into());
            reef_daemon::error::detail_of(&status)
        });
        let read = reef_api::access(path) == Some(v1::Access::Read);
        let refused = detail
            .as_ref()
            .is_some_and(|d| d.reason == "WEB_ACCESS_DENIED");
        assert_eq!(refused, !read, "{path}");
        assert!(
            detail
                .as_ref()
                .is_none_or(|d| d.reason != "WEB_SESSION_INVALID"),
            "{path}"
        );
        if refused {
            let detail = detail.unwrap();
            assert_eq!(detail.metadata["method"], path);
            let expected = match reef_api::access(path) {
                Some(v1::Access::Write) => "write",
                Some(v1::Access::Admin) => "admin",
                _ => "unknown",
            };
            assert_eq!(detail.metadata["access"], expected, "{path}");
        }
    }
    // Through tonic: a session cannot mint sessions.
    let status = daemon_service(&web, Some(&token))
        .create_web_session(v1::CreateWebSessionRequest { launch_file: true })
        .await
        .unwrap_err();
    assert_eq!(reason(&status), "WEB_ACCESS_DENIED");
    assert_eq!(metadata(&status, "access"), "admin");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_origins_are_refused() {
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, false).await;
    let at = authority_of(&web);
    let bearer = format!("Bearer {}", token_of(&web));
    let own = format!("http://{at}");
    for (name, value) in [
        ("origin", "http://attacker.example"),
        ("origin", "null"),
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-site", "same-site"),
    ] {
        let response = raw_status_call(&at, &[(name, value), ("authorization", &bearer)]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}: {value}");
        assert!(response.body().is_empty());
    }
    let response = raw_status_call(
        &at,
        &[
            ("origin", &own),
            ("sec-fetch-site", "same-origin"),
            ("authorization", &bearer),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        trailers(response.body())
            .lines()
            .any(|line| line.trim_end() == "grpc-status:0"),
        "{:?}",
        trailers(response.body())
    );
    let options = send(
        &at,
        Method::OPTIONS,
        "/reef.api.v1.DaemonService/GetDaemonStatus",
        &[
            ("origin", "http://attacker.example"),
            ("access-control-request-method", "POST"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(options.status(), StatusCode::METHOD_NOT_ALLOWED);
    let upgrade = send(
        &at,
        Method::GET,
        "/",
        &[("upgrade", "websocket"), ("connection", "upgrade")],
        Vec::new(),
    )
    .await;
    assert_eq!(upgrade.status(), StatusCode::BAD_REQUEST);
    for content_type in [
        "application/grpc-web-text",
        "application/grpc-web-text+proto",
        "application/grpc",
        "application/json",
        "text/plain",
    ] {
        let response = send(
            &at,
            Method::POST,
            "/reef.api.v1.DaemonService/GetDaemonStatus",
            &[("content-type", content_type), ("authorization", &bearer)],
            frame(&GetDaemonStatusRequest::default()),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type}"
        );
    }
    for response in [&options, &upgrade] {
        assert!(
            !response
                .headers()
                .keys()
                .any(|name| name.as_str().starts_with("access-control-")),
        );
    }
    // Native gRPC over HTTP/2 (prior knowledge): refused as well.
    let channel = tonic::transport::Endpoint::from_shared(web.url.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut native = DaemonServiceClient::new(channel);
    let mut request = Request::new(GetDaemonStatusRequest::default());
    request
        .metadata_mut()
        .insert("authorization", bearer.parse().unwrap());
    assert!(native.get_daemon_status(request).await.is_err());
}

async fn create(client: &Client, repo: &SyntheticRepo) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "web-test".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Describes `ws`'s change; the describe's operation id. Streams are
/// matched by id: an operation's description is jj's, not the change's.
async fn describe(client: &Client, ws: &v1::Workspace, text: &str) -> String {
    client
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: text.to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap()
        .operation
        .unwrap()
        .id
}

/// The ids of the describes in `frame`'s operations, in order.
fn described(frame: &v1::SubscribeOperationsResponse) -> Vec<String> {
    frame
        .operations
        .iter()
        .filter(|op| op.kind == i32::from(v1::OperationKind::Describe))
        .map(|op| op.id.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario, in its order")]
async fn streams_over_grpc_web() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let ws = create(&daemon.client, &repo).await;
    let web = session(&daemon.client, false).await;
    let token = token_of(&web);
    let (service, origin) = web_pair(&web, &token);
    let mut ops = OperationServiceClient::with_origin(service, origin)
        .subscribe_operations(SubscribeOperationsRequest::default())
        .await
        .unwrap()
        .into_inner();
    let (service, origin) = web_pair(&web, &token);
    let mut presence = PresenceServiceClient::with_origin(service, origin)
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap()
        .into_inner();
    let first = tokio::time::timeout(SECOND, ops.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.live);
    let synced = tokio::time::timeout(SECOND, async {
        loop {
            let frame = presence.message().await.unwrap().unwrap();
            if frame.synced {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert!(synced.synced);

    // Twenty describes and twenty reports, each within 2 s.
    for n in 0..20 {
        let text = format!("web describe {n}");
        let begun = Instant::now();
        let id = describe(&daemon.client, &ws, &text).await;
        loop {
            let frame = tokio::time::timeout(2 * SECOND, ops.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if described(&frame).contains(&id) {
                break;
            }
        }
        assert!(begun.elapsed() < 2 * SECOND);
        let activity = format!("web report {n}");
        daemon
            .client
            .report_presence(ReportPresenceRequest {
                agent: "web-test".to_owned(),
                kind: PresenceKind::Activity.into(),
                activity: activity.clone(),
                ..ReportPresenceRequest::default()
            })
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(2 * SECOND, presence.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let seen = frame.events.iter().any(|event| {
                event
                    .presence
                    .as_ref()
                    .is_some_and(|p| p.activity == activity)
            });
            if seen {
                break;
            }
        }
    }

    // Left unread through 200 describes, the operation stream holds back
    // without stopping presence, then delivers all 200 in order.
    let mut ids = Vec::new();
    for n in 0..200 {
        ids.push(describe(&daemon.client, &ws, &format!("unread {n}")).await);
    }
    daemon
        .client
        .report_presence(ReportPresenceRequest {
            agent: "web-test".to_owned(),
            kind: PresenceKind::Activity.into(),
            activity: "while unread".to_owned(),
            ..ReportPresenceRequest::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(2 * SECOND, async {
        loop {
            let frame = presence.message().await.unwrap().unwrap();
            if frame.events.iter().any(|e| {
                e.presence
                    .as_ref()
                    .is_some_and(|p| p.activity == "while unread")
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(10 * SECOND, async {
        while seen.len() < ids.len() {
            let frame = ops.message().await.unwrap().unwrap();
            seen.extend(described(&frame).into_iter().filter(|id| ids.contains(id)));
        }
    })
    .await
    .unwrap();
    assert_eq!(seen, ids);

    // Shutdown ends both streams with UNAVAILABLE and no reason.
    daemon.handle.shutdown();
    for result in [
        tokio::time::timeout(2 * SECOND, async {
            loop {
                match ops.message().await {
                    Ok(Some(_)) => {}
                    Ok(None) => break None,
                    Err(status) => break Some(status),
                }
            }
        })
        .await
        .unwrap(),
        tokio::time::timeout(2 * SECOND, async {
            loop {
                match presence.message().await {
                    Ok(Some(_)) => {}
                    Ok(None) => break None,
                    Err(status) => break Some(status),
                }
            }
        })
        .await
        .unwrap(),
    ] {
        let status = result.expect("an error at shutdown");
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(reason(&status), "");
    }
}

/// A gRPC-web service with `token` and the session's origin.
fn web_pair(session: &CreateWebSessionResponse, token: &str) -> (Web, Uri) {
    web(session, Some(token))
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_files_go() {
    let daemon = TestDaemon::start_with(|config: &mut Config| {
        config.web.launch_file_ttl = Duration::from_millis(200);
    })
    .await;
    // Used: gone after the first authenticated request.
    let used = session(&daemon.client, true).await;
    assert!(Path::new(&used.launch_path).exists());
    status_over_web(&used, Some(&token_of(&used)))
        .await
        .unwrap();
    assert!(!Path::new(&used.launch_path).exists());
    // Unused: gone after its time to live.
    let unused = session(&daemon.client, true).await;
    assert!(Path::new(&unused.launch_path).exists());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!Path::new(&unused.launch_path).exists());
    // None asked for: none written.
    let none = session(&daemon.client, false).await;
    assert_eq!(none.launch_path, "");
    let ui = daemon.state_dir().join("ui");
    assert_eq!(std::fs::read_dir(&ui).unwrap().count(), 0);
    // Planted before a start: gone after it.
    let planted = ui.join("open-0123456789abcdef.html");
    std::fs::write(&planted, "stale").unwrap();
    let keep = ui.join("notes.txt");
    std::fs::write(&keep, "keep").unwrap();
    let daemon = daemon.restart().await;
    assert!(!planted.exists());
    assert!(keep.exists());
    drop(daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_die_with_the_daemon() {
    let daemon = TestDaemon::start().await;
    let before = session(&daemon.client, false).await;
    status_over_web(&before, Some(&token_of(&before)))
        .await
        .unwrap();
    let daemon = daemon.restart().await;
    let after = session(&daemon.client, false).await;
    assert_ne!(after.url, before.url);
    let stale = token_of(&before);
    let status = status_over_web(&after, Some(&stale)).await.unwrap_err();
    assert_eq!(reason(&status), "WEB_SESSION_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_cap_and_idle() {
    let daemon = TestDaemon::start_with(|config: &mut Config| {
        config.web.max_sessions = 2;
        config.web.session_idle = Duration::from_millis(300);
    })
    .await;
    let a = session(&daemon.client, false).await;
    let b = session(&daemon.client, false).await;
    status_over_web(&a, Some(&token_of(&a))).await.unwrap();
    let c = session(&daemon.client, false).await;
    // b was the least recently used.
    let refused = status_over_web(&b, Some(&token_of(&b))).await.unwrap_err();
    assert_eq!(reason(&refused), "WEB_SESSION_INVALID");
    status_over_web(&a, Some(&token_of(&a))).await.unwrap();
    status_over_web(&c, Some(&token_of(&c))).await.unwrap();
    // Idle for longer than the limit: refused.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let refused = status_over_web(&c, Some(&token_of(&c))).await.unwrap_err();
    assert_eq!(reason(&refused), "WEB_SESSION_INVALID");
    // A stream open for longer than the limit keeps its session live.
    let d = session(&daemon.client, false).await;
    let token = token_of(&d);
    let (service, origin) = web_pair(&d, &token);
    let mut stream = PresenceServiceClient::with_origin(service, origin)
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap()
        .into_inner();
    stream.message().await.unwrap();
    tokio::time::sleep(SECOND).await;
    status_over_web(&d, Some(&token)).await.unwrap();
    drop(stream);
}

#[tokio::test(flavor = "multi_thread")]
async fn bind_failure_is_reported() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = held.local_addr().unwrap().to_string();
    let listen = address.clone();
    let daemon = TestDaemon::start_with(move |config: &mut Config| {
        config.file.ui.listen = listen;
    })
    .await;
    let error = daemon.client.create_web_session(false).await.unwrap_err();
    let status = status_of(&error);
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(reason(status), "WEB_UI_UNAVAILABLE");
    assert_eq!(metadata(status, "address"), address);
    assert_ne!(metadata(status, "error"), "");
    // The socket API still answers.
    daemon.client.daemon_status(true).await.unwrap();
    drop(held);
    let web = session(&daemon.client, false).await;
    assert_eq!(web.url, format!("http://{address}/"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_loopback_serves_with_its_host() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipped: no IPv6 loopback");
        return;
    }
    let daemon = TestDaemon::start_with(|config: &mut Config| {
        config.file.ui.listen = "[::1]:0".to_owned();
    })
    .await;
    let web = session(&daemon.client, false).await;
    assert!(web.url.starts_with("http://[::1]:"), "{}", web.url);
    let at = authority_of(&web);
    assert_eq!(get(&at, "/").await.status(), StatusCode::OK);
    status_over_web(&web, Some(&token_of(&web))).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_cap() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let daemon = TestDaemon::start_with(|config: &mut Config| {
        config.web.max_connections = 2;
    })
    .await;
    let web = session(&daemon.client, false).await;
    let at = authority_of(&web);
    let request = format!("GET / HTTP/1.1\r\nhost: {at}\r\n\r\n");
    let open = |request: String| {
        let at = at.clone();
        async move {
            let mut stream = tokio::net::TcpStream::connect(&at).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut buffer = vec![0; 16];
            let read = tokio::time::timeout(SECOND, stream.read(&mut buffer)).await;
            (stream, read.ok().and_then(Result::ok).unwrap_or(0))
        }
    };
    let (first, read) = open(request.clone()).await;
    assert!(read > 0);
    let (second, read) = open(request.clone()).await;
    assert!(read > 0);
    let (_third, read) = open(request.clone()).await;
    assert_eq!(read, 0, "a third connection is closed without a response");
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_fourth, read) = open(request.clone()).await;
    assert!(read > 0);
    drop(second);
}

/// A writer the test's tracing subscriber logs into.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_never_reach_logs() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .init();
    let daemon = TestDaemon::start().await;
    let web = session(&daemon.client, true).await;
    let token = token_of(&web);
    let at = authority_of(&web);
    let bearer = format!("Bearer {token}");
    status_over_web(&web, Some(&token)).await.unwrap();
    let denied = daemon_service(&web, Some(&token))
        .create_web_session(v1::CreateWebSessionRequest::default())
        .await
        .unwrap_err();
    raw_status_call(
        &at,
        &[
            ("origin", "http://attacker.example"),
            ("authorization", &bearer),
        ],
    )
    .await;
    raw_status_call(&at, &[("authorization", "Bearer wrong")]).await;
    let (service, origin) = web_pair(&web, &token);
    let mut stream = PresenceServiceClient::with_origin(service, origin)
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap()
        .into_inner();
    stream.message().await.unwrap();
    drop(stream);
    daemon.handle.shutdown();
    let TestDaemon { handle, .. } = daemon;
    handle.join().await.unwrap();
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("web UI listening"), "the capture works");
    assert!(!logs.contains(&token), "the token is in the logs");
    assert!(!denied.message().contains(&token));
    assert!(!format!("{:?}", denied.metadata()).contains(&token));
}
