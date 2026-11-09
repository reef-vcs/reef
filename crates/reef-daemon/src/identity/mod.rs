//! Operator identities and session keys (R3.03, ADR 0057).
//!
//! An operator identity is the person or CI pipeline a key answers to:
//! `person` or `ci`, machine-wide, named once and forever. Agents are not
//! identities; they are the declared actor of a key issued for one. The
//! daemon holds one current authority key ([`authority`]), an ed25519 key
//! in `<state>/identity/authority.pem` that is never backed up, and
//! certifies short-lived session keys with it ([`issue`]): the caller
//! generates the key, proves it holds it, and gets back a certificate in
//! `reef-evidence`'s format. Every evidence read reports the identity its
//! key is bound to, and whether the key was valid and unrevoked when the
//! daemon first saw the evidence ([`read`]). GC sweeps expired keys no
//! evidence names ([`retention`]). Every issuance and revocation is logged
//! under `reef_daemon::identity::audit` ([`audit`]).
//!
//! On one uid a certificate proves only that the daemon's user, or
//! something running as that user, asked for it (ADR 0027, S-58). What an
//! identity does distinguish is other local users, remote callers (whose
//! linked token decides the identity), and, after R3.43, a present human.

pub mod audit;
pub mod authority;
pub mod issue;
pub mod read;
pub mod retention;
pub mod sigstore;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use rand::RngExt as _;
use reef_evidence::session::{IdentityKind, is_name};
use reef_lib::error::{Classify, ErrorKind};

use crate::config::{Config, SessionKeysSection};
use crate::db::{Db, DbError, IdentityRow, META_DEFAULT_IDENTITY_ID};
use crate::error::{Advice, Advise};
use crate::model::Timestamp;

pub use authority::{AuthorityError, Current};
pub use issue::Refusal;

/// The most unexpired session keys one identity may hold.
pub const MAX_SESSION_KEYS: usize = 10_000;

/// The shortest life a session key may be issued for: what `reef evidence
/// add` asks for, so `[session_keys] max_lifetime` can never refuse it.
pub const MIN_LIFETIME: Duration = Duration::from_mins(5);

/// The name of the default identity when the login name gives none.
const FALLBACK_NAME: &str = "operator";

/// The longest display name, in bytes.
const MAX_DISPLAY_NAME_BYTES: usize = 128;

/// The longest email, in bytes.
const MAX_EMAIL_BYTES: usize = 254;

