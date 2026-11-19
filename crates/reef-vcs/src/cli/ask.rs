//! `reef ask` (R3.32, ADR 0083): an agent asks a person for help on a
//! change, or on its conflict. The help request names what is wrong, what
//! the agent tried, and its question; a person answers with `reef comment
//! answer`, which is an instruction for the session that asked, and
//! `--wait` takes it as it arrives, as `reef comment next --wait` does.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use reef_api::v1::{self, ClaimInstructionRequest, CreateCommentRequest};
use reef_client::json::reverse_hex;
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::AdviceCtx;
use super::comment::{
    BriefComment, Comment, body, claim_or_wait, client_session, declare_instruction, frame,
};
use super::conflict::clean;
use super::out::TextRender;
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// `reef ask`.
#[derive(Debug, clap::Args)]
pub(crate) struct AskArgs {
    /// The change it is about.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// What is wrong.
    #[arg(
        short = 'm',
        long = "message",
        value_name = "TEXT",
        conflicts_with = "file"
    )]
    message: Option<String>,
    /// Read what is wrong from a file (`-` for stdin).
    #[arg(short = 'F', long, value_name = "FILE")]
    file: Option<PathBuf>,
    /// What you tried.
    #[arg(long, value_name = "TEXT")]
    tried: String,
    /// The question for a person.
    #[arg(long, value_name = "TEXT")]
    question: Option<String>,
    /// Why: a person's decision, the change's conflict, blocked (access, a
    /// credential, the environment), an unclear task, or other.
    #[arg(long, value_enum, default_value = "other")]
    reason: ReasonArg,
    /// With --reason conflict: the assignment (asg_…) or conflict (cfl_…)
    /// it is about [default: the change's conflict].
    #[arg(long, value_name = "ID")]
    conflict: Option<String>,
    /// The file it is about, repository-relative.
    #[arg(long, value_name = "PATH")]
    path: Option<String>,
    /// The lines it is about: N or N-M, 1-based.
    #[arg(long, value_name = "N[-M]", requires = "path")]
    lines: Option<String>,
    /// The session an answer goes to [default: the harness's, from its environment].
    #[arg(long, value_name = "ID")]
    client_session: Option<String>,
    /// Wait this long for the answer, and print it.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    wait: Option<Duration>,
    #[command(flatten)]
    scope: ScopeArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ReasonArg {
    Decision,
    Conflict,
    Blocked,
    Unclear,
    Other,
}

impl From<ReasonArg> for v1::HelpReason {
    fn from(reason: ReasonArg) -> Self {
        match reason {
            ReasonArg::Decision => Self::Decision,
            ReasonArg::Conflict => Self::Conflict,
            ReasonArg::Blocked => Self::Blocked,
            ReasonArg::Unclear => Self::Unclear,
            ReasonArg::Other => Self::Other,
        }
    }
}

/// `reef ask`'s result: the help request, and with `--wait` the answer
/// taken for this session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Asked {
    /// The help request.
    comment: Comment,
    /// The first instruction taken for this session on the change, which is
    /// the answer unless another was waiting for it; absent without
    /// `--wait`, or when none came in time.
    answer: Option<Comment>,
    /// The answer framed for a model: who wrote it, its text, what to do
    /// when done.
    frame: Option<String>,
}

/// `ask` in brief: the help request's brief, and the frame when an answer
/// came.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefAsked<'a> {
    comment: BriefComment<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<&'a str>,
}

impl<'a> From<&'a Asked> for BriefAsked<'a> {
    fn from(asked: &'a Asked) -> Self {
        Self {
            comment: BriefComment::from(&asked.comment),
            frame: asked.frame.as_deref(),
        }
    }
}

impl TextRender for Asked {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        self.comment.render(w)?;
        if let Some(frame) = &self.frame {
            writeln!(w)?;
            for line in frame.lines() {
                writeln!(w, "{}", clean(line))?;
            }
        }
        Ok(())
    }
}

/// `reef ask`.
pub(crate) async fn run(ctx: &Ctx, args: AskArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let text = body(args.message.as_ref(), args.file.as_ref())?;
    let (start_line, end_line) = match &args.lines {
        Some(lines) => super::comment::parse_lines(lines)?,
        None => (0, 0),
    };
    let reason = if args.conflict.is_some() {
        ReasonArg::Conflict
    } else {
        args.reason
    };
    let session = client_session(args.client_session.clone());
    let request = CreateCommentRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.clone(),
        anchor: args.path.as_ref().map(|path| v1::CommentAnchor {
            path: path.clone(),
            side: v1::LineSide::New.into(),
            start_line,
            end_line,
        }),
        kind: v1::CommentKind::HelpRequest.into(),
        text,
        help: Some(v1::HelpInput {
            reason: v1::HelpReason::from(reason).into(),
            tried: args.tried.clone(),
            question: args.question.clone().unwrap_or_default(),
            conflict: args.conflict.clone().unwrap_or_default(),
        }),
        client_session: session.clone(),
        ..CreateCommentRequest::default()
    };
    let response = Box::pin(ctx.call(ctx.client.create_comment(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let asked = response.comment.unwrap_or_default();
    let change = reverse_hex(&asked.change_id);
    let answer = match args.wait {
        Some(wait) => {
            let claim = ClaimInstructionRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                client_session: session.clone(),
                change: change.clone(),
                channel: v1::DeliveryChannel::Cli.into(),
                ..ClaimInstructionRequest::default()
            };
            Box::pin(claim_or_wait(ctx, claim, Some(wait)))
                .await
                .map_err(|error| advice.refine(error))?
        }
        None => None,
    };
    if let Some(answer) = &answer {
        let declared = answer
            .instruction
            .as_ref()
            .map(|instruction| instruction.claimed_client_session.clone())
            .filter(|session| !session.is_empty())
            .unwrap_or_else(|| session.clone());
        declare_instruction(ctx, &declared, &answer.text_sha256).await;
    } else {
        let waited = if args.wait.is_some() {
            "no answer yet; "
        } else {
            ""
        };
        ctx.out.note(&format!(
            "reef: {waited}asked {}; take the answer with `reef comment next --wait 10m --change {change}`",
            asked.id
        ));
    }
    let result = Asked {
        comment: Comment::from(&asked),
        frame: answer.as_ref().map(frame),
        answer: answer.as_ref().map(Comment::from),
    };
    ctx.out.object_with(&result, BriefAsked::from)
}
