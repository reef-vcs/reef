//! `reef identity` (R3.03, ADR 0057): operator identities, the daemon's
//! issuing authority, and the session keys it certifies.
//!
//! An identity is the person or CI pipeline a key answers to; agents are
//! the declared actor of a key, not identities. `session issue` generates a
//! key here, has the daemon certify it, and prints it: the private key is a
//! credential, printed only as JSON (`-o json`, or on a pipe), and Reef
//! keeps it nowhere. A certificate says which identity a key is bound to,
//! never that it should be trusted: policy decides that (R3.07).
//!
//! `oidc add|revoke` (R3.42, ADR 0066) set the exact OIDC issuer and
//! subject whose Sigstore keyless certificates an identity accepts, and
//! `session issue --keyless` gets a key certified by Fulcio instead of the
//! daemon, with an OIDC token from a file, `SIGSTORE_ID_TOKEN`, or GitHub
//! Actions; nothing here opens a browser (ADR 0025).

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, CreateIdentityRequest, ListSessionKeysRequest, RevokeIdentityRequest,
    RevokeSessionKeysRequest, RotateAuthorityRequest, revoke_session_keys_request,
};
use reef_client::evidence::session::Actor;
use reef_client::json::{
    self, Authority, AuthorityRotated, Identity, OidcMatch, OidcMatchRevoked, SessionKey,
    SessionKeyIssued,
};
use reef_client::keyless::{KeylessError, KeylessOptions};
use reef_client::{IssueError, brief};
use reef_error::{Advice, ErrorKind};
use serde::Serialize;

use super::advice::AdviceCtx;
use super::out::{TextRender, TextRow, block};
use super::{CliError, Ctx};

/// Ids in text, by their first characters.
const ID_WIDTH: usize = 12;

/// `reef identity`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: IdentityCommand,
}

#[derive(Debug, clap::Subcommand)]
enum IdentityCommand {
    /// Create an operator identity: a person, or a CI pipeline. Names are never reused.
    #[command(after_help = "Example: reef identity create ci-main --kind ci")]
    Create(CreateArgs),
    /// List operator identities (with --all, revoked ones too).
    #[command(visible_alias = "ls")]
    List {
        /// Also revoked identities.
        #[arg(long)]
        all: bool,
    },
    /// Show an identity; with no name, the default one socket requests use.
    #[command(after_help = "Example: reef identity show ci-main")]
    Show {
        /// Its name or id; the default identity when left out.
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Revoke an identity: no more keys, and its keys' signatures stop counting from --since.
    #[command(after_help = "Example: reef identity revoke alice --since-created --reason left")]
    Revoke(RevokeArgs),
    /// Issue, list, show and revoke session keys.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Show or rotate the daemon's issuing authority key.
    #[command(subcommand)]
    Authority(AuthorityCommand),
    /// Add or revoke the OIDC issuers and subjects whose keyless (Sigstore) keys an identity accepts.
    #[command(subcommand)]
    Oidc(OidcCommand),
}

#[derive(Debug, clap::Subcommand)]
enum OidcCommand {
    /// Let an identity accept keyless keys Fulcio certified for this exact OIDC issuer and subject.
    #[command(
        after_help = "Example: reef identity oidc add ci-main --issuer https://token.actions.githubusercontent.com --subject https://github.com/OWNER/REPO/.github/workflows/ci.yml@refs/heads/main"
    )]
    Add {
        /// The identity's name or id.
        #[arg(value_name = "IDENTITY")]
        identity: String,
        /// The OIDC issuer, exactly as the certificate names it (an https:// URL).
        #[arg(long, value_name = "URL")]
        issuer: String,
        /// The certificate's subject, exactly: an email, or a workflow URI.
        #[arg(long, value_name = "TEXT")]
        subject: String,
    },
    /// Revoke a match: keys bound through it stop counting from --since.
    #[command(
        after_help = "Example: reef identity oidc revoke oidc_41c9e07ab2d35f18 --reason moved"
    )]
    Revoke {
        /// The match's id (`reef identity show` lists them).
        #[arg(value_name = "ID")]
        id: String,
        /// Signatures stop counting from this time (RFC 3339, or a duration ago such as 2h); now by default.
        #[arg(long, value_name = "TIME", value_parser = super::history::when)]
        since: Option<SystemTime>,
        /// From the match's creation: everything its keys signed reads revoked.
        #[arg(long, conflicts_with = "since")]
        since_created: bool,
        /// Why, recorded with the revocation.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
}

