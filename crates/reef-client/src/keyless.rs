//! Sigstore keyless signing (R3.42, ADR 0066): a session key certified by
//! Fulcio against an OIDC token, whose binding statement Rekor logs, and
//! whose bundle the daemon registers and verifies offline.
//!
//! [`Client::issue_keyless_session_key`] generates an ed25519 key, gets an
//! OIDC token for audience `sigstore` from the first source in
//! [`PROVIDERS`] that applies, asks Fulcio for a certificate (proving the
//! key with a signature over the token's `email` or `sub`), signs the binding
//! statement, logs it in Rekor v1 as a `dsse` 0.0.1 entry, builds the
//! Sigstore bundle v0.3, and registers it (`RegisterSigstoreKey`). The key
//! signs evidence with `reef_evidence::Claim::sign` like any session key.
//!
//! Nothing here opens a browser or waits for a person (ADR 0025): a person's
//! token comes from a file or `SIGSTORE_ID_TOKEN`, obtained by a tool the
//! person runs. HTTPS goes through the client's own rustls on ring with the
//! OS's roots; plain HTTP only to a loopback host, for tests and local
//! deployments. The token is never logged.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use reef_api::v1::RegisterSigstoreKeyRequest;
use reef_evidence::session::Actor;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use zeroize::Zeroizing;

use crate::{Client, ClientError, SessionKey};

/// The public-good Fulcio.
pub const PUBLIC_FULCIO: &str = "https://fulcio.sigstore.dev";

/// The public-good Rekor (v1).
pub const PUBLIC_REKOR: &str = "https://rekor.sigstore.dev";

/// The OIDC audience Fulcio requires.
pub const AUDIENCE: &str = "sigstore";

/// The largest response read from Fulcio, Rekor or a token endpoint.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Where an OIDC token comes from, first match wins: the one table of
/// sources, with no special case elsewhere.
pub const PROVIDERS: &[(&str, &str)] = &[
    (
        "file",
        "--oidc-token-file PATH (or KeylessOptions::token_file)",
    ),
    (
        "environment",
        "SIGSTORE_ID_TOKEN: the token itself (GitLab's id_tokens, or any CI or script)",
    ),
    (
        "github-actions",
        "ACTIONS_ID_TOKEN_REQUEST_URL and ACTIONS_ID_TOKEN_REQUEST_TOKEN (permissions: id-token: write)",
    ),
];

/// Where to sign keylessly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeylessOptions {
    /// Fulcio's base URL.
    pub fulcio_url: String,
    /// Rekor's base URL.
    pub rekor_url: String,
    /// A file holding the OIDC token; `None` tries the environment.
    pub token_file: Option<PathBuf>,
    /// Each request's timeout.
    pub timeout: Duration,
}

impl Default for KeylessOptions {
    fn default() -> Self {
        Self {
            fulcio_url: PUBLIC_FULCIO.to_owned(),
            rekor_url: PUBLIC_REKOR.to_owned(),
            token_file: None,
            timeout: Duration::from_secs(30),
        }
    }
}

impl KeylessOptions {
    /// The defaults, with `REEF_SIGSTORE_FULCIO_URL` and
    /// `REEF_SIGSTORE_REKOR_URL` when set.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let mut options = Self::default();
        if let Some(url) = var("REEF_SIGSTORE_FULCIO_URL") {
            options.fulcio_url = url;
        }
        if let Some(url) = var("REEF_SIGSTORE_REKOR_URL") {
            options.rekor_url = url;
        }
        options
    }
}

