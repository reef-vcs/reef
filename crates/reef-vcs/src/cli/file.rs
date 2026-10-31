//! `reef file read` and `file list` (R2.27): a file or directory at any
//! revision, without checking it out. `file read` prints the bytes, even
//! on a pipe, one range at a time; `-o json` asks for the object. There is
//! no `file edit`: from a shell, the workspace is the editor, and agents
//! without a directory reach `EditFiles` through MCP (R2.13).

use reef_api::v1::{ListFilesRequest, ReadFileRequest};
use reef_client::json::{FileEntry, FileRead};
use reef_error::{Advice, ErrorKind};

use super::advice::AdviceCtx;
use super::render_vcs::FileLines;
use super::scope::{self, ReadArgs, Scope, ScopeArgs};
use super::{CliError, Ctx};

/// `reef file`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: FileCommand,
}

#[derive(Debug, clap::Subcommand)]
enum FileCommand {
    /// Print a file's bytes at a revision.
    #[command(
        visible_alias = "cat",
        after_help = "Example: reef file read -r 'trunk()' src/main.rs"
    )]
    Read {
        /// The file, relative to the current directory.
        #[arg(value_name = "PATH")]
        path: String,
        /// The revision.
        #[arg(short = 'r', long, value_name = "REV", default_value = "@")]
        revision: String,
        #[command(flatten)]
        read: ReadArgs,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// List a directory at a revision.
    #[command(visible_alias = "ls")]
    List {
        /// The directory, relative to the current directory [default: the current directory].
        #[arg(value_name = "PATH")]
        path: Option<String>,
        /// The revision.
        #[arg(short = 'r', long, value_name = "REV", default_value = "@")]
        revision: String,
        /// List every file below it instead of its entries.
        #[arg(long)]
        recursive: bool,
        #[command(flatten)]
        read: ReadArgs,
        #[command(flatten)]
        scope: ScopeArgs,
    },
}

impl Args {
    /// Whether this is `file read`, whose output is the file's bytes unless
    /// JSON is asked for.
    pub(crate) fn is_read(&self) -> bool {
        matches!(self.command, FileCommand::Read { .. })
    }
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        FileCommand::Read {
            path,
            revision,
            read,
            scope,
        } => {
            let scope = scope::resolve(ctx, &scope).await?;
            let mut paths = scope::given_paths(&scope, &[path])?;
            if paths.first().is_some_and(String::is_empty) {
                return Err(root_is_a_directory(&scope, &revision));
            }
            let advice = AdviceCtx {
                operation_given: read.at.given(),
                snapshots_first: !read.no_snapshot,
                ..AdviceCtx::new(&scope)
            };
            let request = ReadFileRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision,
                path: paths.pop().unwrap_or_default(),
                at_operation: read.at_operation(ctx, &scope).await?,
                skip_snapshot: read.no_snapshot,
                ..ReadFileRequest::default()
            };
            read_file(ctx, request)
                .await
                .map_err(|error| advice.refine(error))
        }
        FileCommand::List {
            path,
            revision,
            recursive,
            read,
            scope,
        } => {
            let scope = scope::resolve(ctx, &scope).await?;
            let path = match path {
                Some(path) => scope::given_paths(&scope, &[path])?
                    .pop()
                    .unwrap_or_default(),
                None => scope::default_list_path(&scope)?,
            };
            let advice = AdviceCtx {
                operation_given: read.at.given(),
                snapshots_first: !read.no_snapshot,
                ..AdviceCtx::new(&scope)
            };
            let request = ListFilesRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision,
                path,
                recursive,
                at_operation: read.at_operation(ctx, &scope).await?,
                skip_snapshot: read.no_snapshot,
                ..ListFilesRequest::default()
            };
            let response = ctx
                .call(ctx.client.list_files(request))
                .await
                .map_err(|error| advice.refine(error))?;
            ctx.out.object(&FileLines(
                response.entries.iter().map(FileEntry::from).collect(),
            ))
        }
    }
}

/// `file read` of the root, which the API refuses as an invalid path: it is
/// a directory, as for any other.
fn root_is_a_directory(scope: &Scope, revision: &str) -> CliError {
    let mut next: Vec<String> = ["reef", "file", "list", "-r", revision]
        .map(str::to_owned)
        .to_vec();
    next.extend(scope.flags());
    CliError::Precheck {
        kind: ErrorKind::Conflict,
        reason: reef_api::error::NOT_A_FILE,
        message: "the repository's root is a directory, not a file".to_owned(),
        advice: Advice::run("It is a directory; list it instead.", next),
    }
}

/// The file `request` names: its bytes written range by range as they
/// arrive, or the whole object with `-o json`. Ranges after the first read
/// the commit the first one read, so a write in between cannot mix two
/// versions.
async fn read_file(ctx: &Ctx, request: ReadFileRequest) -> Result<(), CliError> {
    if ctx.out.is_json() {
        let whole = ctx.call(ctx.client.read_file_to_end(request)).await?;
        return ctx.out.object(&FileRead::from(&whole));
    }
    let first = ctx.call(ctx.client.read_file(request.clone())).await?;
    if !ctx.out.raw_open(&first.content)? {
        return Ok(());
    }
    let mut offset = u64::try_from(first.content.len()).unwrap_or(u64::MAX);
    let mut next = ReadFileRequest {
        revision: first.commit_id.clone(),
        skip_snapshot: true,
        ..request
    };
    while offset < first.size {
        next.offset = offset;
        let range = ctx.call(ctx.client.read_file(next.clone())).await?;
        if range.content.is_empty() {
            break;
        }
        if !ctx.out.raw_open(&range.content)? {
            break;
        }
        offset = offset.saturating_add(u64::try_from(range.content.len()).unwrap_or(u64::MAX));
    }
    Ok(())
}