/// `reef identity create`.
#[derive(Debug, clap::Args)]
struct CreateArgs {
    /// Its name: lowercase letters, digits, '.', '_' and '-', at most 63.
    #[arg(value_name = "NAME")]
    name: String,
    /// What it is: person (a human operator) or ci (a pipeline, keyed only through a linked remote token).
    #[arg(long, value_parser = ["person", "ci"])]
    kind: String,
    /// A name to display, unverified.
    #[arg(long, value_name = "TEXT")]
    display_name: Option<String>,
    /// An email to display, checked for its form only.
    #[arg(long, value_name = "ADDR")]
    email: Option<String>,
    /// An OIDC issuer and subject whose keyless (Sigstore) keys it accepts; repeatable.
    #[arg(long, value_name = "ISSUER=SUBJECT", value_parser = oidc_spec)]
    oidc: Vec<(String, String)>,
}

/// `ISSUER=SUBJECT`, split at the first `=`.
fn oidc_spec(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .filter(|(issuer, subject)| !issuer.is_empty() && !subject.is_empty())
        .map(|(issuer, subject)| (issuer.to_owned(), subject.to_owned()))
        .ok_or_else(|| "expected ISSUER=SUBJECT".to_owned())
}

/// `reef identity revoke`.
#[derive(Debug, clap::Args)]
struct RevokeArgs {
    /// Its name or id.
    #[arg(value_name = "NAME")]
    name: String,
    /// Its keys' signatures stop counting from this time (RFC 3339, or a duration ago such as 2h); now by default.
    #[arg(long, value_name = "TIME", value_parser = super::history::when)]
    since: Option<SystemTime>,
    /// From its creation: everything its keys signed reads revoked (a compromise).
    #[arg(long, conflicts_with = "since")]
    since_created: bool,
    /// Why, recorded with the revocation.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum SessionCommand {
    /// Issue a session key; with -o json (or on a pipe) its private key, a credential, printed once.
    #[command(after_help = "Example: reef identity session issue --actor executor -o json")]
    Issue(IssueArgs),
    /// List session keys: live ones, or with --all expired and revoked ones too.
    #[command(visible_alias = "ls")]
    List {
        /// Only this identity's keys.
        #[arg(long, value_name = "NAME")]
        identity: Option<String>,
        /// Only this session's keys.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Also expired and revoked keys.
        #[arg(long)]
        all: bool,
        /// Only keys certified by this issuer: reef-daemon, or sigstore (keyless).
        #[arg(long, value_parser = ["reef-daemon", "sigstore"])]
        issuer: Option<String>,
    },
    /// Show a session key and what its certificate binds it to.
    #[command(after_help = "Example: reef identity session show 9c41")]
    Show {
        /// Its key id, or a unique prefix of at least 4 hex digits.
        #[arg(value_name = "KEY_ID")]
        key_id: String,
    },
    /// Revoke a key, or every key of a session: signatures stop counting from --since.
    #[command(
        after_help = "Example: reef identity session revoke --session ses_41c9e07ab2d35f18 --since-issued"
    )]
    Revoke(SessionRevokeArgs),
}

/// `reef identity session issue`.
#[derive(Debug, clap::Args)]
struct IssueArgs {
    /// The identity; the default one when left out (through a remote daemon, the token's).
    #[arg(long, value_name = "NAME")]
    identity: Option<String>,
    /// What the key is for: agent, executor, or human (declared, not verified). Agent by default.
    #[arg(long, value_parser = ["agent", "executor", "human"], default_value = "agent")]
    actor: String,
    /// How long it is valid, such as 30m; the daemon's `[session_keys] lifetime` by default.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    lifetime: Option<Duration>,
    /// Renew: a new key in this session, with its identity and actor.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Keyless: Sigstore's Fulcio certifies the key against an OIDC token, and its identity is whichever accepts the token's issuer and subject.
    #[arg(long, conflicts_with_all = ["identity", "lifetime"])]
    keyless: bool,
    /// With --keyless, the OIDC token's file (0600); else `SIGSTORE_ID_TOKEN` or GitHub Actions' token.
    #[arg(long, value_name = "PATH", requires = "keyless", conflicts_with_all = ["identity", "lifetime"])]
    oidc_token_file: Option<PathBuf>,
}

