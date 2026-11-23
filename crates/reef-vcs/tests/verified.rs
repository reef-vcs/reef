//! `reef evidence add --actor human --verified` and `reef identity passkey`
//! (R3.43) against a daemon in this process: the CLI asks for an approval
//! and prints its page's URL, and the test plays the person's browser,
//! approving or declining it through the web UI's gRPC-web with a software
//! passkey (`reef_evidence::webauthn::testing::Authenticator`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::process::{Output, Stdio};

use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use reef_api::v1::identity_service_client::IdentityServiceClient;
use reef_api::v1::{
    self, EditFilesRequest, FileEdit, GetApprovalRequest, PasskeyAssertion, RegisterPasskeyRequest,
    ResolveApprovalRequest, StartPasskeyRegistrationRequest, edit_files_request, file_edit,
    resolve_approval_request,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_daemon::testing::{WebConnector, web_connector};
use reef_evidence::webauthn::Algorithm;
use reef_evidence::webauthn::testing::Authenticator;
use reef_testing::synth::Preset;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tonic::body::Body;
use tonic::codegen::http::Uri;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Status};
use tonic_web::{GrpcWebClientLayer, GrpcWebClientService};
use tower::Layer as _;

mod common;

const RP: &str = "reef.localhost";

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

/// The person's browser: a web UI session at the page's origin.
struct Browser {
    url: Uri,
    token: String,
    /// `http://<run>.reef.localhost:<port>`.
    origin: String,
}

impl Browser {
    /// A browser holding the session in `session_url`.
    fn at(session_url: &str) -> Browser {
        let (url, token) = session_url.split_once("#session=").unwrap();
        let base = url
            .split_once("/approve/")
            .map_or(url, |(base, _)| base)
            .trim_end_matches('/');
        Browser {
            url: format!("{base}/").parse().unwrap(),
            token: token.to_owned(),
            origin: base.to_owned(),
        }
    }

    async fn opened(client: &Client) -> Browser {
        Browser::at(
            &client
                .create_web_session(false, "")
                .await
                .unwrap()
                .session_url,
        )
    }

    fn identities(&self) -> IdentityServiceClient<Web> {
        let client = HttpClient::builder(TokioExecutor::new()).build(web_connector());
        let service = InterceptedService::new(
            GrpcWebClientLayer::new().layer(client),
            Bearer(self.token.clone()),
        );
        IdentityServiceClient::with_origin(service, self.url.clone())
    }

    /// Registers `passkey` for the default identity.
    async fn register(&self, passkey: &mut Authenticator) -> v1::Passkey {
        let started = self
            .identities()
            .start_passkey_registration(StartPasskeyRegistrationRequest::default())
            .await
            .unwrap()
            .into_inner();
        let registered = passkey.register(RP, &self.origin, &challenge(&started.challenge));
        self.identities()
            .register_passkey(RegisterPasskeyRequest {
                challenge: started.challenge,
                label: "Test Touch ID".to_owned(),
                client_data_json: registered.client_data_json,
                authenticator_data: registered.authenticator_data,
                public_key: registered.public_key,
                algorithm: i32::try_from(registered.algorithm).unwrap(),
                approval: None,
            })
            .await
            .unwrap()
            .into_inner()
            .passkey
            .unwrap()
    }

