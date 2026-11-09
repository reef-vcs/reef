//! Fulcio, Rekor and a GitHub Actions OIDC token endpoint over plain HTTP on
//! loopback, backed by a [`TestSigstore`], for the signing client's tests
//! (R3.42). Fulcio checks the proof of possession (an ed25519 signature over
//! the token's `sub`) and issues a leaf for the configured issuer and
//! subject; Rekor logs a `dsse` 0.0.1 entry. The token endpoint answers with
//! a chunked body, so the client's chunked decoding is exercised.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test fixture: a panic is the failure report"
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::{Value, json};

use super::{Entry, Leaf, San, TestSigstore, pem};

/// The bearer token the fake GitHub endpoint expects
/// (`ACTIONS_ID_TOKEN_REQUEST_TOKEN`).
pub const GITHUB_REQUEST_TOKEN: &str = "gh-request-token";

struct State {
    kit: TestSigstore,
    issuer: String,
    subject: San,
    lifetime: AtomicI64,
    fail_fulcio: AtomicBool,
    requests: Mutex<Vec<String>>,
}

/// A running fake; it serves until the process exits.
pub struct FakeSigstore {
    url: String,
    state: Arc<State>,
}

impl std::fmt::Debug for FakeSigstore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSigstore")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl FakeSigstore {
    /// Starts one that certifies keys for `issuer` and `subject`.
    pub fn start(issuer: &str, subject: San) -> FakeSigstore {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let url = format!("http://{}", listener.local_addr().expect("its address"));
        let state = Arc::new(State {
            kit: TestSigstore::new(),
            issuer: issuer.to_owned(),
            subject,
            lifetime: AtomicI64::new(600),
            fail_fulcio: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
        });
        let serving = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&serving);
                std::thread::spawn(move || {
                    let _ = serve(&state, stream);
                });
            }
        });
        FakeSigstore { url, state }
    }

    /// Its base URL, `http://127.0.0.1:<port>`: Fulcio's and Rekor's.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The trusted root that verifies what it issues and logs.
    pub fn trusted_root(&self) -> String {
        self.state.kit.trusted_root()
    }

    /// The local Sigstore behind it, to build bundles directly.
    pub fn kit(&self) -> &TestSigstore {
        &self.state.kit
    }

    /// Issues leaves that live `seconds` from now (and started 30 seconds
    /// ago) from now on.
    pub fn set_lifetime(&self, seconds: i64) {
        self.state.lifetime.store(seconds, Ordering::Relaxed);
    }

    /// Makes Fulcio answer 500 from now on.
    pub fn fail_fulcio(&self, fail: bool) {
        self.state.fail_fulcio.store(fail, Ordering::Relaxed);
    }

    /// An OIDC token for audience `sigstore` naming the subject.
    pub fn token(&self) -> String {
        token(&self.state)
    }

    /// The GitHub Actions variables that make the client fetch a token from
    /// this fake: `ACTIONS_ID_TOKEN_REQUEST_URL` and `_TOKEN`.
    pub fn github_env(&self) -> [(&'static str, String); 2] {
        [
            (
                "ACTIONS_ID_TOKEN_REQUEST_URL",
                format!("{}/token?api-version=2.0", self.url),
            ),
            (
                "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
                GITHUB_REQUEST_TOKEN.to_owned(),
            ),
        ]
    }

    /// Every request line served so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn token(state: &State) -> String {
    let header = B64URL.encode(r#"{"alg":"none","typ":"JWT"}"#);
    // A person's login carries an email beside an opaque `sub`; a CI job's
    // token has only its `sub`.
    let claims = match &state.subject {
        San::Email(email) => json!({
            "iss": state.issuer,
            "sub": "1104512345678901234567",
            "email": email,
            "email_verified": true,
            "aud": "sigstore",
            "exp": super::now() + 300,
        }),
        San::Uri(uri) => json!({
            "iss": state.issuer,
            "sub": uri,
            "aud": "sigstore",
            "exp": super::now() + 300,
        }),
    };
    let claims = B64URL.encode(claims.to_string());
    format!("{header}.{claims}.{}", B64URL.encode("not-signed"))
}

fn respond(stream: &mut TcpStream, code: u16, body: &str, chunked: bool) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    if chunked {
        let half = body.len() / 2;
        write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            half,
            &body[..half],
            body.len() - half,
            &body[half..]
        )
    } else {
        write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

fn serve(state: &State, mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request = line.trim().to_owned();
    let mut length = 0;
    let mut authorization = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => value.trim().clone_into(&mut authorization),
                _ => {}
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    state
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(request.clone());
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    match (method, path) {
        ("GET", "/token") => {
            let wanted = format!("bearer {GITHUB_REQUEST_TOKEN}");
            if !authorization.eq_ignore_ascii_case(&wanted) || !target.contains("audience=sigstore")
            {
                return respond(&mut stream, 401, r#"{"message":"no"}"#, false);
            }
            respond(
                &mut stream,
                200,
                &json!({"value": token(state)}).to_string(),
                true,
            )
        }
        ("POST", "/api/v2/signingCert") => {
            if state.fail_fulcio.load(Ordering::Relaxed) {
                return respond(&mut stream, 500, r#"{"code":13,"message":"down"}"#, false);
            }
            match signing_cert(state, &body) {
                Ok(answer) => respond(&mut stream, 200, &answer, false),
                Err(message) => respond(
                    &mut stream,
                    400,
                    &json!({"code": 3, "message": message}).to_string(),
                    false,
                ),
            }
        }
        ("POST", "/api/v1/log/entries") => match log_entry(state, &body) {
            Ok(answer) => respond(&mut stream, 201, &answer, false),
            Err(message) => respond(
                &mut stream,
                400,
                &json!({"code": 400, "message": message}).to_string(),
                false,
            ),
        },
        _ => respond(&mut stream, 404, r#"{"message":"not found"}"#, false),
    }
}

fn pem_der(text: &str) -> Option<Vec<u8>> {
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    B64.decode(body.trim()).ok()
}

fn signing_cert(state: &State, body: &[u8]) -> Result<String, String> {
    let request: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let token = request["credentials"]["oidcIdentityToken"]
        .as_str()
        .ok_or("no token")?;
    let claims = token.split('.').nth(1).ok_or("not a JWT")?;
    let claims: Value = serde_json::from_slice(&B64URL.decode(claims).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if claims["aud"] != "sigstore" {
        return Err("the token's audience is not sigstore".into());
    }
    // As Fulcio does: the proof is over the email when the token has one.
    let sub = claims["email"]
        .as_str()
        .or_else(|| claims["sub"].as_str())
        .ok_or("no sub")?;
    let key = &request["publicKeyRequest"];
    if key["publicKey"]["algorithm"] != "ED25519" {
        return Err("this fake certifies ED25519 keys".into());
    }
    let spki = pem_der(key["publicKey"]["content"].as_str().ok_or("no key")?)
        .ok_or("the key is not PEM")?;
    let proof = B64
        .decode(key["proofOfPossession"].as_str().ok_or("no proof")?)
        .map_err(|e| e.to_string())?;
    if spki.len() != 44 {
        return Err("not an ed25519 SPKI".into());
    }
    UnparsedPublicKey::new(&ED25519, &spki[12..])
        .verify(sub.as_bytes(), &proof)
        .map_err(|_| "the proof of possession does not verify".to_owned())?;
    let mut leaf = Leaf::new(&state.issuer, state.subject.clone());
    leaf.lifetime = state.lifetime.load(Ordering::Relaxed) + 30;
    let certificate = state.kit.certify(&spki, &leaf);
    Ok(json!({
        "signedCertificateEmbeddedSct": {
            "chain": {"certificates": [pem("CERTIFICATE", &certificate), pem("CERTIFICATE", state.kit.ca_der())]}
        }
    })
    .to_string())
}

fn log_entry(state: &State, body: &[u8]) -> Result<String, String> {
    let request: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if request["kind"] != "dsse" || request["apiVersion"] != "0.0.1" {
        return Err("this fake logs dsse 0.0.1 entries".into());
    }
    let content = &request["spec"]["proposedContent"];
    let envelope = content["envelope"].as_str().ok_or("no envelope")?;
    let verifier = B64
        .decode(content["verifiers"][0].as_str().ok_or("no verifier")?)
        .map_err(|e| e.to_string())?;
    let leaf = pem_der(std::str::from_utf8(&verifier).map_err(|e| e.to_string())?)
        .ok_or("the verifier is not PEM")?;
    Ok(state.kit.log(envelope, &leaf, Entry::default()).to_string())
}