/// `reef identity session revoke`.
#[derive(Debug, clap::Args)]
struct SessionRevokeArgs {
    /// The key id, or a unique prefix of at least 4 hex digits.
    #[arg(value_name = "KEY_ID", conflicts_with = "session")]
    key_id: Option<String>,
    /// Every key of this session, which then cannot be renewed.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Signatures stop counting from this time (RFC 3339, or a duration ago such as 2h); now by default.
    #[arg(long, value_name = "TIME", value_parser = super::history::when)]
    since: Option<SystemTime>,
    /// From the key's issue (or the session's start): everything it signed reads revoked.
    #[arg(long, conflicts_with = "since")]
    since_issued: bool,
    /// Why, recorded with the revocation.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum AuthorityCommand {
    /// Show the daemon's authority key, PEM, to pin; then the retired ones.
    Show,
    /// Retire the authority key and make a new one; with --revoke, its certificates stop counting.
    #[command(
        after_help = "Example: reef identity authority rotate --revoke --since 2026-11-03T00:00:00Z"
    )]
    Rotate {
        /// Also revoke the retired key: everything it issued reads revoked, from its creation unless --since.
        #[arg(long)]
        revoke: bool,
        /// With --revoke: its certificates stop counting from this time.
        #[arg(long, value_name = "TIME", value_parser = super::history::when, requires = "revoke")]
        since: Option<SystemTime>,
        /// Why, recorded with the revocation.
        #[arg(long, value_name = "TEXT", requires = "revoke")]
        reason: Option<String>,
    },
    /// Revoke a retired authority key: its certificates stop counting from --since.
    #[command(
        after_help = "Example: reef identity authority revoke 3fa9 --since-created --reason restored"
    )]
    Revoke {
        /// Its key id, or a unique prefix of at least 4 hex digits.
        #[arg(value_name = "KEY_ID")]
        key_id: String,
        /// Its certificates stop counting from this time (RFC 3339, or a duration ago); now by default.
        #[arg(long, value_name = "TIME", value_parser = super::history::when)]
        since: Option<SystemTime>,
        /// From its creation: everything it issued reads revoked.
        #[arg(long, conflicts_with = "since")]
        since_created: bool,
        /// Why, recorded with the revocation.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
}

/// `at` as a protobuf timestamp.
fn timestamp(at: SystemTime) -> prost_types::Timestamp {
    prost_types::Timestamp::from(at)
}

fn short(id: &str) -> &str {
    id.get(..ID_WIDTH).unwrap_or(id)
}

/// The actor `name` stands for.
fn actor(name: &str) -> Actor {
    Actor::parse(name).unwrap_or(Actor::Agent)
}

impl Args {
    /// Whether the command calls an ADMIN method (create, revoke, session
    /// revoke, authority rotate), which a remote daemon never serves.
    pub(crate) fn is_admin(&self) -> bool {
        matches!(
            self.command,
            IdentityCommand::Create(_)
                | IdentityCommand::Revoke(_)
                | IdentityCommand::Session(SessionCommand::Revoke(_))
                | IdentityCommand::Authority(
                    AuthorityCommand::Rotate { .. } | AuthorityCommand::Revoke { .. }
                )
                | IdentityCommand::Oidc(_)
        )
    }
}

/// A session key generated here and certified by the daemon, within the
/// RPC deadline: what `session issue` prints and `evidence add` signs with.
pub(crate) async fn issue_key(
    ctx: &Ctx,
    identity: &str,
    actor: Actor,
    lifetime: Option<Duration>,
    session: &str,
) -> Result<reef_client::SessionKey, CliError> {
    let issuing = ctx
        .client
        .issue_session_key(identity, actor, lifetime, session);
    match tokio::time::timeout(ctx.timeout, issuing).await {
        Ok(Ok(issued)) => Ok(issued),
        Ok(Err(IssueError::Client(error))) => Err(CliError::Daemon(error)),
        Ok(Err(error)) => Err(CliError::Refused {
            kind: ErrorKind::Internal,
            message: error.to_string(),
            advice: Some(Advice::hint("Run it again.")),
        }),
        Err(_) => Err(CliError::Timeout { after: ctx.timeout }),
    }
}

