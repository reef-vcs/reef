//! Reaching a daemon's remote listener (R2.08, ADR 0049): TLS with a
//! pinned key or the OS's roots, a bearer token, and the profiles that keep
//! the token off every command line.
//!
//! A connection verifies the daemon before it sends anything, so the token
//! never reaches a daemon the client does not trust. With a fingerprint (a
//! profile's, or `REEF_REMOTE_FINGERPRINT`), the leaf's public key must
//! hash to it; name, chain and validity are not checked, the pin being
//! stronger than all three. Without one, the OS's roots and the URL's host
//! name decide, as for any HTTPS client. Either way the handshake
//! signatures are verified. A failure names the fingerprint the daemon
//! presented, so a person can compare it with `reef remote show` on the
//! daemon's machine.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};

/// The environment variable naming a remote: a profile's name, or an
/// `https://` URL with [`TOKEN_ENV`] (R2.08).
pub const REMOTE_ENV: &str = "REEF_REMOTE";
/// The environment variable holding a remote's token; it overrides a
/// profile's.
pub const TOKEN_ENV: &str = "REEF_REMOTE_TOKEN";
/// The environment variable holding a remote's pinned fingerprint.
pub const FINGERPRINT_ENV: &str = "REEF_REMOTE_FINGERPRINT";
/// The profiles' file name in the client's state directory.
pub const PROFILES_FILE: &str = "remotes.toml";
/// The remote listener's default port.
pub const DEFAULT_PORT: u16 = 7470;

/// `sha256:` and the lowercase hex SHA-256 of `leaf`'s `SubjectPublicKeyInfo`
/// (DER): what clients pin, and what the daemon reports. `None` for a
/// certificate that does not parse.
pub fn fingerprint(leaf: &[u8]) -> Option<String> {
    let der = CertificateDer::from(leaf);
    let cert = webpki::EndEntityCert::try_from(&der).ok()?;
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        cert.subject_public_key_info().as_ref(),
    );
    let hex = digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Some(format!("sha256:{hex}"))
}

/// Whether `text` is a fingerprint: `sha256:` and 64 lowercase hex digits.
pub fn is_fingerprint(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A bearer token, kept out of `Debug` output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// `text`, trimmed.
    pub fn new(text: &str) -> Token {
        Token(text.trim().to_owned())
    }

    /// The token itself.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// An `https://` URL's host and port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteUrl {
    /// The host: a DNS name, or an IP address without brackets.
    pub host: String,
    /// The port; 443 when the URL names none.
    pub port: u16,
}

impl RemoteUrl {
    /// `text` if it is `https://<host>[:<port>][/]`.
    pub fn parse(text: &str) -> Option<RemoteUrl> {
        let rest = text.strip_prefix("https://")?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
            return None;
        }
        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, after) = bracketed.split_once(']')?;
            host.parse::<std::net::Ipv6Addr>().ok()?;
            let port = match after {
                "" => 443,
                port => port.strip_prefix(':')?.parse().ok()?,
            };
            (host.to_owned(), port)
        } else {
            match rest.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), port.parse().ok()?),
                None => (rest.to_owned(), 443),
            }
        };
        // An IPv6 host must be bracketed.
        if host.contains(':') && !text.contains('[') {
            return None;
        }
        (!host.is_empty() && port != 0).then_some(RemoteUrl { host, port })
    }

    /// `<host>:<port>`, bracketing an IPv6 host: what the request's
    /// authority says.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// `https://<authority>`.
    pub fn origin(&self) -> String {
        format!("https://{}", self.authority())
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin())
    }
}

/// A remote daemon to connect to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    /// The profile's name; `None` for a URL from the environment.
    pub name: Option<String>,
    /// Where it listens.
    pub url: RemoteUrl,
    /// The pinned fingerprint; `None` trusts the OS's roots.
    pub fingerprint: Option<String>,
    /// The bearer token; `None` only for a connection that sends no
    /// request (a probe for the fingerprint).
    pub token: Option<Token>,
}

impl RemoteTarget {
    /// How errors name it: the profile's name, else the URL.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.url.origin())
    }
}

/// Why the TLS handshake did not verify the daemon.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TlsFailure {
    /// The daemon's key is not the pinned one.
    #[error("the daemon presented {presented}, not the pinned {expected}")]
    PinMismatch {
        /// The pin.
        expected: String,
        /// What the daemon presented.
        presented: String,
    },
    /// No pin, and the OS's roots do not validate the daemon's certificate
    /// for the URL's host.
    #[error("the daemon's certificate ({presented}) is not trusted: {reason}")]
    Untrusted {
        /// What the daemon presented.
        presented: String,
        /// Why the roots refused it.
        reason: String,
    },
    /// The handshake failed some other way.
    #[error("the TLS handshake failed: {reason}")]
    Handshake {
        /// Why.
        reason: String,
    },
}

