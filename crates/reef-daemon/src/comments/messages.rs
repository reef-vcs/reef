//! Comment rows as v1 messages, and the operation attribute's events
//! (R3.12).

use reef_api::v1;
use reef_lib::jj::comments::comment_events_of;
use reef_lib::jj::oplog::OpEntry;

use super::{Estimate, Kind, State};
use crate::db::CommentRow;
use crate::lifecycle::messages::actor_message;
use crate::model::Timestamp;
use crate::service::convert::timestamp;

/// A state as the v1 enum.
pub fn state_message(state: State) -> v1::InstructionState {
    match state {
        State::AwaitingConfirmation => v1::InstructionState::AwaitingConfirmation,
        State::Pending => v1::InstructionState::Pending,
        State::Delivered => v1::InstructionState::Delivered,
        State::Running => v1::InstructionState::Running,
        State::Done => v1::InstructionState::Done,
        State::Failed => v1::InstructionState::Failed,
        State::Cancelled => v1::InstructionState::Cancelled,
    }
}

/// The state a v1 value names; `None` for UNSPECIFIED or an unknown value.
pub fn state_of(value: i32) -> Option<State> {
    Some(match v1::InstructionState::try_from(value).ok()? {
        v1::InstructionState::Unspecified => return None,
        v1::InstructionState::AwaitingConfirmation => State::AwaitingConfirmation,
        v1::InstructionState::Pending => State::Pending,
        v1::InstructionState::Delivered => State::Delivered,
        v1::InstructionState::Running => State::Running,
        v1::InstructionState::Done => State::Done,
        v1::InstructionState::Failed => State::Failed,
        v1::InstructionState::Cancelled => State::Cancelled,
    })
}

/// The kind a v1 value names; `None` for UNSPECIFIED or an unknown value.
pub fn kind_of(value: i32) -> Option<Kind> {
    match v1::CommentKind::try_from(value).ok()? {
        v1::CommentKind::Note => Some(Kind::Note),
        v1::CommentKind::Instruction => Some(Kind::Instruction),
        v1::CommentKind::Unspecified => None,
    }
}

/// The channel a v1 value names, as its column text; `sdk` for
/// UNSPECIFIED.
pub fn channel_of(value: i32) -> &'static str {
    match v1::DeliveryChannel::try_from(value) {
        Ok(v1::DeliveryChannel::Cli) => "cli",
        Ok(v1::DeliveryChannel::Hook) => "hook",
        Ok(v1::DeliveryChannel::Mcp) => "mcp",
        _ => "sdk",
    }
}

fn kind_message(text: &str) -> v1::CommentKind {
    match text {
        "note" => v1::CommentKind::Note,
        "instruction" => v1::CommentKind::Instruction,
        _ => v1::CommentKind::Unspecified,
    }
}

fn column_state(text: &str) -> v1::InstructionState {
    State::parse(text).map_or(v1::InstructionState::Unspecified, state_message)
}

fn cause_message(text: &str) -> v1::InstructionCause {
    match text {
        "request" => v1::InstructionCause::Request,
        "reproposed" => v1::InstructionCause::Reproposed,
        "change_final" => v1::InstructionCause::ChangeFinal,
        "expired" => v1::InstructionCause::Expired,
        _ => v1::InstructionCause::Unspecified,
    }
}

fn channel_message(text: &str) -> v1::DeliveryChannel {
    match text {
        "cli" => v1::DeliveryChannel::Cli,
        "hook" => v1::DeliveryChannel::Hook,
        "mcp" => v1::DeliveryChannel::Mcp,
        "sdk" => v1::DeliveryChannel::Sdk,
        _ => v1::DeliveryChannel::Unspecified,
    }
}

fn source_message(text: &str) -> v1::TargetSource {
    match text {
        "proposal" => v1::TargetSource::Proposal,
        "provenance" => v1::TargetSource::Provenance,
        "none" => v1::TargetSource::None,
        _ => v1::TargetSource::Unspecified,
    }
}

