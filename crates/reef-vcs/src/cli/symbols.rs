//! `reef symbols index`, `upload` and `list` (R2.14): the symbol index of
//! a revision. The daemon extracts tags for Python, TypeScript, TSX,
//! JavaScript, Go and Rust itself; `upload` sends a SCIP index an indexer
//! wrote, each document checked against the file it describes. `index` and
//! `upload` exit non-zero when the tree is still indexing or nothing was
//! accepted; with JSON they then print only the error, so stdout holds one
//! value.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1::{
    IndexSymbolsRequest, ListSymbolsRequest, ScipDocument, ScipTool, SymbolIndexState, SymbolRole,
    SymbolSource, UploadScipDocumentsRequest,
};
use reef_client::json::{self, SymbolOccurrence, SymbolStatus, SymbolUpload, UploadProblem};
use reef_client::scip;
use reef_error::{Advice, ErrorKind};
use serde::Serialize;

use super::Output;
use super::out::TextRender;
use super::scope::{self, ReadArgs, ScopeArgs};
use super::{CliError, Ctx};

/// Documents per upload request, as the API allows.
const MAX_DOCUMENTS: usize = 1000;
/// Document bytes per upload request, under gRPC's 4 MiB with room for the
/// rest of the message.
const MAX_BATCH_BYTES: usize = 3 << 20;

/// `reef symbols`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: SymbolsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum SymbolsCommand {
    /// Index a revision's symbols, waiting for it.
    #[command(after_help = "Example: reef symbols index -r 'trunk()' --wait 2m")]
    Index {
        /// The revision.
        #[arg(short = 'r', long, value_name = "REV", default_value = "@")]
        revision: String,
        /// How long to wait; indexing goes on in the background after.
        #[arg(long, value_name = "DURATION", default_value = "10s", value_parser = humantime::parse_duration)]
        wait: Duration,
        #[command(flatten)]
        read: ReadArgs,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Upload a SCIP index an indexer wrote for a revision.
    #[command(after_help = "Example: scip-typescript index && reef symbols upload index.scip")]
    Upload {
        /// The SCIP index file.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// The revision the indexer ran on.
        #[arg(short = 'r', long, value_name = "REV", default_value = "@")]
        revision: String,
        /// Where the indexed files are on disk [default: the index's project root, else the current directory].
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// The repository path the index's paths are relative to [default: its project root's].
        #[arg(long, value_name = "PATH")]
        prefix: Option<String>,
        /// Check against the last snapshot instead of snapshotting first.
        #[arg(long)]
        no_snapshot: bool,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// List the symbols of a file or directory, or a name's occurrences.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef symbols list --name parse --references"
    )]
    List(ListArgs),
}