/// Why an identity request, or the start of the identity state, failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IdentityError {
    /// A name that is not `[a-z0-9][a-z0-9._-]{0,62}`:
    /// `INVALID_IDENTITY_NAME`.
    #[error("{name:?} is not a valid identity name")]
    InvalidName {
        /// The name.
        name: String,
    },
    /// Another field of a new identity: `INVALID_IDENTITY`.
    #[error("{field}: {message}")]
    InvalidIdentity {
        /// `kind`, `display_name` or `email`.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// An identity already has the name: `IDENTITY_EXISTS`.
    #[error("{} identity is already named {name}", if *revoked { "a revoked" } else { "an" })]
    Exists {
        /// The name.
        name: String,
        /// The name belongs to a revoked identity.
        revoked: bool,
    },
    /// No identity has the name or id: `IDENTITY_NOT_FOUND`.
    #[error("{}", if name.is_empty() { "this daemon has no default identity".to_owned() } else { format!("no identity is named {name}") })]
    NotFound {
        /// The name or id; empty for the default.
        name: String,
    },
    /// The identity is revoked: `IDENTITY_REVOKED`.
    #[error("identity {name} was revoked at {}", revoked_at.to_rfc3339())]
    Revoked {
        /// Its name.
        name: String,
        /// When.
        revoked_at: Timestamp,
    },
    /// A session key request out of its bounds:
    /// `INVALID_SESSION_KEY_REQUEST`.
    #[error("{field}: {message}")]
    InvalidRequest {
        /// `public_key`, `proof`, `actor`, `lifetime` or `session_id`.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// The issuance rules refuse the key: `SESSION_KEY_REFUSED`.
    #[error("{}", rule.message())]
    Refused {
        /// Which rule.
        rule: Refusal,
    },
    /// The key is certified already, or signed evidence as a bare key:
    /// `SESSION_KEY_EXISTS`.
    #[error("key {key_id} {}", if *state == "used" { "has already signed evidence as a bare key" } else { "is already certified" })]
    KeyExists {
        /// The key id.
        key_id: String,
        /// `certified` or `used`.
        state: &'static str,
    },
    /// The identity holds its limit of unexpired keys:
    /// `TOO_MANY_SESSION_KEYS`.
    #[error("identity {identity} holds {limit} unexpired session keys, the limit")]
    TooMany {
        /// The identity's name.
        identity: String,
        /// The limit.
        limit: usize,
    },
    /// No session key has the id or prefix, or no session the id:
    /// `SESSION_KEY_NOT_FOUND`.
    #[error("no {} is {id}", if *field == "session_id" { "session" } else { "session key" })]
    KeyNotFound {
        /// `key_id` or `session_id`.
        field: &'static str,
        /// The id.
        id: String,
    },
    /// A key id prefix of several keys: `AMBIGUOUS_SESSION_KEY_ID`.
    #[error("key id {key_id:?} is ambiguous; it matches {}", candidates.join(", "))]
    AmbiguousKey {
        /// The prefix.
        key_id: String,
        /// Up to ten of the ids it matches.
        candidates: Vec<String>,
    },
    /// A key id that is not 4 to 64 hex digits.
    #[error("key id {key_id:?} must be 4 to 64 hex digits")]
    InvalidKeyId {
        /// The text.
        key_id: String,
    },
    /// A revocation that cannot be: `INVALID_REVOCATION`.
    #[error("{message}")]
    InvalidRevocation {
        /// `invalid_from` or `target`.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// A request for the default identity when there is none usable:
    /// `DEFAULT_IDENTITY_UNAVAILABLE`.
    #[error("{}", if configured.is_empty() { format!("this daemon has no default identity: {problem}") } else { format!("[session_keys] default_identity {configured:?} {problem}") })]
    NoDefault {
        /// `[session_keys] default_identity`; empty when not set.
        configured: String,
        /// Why there is no default.
        problem: &'static str,
    },
    /// A revocation of the identity socket requests use by default:
    /// `IDENTITY_IS_DEFAULT`.
    #[error("identity {name} is the default identity; it cannot be revoked while it is")]
    IsDefault {
        /// Its name.
        name: String,
    },
    /// A key id that names no authority: `AUTHORITY_NOT_FOUND`.
    #[error("no authority key is {key_id}")]
    AuthorityNotFound {
        /// The id or prefix.
        key_id: String,
    },
    /// A key id prefix of several authorities: `AMBIGUOUS_AUTHORITY_ID`.
    #[error("authority key id {key_id:?} is ambiguous; it matches {}", candidates.join(", "))]
    AmbiguousAuthority {
        /// The prefix.
        key_id: String,
        /// The ids it matches.
        candidates: Vec<String>,
    },
    /// A revocation of the current authority, which would go on issuing:
    /// `AUTHORITY_IS_CURRENT`.
    #[error("authority {key_id} is the current one; rotate it to revoke it")]
    AuthorityIsCurrent {
        /// Its key id.
        key_id: String,
    },
    /// A proof of possession that does not verify for the request, for the
    /// current authority: `INVALID_SESSION_KEY_REQUEST` with `field`
    /// `proof` and the current `authority_key_id`.
    #[error(
        "the proof does not verify with key {key_id} for this identity, actor and session, addressed to authority {authority_key_id}"
    )]
    InvalidProof {
        /// The session key's id.
        key_id: String,
        /// The current authority's key id, which a proof must name.
        authority_key_id: String,
    },
    /// A keyless bundle that does not verify: `INVALID_SIGSTORE_BUNDLE`
    /// (R3.42).
    #[error("{0}")]
    InvalidBundle(reef_evidence::sigstore::BundleError),
    /// A keyless certificate no live OIDC match accepts:
    /// `SIGSTORE_IDENTITY_UNMATCHED`.
    #[error("no identity accepts OIDC issuer {issuer} and subject {subject}")]
    Unmatched {
        /// The certificate's OIDC issuer.
        issuer: String,
        /// Its subject.
        subject: String,
    },
    /// Keyless keys are off, or the trusted root is unusable:
    /// `SIGSTORE_UNAVAILABLE`.
    #[error("{}", if *problem == "disabled" { "keyless keys are off ([sigstore] enabled = false)".to_owned() } else { format!("the Sigstore trusted root is not usable: {detail}") })]
    SigstoreUnavailable {
        /// `disabled` or `trusted_root`.
        problem: &'static str,
        /// What is wrong with the root.
        detail: String,
    },
    /// An OIDC match out of its bounds: `INVALID_OIDC_MATCH`.
    #[error("{field}: {message}")]
    InvalidMatch {
        /// `issuer` or `subject`.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// A live match has the issuer and subject: `OIDC_MATCH_EXISTS`.
    #[error("identity {identity} already accepts this OIDC issuer and subject")]
    MatchExists {
        /// The identity holding it, by name.
        identity: String,
    },
    /// No match has the id: `OIDC_MATCH_NOT_FOUND`.
    #[error("no OIDC match is {id}")]
    MatchNotFound {
        /// The id.
        id: String,
    },
    /// The authority key could not be loaded, made or written.
    #[error(transparent)]
    Authority(#[from] AuthorityError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A task on the blocking pool did not finish.
    #[error("an identity task failed: {0}")]
    Task(String),
}

impl Classify for IdentityError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidName { .. }
            | Self::InvalidIdentity { .. }
            | Self::InvalidRequest { .. }
            | Self::AmbiguousKey { .. }
            | Self::InvalidKeyId { .. }
            | Self::InvalidRevocation { .. }
            | Self::InvalidProof { .. }
            | Self::AmbiguousAuthority { .. }
            | Self::InvalidBundle(_)
            | Self::InvalidMatch { .. } => ErrorKind::InvalidArgument,
            Self::NoDefault { .. }
            | Self::IsDefault { .. }
            | Self::AuthorityIsCurrent { .. }
            | Self::Revoked { .. }
            | Self::TooMany { .. }
            | Self::Unmatched { .. }
            | Self::SigstoreUnavailable { .. } => ErrorKind::Conflict,
            Self::Exists { .. } | Self::KeyExists { .. } | Self::MatchExists { .. } => {
                ErrorKind::AlreadyExists
            }
            Self::NotFound { .. }
            | Self::KeyNotFound { .. }
            | Self::AuthorityNotFound { .. }
            | Self::MatchNotFound { .. } => ErrorKind::NotFound,
            Self::Refused { .. } => ErrorKind::PermissionDenied,
            Self::Authority(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for IdentityError {
    fn advice(&self) -> Option<Advice> {
        Some(match self {
            Self::InvalidName { .. } => Advice::hint(
                "Identity names are lowercase letters, digits, '.', '_' and '-', starting with a letter or digit, at most 63.",
            ),
            Self::InvalidIdentity { field, .. } => Advice::hint(match *field {
                "kind" => "Give the identity a kind: person (a human operator) or ci (a pipeline).",
                "email" => "Give an email as local@domain, at most 254 bytes, or none.",
                _ => "A display name is at most 128 bytes, with no control characters.",
            }),
            Self::Exists { revoked: true, .. } => Advice::hint(
                "Identity names are never reused, so a policy that names one cannot be satisfied by a newcomer: pick another name.",
            ),
            Self::Exists { .. } => Advice::run(
                "Pick another name; `reef identity list --all` shows the names taken.",
                ["reef", "identity", "list", "--all"],
            ),
            Self::NotFound { name } if name.is_empty() => Advice::hint(
                "Name an identity with --identity, or set [session_keys] default_identity to a person identity (`reef identity list`).",
            ),
            Self::NotFound { .. } => Advice::run(
                "List the identities to find its name.",
                ["reef", "identity", "list"],
            ),
            Self::Revoked { .. } => Advice::hint(
                "A revoked identity gets no keys and links no tokens: create another identity, under a new name.",
            ),
            Self::InvalidRequest { field, .. } => Advice::hint(match *field {
                "lifetime" => {
                    "Ask for a lifetime from 5 minutes to [session_keys] max_lifetime (`reef config show`)."
                }
                "proof" => {
                    "Sign the proof's pre-authentication encoding with the session key, over the identity, actor and session exactly as sent; docs/identity.md gives the bytes."
                }
                "actor" => "Declare what the key is for: human, agent or executor.",
                "session_id" => {
                    "Continue a session by the id a key of it was issued with (`reef identity session list`), or leave it empty for a new one."
                }
                _ => "Send the DER SubjectPublicKeyInfo of a fresh ed25519 key, 44 bytes.",
            }),
            Self::Refused { rule } => Advice::hint(rule.hint()),
            Self::KeyExists { .. } => Advice::hint(
                "A session key is always fresh: generate a new key for each certificate.",
            ),
            Self::TooMany { .. } => Advice::hint(
                "Revoke the identity's unused sessions (`reef identity session revoke --session <id>`), or wait for keys to expire.",
            ),
            Self::KeyNotFound { .. } => Advice::run(
                "List the session keys, ended ones included, to find its id.",
                ["reef", "identity", "session", "list", "--all"],
            ),
            Self::InvalidKeyId { .. } | Self::AmbiguousKey { .. } => {
                Advice::hint("Give more of the key id: `reef identity session list` prints them.")
            }
            Self::InvalidRevocation { .. } => Advice::hint(
                "Give an invalid_from at or before now, or one of --since and --since-issued (--since-created for an identity), not both.",
            ),
            Self::NoDefault { .. } => Advice::hint(
                "Name an identity (--identity), or set [session_keys] default_identity to a live person identity and restart the daemon (`reef identity list`).",
            ),
            Self::IsDefault { .. } => Advice::hint(
                "Set [session_keys] default_identity to another live person identity and restart the daemon first, then revoke this one.",
            ),
            Self::AuthorityNotFound { .. } | Self::AmbiguousAuthority { .. } => Advice::run(
                "Name an authority by its key id or a unique prefix of at least 4 hex digits.",
                ["reef", "identity", "authority", "show"],
            ),
            Self::AuthorityIsCurrent { .. } => Advice::hint(
                "Rotate the current authority with `reef identity authority rotate --revoke`, which revokes it as it retires it.",
            ),
            Self::InvalidProof { .. } => Advice::hint(
                "Sign the proof's pre-authentication encoding over the current authority's key id (ListAuthorities) and the identity, actor and session exactly as sent; docs/identity.md gives the bytes. A rotation in between changes the authority: fetch it and retry.",
            ),
            Self::InvalidBundle(error) => Advice::hint(error.step.hint()),
            Self::Unmatched { issuer, subject } => Advice::hint(format!(
                "On the daemon's machine, let an identity accept this certificate: reef identity oidc add <identity> --issuer {issuer} --subject {subject}"
            )),
            Self::SigstoreUnavailable { problem, .. } => Advice::hint(if *problem == "disabled" {
                "Set [sigstore] enabled = true in the daemon's config.toml and restart it to register keyless keys."
            } else {
                "Fix [sigstore] trusted_root (an absolute path to a trusted_root.json), or empty it for the root Reef ships, and restart the daemon."
            }),
            Self::InvalidMatch { field, .. } => Advice::hint(if *field == "issuer" {
                "Give the OIDC issuer exactly as the certificate names it: an https:// URL, such as https://token.actions.githubusercontent.com."
            } else {
                "Give the subject exactly as the certificate's SAN: an email, or a workflow URI such as https://github.com/OWNER/REPO/.github/workflows/WF.yml@refs/heads/main."
            }),
            Self::MatchExists { .. } => Advice::hint(
                "A certificate binds to one identity: revoke the other identity's match first (`reef identity show <identity>` lists its matches).",
            ),
            Self::MatchNotFound { .. } => Advice::hint(
                "`reef identity show <identity>` lists an identity's OIDC matches with their ids.",
            ),
            Self::Authority(error) => return error.advice(),
            Self::Db(error) => return error.advice(),
            Self::Task(_) => return None,
        })
    }
}

impl IdentityError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    #[expect(clippy::too_many_lines, reason = "one arm per reason")]
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::InvalidName { name } => {
                (reason::INVALID_IDENTITY_NAME, vec![("name", name.clone())])
            }
            Self::InvalidIdentity { field, .. } => (
                reason::INVALID_IDENTITY,
                vec![("field", (*field).to_owned())],
            ),
            Self::Exists { name, revoked } => (
                reason::IDENTITY_EXISTS,
                vec![("name", name.clone()), ("revoked", revoked.to_string())],
            ),
            Self::NotFound { name } => (reason::IDENTITY_NOT_FOUND, vec![("name", name.clone())]),
            Self::Revoked { name, revoked_at } => (
                reason::IDENTITY_REVOKED,
                vec![
                    ("name", name.clone()),
                    ("revoked_at", revoked_at.to_rfc3339()),
                ],
            ),
            Self::InvalidRequest { field, .. } => (
                reason::INVALID_SESSION_KEY_REQUEST,
                vec![("field", (*field).to_owned())],
            ),
            Self::Refused { rule } => (
                reason::SESSION_KEY_REFUSED,
                vec![("rule", rule.as_str().to_owned())],
            ),
            Self::KeyExists { key_id, state } => (
                reason::SESSION_KEY_EXISTS,
                vec![("key_id", key_id.clone()), ("state", (*state).to_owned())],
            ),
            Self::TooMany { identity, limit } => (
                reason::TOO_MANY_SESSION_KEYS,
                vec![("identity", identity.clone()), ("limit", limit.to_string())],
            ),
            Self::KeyNotFound { field, id } => {
                (reason::SESSION_KEY_NOT_FOUND, vec![(*field, id.clone())])
            }
            Self::AmbiguousKey { key_id, candidates } => (
                reason::AMBIGUOUS_SESSION_KEY_ID,
                vec![
                    ("key_id", key_id.clone()),
                    ("key_ids", candidates.join(",")),
                ],
            ),
            Self::InvalidRevocation { field, .. } => (
                reason::INVALID_REVOCATION,
                vec![("field", (*field).to_owned())],
            ),
            Self::Authority(AuthorityError::Unavailable { path, .. }) => (
                reason::AUTHORITY_UNAVAILABLE,
                vec![("path", path.display().to_string())],
            ),
            Self::NoDefault {
                configured,
                problem,
            } => (
                reason::DEFAULT_IDENTITY_UNAVAILABLE,
                vec![
                    ("configured", configured.clone()),
                    ("problem", (*problem).to_owned()),
                ],
            ),
            Self::IsDefault { name } => (reason::IDENTITY_IS_DEFAULT, vec![("name", name.clone())]),
            Self::AuthorityNotFound { key_id } => (
                reason::AUTHORITY_NOT_FOUND,
                vec![("key_id", key_id.clone())],
            ),
            Self::AmbiguousAuthority { key_id, candidates } => (
                reason::AMBIGUOUS_AUTHORITY_ID,
                vec![
                    ("key_id", key_id.clone()),
                    ("key_ids", candidates.join(",")),
                ],
            ),
            Self::AuthorityIsCurrent { key_id } => (
                reason::AUTHORITY_IS_CURRENT,
                vec![("key_id", key_id.clone())],
            ),
            Self::InvalidProof {
                key_id,
                authority_key_id,
            } => (
                reason::INVALID_SESSION_KEY_REQUEST,
                vec![
                    ("field", "proof".to_owned()),
                    ("key_id", key_id.clone()),
                    ("authority_key_id", authority_key_id.clone()),
                ],
            ),
            Self::InvalidBundle(error) => (
                reason::INVALID_SIGSTORE_BUNDLE,
                vec![
                    ("step", error.step.as_str().to_owned()),
                    ("problem", error.problem.clone()),
                ],
            ),
            Self::Unmatched { issuer, subject } => (
                reason::SIGSTORE_IDENTITY_UNMATCHED,
                vec![("issuer", issuer.clone()), ("subject", subject.clone())],
            ),
            Self::SigstoreUnavailable { problem, .. } => (
                reason::SIGSTORE_UNAVAILABLE,
                vec![("problem", (*problem).to_owned())],
            ),
            Self::InvalidMatch { field, .. } => (
                reason::INVALID_OIDC_MATCH,
                vec![("field", (*field).to_owned())],
            ),
            Self::MatchExists { identity } => (
                reason::OIDC_MATCH_EXISTS,
                vec![("identity", identity.clone())],
            ),
            Self::MatchNotFound { id } => (reason::OIDC_MATCH_NOT_FOUND, vec![("id", id.clone())]),
            Self::InvalidKeyId { .. } | Self::Authority(_) | Self::Db(_) | Self::Task(_) => {
                return None;
            }
        })
    }
}