/// A keyless session key (R3.42): certified by Fulcio against an OIDC token
/// and registered with the daemon. Fulcio and Rekor are the public-good
/// instances unless `REEF_SIGSTORE_FULCIO_URL` and `_REKOR_URL` say
/// otherwise; each request has its own 30-second timeout.
pub(crate) async fn keyless_key(
    ctx: &Ctx,
    actor: Actor,
    session: &str,
    token_file: Option<PathBuf>,
) -> Result<reef_client::SessionKey, CliError> {
    let options = KeylessOptions {
        token_file,
        ..KeylessOptions::from_env()
    };
    let var = |name: &str| std::env::var(name).ok();
    match ctx
        .client
        .issue_keyless_session_key(actor, session, &options, &var)
        .await
    {
        Ok(issued) => Ok(issued),
        Err(KeylessError::Client(error)) => Err(CliError::Daemon(error)),
        Err(
            error @ (KeylessError::NoToken
            | KeylessError::TokenFile { .. }
            | KeylessError::Token(_)),
        ) => Err(CliError::Refused {
            kind: ErrorKind::InvalidArgument,
            message: error.to_string(),
            advice: Some(Advice::hint(
                "Give --oidc-token-file PATH (mode 0600) with an OIDC token for audience sigstore, set SIGSTORE_ID_TOKEN, or run in GitHub Actions with `permissions: id-token: write`.",
            )),
        }),
        Err(error) => Err(CliError::Refused {
            kind: ErrorKind::Unavailable,
            message: error.to_string(),
            advice: Some(Advice::hint(
                "Check that Fulcio and Rekor are reachable (REEF_SIGSTORE_FULCIO_URL, REEF_SIGSTORE_REKOR_URL), then run it again.",
            )),
        }),
    }
}

/// Runs `reef identity`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let advice = AdviceCtx::unscoped();
    let refine = |error: CliError| advice.refine(error);
    match args.command {
        IdentityCommand::Create(args) => {
            let kind = if args.kind == "ci" {
                v1::IdentityKind::Ci
            } else {
                v1::IdentityKind::Person
            };
            let response = ctx
                .call(
                    ctx.client.create_identity(CreateIdentityRequest {
                        name: args.name,
                        kind: kind.into(),
                        display_name: args.display_name.unwrap_or_default(),
                        email: args.email.unwrap_or_default(),
                        oidc: args
                            .oidc
                            .into_iter()
                            .map(|(issuer, subject)| v1::OidcMatchSpec { issuer, subject })
                            .collect(),
                    }),
                )
                .await
                .map_err(refine)?;
            let identity = Identity::from(&response.identity.unwrap_or_default());
            ctx.out
                .object_with(&Shown(identity), |shown| brief::Identity::from(&shown.0))
        }
        IdentityCommand::List { all } => {
            let identities = ctx
                .call(ctx.client.list_identities_all(all))
                .await
                .map_err(refine)?;
            let listed: Vec<Identity> = identities.iter().map(Identity::from).collect();
            ctx.out.list_with(&listed, |listed| {
                listed.iter().map(brief::Identity::from).collect::<Vec<_>>()
            })
        }
        IdentityCommand::Show { name } => {
            let response = ctx
                .call(ctx.client.get_identity(name.as_deref().unwrap_or_default()))
                .await
                .map_err(refine)?;
            let identity = Identity::from(&response.identity.unwrap_or_default());
            ctx.out
                .object_with(&Shown(identity), |shown| brief::Identity::from(&shown.0))
        }
        IdentityCommand::Revoke(args) => {
            let response = ctx
                .call(ctx.client.revoke_identity(RevokeIdentityRequest {
                    name: args.name,
                    invalid_from: args.since.map(timestamp),
                    since_created: args.since_created,
                    reason: args.reason.unwrap_or_default(),
                }))
                .await
                .map_err(refine)?;
            let identity = Identity::from(&response.identity.unwrap_or_default());
            ctx.out.object_with(&RevokedIdentity(identity), |revoked| {
                brief::Identity::from(&revoked.0)
            })
        }
        IdentityCommand::Session(command) => session(ctx, command, &advice).await,
        IdentityCommand::Authority(command) => authority(ctx, command, &advice).await,
        IdentityCommand::Oidc(command) => oidc(ctx, command, &advice).await,
    }
}

