//! `reef remote` (R2.08, ADR 0049): reaching a daemon on another machine.
//!
//! On the daemon's machine, `remote token create|list|rotate|revoke` issue
//! and end the tokens its remote listener accepts, and `remote show` says
//! what the listener presents. On the other machine, `remote add|list|
//! remove` keep profiles in `<state>/remotes.toml` (`0600`), the token in
//! the file and never on a command line, and `--remote <name>` (or
//! `REEF_REMOTE`) points any command at one.

use std::io::{self, BufRead as _, Write};
use std::path::PathBuf;
use std::time::Duration;

use reef_api::v1::RemoteScope;
use reef_client::json::{CreatedRemoteToken, RemoteListener, RemoteToken};
use reef_client::remote::{self, Profile, Profiles, RemoteTarget, RemoteUrl, Token};
use reef_client::{Client, ClientError};
use reef_error::{Advice, ErrorKind};
use serde::Serialize;

use super::out::{TextRender, TextRow, block};
use super::{CliError, Ctx, Global, Out};

/// `reef remote`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Debug, clap::Subcommand)]
enum RemoteCommand {
    /// Save a remote daemon as a profile, checking its certificate and token.
    #[command(
        after_help = "Example: reef remote add vm https://vm.example:7470 --fingerprint sha256:… --token-file - < token.txt"
    )]
    Add {
        /// The profile's name, for --remote.
        name: String,
        /// The daemon's remote listener: `https://<host>[:<port>]`.
        url: String,
        /// The daemon's key fingerprint (`reef remote show` on its machine) to pin.
        #[arg(long, value_name = "SHA256")]
        fingerprint: Option<String>,
        /// Read the token from this file, or `-` for stdin (one line).
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
        /// Save without connecting.
        #[arg(long)]
        no_check: bool,
    },
    /// List the saved remote daemons (never their tokens).
    List,
    /// Delete a saved remote daemon.
    Remove {
        /// The profile's name.
        name: String,
    },
    /// Show the remote listener: its state, address, and the fingerprint clients pin.
    Show,
    /// Issue, list, rotate and revoke the tokens this daemon's remote listener accepts.
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(Debug, clap::Subcommand)]
enum TokenCommand {
    /// Issue a token: printed once, with the line to run on the other machine.
    Create {
        /// The token's name, such as laptop or ci-runner-3.
        name: String,
        /// `read`, or `write` to also change changes and workspaces.
        #[arg(long, value_enum, default_value = "read")]
        scope: Scope,
        /// How long it lives (30d, 12h), or `never`.
        #[arg(long, value_name = "DURATION", default_value = "30d")]
        expires: String,
    },
    /// List live tokens (with --all, revoked and expired ones too).
    List {
        /// Also revoked and expired tokens.
        #[arg(long)]
        all: bool,
    },
    /// Give a token a new secret, printed once.
    Rotate {
        /// The token's name.
        name: String,
        /// How long the old secret keeps working (at most 7d).
        #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
        overlap: Option<Duration>,
    },
    /// End a token now, and the streams it holds.
    Revoke {
        /// The token's name.
        name: String,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum Scope {
    Read,
    Write,
}

impl Args {
    /// The commands that only make sense on the daemon's machine, refused
    /// with `--remote`; `None` for the others.
    pub(crate) fn local_only(&self) -> Option<&'static str> {
        match self.command {
            RemoteCommand::Token(_) => Some("remote token"),
            _ => None,
        }
    }
}

pub(crate) async fn run(global: &Global, out: &Out, args: Args) -> Result<(), CliError> {
    match args.command {
        RemoteCommand::Add {
            name,
            url,
            fingerprint,
            token_file,
            no_check,
        } => {
            add(
                global,
                out,
                &name,
                &url,
                fingerprint.as_deref(),
                &token_file,
                no_check,
            )
            .await
        }
        RemoteCommand::List => list(global, out),
        RemoteCommand::Remove { name } => remove(global, out, &name),
        RemoteCommand::Show => {
            let ctx = super::connect(global, out).await?;
            show(&ctx).await
        }
        RemoteCommand::Token(command) => {
            let ctx = super::connect(global, out).await?;
            token(&ctx, command).await
        }
    }
}

fn refused(kind: ErrorKind, message: String, advice: Option<Advice>) -> CliError {
    CliError::Refused {
        kind,
        message,
        advice,
    }
}

fn profiles(global: &Global) -> Result<Profiles, CliError> {
    Profiles::load(&global.state_dir()?).map_err(|error| {
        refused(
            ErrorKind::InvalidArgument,
            reef_error::ErrorChain(&error).to_string(),
            None,
        )
    })
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.len() <= 63
        && bytes.all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

/// The token in `path`, or on stdin for `-`: its first line.
fn read_token(path: &std::path::Path) -> Result<Token, CliError> {
    let mut line = String::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|source| CliError::Io {
                op: "read the token from",
                path: path.to_owned(),
                source,
            })?;
    } else {
        let text = std::fs::read_to_string(path).map_err(|source| CliError::Io {
            op: "read the token from",
            path: path.to_owned(),
            source,
        })?;
        text.lines()
            .next()
            .unwrap_or_default()
            .clone_into(&mut line);
    }
    let token = Token::new(&line);
    if token.expose().is_empty() {
        return Err(refused(
            ErrorKind::InvalidArgument,
            "no token was given".to_owned(),
            Some(Advice::hint(
                "Pass the one line `reef remote token create` printed on the daemon's machine, in a file or on stdin (--token-file -).",
            )),
        ));
    }
    Ok(token)
}