/// The daemon's identity state, in `AppState`: the current authority key,
/// the default identity, and the issuance settings.
pub struct Identities {
    db: Arc<Db>,
    /// `<state>/identity`.
    dir: PathBuf,
    /// This machine's hostname, for new authorities.
    host: String,
    /// The current authority key; issuance holds it read only for the
    /// signature, and rotation swaps it.
    authority: Arc<RwLock<Current>>,
    /// The default identity's id: `[session_keys] default_identity`'s, else
    /// the one created at the first start.
    default_id: Option<String>,
    /// Why there is no default, when there is none.
    default_problem: &'static str,
    /// `[session_keys]`.
    settings: SessionKeysSection,
    /// [`MAX_SESSION_KEYS`], lowered by tests.
    limit: AtomicUsize,
    /// `[sigstore]` and its trusted root (R3.42).
    sigstore: sigstore::Keyless,
}

impl std::fmt::Debug for Identities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identities")
            .field("dir", &self.dir)
            .field("default_id", &self.default_id)
            .finish_non_exhaustive()
    }
}

/// The default identity's name for the login `user`: lowercased, every
/// character outside `[a-z0-9._-]` replaced by `-`, at most 63; `operator`
/// when that is empty or does not match the name pattern.
pub fn default_name(user: &str) -> String {
    let name: String = user
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || ".-_".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    if is_name(&name) {
        name
    } else {
        FALLBACK_NAME.to_owned()
    }
}