/// Why keyless signing failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeylessError {
    /// No source of an OIDC token applies.
    #[error(
        "no OIDC token for keyless signing: give --oidc-token-file, set SIGSTORE_ID_TOKEN, or run in GitHub Actions with id-token: write"
    )]
    NoToken,
    /// The token file cannot be used.
    #[error("the OIDC token file {path}: {problem}")]
    TokenFile {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        problem: String,
    },
    /// The token is not a JWT for audience `sigstore` with a subject.
    #[error("the OIDC token is not usable: {0}")]
    Token(String),
    /// A service answered with an error, or not at all.
    #[error("{service}: {problem}")]
    Service {
        /// `fulcio`, `rekor` or the token endpoint.
        service: &'static str,
        /// What went wrong: the HTTP status and the start of the body, or
        /// the I/O error.
        problem: String,
    },
    /// No key could be generated.
    #[error(transparent)]
    Key(reef_evidence::KeyError),
    /// The daemon refused the bundle.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// An OIDC token: a credential, wiped on drop, never printed.
pub struct OidcToken(Zeroizing<String>);

impl std::fmt::Debug for OidcToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OidcToken(..)")
    }
}

impl OidcToken {
    /// `text` as a token, checked for its form: a JWT whose claims have a
    /// `sub` and whose `aud` is (or includes) `sigstore`.
    pub fn parse(text: &str) -> Result<OidcToken, KeylessError> {
        let token = OidcToken(Zeroizing::new(text.trim().to_owned()));
        token.subject()?;
        Ok(token)
    }

    fn claims(&self) -> Result<Value, KeylessError> {
        let mut parts = self.0.split('.');
        let (Some(_), Some(claims), Some(_), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(KeylessError::Token(
                "not a JWT (three dot-separated parts)".into(),
            ));
        };
        let claims = B64URL
            .decode(claims.trim_end_matches('='))
            .map_err(|_| KeylessError::Token("its claims are not base64url".into()))?;
        serde_json::from_slice(&claims)
            .map_err(|_| KeylessError::Token("its claims are not JSON".into()))
    }

    /// What Fulcio's proof of possession signs: the token's `email` when it
    /// has one (a person's login), else its `sub` (a CI job's), as Fulcio
    /// and other Sigstore clients do; checks the audience too.
    pub fn subject(&self) -> Result<String, KeylessError> {
        let claims = self.claims()?;
        audience_ok(&claims)?;
        if let Some(email) = claims["email"].as_str().filter(|email| !email.is_empty()) {
            return Ok(email.to_owned());
        }
        claims["sub"]
            .as_str()
            .filter(|sub| !sub.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| KeylessError::Token("it has no sub".into()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a token's claims name audience `sigstore`.
fn audience_ok(claims: &Value) -> Result<(), KeylessError> {
    let ok = match &claims["aud"] {
        Value::String(aud) => aud == AUDIENCE,
        Value::Array(auds) => auds.iter().any(|aud| aud == AUDIENCE),
        _ => false,
    };
    if !ok {
        return Err(KeylessError::Token(format!(
            "its audience is not {AUDIENCE:?}"
        )));
    }
    Ok(())
}

/// The GitHub Actions token endpoint's answer.
#[derive(serde::Deserialize)]
struct TokenAnswer {
    value: String,
}

/// The OIDC token from the first source in [`PROVIDERS`] that applies,
/// reading the environment through `var`.
pub async fn oidc_token(
    options: &KeylessOptions,
    var: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<OidcToken, KeylessError> {
    if let Some(path) = &options.token_file {
        return read_token_file(path);
    }
    if let Some(token) = var("SIGSTORE_ID_TOKEN")
        .map(Zeroizing::new)
        .filter(|token| !token.trim().is_empty())
    {
        return OidcToken::parse(&token);
    }
    if let (Some(url), Some(bearer)) = (
        var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
    ) {
        let separator = if url.contains('?') { '&' } else { '?' };
        let url = format!("{url}{separator}audience={AUDIENCE}");
        let bearer = Zeroizing::new(format!("bearer {bearer}"));
        let body = Zeroizing::new(
            request(
                "the GitHub Actions token endpoint",
                "GET",
                &url,
                Some(bearer.as_str()),
                None,
                options.timeout,
            )
            .await?,
        );
        let answer: TokenAnswer =
            serde_json::from_slice(&body).map_err(|_| KeylessError::Service {
                service: "the GitHub Actions token endpoint",
                problem: "its answer is not JSON with a value".into(),
            })?;
        let value = Zeroizing::new(answer.value);
        return OidcToken::parse(&value);
    }
    Err(KeylessError::NoToken)
}

fn read_token_file(path: &std::path::Path) -> Result<OidcToken, KeylessError> {
    let bad = |problem: String| KeylessError::TokenFile {
        path: path.to_owned(),
        problem,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .map_err(|error| bad(error.to_string()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(bad(format!(
                "its mode is {:o}; a token is a credential, so the file must be 0600 or tighter",
                mode & 0o777
            )));
        }
    }
    let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| bad(e.to_string()))?);
    OidcToken::parse(&text)
}

/// A URL split for a request.
struct Url<'a> {
    tls: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url<'a>(service: &'static str, url: &'a str) -> Result<Url<'a>, KeylessError> {
    let bad = |problem: &str| KeylessError::Service {
        service,
        problem: format!("{url:?}: {problem}"),
    };
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(bad("not an http(s) URL"));
    };
    if url.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err(bad("it has a space or a control character"));
    }
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| bad("an IPv6 host has no closing bracket"))?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| bad("its port is not a number"))?,
            None if after.is_empty() => default_port,
            None => return Err(bad("text after the IPv6 host")),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse().map_err(|_| bad("its port is not a number"))?,
            ),
            None => (authority, default_port),
        }
    };
    if host.is_empty() {
        return Err(bad("it has no host"));
    }
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !tls && !loopback {
        return Err(bad("plain http is allowed only to a loopback host"));
    }
    Ok(Url {
        tls,
        host,
        port,
        path,
    })
}