/// What a verifier saw, for the error after a failed handshake.
#[derive(Debug, Default)]
struct Seen {
    failure: Option<TlsFailure>,
}

/// Verifies the daemon by its pin, or by the OS's roots, recording why it
/// refused.
#[derive(Debug)]
struct Verifier {
    pin: Option<String>,
    roots: Option<Arc<WebPkiServerVerifier>>,
    algorithms: WebPkiSupportedAlgorithms,
    seen: Arc<Mutex<Seen>>,
}

impl Verifier {
    fn refuse(&self, failure: TlsFailure) -> rustls::Error {
        let error = rustls::Error::General(failure.to_string());
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failure = Some(failure);
        error
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented =
            fingerprint(end_entity).unwrap_or_else(|| "an unparsable certificate".to_owned());
        if let Some(pin) = &self.pin {
            return if *pin == presented {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(self.refuse(TlsFailure::PinMismatch {
                    expected: pin.clone(),
                    presented,
                }))
            };
        }
        let Some(roots) = &self.roots else {
            return Err(self.refuse(TlsFailure::Untrusted {
                presented,
                reason: "the OS's trusted roots could not be read".to_owned(),
            }));
        };
        roots
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .map_err(|error| {
                self.refuse(TlsFailure::Untrusted {
                    presented,
                    reason: error.to_string(),
                })
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The OS's trusted roots; `None` when none could be read.
fn native_roots(provider: &Arc<CryptoProvider>) -> Option<Arc<WebPkiServerVerifier>> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return None;
    }
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(provider))
        .build()
        .ok()
}

/// A TLS client config for `pin`, and where its verifier records a
/// refusal.
fn client_config(pin: Option<&str>) -> Result<(ClientConfig, Arc<Mutex<Seen>>), TlsFailure> {
    let provider = provider();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let verifier = Verifier {
        pin: pin.map(str::to_owned),
        roots: if pin.is_none() {
            native_roots(&provider)
        } else {
            None
        },
        algorithms: provider.signature_verification_algorithms,
        seen: Arc::clone(&seen),
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|error| TlsFailure::Handshake {
            reason: error.to_string(),
        })?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok((config, seen))
}

/// A verified TLS connection to `url`, or why it failed. `Err(Ok(_))` is
/// a refusal of the daemon's certificate; `Err(Err(_))` an I/O failure
/// before or during the handshake.
pub(crate) async fn tls_connect(
    url: &RemoteUrl,
    pin: Option<&str>,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, Result<TlsFailure, io::Error>> {
    let (config, seen) = client_config(pin).map_err(Ok)?;
    let server_name = ServerName::try_from(url.host.clone())
        .map_err(|error| Err(io::Error::new(io::ErrorKind::InvalidInput, error)))?;
    let tcp = tokio::net::TcpStream::connect((url.host.as_str(), url.port))
        .await
        .map_err(Err)?;
    tcp.set_nodelay(true).ok();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    match connector.connect(server_name, tcp).await {
        Ok(stream) => Ok(stream),
        Err(error) => {
            let failure = seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .failure
                .take();
            Err(match failure {
                Some(failure) => Ok(failure),
                None if error.kind() == io::ErrorKind::InvalidData => Ok(TlsFailure::Handshake {
                    reason: error.to_string(),
                }),
                None => Err(error),
            })
        }
    }
}

/// The fingerprint the daemon at `url` presents, read with a handshake
/// that sends nothing after it.
pub async fn presented_fingerprint(url: &RemoteUrl) -> Result<String, io::Error> {
    // A pin that matches nothing: the verifier records what was presented.
    let impossible = format!("sha256:{}", "0".repeat(64));
    match tls_connect(url, Some(&impossible)).await {
        Err(Ok(TlsFailure::PinMismatch { presented, .. })) => Ok(presented),
        Err(Ok(failure)) => Err(io::Error::other(failure)),
        Err(Err(error)) => Err(error),
        Ok(_) => Err(io::Error::other("the daemon presented the impossible pin")),
    }
}

/// One profile in `remotes.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `https://<host>[:<port>]`.
    pub url: String,
    /// The pinned fingerprint, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The bearer token.
    pub token: Token,
}

/// Failure to read or write the profiles.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// The file could not be read or written.
    #[error("{}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid.
    #[error("{} is not a valid profiles file", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The error.
        #[source]
        source: Box<toml::de::Error>,
    },
}

/// The profiles: `<client state>/remotes.toml`, `0600`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profiles {
    path: PathBuf,
    profiles: BTreeMap<String, Profile>,
}

impl Profiles {
    /// The profiles in `state_dir`; none when the file is absent.
    pub fn load(state_dir: &Path) -> Result<Profiles, ProfileError> {
        let path = state_dir.join(PROFILES_FILE);
        let profiles = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ProfileError::Parse {
                path: path.clone(),
                source: Box::new(source),
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(source) => return Err(ProfileError::Io { path, source }),
        };
        Ok(Profiles { path, profiles })
    }

