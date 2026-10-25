//! `reef ws move` (R1.06): a workspace onto another change, keeping its
//! directory and lease.

use reef_api::json::Moved;
use reef_api::proto::v1::create_request::MissingStores;
use reef_api::proto::v1::{MoveRequest, move_request};

use super::repo::missing;
use super::{CliError, Ctx};

/// `reef ws move`.
#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("to").required(true).args(["change", "new_on"])))]
pub(crate) struct Args {
    /// A workspace id, unique prefix, or path inside it.
    workspace: String,
    /// Edit this change (a change id prefix, commit id prefix, or bookmark);
    /// the workspace's own change remounts its shared stores.
    #[arg(long, value_name = "REV")]
    change: Option<String>,
    /// Start a new change on this revision.
    #[arg(long, value_name = "REV")]
    new_on: Option<String>,
    /// Move even if a shared store the target declares is not built; the
    /// workspace is marked `store-missing` [default: fail, naming the `reef
    /// store build` to run].
    #[arg(long)]
    skip_missing_stores: bool,
}

/// Snapshots the workspace, moves it, and prints it (its path in text
/// mode, as `ws create` does).
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let target = match (args.change, args.new_on) {
        (Some(change), _) => move_request::Target::Change(change),
        (None, Some(new_on)) => move_request::Target::NewOn(new_on),
        (None, None) => unreachable!("clap requires --change or --new-on"),
    };
    let missing_stores = if args.skip_missing_stores {
        MissingStores::Skip
    } else {
        MissingStores::Fail
    };
    let response = ctx
        .call(ctx.client.move_workspace(MoveRequest {
            repo: String::new(),
            workspace: args.workspace,
            target: Some(target),
            missing_stores: missing_stores.into(),
        }))
        .await?;
    let moved = Moved::from_response(&response).ok_or_else(|| missing("workspace"))?;
    ctx.out.object(&moved)
}