/// A new id: `prefix` and 8 bytes from the thread's CSPRNG, in hex.
pub(crate) fn new_id(prefix: &str) -> String {
    use std::fmt::Write as _;
    let bytes: [u8; 8] = rand::rng().random();
    bytes.iter().fold(String::from(prefix), |mut id, b| {
        let _ = write!(id, "{b:02x}");
        id
    })
}

/// Why `display_name` is not one, if it is not.
pub(crate) fn display_name_problem(display_name: &str) -> Option<String> {
    if display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Some(format!(
            "{} bytes; a display name is at most {MAX_DISPLAY_NAME_BYTES}",
            display_name.len()
        ));
    }
    display_name
        .chars()
        .any(char::is_control)
        .then(|| "a display name has no control characters".to_owned())
}

/// Why `email` is not one, if it is not: `local@domain`, one `@`, no
/// whitespace or control characters, at most 254 bytes. Its form only.
pub(crate) fn email_problem(email: &str) -> Option<String> {
    let bad = email.len() > MAX_EMAIL_BYTES
        || email.chars().any(|c| c.is_whitespace() || c.is_control())
        || !matches!(email.split_once('@'), Some((local, domain)) if !local.is_empty() && !domain.is_empty() && !domain.contains('@'));
    bad.then(|| format!("{email:?} is not local@domain of at most {MAX_EMAIL_BYTES} bytes"))
}