    /// The profile named `name`.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Every profile, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Profile)> {
        self.profiles.iter()
    }

    /// Adds or replaces `name`.
    pub fn insert(&mut self, name: &str, profile: Profile) {
        self.profiles.insert(name.to_owned(), profile);
    }

    /// Removes `name`; whether it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.profiles.remove(name).is_some()
    }

    /// Writes the file, `0600`, through a temporary file beside it; the
    /// directory is created `0700` if missing.
    pub fn save(&self) -> Result<(), ProfileError> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let io = |source| ProfileError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(io)?;
        }
        let text = toml::to_string(&self.profiles).map_err(|error| io(io::Error::other(error)))?;
        let tmp = self.path.with_extension("toml.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let url = RemoteUrl::parse("https://vm.example:7470/").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("vm.example", 7470));
        assert_eq!(url.origin(), "https://vm.example:7470");
        assert_eq!(RemoteUrl::parse("https://vm.example").unwrap().port, 443);
        let v6 = RemoteUrl::parse("https://[::1]:9").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 9));
        assert_eq!(v6.authority(), "[::1]:9");
        for bad in [
            "http://vm:1",
            "https://",
            "https://vm:x",
            "https://vm:0",
            "https://vm/path",
            "https://u@vm",
            "https://::1",
            "vm:7470",
        ] {
            assert_eq!(RemoteUrl::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn fingerprints() {
        assert!(is_fingerprint(&format!("sha256:{}", "a1".repeat(32))));
        assert!(!is_fingerprint(&format!("sha256:{}", "A1".repeat(32))));
        assert!(!is_fingerprint("sha256:abc"));
        assert_eq!(fingerprint(b"not a certificate"), None);
    }

    #[test]
    fn tokens_stay_out_of_debug() {
        let token = Token::new(" reef_0011_secret \n");
        assert_eq!(token.expose(), "reef_0011_secret");
        let target = RemoteTarget {
            name: Some("vm".to_owned()),
            url: RemoteUrl::parse("https://vm:7470").unwrap(),
            fingerprint: None,
            token: Some(token.clone()),
        };
        assert!(!format!("{target:?}").contains("secret"));
        let profile = Profile {
            url: "https://vm:7470".to_owned(),
            fingerprint: None,
            token,
        };
        assert!(!format!("{profile:?}").contains("secret"));
    }

    #[derive(Debug)]
    struct Always(Arc<rustls::sign::CertifiedKey>);

    impl rustls::server::ResolvesServerCert for Always {
        fn resolve(
            &self,
            _: rustls::server::ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }
    }

    /// A TLS server on loopback presenting `cert`, signing with `key`; its
    /// address.
    async fn server(cert: CertificateDer<'static>, key: &rcgen::KeyPair) -> std::net::SocketAddr {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        let provider = provider();
        let signer = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                key.serialize_der(),
            )))
            .unwrap();
        // No `keys_match`: this may be a certificate without its key.
        let certified = rustls::sign::CertifiedKey::new(vec![cert], signer);
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Always(Arc::new(certified))));
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let _ = acceptor.accept(tcp).await;
            }
        });
        addr
    }

    // R2.08: the pin verifier checks the handshake signature, so a server
    // presenting the pinned certificate without its key is refused.
    #[tokio::test]
    async fn a_pinned_certificate_needs_its_key() {
        let alg = &rcgen::PKCS_ECDSA_P256_SHA256;
        let (real, other) = (
            rcgen::KeyPair::generate_for(alg).unwrap(),
            rcgen::KeyPair::generate_for(alg).unwrap(),
        );
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .self_signed(&real)
            .unwrap();
        let der = cert.der().clone();
        let pin = fingerprint(&der).unwrap();
        let url = |addr: std::net::SocketAddr| RemoteUrl {
            host: "127.0.0.1".to_owned(),
            port: addr.port(),
        };
        let honest = server(der.clone(), &real).await;
        assert!(tls_connect(&url(honest), Some(&pin)).await.is_ok());
        let impostor = server(der, &other).await;
        let refused = tls_connect(&url(impostor), Some(&pin)).await;
        assert!(refused.is_err(), "the impostor's handshake was accepted");
    }

    #[test]
    fn profiles_round_trip_privately() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let mut profiles = Profiles::load(&state).unwrap();
        assert_eq!(profiles.iter().count(), 0);
        profiles.insert(
            "vm",
            Profile {
                url: "https://vm:7470".to_owned(),
                fingerprint: Some(format!("sha256:{}", "ab".repeat(32))),
                token: Token::new("reef_x"),
            },
        );
        profiles.save().unwrap();
        let mode = std::fs::metadata(profiles.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let again = Profiles::load(&state).unwrap();
        assert_eq!(again, profiles);
        let mut again = again;
        assert!(again.remove("vm"));
        assert!(!again.remove("vm"));
    }
}
