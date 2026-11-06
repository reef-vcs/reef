//! `reef evidence add`, `list`, `show` and `key generate` (R3.01, ADR
//! 0055): signed claims about a tree, such as a test run's result.
//!
//! `add` signs locally: it reads the revision (snapshotting `@` first),
//! builds the claim about its tree, signs it with the caller's ed25519 key
//! file, and sends the envelope with the commit id it read, so the link
//! cannot name another tree than the one signed. `add --envelope` sends an
//! envelope signed elsewhere. `key generate` writes a key pair and talks to
//! no daemon. `show` prints `signature: valid` or `invalid` and exits 0
//! either way: valid means the signature checks out, never that the key is
//! trusted.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use reef_api::v1::{AddEvidenceRequest, GetChangeRequest, ListEvidenceRequest};
use reef_client::brief;
use reef_client::evidence::{self as format, Artifact, Claim, Outcome, PublicKey, SigningKey};
use reef_client::json::{self, Evidence, EvidenceAdded, EvidenceKey};
use reef_error::{Advice, ErrorKind};
use serde::Serialize;
use serde_json::{Map, Value};

use super::advice::AdviceCtx;
use super::out::{Out, TextRender, block};
use super::render::{now_ms, span};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// Ids and key ids in text, by their first characters.
const ID_WIDTH: usize = 12;
/// `list`'s default `--limit`.
const LIST_LIMIT: usize = 100;
/// A change id in hex.
const CHANGE_ID_LEN: usize = 32;

/// `reef evidence`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: EvidenceCommand,
}

#[derive(Debug, clap::Subcommand)]
enum EvidenceCommand {
    /// Sign a claim about a revision's tree and store it.
    #[command(
        after_help = "Example: reef evidence add --kind test --result pass --key ci.pem --artifact test.log"
    )]
    Add(AddArgs),
    /// List evidence: of the repository, on a revision's tree, or linked to a change.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef evidence list -r @ --kind test"
    )]
    List(ListArgs),
    /// Show one evidence object, its statement, and whether its signature verifies.
    #[command(after_help = "Example: reef evidence show 3f9a --envelope | shasum -a 256")]
    Show(ShowArgs),
    /// Make signing keys.
    #[command(subcommand)]
    Key(KeyCommand),
}