/// `reef symbols list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// A file or directory, relative to the current directory.
    #[arg(value_name = "PATH")]
    path: Option<String>,
    /// The revision.
    #[arg(short = 'r', long, value_name = "REV", default_value = "@")]
    revision: String,
    /// Occurrences of this exact name.
    #[arg(long, value_name = "NAME", conflicts_with = "symbol")]
    name: Option<String>,
    /// Occurrences of this exact SCIP symbol string.
    #[arg(long, value_name = "SYMBOL")]
    symbol: Option<String>,
    /// Only this kind (function, method, class, ...); repeatable.
    #[arg(long, value_name = "KIND")]
    kind: Vec<String>,
    /// Only definitions.
    #[arg(long, conflicts_with = "references")]
    definitions: bool,
    /// Only references.
    #[arg(long)]
    references: bool,
    /// Answer from this source for every file.
    #[arg(long, value_name = "SOURCE", value_parser = ["tags", "scip"])]
    source: Option<String>,
    /// At most this many.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    #[command(flatten)]
    read: ReadArgs,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        SymbolsCommand::Index {
            revision,
            wait,
            read,
            scope,
        } => index(ctx, revision, wait, read, &scope).await,
        SymbolsCommand::Upload {
            file,
            revision,
            root,
            prefix,
            no_snapshot,
            scope,
        } => upload(ctx, &file, revision, root, prefix, no_snapshot, &scope).await,
        SymbolsCommand::List(args) => list(ctx, args).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let ListArgs {
        path,
        revision,
        name,
        symbol,
        kind,
        definitions,
        references,
        source,
        limit,
        read,
        scope,
    } = args;
    let scope = scope::resolve(ctx, &scope).await?;
    let path = match path {
        Some(path) => scope::given_paths(&scope, &[path])?
            .pop()
            .unwrap_or_default(),
        None if name.is_some() || symbol.is_some() => String::new(),
        None => scope::default_list_path(&scope)?,
    };
    let kinds = kind
        .iter()
        .map(|k| {
            json::symbol_kind_of(k).map(|k| k as i32).ok_or_else(|| CliError::Precheck {
                kind: ErrorKind::InvalidArgument,
                reason: reef_api::error::INVALID_SYMBOL_QUERY,
                message: format!("{k} is not a symbol kind"),
                advice: Advice::hint(
                    "Kinds: module, class, struct, interface, enum, type, function, method, field, variable, constant, macro.",
                ),
            })
        })
        .collect::<Result<_, _>>()?;
    let roles = match (definitions, references) {
        (true, _) => vec![SymbolRole::Definition as i32],
        (_, true) => vec![SymbolRole::Reference as i32],
        _ => Vec::new(),
    };
    let source = match source.as_deref() {
        Some("tags") => SymbolSource::Tags,
        Some("scip") => SymbolSource::Scip,
        _ => SymbolSource::Unspecified,
    };
    let request = ListSymbolsRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision,
        at_operation: read.at_operation(ctx, &scope).await?,
        skip_snapshot: read.no_snapshot,
        path,
        name: name.unwrap_or_default(),
        symbol: symbol.unwrap_or_default(),
        roles,
        kinds,
        source: source as i32,
        ..ListSymbolsRequest::default()
    };
    let symbols = match limit {
        // Pages until `limit`, each read at the first page's operation.
        Some(limit) => {
            let mut request = ListSymbolsRequest {
                page_size: i32::try_from(limit.clamp(1, 1000)).unwrap_or(1000),
                ..request
            };
            let mut symbols = Vec::new();
            loop {
                let page = ctx
                    .call(ctx.client.list_symbols_page(request.clone()))
                    .await?;
                symbols.extend(page.symbols);
                if symbols.len() >= limit || page.next_page_token.is_empty() {
                    break;
                }
                request.page_token = page.next_page_token;
            }
            symbols.truncate(limit);
            symbols
        }
        None => ctx.call(ctx.client.list_symbols(request)).await?.symbols,
    };
    ctx.out.object(&SymbolLines(
        symbols.iter().map(SymbolOccurrence::from).collect(),
    ))
}

async fn index(
    ctx: &Ctx,
    revision: String,
    wait: Duration,
    read: ReadArgs,
    scope: &ScopeArgs,
) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, scope).await?;
    let request = IndexSymbolsRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision,
        at_operation: read.at_operation(ctx, &scope).await?,
        skip_snapshot: read.no_snapshot,
        wait: Some(wait.try_into().unwrap_or_default()),
    };
    let response = ctx
        .call_within(
            ctx.timeout.max(wait + Duration::from_secs(10)),
            ctx.client.index_symbols(request),
        )
        .await?;
    let status = response.status.unwrap_or_default();
    let complete = status.state == SymbolIndexState::Complete as i32;
    // In JSON, stdout holds one value: the status, or the error.
    if complete || ctx.out.mode() == Output::Text {
        ctx.out.object(&Status(SymbolStatus::from(&status)))?;
    }
    if complete {
        return Ok(());
    }
    let longer = humantime::format_duration((wait * 6).max(Duration::from_secs(60))).to_string();
    let mut next: Vec<String> = [
        "reef",
        "symbols",
        "index",
        "-r",
        &status.commit_id,
        "--wait",
        &longer,
    ]
    .map(str::to_owned)
    .to_vec();
    next.extend(scope.flags());
    Err(CliError::Precheck {
        kind: ErrorKind::Unavailable,
        reason: reef_api::error::SYMBOLS_NOT_INDEXED,
        message: format!(
            "commit {} is still indexing: {} of {} files",
            status.commit_id, status.files_indexed, status.files
        ),
        advice: Advice::run("Indexing continues in the background.", next),
    })
}