impl Identities {
    /// The identity state at start, after the migrations and before the
    /// socket serves: the default identity created if there is no identity
    /// at all, `[session_keys] default_identity` resolved, and the authority
    /// key loaded, checked, or made. Blocking.
    pub fn start(config: &Config, db: Arc<Db>) -> Result<Identities, IdentityError> {
        let dir = config.paths.identity.clone();
        let host = config.operation.hostname.clone();
        let (default_id, default_problem) = default_identity(&db, &config.file.session_keys)?;
        let current = authority::load(&dir, &db, &host)?;
        Ok(Identities {
            db,
            dir,
            host,
            authority: Arc::new(RwLock::new(current)),
            default_id,
            default_problem,
            settings: config.file.session_keys.clone(),
            limit: AtomicUsize::new(MAX_SESSION_KEYS),
            sigstore: sigstore::Keyless::load(&config.file.sigstore),
        })
    }

    /// The default identity's id, if any.
    pub fn default_id(&self) -> Option<&str> {
        self.default_id.as_deref()
    }

    /// The current authority's row.
    pub fn current_authority(&self) -> crate::db::AuthorityRow {
        self.authority
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .row
            .clone()
    }

    /// `<state>/identity`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The most unexpired keys one identity may hold.
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Lowers [`MAX_SESSION_KEYS`], for the test of the limit.
    #[doc(hidden)]
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// `[session_keys]`.
    pub fn settings(&self) -> &SessionKeysSection {
        &self.settings
    }