#[derive(Debug, clap::Subcommand)]
enum KeyCommand {
    /// Write a new ed25519 key to PATH (PKCS#8, mode 0600) and PATH.pub; a local key until session keys.
    #[command(after_help = "Example: reef evidence key generate ~/.config/reef/ci.pem")]
    Generate {
        /// Where to write the private key; the public key goes to PATH.pub. Never overwritten.
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
}

/// `reef evidence add`.
#[derive(Debug, clap::Args)]
struct AddArgs {
    /// What was checked: test, build, lint, policy, or another lowercase kind.
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,
    /// How it came out: pass, fail, or error (no verdict).
    #[arg(long, value_name = "RESULT", value_parser = ["pass", "fail", "error"])]
    result: Option<String>,
    /// The ed25519 private key to sign with, PKCS#8 PEM, readable only by you.
    #[arg(long, value_name = "PEM")]
    key: Option<PathBuf>,
    /// The change the evidence is for, whose tree it is about [default: the workspace's change].
    #[arg(short = 'r', long, value_name = "REV")]
    revision: Option<String>,
    /// The test or build target covered [default: the whole check].
    #[arg(long, value_name = "TARGET")]
    target: Option<String>,
    /// When the check started: RFC 3339, or a duration ago such as 5m [default: now].
    #[arg(long, value_name = "TIME")]
    started: Option<String>,
    /// When it finished [default: now].
    #[arg(long, value_name = "TIME")]
    finished: Option<String>,
    /// A file the check produced, recorded by SHA-256 (not uploaded); repeatable.
    #[arg(long, value_name = "PATH")]
    artifact: Vec<PathBuf>,
    /// A KEY=VALUE note in the claim's annotations; repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    annotation: Vec<String>,
    /// Send this DSSE envelope, signed elsewhere, instead of signing.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["kind", "result", "key", "target", "started", "finished", "artifact", "annotation"], requires = "public_key")]
    envelope: Option<PathBuf>,
    /// The envelope signer's public key, SPKI PEM (`-----BEGIN PUBLIC KEY-----`).
    #[arg(long, value_name = "PEM", requires = "envelope", conflicts_with_all = ["kind", "result", "key", "target", "started", "finished", "artifact", "annotation"])]
    public_key: Option<PathBuf>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef evidence list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Evidence on this revision's tree [default: the repository's, all of it].
    #[arg(short = 'r', long, value_name = "REV", conflicts_with = "tree")]
    revision: Option<String>,
    /// Evidence on this tree id.
    #[arg(long, value_name = "ID")]
    tree: Option<String>,
    /// Evidence added for this change, on any tree it had.
    #[arg(long, value_name = "CHANGE")]
    change: Option<String>,
    /// Only this kind; repeatable.
    #[arg(long, value_name = "KIND")]
    kind: Vec<String>,
    /// Only this result; repeatable.
    #[arg(long, value_name = "RESULT", value_parser = ["pass", "fail", "error"])]
    result: Vec<String>,
    /// Only this target.
    #[arg(long, value_name = "TARGET")]
    target: Option<String>,
    /// Only evidence signed by this key id; repeatable.
    #[arg(long = "key", value_name = "KEY_ID")]
    key: Vec<String>,
    /// Show at most this many; 0 for all. A cut is noted on stderr.
    #[arg(long, value_name = "N", default_value_t = LIST_LIMIT)]
    limit: usize,
    /// Read the last snapshot instead of snapshotting the workspace first.
    #[arg(long)]
    no_snapshot: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef evidence show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// The evidence id, or a unique prefix of at least 4 hex digits.
    #[arg(value_name = "ID")]
    id: String,
    /// Print the canonical envelope's bytes alone, whose SHA-256 is the id.
    #[arg(long)]
    envelope: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

impl Args {
    /// Whether the command runs without a daemon (`key generate`).
    pub(crate) fn is_offline(&self) -> bool {
        matches!(self.command, EvidenceCommand::Key(_))
    }