/// A `file://` URI as a path, its percent-escapes decoded: indexers write
/// `project_root` as a URI, so `Application Support` is `Application%20Support`.
fn file_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    let path = path.strip_prefix("localhost").unwrap_or(path);
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or(b'%'));
                i += 3;
            }
            (byte, ..) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// The documents of an index as the API takes them: each at `prefix` joined
/// to its relative path, with the digest of its file under `root`. Those
/// whose file cannot be read are returned apart, not sent.
fn documents_of(
    index: Vec<scip::Document>,
    prefix: &str,
    root: &Path,
) -> (Vec<ScipDocument>, Vec<UploadProblem>) {
    let mut skipped = Vec::new();
    let mut documents = Vec::new();
    for document in index {
        let path = if prefix.is_empty() {
            document.relative_path.clone()
        } else {
            format!("{prefix}/{}", document.relative_path)
        };
        let Ok(content) = std::fs::read(root.join(&document.relative_path)) else {
            skipped.push(UploadProblem {
                path,
                reason: "unreadable".into(),
                message: None,
            });
            continue;
        };
        documents.push(ScipDocument {
            path,
            content_digest: scip::content_digest(&content),
            document: document.bytes,
        });
    }
    (documents, skipped)
}

async fn upload(
    ctx: &Ctx,
    file: &Path,
    revision: String,
    root: Option<PathBuf>,
    prefix: Option<String>,
    no_snapshot: bool,
    scope: &ScopeArgs,
) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, scope).await?;
    let precheck = |message: String, hint: &str| CliError::Precheck {
        kind: ErrorKind::InvalidArgument,
        reason: reef_api::error::INVALID_UPLOAD,
        message,
        advice: Advice::hint(hint),
    };
    let bytes = std::fs::read(file).map_err(|error| {
        precheck(
            format!("cannot read {}: {error}", file.display()),
            "Pass the index file an indexer wrote, such as index.scip.",
        )
    })?;
    let index = scip::split(&bytes)
        .map_err(|error| precheck(error.to_string(), "Pass a SCIP index (index.scip)."))?;
    let project_root = file_uri(&index.project_root);
    let prefix = match prefix {
        Some(prefix) => prefix.trim_matches('/').to_owned(),
        None => project_root
            .as_deref()
            .and_then(|root| scope::inside(&scope.dir, root))
            .unwrap_or_default(),
    };
    let root = root.or(project_root).map_or_else(scope::current_dir, Ok)?;
    let (documents, mut skipped) = documents_of(index.documents, &prefix, &root);
    let sizes: Vec<usize> = documents
        .iter()
        .map(|d| d.document.len() + d.path.len() + 80)
        .collect();
    let (batches, too_large) = scip::batches(&sizes, MAX_DOCUMENTS, MAX_BATCH_BYTES);
    for i in too_large {
        skipped.push(UploadProblem {
            path: documents[i].path.clone(),
            reason: "too_large".into(),
            message: None,
        });
    }
    let tool = ScipTool {
        name: index.tool_name,
        version: index.tool_version,
    };
    let mut result = SymbolUpload {
        commit_id: String::new(),
        accepted: 0,
        rejected: Vec::new(),
        skipped,
        status: None,
    };
    // The first batch resolves the revision; the rest name its commit.
    let mut revision = revision;
    let mut no_snapshot = no_snapshot;
    for batch in batches {
        let response = ctx
            .call(
                ctx.client
                    .upload_scip_documents(UploadScipDocumentsRequest {
                        repo: scope.repo_id.clone(),
                        workspace: scope.workspace_id(),
                        revision: revision.clone(),
                        skip_snapshot: no_snapshot,
                        tool: Some(tool.clone()),
                        documents: batch.iter().map(|&i| documents[i].clone()).collect(),
                    }),
            )
            .await?;
        revision.clone_from(&response.commit_id);
        no_snapshot = true;
        result.commit_id.clone_from(&response.commit_id);
        result.accepted += response.accepted;
        result
            .rejected
            .extend(response.rejected.iter().map(|r| UploadProblem {
                path: r.path.clone(),
                reason: json::scip_rejection_name(r.reason).to_owned(),
                message: Some(r.message.clone()),
            }));
        result.status = response.status.as_ref().map(SymbolStatus::from);
    }
    if result.accepted > 0 || ctx.out.mode() == Output::Text {
        ctx.out.object(&Uploaded(result.clone()))?;
    }
    if result.accepted > 0 {
        return Ok(());
    }
    let first = result
        .rejected
        .first()
        .or(result.skipped.first())
        .map_or_else(
            || "no documents".to_owned(),
            |p| format!("{}: {}", p.path, p.reason),
        );
    Err(precheck(
        format!("no document was accepted ({first})"),
        "Run the indexer on the revision you upload for, from a checkout of it, and pass --root where its files are.",
    ))
}