async fn oidc(ctx: &Ctx, command: OidcCommand, advice: &AdviceCtx) -> Result<(), CliError> {
    let refine = |error: CliError| advice.refine(error);
    match command {
        OidcCommand::Add {
            identity,
            issuer,
            subject,
        } => {
            let response = ctx
                .call(ctx.client.add_oidc_match(&identity, &issuer, &subject))
                .await
                .map_err(refine)?;
            let shown = OidcMatch::from(&response.r#match.unwrap_or_default());
            ctx.out.object(&AddedMatch(shown))
        }
        OidcCommand::Revoke {
            id,
            since,
            since_created,
            reason,
        } => {
            let response = ctx
                .call(ctx.client.revoke_oidc_match(v1::RevokeOidcMatchRequest {
                    id,
                    invalid_from: since.map(timestamp),
                    since_created,
                    reason: reason.unwrap_or_default(),
                }))
                .await
                .map_err(refine)?;
            let shown = OidcMatchRevoked {
                matched: OidcMatch::from(&response.r#match.unwrap_or_default()),
                key_ids: response.key_ids,
            };
            ctx.out.object(&RevokedMatch(shown))
        }
    }
}

/// `identity oidc add`'s match.
#[derive(Serialize)]
#[serde(transparent)]
struct AddedMatch(OidcMatch);

impl TextRender for AddedMatch {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let m = &self.0;
        writeln!(w, "match {}: {} {}", m.id, m.issuer, m.subject)
    }
}

/// `identity oidc revoke`'s match and the keys it covers.
#[derive(Serialize)]
#[serde(transparent)]
struct RevokedMatch(OidcMatchRevoked);

impl TextRender for RevokedMatch {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let m = &self.0.matched;
        let from = m
            .revocation
            .as_ref()
            .map_or_else(|| "-".to_owned(), revocation_text);
        writeln!(
            w,
            "revoked match {} ({} {}), {from}; {} keys bound through it",
            m.id,
            m.issuer,
            m.subject,
            self.0.key_ids.len()
        )
    }
}

#[expect(clippy::too_many_lines, reason = "one arm per command")]
async fn session(ctx: &Ctx, command: SessionCommand, advice: &AdviceCtx) -> Result<(), CliError> {
    let refine = |error: CliError| advice.refine(error);
    match command {
        SessionCommand::Issue(args) => {
            // A key whose private half cannot be printed is a key wasted:
            // refused before anything is certified.
            if !ctx.out.is_json() {
                return Err(CliError::Refused {
                    kind: ErrorKind::InvalidArgument,
                    message: "`identity session issue` prints its private key only as JSON".into(),
                    advice: Some(Advice::hint(
                        "Run it with -o json (or on a pipe) to receive the key, a credential printed once.",
                    )),
                });
            }
            let issued = if args.keyless {
                keyless_key(
                    ctx,
                    actor(&args.actor),
                    args.session.as_deref().unwrap_or_default(),
                    args.oidc_token_file,
                )
                .await
            } else {
                issue_key(
                    ctx,
                    args.identity.as_deref().unwrap_or_default(),
                    actor(&args.actor),
                    args.lifetime,
                    args.session.as_deref().unwrap_or_default(),
                )
                .await
            }
            .map_err(refine)?;
            let certificate =
                serde_json::from_slice(&issued.certificate).unwrap_or(serde_json::Value::Null);
            let shown = SessionKeyIssued {
                key: SessionKey::from(&issued.record),
                private_key: issued.private_key_pem.as_str().to_owned(),
                certificate,
            };
            ctx.out.object(&Issued(shown))
        }
        SessionCommand::List {
            identity,
            session,
            all,
            issuer,
        } => {
            let issuer_kind = match issuer.as_deref() {
                Some("sigstore") => v1::IssuerKind::Sigstore,
                Some(_) => v1::IssuerKind::ReefDaemon,
                None => v1::IssuerKind::Unspecified,
            };
            let keys = ctx
                .call(ctx.client.list_session_keys_all(ListSessionKeysRequest {
                    identity: identity.unwrap_or_default(),
                    session_id: session.unwrap_or_default(),
                    include_ended: all,
                    issuer_kind: issuer_kind.into(),
                    ..ListSessionKeysRequest::default()
                }))
                .await
                .map_err(refine)?;
            let listed: Vec<SessionKey> = keys.iter().map(SessionKey::from).collect();
            ctx.out.list_with(&listed, |listed| {
                listed
                    .iter()
                    .map(brief::SessionKey::from)
                    .collect::<Vec<_>>()
            })
        }
        SessionCommand::Show { key_id } => {
            let response = ctx
                .call(ctx.client.get_session_key(&key_id))
                .await
                .map_err(refine)?;
            let key = SessionKey::from(&response.key.unwrap_or_default());
            ctx.out
                .object_with(&ShownKey(key), |shown| brief::SessionKey::from(&shown.0))
        }
        SessionCommand::Revoke(args) => {
            let target = match (args.key_id, args.session) {
                (Some(key_id), _) => revoke_session_keys_request::Target::KeyId(key_id),
                (None, Some(session)) => revoke_session_keys_request::Target::SessionId(session),
                (None, None) => {
                    return Err(CliError::Usage(
                        "name a key id, or a session with --session".into(),
                    ));
                }
            };
            let response = ctx
                .call(ctx.client.revoke_session_keys(RevokeSessionKeysRequest {
                    target: Some(target),
                    invalid_from: args.since.map(timestamp),
                    since_issued: args.since_issued,
                    reason: args.reason.unwrap_or_default(),
                }))
                .await
                .map_err(refine)?;
            let keys: Vec<SessionKey> = response.keys.iter().map(SessionKey::from).collect();
            ctx.out.object_with(&RevokedKeys(keys), |revoked| {
                revoked
                    .0
                    .iter()
                    .map(brief::SessionKey::from)
                    .collect::<Vec<_>>()
            })
        }
    }
}