    /// Runs an offline command.
    pub(crate) fn run_offline(self, out: &Out) -> Result<(), CliError> {
        match self.command {
            EvidenceCommand::Key(KeyCommand::Generate { path }) => generate(out, &path),
            _ => Ok(()),
        }
    }
}

/// Runs a command that talks to the daemon. `add` reads and checks its
/// key, claim and files before it connects, so a refused key file neither
/// starts a daemon nor registers a repository.
pub(crate) async fn run(global: &super::Global, out: &Out, args: Args) -> Result<(), CliError> {
    match args.command {
        EvidenceCommand::Add(args) => {
            let prepared = prepare(&args)?;
            add(&super::connect(global, out).await?, args, prepared).await
        }
        EvidenceCommand::List(args) => {
            check_key_ids(&args.key)?;
            list(&super::connect(global, out).await?, args).await
        }
        EvidenceCommand::Show(args) => show(&super::connect(global, out).await?, args).await,
        EvidenceCommand::Key(KeyCommand::Generate { path }) => generate(out, &path),
    }
}

/// `--key` filters must be full key ids: the text output shows 12 digits,
/// and a prefix would silently match nothing.
fn check_key_ids(key_ids: &[String]) -> Result<(), CliError> {
    let full = |id: &str| {
        id.len() == format::keys::KEY_ID_LEN
            && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    match key_ids.iter().find(|id| !full(id)) {
        None => Ok(()),
        Some(id) => Err(refused(
            ErrorKind::InvalidArgument,
            format!("--key {id:?} is not a full key id (64 lowercase hex digits)"),
            Advice::hint(
                "Filter by the full key id: `reef evidence show <id>` prints an object's key, and `-o json` lists print `key_id` whole.",
            ),
        )),
    }
}

fn refused(kind: ErrorKind, message: String, advice: Advice) -> CliError {
    CliError::Refused {
        kind,
        message,
        advice: Some(advice),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, CliError> {
    std::fs::read(path).map_err(|source| CliError::Io {
        op: "read",
        path: path.to_owned(),
        source,
    })
}

/// `<path>.pub`.
fn public_path(path: &Path) -> PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(".pub");
    PathBuf::from(text)
}

/// `reef evidence key generate <path>`.
fn generate(out: &Out, path: &Path) -> Result<(), CliError> {
    let public = public_path(path);
    for existing in [path, public.as_path()] {
        if existing.symlink_metadata().is_ok() {
            return Err(refused(
                ErrorKind::AlreadyExists,
                format!("{} exists; a key is never overwritten", existing.display()),
                Advice::hint("Pick another path, or remove the old key pair first."),
            ));
        }
    }
    let (private_pem, public_pem) = SigningKey::generate().map_err(|error| {
        refused(
            ErrorKind::Internal,
            error.to_string(),
            Advice::hint("Run it again."),
        )
    })?;
    let key_id = PublicKey::from_pem(&public_pem)
        .map_err(|error| refused(ErrorKind::Internal, error.to_string(), Advice::default()))?
        .id()
        .to_string();
    write_pair(path, &public, &private_pem, &public_pem)?;
    let key = EvidenceKey {
        key_id,
        path: path.display().to_string(),
        public_key_path: public.display().to_string(),
    };
    out.object(&GeneratedKey(key))
}

/// Writes a new key pair: `private` with mode 0600, then `public`, each
/// created new. A failure part way removes what this call wrote, so no
/// private key is left without its public key. A file that appeared
/// meanwhile (`create_new` refused it) is not this call's and stays.
fn write_pair(
    private: &Path,
    public: &Path,
    private_pem: &str,
    public_pem: &str,
) -> Result<(), CliError> {
    let write = |target: &Path, text: &str, mode: u32| -> Result<(), CliError> {
        let io = |source| CliError::Io {
            op: "write",
            path: target.to_owned(),
            source,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(target)
            .map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)
    };
    let ours = |error: &CliError| !matches!(error, CliError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists);
    if let Err(error) = write(private, private_pem, 0o600) {
        if ours(&error) {
            let _ = std::fs::remove_file(private);
        }
        return Err(error);
    }
    if let Err(error) = write(public, public_pem, 0o644) {
        let _ = std::fs::remove_file(private);
        if ours(&error) {
            let _ = std::fs::remove_file(public);
        }
        return Err(error);
    }
    Ok(())
}

/// The private key at `path`, refused when its group or others can read
/// it. The mode is read from the open file, so it is the file read.
fn load_key(path: &Path) -> Result<SigningKey, CliError> {
    use std::io::Read as _;
    let io = |source| CliError::Io {
        op: "read",
        path: path.to_owned(),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(io)?;
    let mode = file.metadata().map_err(io)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(refused(
            ErrorKind::PermissionDenied,
            format!(
                "{} is readable by others (mode {:o}); a signing key must be private",
                path.display(),
                mode & 0o777
            ),
            Advice::hint(format!("chmod 600 {}", path.display())),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|_| {
        refused(
            ErrorKind::InvalidArgument,
            format!("{} is not a PEM file", path.display()),
            key_advice(),
        )
    })?;
    SigningKey::from_pkcs8_pem(&text).map_err(|error| {
        refused(
            ErrorKind::InvalidArgument,
            format!("{}: {error}", path.display()),
            key_advice(),
        )
    })
}

fn key_advice() -> Advice {
    Advice::hint(
        "Pass an ed25519 private key in PKCS#8 PEM: `reef evidence key generate <path>` writes one, as does OpenSSL 3's `openssl genpkey -algorithm ed25519`.",
    )
}

/// A time flag: RFC 3339, or a duration before now; `None`: now.
fn time_flag(flag: &str, text: Option<&str>) -> Result<SystemTime, CliError> {
    let hint = || {
        Advice::hint(
            "Give an RFC 3339 time from 1970 to 9999, such as 2026-11-05T10:00:00Z, or a duration ago such as 90s.",
        )
    };
    let at = match text {
        None => return Ok(SystemTime::now()),
        Some(text) => super::history::when(text).map_err(|message| {
            refused(
                ErrorKind::InvalidArgument,
                format!("--{flag}: {message}"),
                hint(),
            )
        })?,
    };
    match format::statement::format_time(flag, at) {
        Ok(_) => Ok(at),
        Err(_) => Err(refused(
            ErrorKind::InvalidArgument,
            format!("--{flag}: evidence times are from 1970-01-01 to 9999-12-31"),
            hint(),
        )),
    }
}

fn invalid_claim(message: String) -> CliError {
    CliError::Precheck {
        kind: ErrorKind::InvalidArgument,
        reason: reef_api::error::INVALID_EVIDENCE,
        message,
        advice: Advice::hint(
            "Kinds are lowercase letters, digits and '-', starting with a letter, at most 63; a target is 1 to 512 bytes with no control characters.",
        ),
    }
}

/// The claim `args` describe.
fn claim_of(args: &AddArgs) -> Result<Claim, CliError> {
    let kind = args.kind.clone().unwrap_or_default();
    if !format::verify::is_kind(&kind) {
        return Err(invalid_claim(format!("{kind:?} is not an evidence kind")));
    }
    if let Some(problem) = args
        .target
        .as_deref()
        .and_then(format::verify::target_problem)
    {
        return Err(invalid_claim(format!("--target: {problem}")));
    }
    let result = Outcome::parse(args.result.as_deref().unwrap_or_default())
        .ok_or_else(|| invalid_claim("--result is pass, fail or error".into()))?;
    let started_on = time_flag("started", args.started.as_deref())?;
    let finished_on = match args.finished.as_deref() {
        None => SystemTime::now().max(started_on),
        text => time_flag("finished", text)?,
    };
    if finished_on < started_on {
        return Err(invalid_claim("--finished is before --started".into()));
    }
    let mut artifacts = Vec::with_capacity(args.artifact.len());
    for path in &args.artifact {
        let file = std::fs::File::open(path).map_err(|source| CliError::Io {
            op: "read",
            path: path.clone(),
            source,
        })?;
        let sha256 = format::sha256_hex_of(file).map_err(|source| CliError::Io {
            op: "read",
            path: path.clone(),
            source,
        })?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        artifacts.push(Artifact {
            name,
            sha256,
            uri: None,
            media_type: None,
        });
    }
    let mut annotations = Map::new();
    for pair in &args.annotation {
        let Some((key, value)) = pair.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            return Err(refused(
                ErrorKind::InvalidArgument,
                format!("--annotation {pair:?} is not KEY=VALUE"),
                Advice::hint("Give each note as KEY=VALUE, such as --annotation runner=ci-7."),
            ));
        };
        annotations.insert(key.to_owned(), Value::from(value));
    }
    Ok(Claim {
        kind,
        result,
        target: args.target.clone(),
        started_on,
        finished_on,
        artifacts,
        annotations,
    })
}

/// `reef evidence add`.
/// What `add` reads before it connects: the key and claim to sign, or the
/// envelope and public key to send.
struct Prepared {
    signing: Option<(SigningKey, Claim)>,
    given: Option<(Vec<u8>, Vec<u8>)>,
}

/// `add`'s local work, before any call: the key file's mode and contents,
/// the claim (checked as the daemon would, on a placeholder tree), the
/// artifacts' digests, or the envelope and public key files.
fn prepare(args: &AddArgs) -> Result<Prepared, CliError> {
    if args.envelope.is_none()
        && (args.kind.is_none() || args.result.is_none() || args.key.is_none())
    {
        return Err(CliError::Usage(
            "give --kind, --result and --key to sign a claim, or --envelope and --public-key to send one signed elsewhere".into(),
        ));
    }
    // The key and the claim before any call: a bad key sends nothing.
    let signing = match (&args.envelope, &args.key) {
        (None, Some(key)) => Some((load_key(key)?, claim_of(args)?)),
        _ => None,
    };
    let given = match &args.envelope {
        Some(path) => {
            let public = args.public_key.as_deref().unwrap_or(Path::new(""));
            let text = String::from_utf8(read_file(public)?).unwrap_or_default();
            let key = PublicKey::from_pem(&text).map_err(|error| {
                refused(
                    ErrorKind::InvalidArgument,
                    format!("{}: {error}", public.display()),
                    Advice::hint(
                        "Pass the signer's public key as SPKI PEM (-----BEGIN PUBLIC KEY-----), as <path>.pub from `reef evidence key generate`.",
                    ),
                )
            })?;
            Some((read_file(path)?, key.spki().to_vec()))
        }
        None => None,
    };
    if let Some((_, claim)) = &signing {
        let placeholder = format::TreeDigest::git(&"0".repeat(40))
            .map_err(|error| invalid_claim(error.to_string()))?;
        claim
            .statement(&placeholder)
            .map_err(|error| invalid_claim(error.to_string()))?;
    }
    Ok(Prepared { signing, given })
}

/// `reef evidence add`, once [`prepare`] has read its files.
async fn add(ctx: &Ctx, args: AddArgs, prepared: Prepared) -> Result<(), CliError> {
    let Prepared { signing, given } = prepared;
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let revision = args
        .revision
        .clone()
        .or_else(|| signing.is_some().then(|| "@".to_owned()));
    // The revision as the CLI reads it, after a snapshot when it is `@`.
    let change = match &revision {
        Some(revision) => Some(
            ctx.call(ctx.client.get_change(GetChangeRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision: revision.clone(),
                ..GetChangeRequest::default()
            }))
            .await
            .map_err(|error| advice.refine(error))?
            .change
            .unwrap_or_default(),
        ),
        None => None,
    };
    if let Some(change) = &change
        && (change.conflict || change.tree_id.is_empty())
    {
        return Err(CliError::Precheck {
            kind: ErrorKind::Conflict,
            reason: reef_api::error::CONFLICTED_REVISION,
            message: format!(
                "commit {} is conflicted, so it has no single tree for evidence to be about",
                change.commit_id
            ),
            advice: Advice::hint(
                "Resolve the conflict first; a conflicted change has no tree to test.",
            ),
        });
    }
    let (envelope, public_key) = match (signing, given) {
        (Some((key, claim)), _) => {
            let tree_id = change
                .as_ref()
                .map(|c| c.tree_id.as_str())
                .unwrap_or_default();
            let tree = format::TreeDigest::git(tree_id)
                .map_err(|error| invalid_claim(error.to_string()))?;
            let envelope = claim
                .sign(&tree, &key)
                .map_err(|error| invalid_claim(error.to_string()))?
                .to_bytes();
            (envelope, key.public_key().spki().to_vec())
        }
        (None, Some(given)) => given,
        (None, None) => {
            return Err(CliError::Usage(
                "give --kind, --result and --key, or --envelope and --public-key".into(),
            ));
        }
    };
    let response = ctx
        .call(ctx.client.add_evidence(AddEvidenceRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            envelope,
            public_key,
            revision: change.map(|c| c.commit_id).unwrap_or_default(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let added = EvidenceAdded {
        evidence: Evidence::from(&response.evidence.unwrap_or_default()),
        created: response.created,
    };
    ctx.out.object(&Added(added))
}

/// A change id for `--change`: a full one as given, else what the
/// revision `text` names.
async fn change_id(ctx: &Ctx, scope: &scope::Scope, text: &str) -> Result<String, CliError> {
    if text.len() == CHANGE_ID_LEN {
        return Ok(text.to_owned());
    }
    let change = ctx
        .call(ctx.client.get_change(GetChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: text.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        }))
        .await?;
    Ok(change.change.unwrap_or_default().change_id)
}

/// `reef evidence list`.
async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        snapshots_first: true,
        ..AdviceCtx::new(&scope)
    };
    let change = match &args.change {
        Some(text) => change_id(ctx, &scope, text)
            .await
            .map_err(|error| advice.refine(error))?,
        None => String::new(),
    };
    let request = ListEvidenceRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.unwrap_or_default(),
        tree_id: args.tree.unwrap_or_default(),
        change_id: change,
        kinds: args.kind,
        results: args
            .result
            .iter()
            .filter_map(|name| json::evidence_result_of(name))
            .map(i32::from)
            .collect(),
        target: args.target.unwrap_or_default(),
        key_ids: args.key,
        skip_snapshot: args.no_snapshot,
        ..ListEvidenceRequest::default()
    };
    let by_change = !request.change_id.is_empty();
    let limited = ctx
        .call(ctx.client.list_evidence(request, args.limit))
        .await
        .map_err(|error| advice.refine(error))?;
    if limited.more {
        ctx.out.note(&format!(
            "reef: showing the newest {} objects; --limit 0 shows all",
            limited.items.len()
        ));
    }
    let lines = Lines {
        items: limited.items.iter().map(Evidence::from).collect(),
        by_change,
    };
    ctx.out
        .object_with(&lines, |lines| -> Vec<brief::Evidence<'_>> {
            lines.items.iter().map(brief::Evidence::from).collect()
        })
}

/// `reef evidence show`.
async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let response = ctx
        .call(ctx.client.get_evidence(&scope.repo_id, &args.id))
        .await
        .map_err(|error| advice.refine(error))?;
    let evidence = response.evidence.unwrap_or_default();
    if args.envelope {
        return ctx.out.raw(&evidence.envelope);
    }
    ctx.out.object(&Shown(Evidence::from(&evidence)))
}