/// `symbols index`'s status.
#[derive(Serialize)]
#[serde(transparent)]
struct Status(SymbolStatus);

impl TextRender for Status {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let s = &self.0;
        let mut line = format!(
            "{} {}: {} of {} files",
            s.commit_id.get(..12).unwrap_or(&s.commit_id),
            s.state.as_deref().unwrap_or(json::UNKNOWN),
            s.files_indexed,
            s.files
        );
        if s.files_skipped > 0 {
            let _ = write!(line, ", {} skipped", s.files_skipped);
        }
        if s.scip_files > 0 {
            let _ = write!(line, ", {} from SCIP", s.scip_files);
        }
        writeln!(w, "{line}")?;
        for (language, files) in &s.languages {
            writeln!(w, "  {language}: {files}")?;
        }
        Ok(())
    }
}

/// `symbols upload`'s result.
#[derive(Serialize)]
#[serde(transparent)]
struct Uploaded(SymbolUpload);

impl TextRender for Uploaded {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let u = &self.0;
        writeln!(
            w,
            "{} accepted, {} rejected, {} not sent",
            u.accepted,
            u.rejected.len(),
            u.skipped.len()
        )?;
        for p in u.rejected.iter().chain(&u.skipped) {
            writeln!(w, "  {}: {}", p.path, p.reason)?;
        }
        Ok(())
    }
}

/// `symbols list`'s occurrences: one line each, 1-based for people.
#[derive(Serialize)]
#[serde(transparent)]
struct SymbolLines(Vec<SymbolOccurrence>);

impl TextRender for SymbolLines {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for o in &self.0 {
            let [line, col, ..] = o.range.unwrap_or_default();
            writeln!(
                w,
                "{}:{}:{}  {}  {}  {}",
                o.path,
                line + 1,
                col + 1,
                o.kind.as_deref().unwrap_or("-"),
                o.role.as_deref().unwrap_or("-"),
                o.name
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode() {
        assert_eq!(
            file_uri("file:///Users/a/Library/Application%20Support/ws"),
            Some(PathBuf::from("/Users/a/Library/Application Support/ws"))
        );
        assert_eq!(
            file_uri("file://localhost/x%2Fy"),
            Some(PathBuf::from("/x/y"))
        );
        assert_eq!(file_uri("file:///100%"), Some(PathBuf::from("/100%")));
        assert_eq!(file_uri("https://example.com/x"), None);
    }
}