async fn authority(
    ctx: &Ctx,
    command: AuthorityCommand,
    advice: &AdviceCtx,
) -> Result<(), CliError> {
    let refine = |error: CliError| advice.refine(error);
    match command {
        AuthorityCommand::Show => {
            let authorities = ctx
                .call(ctx.client.list_authorities_all())
                .await
                .map_err(refine)?;
            let shown: Vec<Authority> = authorities.iter().map(Authority::from).collect();
            ctx.out.object(&Authorities(shown))
        }
        AuthorityCommand::Rotate {
            revoke,
            since,
            reason,
        } => {
            let response = ctx
                .call(ctx.client.rotate_authority(RotateAuthorityRequest {
                    revoke,
                    invalid_from: since.map(timestamp),
                    reason: reason.unwrap_or_default(),
                }))
                .await
                .map_err(refine)?;
            let rotated = AuthorityRotated {
                retired: Authority::from(&response.retired.unwrap_or_default()),
                current: Authority::from(&response.current.unwrap_or_default()),
            };
            ctx.out.object(&Rotated(rotated))
        }
        AuthorityCommand::Revoke {
            key_id,
            since,
            since_created,
            reason,
        } => {
            let response = ctx
                .call(ctx.client.revoke_authority(v1::RevokeAuthorityRequest {
                    key_id,
                    invalid_from: since.map(timestamp),
                    since_created,
                    reason: reason.unwrap_or_default(),
                }))
                .await
                .map_err(refine)?;
            let revoked = Authority::from(&response.authority.unwrap_or_default());
            ctx.out.object(&RevokedAuthority(revoked))
        }
    }
}

/// `identity authority revoke`'s authority.
#[derive(Serialize)]
#[serde(transparent)]
struct RevokedAuthority(Authority);

impl TextRender for RevokedAuthority {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let authority = &self.0;
        let from = authority
            .revocation
            .as_ref()
            .map_or_else(|| "-".to_owned(), revocation_text);
        writeln!(w, "revoked authority {}, {from}", authority.key_id)
    }
}

/// A revocation, as text: `from <time> (<reason>)`.
fn revocation_text(revocation: &json::Revocation) -> String {
    let from = revocation.invalid_from.as_deref().unwrap_or("-");
    match &revocation.reason {
        Some(reason) => format!("from {from} ({reason})"),
        None => format!("from {from}"),
    }
}