fn short(id: &str) -> &str {
    id.get(..ID_WIDTH).unwrap_or(id)
}

/// How long a check ran, as text: `850ms`, `72s`, `3m`.
fn duration_text(ms: Option<u64>) -> String {
    match ms {
        None => "-".to_owned(),
        Some(ms) if ms < 1000 => format!("{ms}ms"),
        Some(ms) => span(i64::try_from(ms).unwrap_or(i64::MAX)),
    }
}

/// One evidence object as a line of text.
fn line(evidence: &Evidence, by_change: bool) -> String {
    let mut words = vec![
        short(&evidence.id).to_owned(),
        evidence.kind.clone(),
        evidence.result.clone().unwrap_or_else(|| "-".into()),
    ];
    if let Some(target) = &evidence.target {
        words.push(target.clone());
    }
    words.push(format!("key {}", short(&evidence.key_id)));
    words.push(duration_text(evidence.duration_ms));
    let added = evidence
        .added_at
        .as_deref()
        .and_then(|at| humantime::parse_rfc3339(at).ok())
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_millis()).ok());
    words.push(added.map_or_else(
        || "-".into(),
        |at| format!("added {} ago", span(now_ms() - at)),
    ));
    if by_change && !evidence.current {
        words.push("(not current)".into());
    }
    if evidence.signature.as_deref() != Some("valid") {
        words.push("(signature invalid)".into());
    }
    words.join(" ")
}