/// The TLS configuration for Sigstore's services: rustls on ring with the
/// OS's roots.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    let (added, _) =
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if added == 0 {
        return Err("no trusted root certificate could be read from the OS".into());
    }
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// One HTTP/1.1 request; the body of a 2xx answer.
async fn request(
    service: &'static str,
    method: &str,
    url: &str,
    authorization: Option<&str>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>, KeylessError> {
    let parsed = parse_url(service, url)?;
    let failed = |problem: String| KeylessError::Service { service, problem };
    let exchange = async {
        let stream = tokio::net::TcpStream::connect((parsed.host, parsed.port))
            .await
            .map_err(|error| failed(error.to_string()))?;
        let host_header = match (parsed.host.contains(':'), parsed.port) {
            (true, port) => format!("[{}]:{port}", parsed.host),
            (false, 443) if parsed.tls => parsed.host.to_owned(),
            (false, 80) if !parsed.tls => parsed.host.to_owned(),
            (false, port) => format!("{}:{port}", parsed.host),
        };
        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: reef/{}\r\nAccept: application/json\r\nConnection: close\r\n",
            parsed.path,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(authorization) = authorization {
            head.push_str("Authorization: ");
            head.push_str(authorization);
            head.push_str("\r\n");
        }
        if let Some(body) = body {
            let _ = write!(
                head,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        head.push_str("\r\n");
        let head = Zeroizing::new(head);
        if parsed.tls {
            let config = tls_config().map_err(failed)?;
            let name = rustls::pki_types::ServerName::try_from(parsed.host.to_owned())
                .map_err(|error| failed(error.to_string()))?;
            let stream = tokio_rustls::TlsConnector::from(config)
                .connect(name, stream)
                .await
                .map_err(|error| failed(error.to_string()))?;
            exchange_on(stream, head.as_bytes(), body).await
        } else {
            exchange_on(stream, head.as_bytes(), body).await
        }
        .map_err(|error| failed(error.to_string()))
    };
    let raw = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| failed(format!("no answer within {}s", timeout.as_secs())))??;
    let (status, answer) = parse_response(&raw).map_err(|problem| failed(problem.to_owned()))?;
    if !(200..300).contains(&status) {
        let start: String = String::from_utf8_lossy(&answer).chars().take(300).collect();
        return Err(failed(format!("HTTP {status}: {start}")));
    }
    Ok(answer)
}

async fn exchange_on<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    head: &[u8],
    body: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    stream.write_all(head).await?;
    if let Some(body) = body {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    let mut raw = Vec::new();
    let mut limited = (&mut stream).take(u64::try_from(MAX_RESPONSE_BYTES).unwrap_or(u64::MAX));
    // A TLS peer that closes without close_notify still gave its answer.
    match limited.read_to_end(&mut raw).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
        Err(error) => return Err(error),
    }
    Ok(raw)
}

/// An HTTP/1.1 answer's status and body, chunked or not.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), &'static str> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("the answer has no end of headers")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "the headers are not UTF-8")?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or("the answer has no status")?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                "content-length" => length = value.parse::<usize>().ok(),
                _ => {}
            }
        }
    }
    if chunked {
        return Ok((status, dechunk(body)?));
    }
    let body = match length {
        Some(length) if length <= body.len() => &body[..length],
        Some(_) => return Err("the answer is shorter than its Content-Length"),
        None => body,
    };
    Ok((status, body.to_vec()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    loop {
        let end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("a chunk has no size line")?;
        let size = std::str::from_utf8(&body[..end])
            .ok()
            .and_then(|line| usize::from_str_radix(line.split(';').next()?.trim(), 16).ok())
            .ok_or("a chunk's size is not hex")?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        let end = size
            .checked_add(2)
            .filter(|end| *end <= body.len())
            .ok_or("a chunk is cut short")?;
        if &body[size..end] != b"\r\n" {
            return Err("a chunk does not end with CRLF");
        }
        if out.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err("the answer is too large");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[end..];
    }
}

/// PEM of `der` under `label`.
fn pem(label: &str, der: &[u8]) -> String {
    reef_evidence::keys::pem_encode(label, der)
}

/// The DER of the first PEM block of `text`.
fn pem_der(text: &str) -> Option<Vec<u8>> {
    let body: String = text
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    B64.decode(body.trim()).ok()
}

/// Asks Fulcio to certify `key` for `token`; the leaf's DER.
async fn certify(
    options: &KeylessOptions,
    token: &OidcToken,
    key: &reef_evidence::SigningKey,
) -> Result<Vec<u8>, KeylessError> {
    let subject = token.subject()?;
    let body = Zeroizing::new(
        json!({
            "credentials": {"oidcIdentityToken": token.as_str()},
            "publicKeyRequest": {
                "publicKey": {"algorithm": "ED25519", "content": pem("PUBLIC KEY", key.public_key().spki())},
                "proofOfPossession": B64.encode(key.sign_bytes(subject.as_bytes())),
            }
        })
        .to_string(),
    );
    let url = format!(
        "{}/api/v2/signingCert",
        options.fulcio_url.trim_end_matches('/')
    );
    let answer = request(
        "fulcio",
        "POST",
        &url,
        None,
        Some(body.as_bytes()),
        options.timeout,
    )
    .await?;
    let value: Value = serde_json::from_slice(&answer).map_err(|_| KeylessError::Service {
        service: "fulcio",
        problem: "its answer is not JSON".into(),
    })?;
    let chain = value
        .get("signedCertificateEmbeddedSct")
        .or_else(|| value.get("signedCertificateDetachedSct"))
        .and_then(|signed| signed["chain"]["certificates"].as_array())
        .and_then(|certificates| certificates.first())
        .and_then(Value::as_str)
        .and_then(pem_der);
    chain.ok_or(KeylessError::Service {
        service: "fulcio",
        problem: "its answer has no certificate".into(),
    })
}

/// Logs `envelope` (its JSON), signed by `leaf`'s key, in Rekor v1 as a
/// `dsse` 0.0.1 entry; the entry, as Rekor answers.
async fn log(options: &KeylessOptions, envelope: &str, leaf: &[u8]) -> Result<Value, KeylessError> {
    let body = json!({
        "apiVersion": "0.0.1",
        "kind": "dsse",
        "spec": {"proposedContent": {
            "envelope": envelope,
            "verifiers": [B64.encode(pem("CERTIFICATE", leaf))]
        }}
    })
    .to_string();
    let url = format!(
        "{}/api/v1/log/entries",
        options.rekor_url.trim_end_matches('/')
    );
    let answer = request(
        "rekor",
        "POST",
        &url,
        None,
        Some(body.as_bytes()),
        options.timeout,
    )
    .await?;
    serde_json::from_slice(&answer).map_err(|_| KeylessError::Service {
        service: "rekor",
        problem: "its answer is not JSON".into(),
    })
}

/// A Sigstore bundle v0.3 of `envelope`, certified by `leaf` and logged as
/// `rekor` answered.
pub fn bundle(leaf: &[u8], rekor: &Value, envelope: &str) -> Result<String, String> {
    let (_, entry) = rekor
        .as_object()
        .and_then(|map| map.iter().next())
        .ok_or("Rekor's answer has no entry")?;
    let hex_b64 = |value: &Value| -> Result<String, String> {
        let text = value.as_str().ok_or("a hex field is missing")?;
        let bytes: Result<Vec<u8>, _> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(text.get(i..i + 2).unwrap_or("zz"), 16))
            .collect();
        Ok(B64.encode(bytes.map_err(|_| "a hex field is not hex")?))
    };
    let number = |value: &Value| -> Result<String, String> {
        value
            .as_u64()
            .map(|n| n.to_string())
            .ok_or_else(|| "a number field is missing".to_owned())
    };
    let proof = &entry["verification"]["inclusionProof"];
    let hashes: Result<Vec<String>, String> = proof["hashes"]
        .as_array()
        .ok_or("the inclusion proof has no hashes")?
        .iter()
        .map(hex_b64)
        .collect();
    let envelope: Value = serde_json::from_str(envelope).map_err(|e| e.to_string())?;
    Ok(json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {
            "certificate": {"rawBytes": B64.encode(leaf)},
            "tlogEntries": [{
                "logIndex": number(&entry["logIndex"])?,
                "logId": {"keyId": hex_b64(&entry["logID"])?},
                "kindVersion": {"kind": "dsse", "version": "0.0.1"},
                "integratedTime": number(&entry["integratedTime"])?,
                "inclusionPromise": {"signedEntryTimestamp": entry["verification"]["signedEntryTimestamp"]},
                "inclusionProof": {
                    "logIndex": number(&proof["logIndex"])?,
                    "rootHash": hex_b64(&proof["rootHash"])?,
                    "treeSize": number(&proof["treeSize"])?,
                    "hashes": hashes?,
                    "checkpoint": {"envelope": proof["checkpoint"]}
                },
                "canonicalizedBody": entry["body"]
            }]
        },
        "dsseEnvelope": envelope
    })
    .to_string())
}