    /// Approves `id` with `passkey`, or declines it.
    async fn resolve(&self, id: &str, passkey: Option<&mut Authenticator>) {
        let resolution = match passkey {
            Some(passkey) => {
                let approval = self
                    .identities()
                    .get_approval(GetApprovalRequest {
                        id: id.to_owned(),
                        wait: None,
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .approval
                    .unwrap();
                let asserted = passkey.assert(RP, &self.origin, &challenge(&approval.challenge));
                resolve_approval_request::Resolution::Approve(PasskeyAssertion {
                    credential_id: passkey.credential_id().to_vec(),
                    authenticator_data: asserted.authenticator_data,
                    client_data_json: asserted.client_data_json,
                    signature: asserted.signature,
                })
            }
            None => resolve_approval_request::Resolution::Decline("not mine".to_owned()),
        };
        self.identities()
            .resolve_approval(ResolveApprovalRequest {
                id: id.to_owned(),
                resolution: Some(resolution),
            })
            .await
            .unwrap();
    }
}

fn challenge(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().unwrap()
}

/// A new commit on `root()` outside any workspace.
async fn commit(client: &Client, repo: &str) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn("root()".to_owned())),
            edits: vec![FileEdit {
                path: "a.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"a\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo(Preset::Small).await
}

/// `reef --no-autostart` against `daemon`, with a clean environment.
fn reef(daemon: &TestDaemon) -> tokio::process::Command {
    common::daemon_bin();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_reef"));
    for (var, _) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("REEF_") {
            command.env_remove(var);
        }
    }
    for var in [
        "AI_AGENT",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "RUST_LOG",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", daemon.state_dir())
        .arg("--no-autostart");
    command
}

/// `reef evidence add --verified` signing off `change`: the approval page's
/// URL it printed, and the running command.
async fn sign_off(
    daemon: &TestDaemon,
    repo: &str,
    change: &v1::Change,
    extra: &[&str],
) -> (String, Vec<String>, tokio::process::Child) {
    let mut args = vec![
        "evidence",
        "add",
        "--kind",
        "signoff",
        "--result",
        "pass",
        "--actor",
        "human",
        "--verified",
        "--print-url",
        "--repo",
        repo,
        "-r",
        &change.commit_id,
        "-o",
        "json",
    ];
    args.extend_from_slice(extra);
    let mut child = reef(daemon)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        let url = line.contains("#session=").then(|| line.clone());
        seen.push(line);
        if let Some(url) = url {
            // The waiting line follows the URL.
            seen.push(lines.next_line().await.unwrap().unwrap_or_default());
            let args = args.iter().map(|arg| (*arg).to_owned()).collect();
            tokio::spawn(async move { while lines.next_line().await.unwrap().is_some() {} });
            return (url, args, child);
        }
    }
    panic!("no approval URL on stderr: {seen:?}");
}

/// The command's exit, with stdout checked against `reef schema`.
async fn finished(args: &[String], mut child: tokio::process::Child) -> Output {
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .await
        .unwrap();
    let status = child.wait().await.unwrap();
    let output = Output {
        status,
        stdout,
        stderr: Vec::new(),
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    common::schema::check(&args, &output);
    output
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The approval id in an approval page's URL.
fn approval_id(url: &str) -> &str {
    url.split_once("/approve/")
        .unwrap()
        .1
        .split_once('#')
        .unwrap()
        .0
}

// The brief's reef-vcs test: a verified sign-off approved through the web
// UI is added with presence `user_verified`; a declined one and one still
// pending at `--timeout` exit 5 with their reasons; `passkey list` prints
// the `[passkeys]` line that pins the passkey.
#[tokio::test(flavor = "multi_thread")]
async fn verified_sign_offs() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let mut passkey = Authenticator::new(Algorithm::Es256);
    let registered = Browser::opened(client).await.register(&mut passkey).await;
    let change = commit(client, &repo.id).await;

    let (url, args, child) = sign_off(&daemon, &repo.id, &change, &[]).await;
    assert!(url.contains(".reef.localhost:"), "{url}");
    Browser::at(&url)
        .resolve(approval_id(&url), Some(&mut passkey))
        .await;
    let output = finished(&args, child).await;
    assert!(output.status.success(), "{output:?}");
    let added = json(&output);
    let key = &added["evidence"]["identity"]["key"];
    assert_eq!(key["presence"], "user_verified", "{added}");
    assert_eq!(
        key["passkey"]["fingerprint"],
        registered.fingerprint.as_str()
    );
    assert_eq!(key["approves"]["change_id"], change.change_id.as_str());

    let (url, args, child) = sign_off(&daemon, &repo.id, &change, &[]).await;
    Browser::at(&url).resolve(approval_id(&url), None).await;
    let output = finished(&args, child).await;
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert_eq!(json(&output)["error"]["reason"], "APPROVAL_DECLINED");

    let (_, args, child) = sign_off(&daemon, &repo.id, &change, &["--timeout", "1s"]).await;
    let output = finished(&args, child).await;
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let error = &json(&output)["error"];
    assert_eq!(error["reason"], "APPROVAL_PENDING");
    assert!(error.get("next").is_none_or(Value::is_null), "{error}");

    let output = reef(&daemon)
        .args(["identity", "passkey", "list", "-o", "text"])
        .output()
        .await
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    // A test daemon's port is the kernel's, which no policy can pin (#346).
    assert!(
        text.contains(&registered.identity) && text.contains("[ui] listen"),
        "{text}"
    );
    daemon.stop().await;
}