/// `identity create`'s and `show`'s identity.
#[derive(Serialize)]
#[serde(transparent)]
struct Shown(Identity);

fn identity_pairs(identity: &Identity) -> Vec<(&'static str, String)> {
    let or_dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
    vec![
        ("name", identity.name.clone()),
        ("id", identity.id.clone()),
        ("kind", or_dash(identity.kind.as_deref())),
        (
            "default",
            if identity.is_default { "yes" } else { "no" }.to_owned(),
        ),
        ("display name", or_dash(identity.display_name.as_deref())),
        ("email", or_dash(identity.email.as_deref())),
        ("created", or_dash(identity.created_at.as_deref())),
        (
            "revoked",
            identity
                .revocation
                .as_ref()
                .map_or_else(|| "-".to_owned(), revocation_text),
        ),
        ("live keys", identity.live_session_keys.to_string()),
        (
            "remote tokens",
            if identity.remote_token_names.is_empty() {
                "-".to_owned()
            } else {
                identity.remote_token_names.join(" ")
            },
        ),
        (
            "oidc",
            if identity.oidc_matches.is_empty() {
                "-".to_owned()
            } else {
                identity
                    .oidc_matches
                    .iter()
                    .map(|m| {
                        let state = if m.revocation.is_some() {
                            " (revoked)"
                        } else {
                            ""
                        };
                        format!("{} {} {}{state}", m.id, m.issuer, m.subject)
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            },
        ),
    ]
}

impl TextRender for Shown {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        block(w, &identity_pairs(&self.0))
    }
}

impl TextRow for Identity {
    const HEADER: &'static [&'static str] = &["NAME", "KIND", "STATE", "KEYS", "ID"];

    fn row(&self) -> Vec<String> {
        let state = match (&self.revocation, self.is_default) {
            (Some(_), _) => "revoked",
            (None, true) => "live, default",
            (None, false) => "live",
        };
        vec![
            self.name.clone(),
            self.kind.clone().unwrap_or_default(),
            state.to_owned(),
            self.live_session_keys.to_string(),
            self.id.clone(),
        ]
    }
}

/// `identity revoke`'s identity, with its linked tokens to revoke.
#[derive(Serialize)]
#[serde(transparent)]
struct RevokedIdentity(Identity);

impl TextRender for RevokedIdentity {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let identity = &self.0;
        let from = identity
            .revocation
            .as_ref()
            .map_or_else(|| "-".to_owned(), revocation_text);
        writeln!(w, "revoked identity {}, {from}", identity.name)?;
        if !identity.remote_token_names.is_empty() {
            writeln!(
                w,
                "Its linked remote tokens still work for reads and writes; revoke them:"
            )?;
            for name in &identity.remote_token_names {
                writeln!(w, "  reef remote token revoke {name}")?;
            }
        }
        Ok(())
    }
}

/// A key's state at this read: `live`, `expired`, `revoked` or `invalid`.
fn key_state(key: &SessionKey) -> &'static str {
    if !key.certificate_valid {
        return "invalid";
    }
    if key.revocation.is_some() {
        return "revoked";
    }
    let expired = key
        .not_after
        .as_deref()
        .and_then(|at| humantime::parse_rfc3339(at).ok())
        .is_some_and(|at| at < SystemTime::now());
    if expired { "expired" } else { "live" }
}

impl TextRow for SessionKey {
    const HEADER: &'static [&'static str] = &[
        "KEY",
        "IDENTITY",
        "ACTOR",
        "SESSION",
        "VALID UNTIL",
        "STATE",
    ];

    fn row(&self) -> Vec<String> {
        vec![
            short(&self.key_id).to_owned(),
            self.identity.clone(),
            self.actor.clone().unwrap_or_default(),
            self.session_id.clone(),
            self.not_after.clone().unwrap_or_default(),
            key_state(self).to_owned(),
        ]
    }
}

/// `identity session show`'s key.
#[derive(Serialize)]
#[serde(transparent)]
struct ShownKey(SessionKey);

