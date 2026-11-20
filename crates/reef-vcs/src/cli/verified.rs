//! `reef evidence add --actor human --verified` (R3.43, ADR 0085): a key a
//! person approves with a passkey for one sign-off.
//!
//! The CLI generates the key here and asks the daemon for an approval of
//! the exact statement it will sign (`RequestApproval`). It opens the web
//! UI's approval page for it, or prints the page's URL with `--print-url`,
//! and waits (`GetApproval`) until the person approves or declines it
//! there, it expires, or `--timeout` ends (2 minutes by default). Nothing
//! is asked on the terminal (ADR 0025): only a passkey's touch in the
//! browser approves, and the daemon certifies the key only then.

use std::time::{Duration, Instant};

use reef_api::v1::{self, ApprovalState, GetApprovalRequest, RequestApprovalRequest};
use reef_client::evidence::SigningKey;
use reef_client::evidence::session::{Actor, ProofFor, sign_proof};
use reef_client::json::reverse_hex;
use reef_error::{Advice, ErrorKind};

use super::{CliError, Ctx, ui};

/// How long `--verified` waits for the approval without `--timeout`: an
/// approval expires 2 minutes after it is requested.
const APPROVAL_WAIT: Duration = Duration::from_mins(2);

/// What the approval is about, for the waiting line.
pub(crate) struct Asked<'a> {
    pub(crate) repo: &'a str,
    pub(crate) workspace: String,
    pub(crate) identity: &'a str,
    pub(crate) change: &'a v1::Change,
    pub(crate) statement: &'a [u8],
}

/// A fresh key for `asked`, approved by the person's passkey in the web
/// UI, or the reason it was not. It signs `asked.statement` for the
/// change's commit, and nothing else.
pub(crate) async fn approved_key(
    ctx: &Ctx,
    asked: Asked<'_>,
    print_url: bool,
) -> Result<SigningKey, CliError> {
    let key = fresh_key()?;
    let authority = ctx
        .call(ctx.client.list_authorities_all())
        .await?
        .into_iter()
        .find(|authority| authority.current)
        .map(|authority| authority.key_id)
        .unwrap_or_default();
    let authority = reef_client::evidence::KeyId::parse(&authority)
        .unwrap_or_else(|| reef_client::evidence::KeyId::of(&[]));
    let proof = sign_proof(
        &key,
        &ProofFor {
            authority: &authority,
            identity: asked.identity,
            actor: Actor::Human,
            session: "",
        },
    );
    let approval = ctx
        .call(ctx.client.request_approval(RequestApprovalRequest {
            identity: asked.identity.to_owned(),
            repo: asked.repo.to_owned(),
            workspace: asked.workspace,
            revision: asked.change.commit_id.clone(),
            statement: asked.statement.to_vec(),
            public_key: key.public_key().spki().to_vec(),
            proof,
        }))
        .await?
        .approval
        .unwrap_or_default();
    show_page(ctx, &approval.id, print_url).await?;
    ctx.out.note(&format!(
        "Waiting for your approval in the browser: {} {} ({}) as {}, expires in 2 minutes",
        what(&approval),
        reverse_hex(&approval.change_id)
            .get(..8)
            .unwrap_or_default(),
        asked.change.description.lines().next().unwrap_or_default(),
        approval.identity,
    ));
    wait(ctx, &approval.id).await?;
    Ok(key)
}

/// A new ed25519 key, held in memory only.
fn fresh_key() -> Result<SigningKey, CliError> {
    let internal = |error: reef_client::evidence::KeyError| CliError::Refused {
        kind: ErrorKind::Internal,
        message: error.to_string(),
        advice: Some(Advice::hint("Run it again.")),
    };
    let (pem, _) = SigningKey::generate().map_err(internal)?;
    let pem = zeroize::Zeroizing::new(pem);
    SigningKey::from_pkcs8_pem(&pem).map_err(internal)
}

/// "sign off", or the kind and result the statement claims.
fn what(approval: &v1::Approval) -> String {
    match (approval.kind.as_str(), approval.result.as_str()) {
        ("signoff", "pass") => "sign off".to_owned(),
        (kind, result) => format!("{kind} {result}"),
    }
}

/// Opens the approval page `id` in a browser through a new web session, or
/// prints its URL (`--print-url`, or where no browser can be opened).
async fn show_page(ctx: &Ctx, id: &str, print_url: bool) -> Result<(), CliError> {
    let print = print_url || ui::no_browser_here().is_some();
    let session = ctx
        .call(
            ctx.client
                .create_web_session(!print, &format!("/approve/{id}")),
        )
        .await?;
    let launch = std::path::PathBuf::from(&session.launch_path);
    let opened = !print
        && tokio::task::spawn_blocking(move || ui::open(&launch))
            .await
            .unwrap_or(false);
    if !opened {
        ctx.out.note(&format!(
            "Open this page to approve; the URL is a credential for the web UI until the daemon stops:\n{}",
            session.session_url
        ));
    }
    Ok(())
}

/// Waits until `id` is no longer pending: approved, or the error that says
/// why not.
async fn wait(ctx: &Ctx, id: &str) -> Result<(), CliError> {
    let limit = ctx.timeout_flag.unwrap_or(APPROVAL_WAIT);
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ended(
                reef_api::error::APPROVAL_PENDING,
                format!(
                    "approval {id} is still pending after {} (--timeout); nothing was signed",
                    humantime::format_duration(limit)
                ),
            ));
        }
        let response = ctx
            .call_within(
                left + ctx.timeout,
                ctx.client.get_approval(GetApprovalRequest {
                    id: id.to_owned(),
                    wait: prost_types::Duration::try_from(left).ok(),
                }),
            )
            .await;
        let approval = match response {
            Ok(response) => response.approval.unwrap_or_default(),
            Err(CliError::Daemon(error))
                if error.reason() == reef_api::error::APPROVAL_NOT_FOUND =>
            {
                return Err(ended(
                    reef_api::error::APPROVAL_NOT_FOUND,
                    format!(
                        "approval {id} is gone: the daemon restarted, or it ended a while ago; nothing was signed"
                    ),
                ));
            }
            Err(error) => return Err(error),
        };
        match approval.state() {
            ApprovalState::Approved => return Ok(()),
            ApprovalState::Declined => {
                let why = if approval.declined_reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", approval.declined_reason)
                };
                return Err(ended(
                    reef_api::error::APPROVAL_DECLINED,
                    format!("approval {id} was declined in the browser{why}; nothing was signed"),
                ));
            }
            ApprovalState::Expired => {
                return Err(ended(
                    reef_api::error::APPROVAL_EXPIRED,
                    format!(
                        "approval {id} expired before a passkey approved it; nothing was signed"
                    ),
                ));
            }
            ApprovalState::Pending | ApprovalState::Unspecified => {}
        }
    }
}

/// An approval that ended without a key: exit 5, and no `next`, since
/// nothing a command runs can approve.
fn ended(reason: &'static str, message: String) -> CliError {
    CliError::Precheck {
        kind: ErrorKind::Conflict,
        reason,
        message,
        advice: Advice::hint(
            "Run the command again for a new approval, and approve it with your passkey on the page it opens.",
        ),
    }
}