/// `evidence list`'s objects: a JSON array, one line each in text.
#[derive(Serialize)]
#[serde(transparent)]
struct Lines {
    items: Vec<Evidence>,
    #[serde(skip)]
    by_change: bool,
}

impl TextRender for Lines {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for evidence in &self.items {
            writeln!(w, "{}", line(evidence, self.by_change))?;
        }
        Ok(())
    }
}

/// `evidence show`'s object.
#[derive(Serialize)]
#[serde(transparent)]
struct Shown(Evidence);

impl TextRender for Shown {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let e = &self.0;
        let or_dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
        let mut pairs = vec![
            ("id", e.id.clone()),
            ("tree", e.tree_id.clone()),
            ("kind", e.kind.clone()),
            ("result", or_dash(e.result.as_deref())),
            ("target", or_dash(e.target.as_deref())),
            ("key", e.key_id.clone()),
            ("started", or_dash(e.started_at.as_deref())),
            ("finished", or_dash(e.finished_at.as_deref())),
            ("added", or_dash(e.added_at.as_deref())),
            ("added by", or_dash(e.added_by_agent.as_deref())),
            (
                "changes",
                if e.change_ids.is_empty() {
                    "-".to_owned()
                } else {
                    e.change_ids
                        .iter()
                        .map(|id| json::reverse_hex(id))
                        .collect::<Vec<_>>()
                        .join(" ")
                },
            ),
        ];
        for artifact in &e.artifacts {
            pairs.push((
                "artifact",
                format!("{} sha256:{}", artifact.name, artifact.sha256),
            ));
        }
        pairs.push(("signature", or_dash(e.signature.as_deref())));
        block(w, &pairs)?;
        if let Some(payload) = signed_payload(e) {
            // The payload exactly as signed: re-printing it through a JSON
            // value would reorder its keys.
            writeln!(w, "statement, as signed:")?;
            writeln!(w, "  {}", String::from_utf8_lossy(&payload))?;
        }
        writeln!(
            w,
            "A valid signature says this key signed the claim, not that the key is trusted."
        )
    }
}