impl TextRender for ShownKey {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let k = &self.0;
        let or_dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
        block(
            w,
            &[
                ("key", k.key_id.clone()),
                (
                    "identity",
                    format!(
                        "{} ({})",
                        k.identity,
                        k.identity_kind.as_deref().unwrap_or("-")
                    ),
                ),
                ("actor", or_dash(k.actor.as_deref())),
                ("agent", or_dash(k.agent.as_deref())),
                ("session", k.session_id.clone()),
                (
                    "presence",
                    format!("{} (not verified)", k.presence.as_deref().unwrap_or("-")),
                ),
                ("custody", or_dash(k.custody.as_deref())),
                ("valid from", or_dash(k.not_before.as_deref())),
                ("valid until", or_dash(k.not_after.as_deref())),
                ("issued", or_dash(k.issued_at.as_deref())),
                ("issuer", or_dash(k.issuer_kind.as_deref())),
                ("authority", k.authority_key_id.clone()),
                (
                    "keyless",
                    k.sigstore.as_ref().map_or_else(
                        || "-".to_owned(),
                        |s| {
                            format!(
                                "{} {}, log index {}",
                                s.oidc_issuer, s.oidc_subject, s.log_index
                            )
                        },
                    ),
                ),
                (
                    "revoked",
                    k.revocation
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), revocation_text),
                ),
                (
                    "certificate",
                    if k.certificate_valid {
                        "valid"
                    } else {
                        "invalid"
                    }
                    .to_owned(),
                ),
                ("state", key_state(k).to_owned()),
            ],
        )
    }
}

/// `identity session issue`'s key: in text, never the private key.
#[derive(Serialize)]
#[serde(transparent)]
struct Issued(SessionKeyIssued);

impl TextRender for Issued {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let key = &self.0.key;
        writeln!(
            w,
            "key {} for {} as {}, session {}, valid until {}",
            short(&key.key_id),
            key.identity,
            key.actor.as_deref().unwrap_or("-"),
            key.session_id,
            key.not_after.as_deref().unwrap_or("-")
        )
    }
}

/// `identity session revoke`'s keys.
#[derive(Serialize)]
#[serde(transparent)]
struct RevokedKeys(Vec<SessionKey>);

impl TextRender for RevokedKeys {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for key in &self.0 {
            let from = key
                .revocation
                .as_ref()
                .map_or_else(|| "-".to_owned(), revocation_text);
            writeln!(
                w,
                "revoked {} ({} as {}, session {}), {from}",
                short(&key.key_id),
                key.identity,
                key.actor.as_deref().unwrap_or("-"),
                key.session_id
            )?;
        }
        Ok(())
    }
}

/// `identity authority show`'s authorities: the current one first.
#[derive(Serialize)]
#[serde(transparent)]
struct Authorities(Vec<Authority>);

impl TextRender for Authorities {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for authority in self.0.iter().filter(|a| a.current) {
            block(
                w,
                &[
                    ("key id", authority.key_id.clone()),
                    ("algorithm", authority.algorithm.clone()),
                    ("host", authority.host.clone()),
                    ("created", authority.created_at.clone().unwrap_or_default()),
                ],
            )?;
            write!(w, "{}", authority.public_key)?;
        }
        let retired: Vec<&Authority> = self.0.iter().filter(|a| !a.current).collect();
        if !retired.is_empty() {
            writeln!(w, "retired:")?;
            for authority in retired {
                let revoked = authority
                    .revocation
                    .as_ref()
                    .map(|revocation| format!(", revoked {}", revocation_text(revocation)))
                    .unwrap_or_default();
                writeln!(
                    w,
                    "  {} {} retired {}{revoked}",
                    authority.key_id,
                    authority.host,
                    authority.retired_at.as_deref().unwrap_or("-")
                )?;
            }
        }
        Ok(())
    }
}

/// `identity authority rotate`'s result.
#[derive(Serialize)]
#[serde(transparent)]
struct Rotated(AuthorityRotated);

impl TextRender for Rotated {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let rotated = &self.0;
        writeln!(w, "retired authority {}", rotated.retired.key_id)?;
        if let Some(revocation) = &rotated.retired.revocation {
            writeln!(
                w,
                "and revoked it {}: everything it issued from then reads revoked",
                revocation_text(revocation)
            )?;
        }
        writeln!(w, "current authority {}", rotated.current.key_id)?;
        write!(w, "{}", rotated.current.public_key)
    }
}