    /// The identity named or with the id `text`; the default for an empty
    /// one.
    pub fn find(&self, text: &str) -> Result<IdentityRow, IdentityError> {
        let text = text.trim();
        let wanted = if text.is_empty() {
            self.default_id.as_deref().unwrap_or_default()
        } else {
            text
        };
        if wanted.is_empty() {
            return Err(IdentityError::NoDefault {
                configured: self.settings.default_identity.trim().to_owned(),
                problem: self.default_problem,
            });
        }
        self.db
            .identity(wanted)?
            .ok_or_else(|| IdentityError::NotFound {
                name: text.to_owned(),
            })
    }
}

/// Resolves the default identity at start: creates one `person` identity,
/// named after the login user, when the database has none, and records its
/// id in `meta`; then `[session_keys] default_identity`, when set, names
/// it. A configured name that is not a live person identity is logged, and
/// the daemon runs with no default: requests for it are refused
/// (`DEFAULT_IDENTITY_UNAVAILABLE`) until the configuration is fixed. The
/// problem is returned beside the id.
fn default_identity(
    db: &Db,
    settings: &SessionKeysSection,
) -> Result<(Option<String>, &'static str), IdentityError> {
    if db.identities()?.is_empty() {
        let name = default_name(&crate::config::os_username());
        let row = IdentityRow {
            id: unused_identity_id(db)?,
            name: name.clone(),
            kind: IdentityKind::Person.as_str().to_owned(),
            display_name: None,
            email: None,
            created_at: Timestamp::now(),
            revocation: None,
        };
        if db.insert_identity(&row)?.is_none() {
            db.set_meta(META_DEFAULT_IDENTITY_ID, &row.id)?;
            tracing::info!(
                identity_id = %row.id,
                identity = %name,
                "created the default operator identity, named after the login user"
            );
            audit::identity_changed("create", &row.id, &row.name, &row.kind);
        }
    }
    let configured = settings.default_identity.trim();
    if !configured.is_empty() {
        let problem = match db.identity(configured)? {
            None => "names no identity",
            Some(row) if row.kind != IdentityKind::Person.as_str() => {
                "names a ci identity; the default is a person"
            }
            Some(row) if row.revocation.is_some() => "names a revoked identity",
            Some(row) => return Ok((Some(row.id), "")),
        };
        tracing::warn!(
            default_identity = configured,
            problem,
            "[session_keys] default_identity is not a live person identity; requests that name no identity are refused until it is fixed"
        );
        return Ok((None, problem));
    }
    let recorded = db.meta(META_DEFAULT_IDENTITY_ID)?;
    match recorded {
        Some(id) if db.identity(&id)?.is_some() => Ok((Some(id), "")),
        _ => {
            tracing::warn!(
                "no default operator identity is recorded; socket requests must name one, or set [session_keys] default_identity"
            );
            Ok((None, "none is recorded"))
        }
    }
}