/// What `remote add` prints.
#[derive(Debug, Serialize)]
struct Added {
    name: String,
    url: String,
    fingerprint: Option<String>,
    checked: bool,
}

impl TextRender for Added {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "Saved {}: {}. Run commands on it with `reef --remote {} …`.",
            self.name, self.url, self.name
        )
    }
}

async fn add(
    global: &Global,
    out: &Out,
    name: &str,
    url_text: &str,
    fingerprint: Option<&str>,
    token_file: &std::path::Path,
    no_check: bool,
) -> Result<(), CliError> {
    if !valid_name(name) {
        return Err(refused(
            ErrorKind::InvalidArgument,
            format!("{name:?} is not a profile name"),
            Some(Advice::hint(
                "Use letters, digits, `.`, `_` and `-`, starting with a letter or digit.",
            )),
        ));
    }
    let Some(url) = RemoteUrl::parse(url_text) else {
        return Err(refused(
            ErrorKind::InvalidArgument,
            format!("{url_text:?} is not https://<host>[:<port>]"),
            Some(Advice::hint(format!(
                "The remote listener speaks only TLS: https://<host>:{}.",
                remote::DEFAULT_PORT
            ))),
        ));
    };
    if let Some(fingerprint) = fingerprint.filter(|f| !remote::is_fingerprint(f)) {
        return Err(refused(
            ErrorKind::InvalidArgument,
            format!("{fingerprint:?} is not a fingerprint"),
            Some(Advice::hint(
                "A fingerprint is `sha256:` and 64 lowercase hex digits, as `reef remote show` prints it.",
            )),
        ));
    }
    let token = read_token(token_file)?;
    let target = RemoteTarget {
        name: Some(name.to_owned()),
        url: url.clone(),
        fingerprint: fingerprint.map(str::to_owned),
        token: Some(token.clone()),
    };
    if !no_check {
        let timeout = global.timeout.unwrap_or(super::DEFAULT_TIMEOUT);
        let checked = tokio::time::timeout(timeout, async {
            Client::connect_remote(&target)
                .await?
                .daemon_status(true)
                .await
        })
        .await
        .map_err(|_| CliError::Timeout { after: timeout })?;
        checked.map_err(|error| add_error(error, name, url_text))?;
    }
    let mut profiles = profiles(global)?;
    profiles.insert(
        name,
        Profile {
            url: url.origin(),
            fingerprint: fingerprint.map(str::to_owned),
            token,
        },
    );
    profiles.save().map_err(|error| {
        refused(
            ErrorKind::Io,
            reef_error::ErrorChain(&error).to_string(),
            None,
        )
    })?;
    out.object(&Added {
        name: name.to_owned(),
        url: url.origin(),
        fingerprint: fingerprint.map(str::to_owned),
        checked: !no_check,
    })
}

/// `remote add`'s failure, with the line that pins what was presented.
fn add_error(error: ClientError, name: &str, url: &str) -> CliError {
    let presented = match &error {
        ClientError::RemoteTls {
            failure:
                remote::TlsFailure::PinMismatch { presented, .. }
                | remote::TlsFailure::Untrusted { presented, .. },
            ..
        } => presented.clone(),
        _ => return CliError::Daemon(error),
    };
    CliError::Advised {
        error: Box::new(error),
        advice: Advice::hint(format!(
            "The daemon at {url} presented {presented}. If `reef remote show` on that machine prints the same, run `reef remote add {name} {url} --fingerprint {presented} --token-file …` again."
        )),
    }
}

/// One profile as `remote list` prints it.
#[derive(Debug, Serialize)]
struct Listed {
    name: String,
    url: String,
    fingerprint: Option<String>,
}

impl TextRow for Listed {
    const HEADER: &'static [&'static str] = &["NAME", "URL", "FINGERPRINT"];

    fn row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.url.clone(),
            self.fingerprint
                .clone()
                .unwrap_or_else(|| "(CA)".to_owned()),
        ]
    }
}