/// The envelope's payload bytes, as signed.
fn signed_payload(evidence: &Evidence) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let payload = evidence.envelope.as_ref()?.get("payload")?.as_str()?;
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()
}

/// `evidence add`'s result.
#[derive(Serialize)]
#[serde(transparent)]
struct Added(EvidenceAdded);

impl TextRender for Added {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let verb = if self.0.created {
            "added"
        } else {
            "already stored"
        };
        writeln!(w, "{verb} {}", line(&self.0.evidence, false))?;
        writeln!(w, "id: {}", self.0.evidence.id)
    }
}

/// `evidence key generate`'s key.
#[derive(Serialize)]
#[serde(transparent)]
struct GeneratedKey(EvidenceKey);

impl TextRender for GeneratedKey {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let key = &self.0;
        block(
            w,
            &[
                ("key id", key.key_id.clone()),
                ("private key", key.path.clone()),
                ("public key", key.public_key_path.clone()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_keys_sit_beside_the_private_key() {
        assert_eq!(
            public_path(Path::new("/k/ci.pem")),
            PathBuf::from("/k/ci.pem.pub")
        );
    }

    // R3.01 review F9: a public key that cannot be written takes the
    // private key with it.
    #[test]
    fn a_failed_pair_leaves_no_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("k.pem");
        let public = dir.path().join("missing").join("k.pem.pub");
        let error = write_pair(&private, &public, "private", "public").unwrap_err();
        assert!(matches!(error, CliError::Io { .. }), "{error}");
        assert!(!private.exists());
        // A private key that already exists is not this call's to remove.
        std::fs::write(&private, "theirs").unwrap();
        write_pair(&private, &dir.path().join("k2.pub"), "private", "public").unwrap_err();
        assert_eq!(std::fs::read_to_string(&private).unwrap(), "theirs");
        assert!(!dir.path().join("k2.pub").exists());
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(duration_text(Some(850)), "850ms");
        assert_eq!(duration_text(Some(72_000)), "1m");
        assert_eq!(duration_text(Some(9_000)), "9s");
        assert_eq!(duration_text(None), "-");
    }
}