/// A new session id, `ses_` and 16 hex digits from the system's random
/// number generator.
fn new_session_id() -> Result<String, KeylessError> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| KeylessError::Token("the system's random number generator failed".into()))?;
    Ok(bytes.iter().fold(String::from("ses_"), |mut id, b| {
        let _ = write!(id, "{b:02x}");
        id
    }))
}

impl Client {
    /// `IdentityService.RegisterSigstoreKey` (R3.42) with a bundle built by
    /// the caller.
    pub async fn register_sigstore_key(
        &self,
        bundle: Vec<u8>,
    ) -> Result<reef_api::v1::RegisterSigstoreKeyResponse, ClientError> {
        self.call(
            "RegisterSigstoreKey",
            self.identities()
                .register_sigstore_key(RegisterSigstoreKeyRequest { bundle }),
        )
        .await
    }

    /// `IdentityService.AddOidcMatch` (R3.42).
    pub async fn add_oidc_match(
        &self,
        identity: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<reef_api::v1::AddOidcMatchResponse, ClientError> {
        let request = reef_api::v1::AddOidcMatchRequest {
            identity: identity.to_owned(),
            r#match: Some(reef_api::v1::OidcMatchSpec {
                issuer: issuer.to_owned(),
                subject: subject.to_owned(),
            }),
        };
        self.call("AddOidcMatch", self.identities().add_oidc_match(request))
            .await
    }

    /// `IdentityService.RevokeOidcMatch` (R3.42).
    pub async fn revoke_oidc_match(
        &self,
        request: reef_api::v1::RevokeOidcMatchRequest,
    ) -> Result<reef_api::v1::RevokeOidcMatchResponse, ClientError> {
        self.call(
            "RevokeOidcMatch",
            self.identities().revoke_oidc_match(request),
        )
        .await
    }

    /// A keyless session key (R3.42): generated here, certified by Fulcio
    /// against an OIDC token from [`PROVIDERS`], its binding statement
    /// (`actor`, this client's agent label, and `session`, a new one when
    /// empty) logged in Rekor, and its bundle registered with the daemon.
    /// `var` reads the environment (`std::env::var` in a real process).
    pub async fn issue_keyless_session_key(
        &self,
        actor: Actor,
        session: &str,
        options: &KeylessOptions,
        var: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<SessionKey, KeylessError> {
        let session = if session.is_empty() {
            new_session_id()?
        } else {
            session.to_owned()
        };
        let token = oidc_token(options, var).await?;
        let (private_key_pem, _) =
            reef_evidence::SigningKey::generate().map_err(KeylessError::Key)?;
        let private_key_pem = Zeroizing::new(private_key_pem);
        let signing_key = reef_evidence::SigningKey::from_pkcs8_pem(&private_key_pem)
            .map_err(KeylessError::Key)?;
        let leaf = certify(options, &token, &signing_key).await?;
        drop(token);
        let payload = binding_payload(&signing_key, actor, self.agent_label().as_deref(), &session);
        let envelope = signing_key
            .sign(reef_evidence::PAYLOAD_TYPE, &payload)
            .to_bytes();
        let envelope = String::from_utf8(envelope).map_err(|_| KeylessError::Service {
            service: "rekor",
            problem: "the envelope is not UTF-8".into(),
        })?;
        let rekor = log(options, &envelope, &leaf).await?;
        let bundle = bundle(&leaf, &rekor, &envelope).map_err(|problem| KeylessError::Service {
            service: "rekor",
            problem,
        })?;
        let mut record = self
            .register_sigstore_key(bundle.into_bytes())
            .await?
            .key
            .unwrap_or_default();
        let certificate = std::mem::take(&mut record.certificate);
        Ok(SessionKey {
            signing_key,
            private_key_pem,
            certificate,
            record,
        })
    }
}