fn list(global: &Global, out: &Out) -> Result<(), CliError> {
    let profiles = profiles(global)?;
    let listed: Vec<Listed> = profiles
        .iter()
        .map(|(name, profile)| Listed {
            name: name.clone(),
            url: profile.url.clone(),
            fingerprint: profile.fingerprint.clone(),
        })
        .collect();
    out.list(&listed)
}

fn remove(global: &Global, out: &Out, name: &str) -> Result<(), CliError> {
    let mut profiles = profiles(global)?;
    if !profiles.remove(name) {
        return Err(refused(
            ErrorKind::NotFound,
            format!("no remote named {name}"),
            Some(Advice::run(
                "`reef remote list` lists the saved ones.",
                ["reef", "remote", "list"],
            )),
        ));
    }
    profiles.save().map_err(|error| {
        refused(
            ErrorKind::Io,
            reef_error::ErrorChain(&error).to_string(),
            None,
        )
    })?;
    out.done(
        &serde_json::json!({ "removed": name }),
        &format!("removed {name}"),
    )
}

/// The remote this command line names, `--remote` else `REEF_REMOTE`: a
/// profile's name, or an `https://` URL with `REEF_REMOTE_TOKEN`.
/// `REEF_REMOTE_TOKEN` and `REEF_REMOTE_FINGERPRINT` override a profile's.
pub(crate) fn target(global: &Global, chosen: &str) -> Result<RemoteTarget, CliError> {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let token_env = env(remote::TOKEN_ENV).map(|token| Token::new(&token));
    let fingerprint_env = env(remote::FINGERPRINT_ENV);
    if chosen.starts_with("https://") {
        let url = RemoteUrl::parse(chosen).ok_or_else(|| {
            refused(
                ErrorKind::InvalidArgument,
                format!("{chosen:?} is not https://<host>[:<port>]"),
                None,
            )
        })?;
        let Some(token) = token_env else {
            return Err(refused(
                ErrorKind::InvalidArgument,
                format!(
                    "{} names a URL, and {} is not set",
                    remote::REMOTE_ENV,
                    remote::TOKEN_ENV
                ),
                Some(Advice::hint(format!(
                    "Set {} to the token, or save a profile with `reef remote add`.",
                    remote::TOKEN_ENV
                ))),
            ));
        };
        return Ok(RemoteTarget {
            name: None,
            url,
            fingerprint: fingerprint_env,
            token: Some(token),
        });
    }
    let profiles = profiles(global)?;
    let Some(profile) = profiles.get(chosen) else {
        return Err(refused(
            ErrorKind::NotFound,
            format!("no remote named {chosen}"),
            Some(Advice::run(
                "`reef remote list` lists the saved ones; `reef remote add` saves one.",
                ["reef", "remote", "list"],
            )),
        ));
    };
    let url = RemoteUrl::parse(&profile.url).ok_or_else(|| {
        refused(
            ErrorKind::InvalidArgument,
            format!("the remote {chosen} has an invalid URL {:?}", profile.url),
            None,
        )
    })?;
    Ok(RemoteTarget {
        name: Some(chosen.to_owned()),
        url,
        fingerprint: fingerprint_env.or_else(|| profile.fingerprint.clone()),
        token: Some(token_env.unwrap_or_else(|| profile.token.clone())),
    })
}

impl TextRender for RemoteListener {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("state", self.state.clone()),
            ("address", self.address.clone()),
            ("names", self.names.join(", ")),
        ];
        if let Some(cert) = &self.certificate {
            pairs.push(("fingerprint", cert.fingerprint.clone()));
            pairs.push(("certificate", cert.source.clone()));
            pairs.push(("expires", cert.not_after.clone().unwrap_or_default()));
        }
        pairs.push(("live_tokens", self.live_tokens.to_string()));
        if let Some(error) = &self.error {
            pairs.push(("error", error.clone()));
        }
        block(w, &pairs)
    }
}

async fn show(ctx: &Ctx) -> Result<(), CliError> {
    let status = ctx.call(ctx.client.daemon_status(true)).await?;
    let Some(remote) = status.remote.as_ref() else {
        return Err(refused(
            ErrorKind::Unsupported,
            format!(
                "the daemon (reef {}) has no remote listener",
                status.version
            ),
            None,
        ));
    };
    ctx.out.object(&RemoteListener::from(remote))
}

impl TextRow for RemoteToken {
    const HEADER: &'static [&'static str] = &[
        "NAME",
        "SCOPE",
        "STATE",
        "EXPIRES",
        "LAST USED",
        "FROM",
        "ID",
    ];

    fn row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.scope.clone(),
            self.state.clone(),
            self.expires_at
                .clone()
                .unwrap_or_else(|| "never".to_owned()),
            self.last_used_at.clone().unwrap_or_default(),
            self.last_used_from.clone().unwrap_or_default(),
            self.id.clone(),
        ]
    }
}