/// A new identity id no identity has.
pub(crate) fn unused_identity_id(db: &Db) -> Result<String, DbError> {
    loop {
        let id = new_id("idn_");
        if !db.identity_id_taken(&id)? {
            return Ok(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names_sanitize() {
        assert_eq!(default_name("Alice"), "alice");
        assert_eq!(default_name("first last"), "first-last");
        assert_eq!(default_name("josé"), "jos-");
        assert_eq!(default_name(""), "operator");
        assert_eq!(default_name("-x"), "operator");
        assert_eq!(default_name(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn identity_fields_are_checked() {
        assert!(email_problem("a@b.example").is_none());
        for bad in ["a", "@b", "a@", "a b@c", "a@b@c"] {
            assert!(email_problem(bad).is_some(), "{bad}");
        }
        assert!(display_name_problem("Alice Example").is_none());
        assert!(display_name_problem("a\nb").is_some());
        assert!(display_name_problem(&"x".repeat(129)).is_some());
        assert!(new_id("ses_").starts_with("ses_") && new_id("ses_").len() == 20);
        assert!(reef_evidence::session::is_session_id(&new_id("ses_")));
    }

    #[test]
    fn agent_bounds_agree_with_the_api() {
        assert_eq!(
            reef_evidence::session::MAX_AGENT_BYTES,
            reef_api::AGENT_MAX_BYTES
        );
    }
}