/// The binding statement's payload, as `reef_evidence::sigstore` defines it
/// (that module is behind a feature the client does not enable): the same
/// JSON, written here.
fn binding_payload(
    key: &reef_evidence::SigningKey,
    actor: Actor,
    agent: Option<&str>,
    session: &str,
) -> Vec<u8> {
    let mut predicate = serde_json::Map::new();
    predicate.insert("actor".into(), Value::String(actor.as_str().into()));
    if let Some(agent) = agent {
        predicate.insert("agent".into(), Value::String(agent.into()));
    }
    predicate.insert("session".into(), Value::String(session.into()));
    predicate.insert("presence".into(), Value::String("none".into()));
    predicate.insert("custody".into(), Value::String("client".into()));
    serde_json::to_vec(&json!({
        "_type": reef_evidence::STATEMENT_TYPE,
        "subject": [{"name": "session-key", "digest": {"sha256": key.public_key().id().as_str()}}],
        "predicateType": reef_evidence::session::SIGSTORE_BINDING_PREDICATE_TYPE,
        "predicate": predicate,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.{}",
            B64URL.encode("{}"),
            B64URL.encode(claims.to_string()),
            B64URL.encode("sig")
        )
    }

    #[test]
    fn tokens_are_checked_for_their_form() {
        // A person's login: Fulcio's proof is over the email.
        let person = jwt(&json!({"sub": "1234", "email": "alice@example.com", "aud": "sigstore"}));
        assert_eq!(
            OidcToken::parse(&person).unwrap().subject().unwrap(),
            "alice@example.com"
        );
        let good = jwt(&json!({"sub": "repo:o/r", "aud": "sigstore"}));
        assert_eq!(
            OidcToken::parse(&good).unwrap().subject().unwrap(),
            "repo:o/r"
        );
        let list = jwt(&json!({"sub": "x", "aud": ["other", "sigstore"]}));
        assert!(OidcToken::parse(&list).is_ok());
        for bad in [
            jwt(&json!({"sub": "x", "aud": "other"})),
            jwt(&json!({"aud": "sigstore"})),
            "a.b".to_owned(),
            "not a token".to_owned(),
        ] {
            assert!(OidcToken::parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            format!("{:?}", OidcToken::parse(&good).unwrap()),
            "OidcToken(..)"
        );
    }

    #[test]
    fn urls() {
        let url = parse_url("x", "https://fulcio.sigstore.dev/api/v2/signingCert").unwrap();
        assert!(url.tls && url.host == "fulcio.sigstore.dev" && url.port == 443);
        assert_eq!(url.path, "/api/v2/signingCert");
        let url = parse_url("x", "http://127.0.0.1:8080/token?a=b").unwrap();
        assert!(!url.tls && url.port == 8080 && url.path == "/token?a=b");
        let url = parse_url("x", "http://[::1]/x").unwrap();
        assert!(url.host == "::1" && url.port == 80);
        let url = parse_url("x", "http://[::1]:9/x").unwrap();
        assert!(url.host == "::1" && url.port == 9);
        assert!(parse_url("x", "http://127.0.0.1/a\r\nX: y").is_err());
        assert!(parse_url("x", "http://fulcio.example/").is_err());
        assert!(parse_url("x", "ftp://x/").is_err());
    }

    #[test]
    fn responses_plain_and_chunked() {
        let plain = b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), (201, b"{}".to_vec()));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (200, b"abcde".to_vec()));
        assert!(parse_response(b"garbage").is_err());
        let huge = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nab\r\n0\r\n\r\n";
        assert!(parse_response(huge).is_err());
        let no_crlf = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXX0\r\n\r\n";
        assert!(parse_response(no_crlf).is_err());
    }

    #[tokio::test]
    async fn the_provider_table_is_followed_in_order() {
        let token = jwt(&json!({"sub": "s", "aud": "sigstore"}));
        let options = KeylessOptions::default();
        let env = |name: &str| (name == "SIGSTORE_ID_TOKEN").then(|| token.clone());
        assert!(oidc_token(&options, &env).await.is_ok());
        let none = |_: &str| None;
        assert!(matches!(
            oidc_token(&options, &none).await,
            Err(KeylessError::NoToken)
        ));
        let dir = std::env::temp_dir().join(format!("reef-keyless-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, &token).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let loose = KeylessOptions {
                token_file: Some(path.clone()),
                ..KeylessOptions::default()
            };
            assert!(matches!(
                oidc_token(&loose, &none).await,
                Err(KeylessError::TokenFile { .. })
            ));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let file = KeylessOptions {
            token_file: Some(path.clone()),
            ..KeylessOptions::default()
        };
        // The file comes first, before the environment.
        assert!(oidc_token(&file, &env).await.is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