/// A stored estimate as a message.
pub fn estimate_message(json: &str) -> v1::InstructionEstimate {
    let estimate: Estimate = serde_json::from_str(json).unwrap_or_default();
    v1::InstructionEstimate {
        digest: estimate.digest,
        computed_at: Some(timestamp(Timestamp(estimate.computed_at))),
        checks_known: estimate.checks_known,
        checks: u32::try_from(estimate.checks).unwrap_or(u32::MAX),
        check_wall_ms: estimate.check_wall_ms,
        check_cpu_ms: estimate.check_cpu_ms,
        agent_known: estimate.agent_known,
        agent_wall_ms: estimate.agent_wall_ms,
        agent_samples: estimate.agent_samples,
        agent_basis: estimate.agent_basis,
        model: estimate.model,
        harness: estimate.harness,
        reported: None,
        unknown: estimate.unknown,
    }
}

/// A committed comment as a message; `last_seen` is its target's client
/// session's, and `outdated` whether its anchored file changed since.
pub fn comment_message(
    row: &CommentRow,
    last_seen: Option<Timestamp>,
    outdated: bool,
) -> v1::Comment {
    let anchor = row.path.as_ref().map(|path| v1::CommentAnchor {
        path: path.clone(),
        side: if row.side.as_deref() == Some("old") {
            v1::LineSide::Old
        } else {
            v1::LineSide::New
        }
        .into(),
        start_line: row.start_line.unwrap_or(0),
        end_line: row.end_line.unwrap_or(0),
    });
    let instruction = row.instruction.as_ref().map(|columns| v1::Instruction {
        state: column_state(&columns.state).into(),
        cause: cause_message(&columns.cause).into(),
        estimate: Some(estimate_message(&columns.estimate)),
        target: Some(v1::InstructionTarget {
            source: source_message(&columns.target_source).into(),
            session_id: columns.target_session_id.clone().unwrap_or_default(),
            client_session: columns.target_client_session.clone().unwrap_or_default(),
            harness: columns.target_harness.clone().unwrap_or_default(),
            agent: columns.target_agent.clone().unwrap_or_default(),
            workspace_id: columns.target_workspace_id.clone().unwrap_or_default(),
            last_seen_at: last_seen.map(timestamp),
        }),
        claimed_client_session: columns.claimed_client_session.clone().unwrap_or_default(),
        claimed_workspace_id: columns.claimed_workspace_id.clone().unwrap_or_default(),
        claimed_agent: columns.claimed_agent.clone().unwrap_or_default(),
        channel: columns
            .channel
            .as_deref()
            .map_or(v1::DeliveryChannel::Unspecified, channel_message)
            .into(),
        taken: columns.taken,
        summary: columns.summary.clone().unwrap_or_default(),
        confirmed_at: columns.confirmed_at.map(timestamp),
        delivered_at: columns.delivered_at.map(timestamp),
        started_at: columns.started_at.map(timestamp),
        finished_at: columns.finished_at.map(timestamp),
    });
    v1::Comment {
        id: row.id.clone(),
        change_id: row.change_id.clone(),
        round: row.round,
        commit_id: row.commit_id.clone(),
        anchor,
        reply_to: row.reply_to.clone().unwrap_or_default(),
        kind: kind_message(&row.kind).into(),
        text: row.text.clone(),
        text_sha256: row.text_sha256.clone(),
        author: Some(v1::CommentAuthor {
            agent: row.agent.clone().unwrap_or_default(),
            actor: actor_message(&row.actor).into(),
            identity: row.identity_name.clone().unwrap_or_default(),
            identity_id: row.identity_id.clone().unwrap_or_default(),
            session_id: row.session_id.clone().unwrap_or_default(),
            remote: row.remote_token.as_ref().map(|token| v1::RemoteCaller {
                token_id: token.clone(),
                token_name: row.remote_token_name.clone().unwrap_or_default(),
            }),
        }),
        created_at: Some(timestamp(row.recorded_at.unwrap_or(row.created_at))),
        operation_id: row.operation_id.clone().unwrap_or_default(),
        operation_collected: row.operation_collected,
        outdated,
        instruction,
    }
}

/// The comment events an operation's `reef.comment` attribute records, as
/// messages.
pub fn operation_comments(entry: &OpEntry) -> Vec<v1::CommentEvent> {
    comment_events_of(&entry.attributes)
        .into_iter()
        .map(|record| v1::CommentEvent {
            id: record.id,
            comment_id: record.comment,
            change_id: record.change,
            round: record.round,
            kind: kind_message(&record.kind).into(),
            event: record.event,
            from: column_state(&record.from).into(),
            to: column_state(&record.to).into(),
            cause: cause_message(&record.cause).into(),
        })
        .collect()
}