impl TextRender for CreatedRemoteToken {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.token)?;
        writeln!(w)?;
        writeln!(
            w,
            "This is token {} ({} scope); it is shown once.",
            self.token_info.name, self.token_info.scope
        )?;
        if self.token_info.expires_at.is_none() {
            writeln!(w, "It never expires: revoke it when it is no longer used.")?;
        }
        match (&self.add, &self.web_url) {
            (Some(add), web) => {
                writeln!(w, "On the other machine, with the token on stdin:")?;
                writeln!(w, "  {}", super::out::shell_line(add))?;
                if let Some(web) = web {
                    writeln!(w, "The web UI (the link holds the token): {web}")?;
                }
            }
            (None, _) => writeln!(
                w,
                "The remote listener is off: start the daemon with `reef daemon start --serve-remote`, or set `[remote] enabled = true`."
            )?,
        }
        Ok(())
    }
}

/// `token`'s create or rotate answer, with how to use it.
async fn created(
    ctx: &Ctx,
    token: &reef_api::v1::RemoteToken,
    secret: String,
) -> Result<CreatedRemoteToken, CliError> {
    let status = ctx.call(ctx.client.daemon_status(true)).await?;
    let listening = status
        .remote
        .as_ref()
        .filter(|remote| remote.state == i32::from(reef_api::v1::RemoteListenerState::Listening));
    let (url, fingerprint) = match listening {
        Some(remote) => {
            let port = remote
                .address
                .rsplit_once(':')
                .map_or(remote::DEFAULT_PORT.to_string(), |(_, port)| {
                    port.to_owned()
                });
            let host = remote.names.first().cloned().unwrap_or_default();
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host
            };
            (
                Some(format!("https://{host}:{port}")),
                remote
                    .certificate
                    .as_ref()
                    .map(|cert| cert.fingerprint.clone()),
            )
        }
        None => (None, None),
    };
    let add = url.as_ref().map(|url| {
        let host = RemoteUrl::parse(url)
            .map(|url| url.host)
            .filter(|host| valid_name(host))
            .unwrap_or_else(|| "daemon".to_owned());
        let mut argv = vec![
            "reef".to_owned(),
            "remote".to_owned(),
            "add".to_owned(),
            host,
            url.clone(),
        ];
        if let Some(fingerprint) = &fingerprint {
            argv.extend(["--fingerprint".to_owned(), fingerprint.clone()]);
        }
        argv.extend(["--token-file".to_owned(), "-".to_owned()]);
        argv
    });
    Ok(CreatedRemoteToken {
        token_info: RemoteToken::from(token),
        web_url: url.as_ref().map(|url| format!("{url}/#session={secret}")),
        token: secret,
        url,
        fingerprint,
        add,
    })
}

fn expiry(text: &str) -> Result<Option<Duration>, CliError> {
    if text == "never" {
        return Ok(None);
    }
    humantime::parse_duration(text)
        .map(Some)
        .map_err(|error| CliError::Usage(format!("--expires {text:?}: {error}")))
}

async fn token(ctx: &Ctx, command: TokenCommand) -> Result<(), CliError> {
    match command {
        TokenCommand::Create {
            name,
            scope,
            expires,
        } => {
            let scope = match scope {
                Scope::Read => RemoteScope::Read,
                Scope::Write => RemoteScope::Write,
            };
            let response = ctx
                .call(
                    ctx.client
                        .create_remote_token(&name, scope, expiry(&expires)?),
                )
                .await?;
            let token = response.token.unwrap_or_default();
            let created = created(ctx, &token, response.secret).await?;
            ctx.out.object(&created)
        }
        TokenCommand::List { all } => {
            let tokens = ctx.call(ctx.client.list_remote_tokens_all(all)).await?;
            let listed: Vec<RemoteToken> = tokens.iter().map(RemoteToken::from).collect();
            ctx.out.list(&listed)
        }
        TokenCommand::Rotate { name, overlap } => {
            let response = ctx
                .call(
                    ctx.client
                        .rotate_remote_token(&name, overlap.unwrap_or_default()),
                )
                .await?;
            let token = response.token.unwrap_or_default();
            let created = created(ctx, &token, response.secret).await?;
            ctx.out.object(&created)
        }
        TokenCommand::Revoke { name } => {
            let response = ctx.call(ctx.client.revoke_remote_token(&name)).await?;
            let token = RemoteToken::from(&response.token.unwrap_or_default());
            ctx.out.done(&token, &format!("revoked {name}"))
        }
    }
}
