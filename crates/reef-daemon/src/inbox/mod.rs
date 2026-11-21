//! The triage inbox (R3.33, ADR 0088, `docs/inbox.md`): a pure function
//! from one repository's records to its items, plus the digest.
//!
//! Nothing here awaits or reads state: `service::inbox` gathers
//! [`Sources`], and [`classify`] places each record by the first row of its
//! object's rule table ([`rules`]) that admits it. Nothing is kept between
//! reads; a person's mark is the only stored state (`db::inbox`).

pub(crate) mod rules;
#[cfg(test)]
mod tests;
mod words;

use std::collections::HashMap;

use reef_api::v1::{self, OperationKind, PresenceKind};

use crate::db::inbox::{DigestBase, Landed};
use crate::model::{RepoId, Timestamp};
use crate::service::convert::timestamp;

/// Items a bucket keeps per repository; its total counts the rest.
pub(crate) const BUCKET_CAP: usize = 200;
/// Landings a digest lists.
pub(crate) const DIGEST_NEWEST: usize = 10;
/// How far back a digest with no mark starts.
pub(crate) const DIGEST_DEFAULT_MS: i64 = 24 * 60 * 60 * 1000;
/// An instruction whose target session was last seen longer ago than this
/// says nobody has picked it up.
pub(crate) const UNCLAIMED_AFTER_MS: i64 = 60 * 60 * 1000;

/// What an item is about. One key per object whatever its reason, so a
/// proposal that moves from watching to needs-you-now keeps its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Object {
    Proposal,
    Help,
    Instruction,
    Conflict,
    Workspace,
}

impl Object {
    pub(crate) const ALL: [Object; 5] = [
        Object::Proposal,
        Object::Help,
        Object::Instruction,
        Object::Conflict,
        Object::Workspace,
    ];

    fn prefix(self) -> &'static str {
        match self {
            Object::Proposal => "proposal",
            Object::Help => "help",
            Object::Instruction => "instruction",
            Object::Conflict => "conflict",
            Object::Workspace => "workspace",
        }
    }

    /// What can change this object's items: the one place a trigger is
    /// declared. `GetInbox`'s `refresh_operations` and `refresh_presence`
    /// are their union.
    pub(crate) const fn triggers(self) -> (&'static [OperationKind], &'static [PresenceKind]) {
        use OperationKind as O;
        match self {
            Object::Proposal => (
                &[
                    O::Transition,
                    O::Land,
                    O::Batch,
                    O::Undo,
                    O::Rebase,
                    O::Describe,
                    O::Abandon,
                ],
                &[PresenceKind::Automatic],
            ),
            Object::Help | Object::Instruction => (&[O::Comment], &[]),
            Object::Conflict => (&[O::ConflictAssignment], &[]),
            Object::Workspace => (
                &[O::CreateWorkspace, O::ReleaseWorkspace, O::MoveWorkspace],
                &[PresenceKind::Attention],
            ),
        }
    }
}

/// An item's key: `<object>:<repo>:<id>`. Clients never parse it.
pub(crate) fn key(object: Object, repo: RepoId, id: &str) -> String {
    format!("{}:{}:{id}", object.prefix(), repo.as_hex())
}

/// Where an item goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bucket {
    NeedsYouNow,
    Watching,
}

/// `v1::InboxReason`, one to one; each rule table names its reasons once
/// (a test checks every reason is in exactly one table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Reason {
    Help,
    Signoff,
    ConflictUnowned,
    ConflictBlocked,
    Returned,
    NoExecutor,
    Attention,
    Confirm,
    InReview,
    InFlight,
}

impl Reason {
    #[cfg(test)]
    pub(crate) const ALL: [Reason; 10] = [
        Reason::Help,
        Reason::Signoff,
        Reason::ConflictUnowned,
        Reason::ConflictBlocked,
        Reason::Returned,
        Reason::NoExecutor,
        Reason::Attention,
        Reason::Confirm,
        Reason::InReview,
        Reason::InFlight,
    ];

    pub(crate) const fn bucket(self) -> Bucket {
        match self {
            Reason::InReview | Reason::InFlight => Bucket::Watching,
            _ => Bucket::NeedsYouNow,
        }
    }

    pub(crate) const fn wire(self) -> v1::InboxReason {
        match self {
            Reason::Help => v1::InboxReason::Help,
            Reason::Signoff => v1::InboxReason::Signoff,
            Reason::ConflictUnowned => v1::InboxReason::ConflictUnowned,
            Reason::ConflictBlocked => v1::InboxReason::ConflictBlocked,
            Reason::Returned => v1::InboxReason::Returned,
            Reason::NoExecutor => v1::InboxReason::NoExecutor,
            Reason::Attention => v1::InboxReason::Attention,
            Reason::Confirm => v1::InboxReason::Confirm,
            Reason::InReview => v1::InboxReason::InReview,
            Reason::InFlight => v1::InboxReason::InFlight,
        }
    }
}

/// One repository's records, as the per-source reads return them. The
/// `service::inbox` shell fills it; tests build it by hand.
#[derive(Debug)]
pub(crate) struct Sources {
    pub repo: RepoId,
    /// Open proposals (`PROPOSED`, `QUEUED`), as `ListProposals` returns
    /// them.
    pub proposals: Vec<v1::ProposalEntry>,
    /// Help requests `OPEN`.
    pub help: Vec<v1::Comment>,
    /// Instructions `AWAITING_CONFIRMATION`, `PENDING`, `DELIVERED` or
    /// `RUNNING`.
    pub instructions: Vec<v1::Comment>,
    /// Open conflict assignments.
    pub assignments: Vec<v1::ConflictAssignment>,
    /// Active workspaces whose `attention` is set.
    pub workspaces: Vec<v1::Workspace>,
    /// When the read was made.
    pub now: Timestamp,
}

impl Sources {
    /// No records of `repo`, read at `now`.
    #[cfg(test)]
    pub(crate) fn empty(repo: RepoId, now: Timestamp) -> Sources {
        Sources {
            repo,
            proposals: Vec::new(),
            help: Vec::new(),
            instructions: Vec::new(),
            assignments: Vec::new(),
            workspaces: Vec::new(),
            now,
        }
    }
}

/// A placed record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub key: String,
    pub reason: Reason,
    pub headline: String,
    pub since: Timestamp,
    pub change_id: String,
    pub commit_id: String,
    pub next: Vec<String>,
    pub record: v1::inbox_item::Record,
}

impl Item {
    pub(crate) fn message(self, repo: RepoId) -> v1::InboxItem {
        v1::InboxItem {
            key: self.key,
            repo_id: repo.as_hex(),
            reason: self.reason.wire().into(),
            headline: self.headline,
            since: Some(timestamp(self.since)),
            change_id: self.change_id,
            commit_id: self.commit_id,
            next: self.next,
            record: Some(self.record),
        }
    }
}

/// One repository's buckets: newest first, cut at [`BUCKET_CAP`], with
/// totals.
#[derive(Debug, Default)]
pub(crate) struct Buckets {
    pub needs_you_now: Vec<Item>,
    pub needs_you_now_total: usize,
    pub watching: Vec<Item>,
    pub watching_total: usize,
}

/// Places every record of `sources` by the first rule of its object that
/// admits it; a record no rule admits is not in the inbox.
pub(crate) fn classify(sources: Sources) -> Buckets {
    let Sources {
        repo,
        proposals,
        help,
        instructions,
        assignments,
        workspaces,
        now,
    } = sources;
    let mut items = Vec::new();
    for entry in proposals {
        if let Some(reason) = rules::place(rules::PROPOSAL, &entry) {
            items.push(proposal_item(repo, reason, entry));
        }
    }
    for comment in help {
        if let Some(reason) = rules::place(rules::HELP, &comment) {
            items.push(comment_item(repo, Object::Help, reason, comment, now));
        }
    }
    for comment in instructions {
        if let Some(reason) = rules::place(rules::INSTRUCTION, &comment) {
            items.push(comment_item(
                repo,
                Object::Instruction,
                reason,
                comment,
                now,
            ));
        }
    }
    for assignment in assignments {
        if let Some(reason) = rules::place(rules::CONFLICT, &assignment) {
            items.push(conflict_item(repo, reason, assignment));
        }
    }
    for workspace in workspaces {
        if let Some(reason) = rules::place(rules::WORKSPACE, &workspace) {
            items.push(workspace_item(repo, reason, workspace));
        }
    }
    items.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.key.cmp(&b.key)));
    let mut buckets = Buckets::default();
    for item in items {
        match item.reason.bucket() {
            Bucket::NeedsYouNow => {
                buckets.needs_you_now_total += 1;
                if buckets.needs_you_now.len() < BUCKET_CAP {
                    buckets.needs_you_now.push(item);
                }
            }
            Bucket::Watching => {
                buckets.watching_total += 1;
                if buckets.watching.len() < BUCKET_CAP {
                    buckets.watching.push(item);
                }
            }
        }
    }
    buckets
}

pub(crate) fn time(at: Option<&prost_types::Timestamp>) -> Timestamp {
    at.map_or(Timestamp(0), |at| {
        Timestamp(at.seconds * 1000 + i64::from(at.nanos / 1_000_000))
    })
}

fn proposal_item(repo: RepoId, reason: Reason, entry: v1::ProposalEntry) -> Item {
    let lifecycle = entry.lifecycle.as_ref();
    let since = time(
        lifecycle
            .and_then(|l| l.last.as_ref())
            .and_then(|t| t.recorded_at.as_ref()),
    );
    let commit_id = entry
        .change
        .as_ref()
        .map(|change| change.commit_id.clone())
        .or_else(|| lifecycle.map(|l| l.commit_id.clone()))
        .unwrap_or_default();
    let next = match reason {
        Reason::NoExecutor => vec!["reef".into(), "executor".into(), "run".into()],
        _ => Vec::new(),
    };
    Item {
        key: key(Object::Proposal, repo, &entry.change_id),
        reason,
        headline: words::proposal(reason, &entry),
        since,
        change_id: entry.change_id.clone(),
        commit_id,
        next,
        record: v1::inbox_item::Record::Proposal(entry),
    }
}

fn comment_item(
    repo: RepoId,
    object: Object,
    reason: Reason,
    comment: v1::Comment,
    now: Timestamp,
) -> Item {
    Item {
        key: key(object, repo, &comment.id),
        reason,
        headline: words::comment(reason, &comment, now),
        since: time(comment.created_at.as_ref()),
        change_id: comment.change_id.clone(),
        commit_id: String::new(),
        next: Vec::new(),
        record: v1::inbox_item::Record::Comment(comment),
    }
}

fn conflict_item(repo: RepoId, reason: Reason, assignment: v1::ConflictAssignment) -> Item {
    let next = match reason {
        Reason::ConflictUnowned => vec![
            "reef".into(),
            "conflict".into(),
            "assign".into(),
            assignment.id.clone(),
            "--queue".into(),
        ],
        Reason::ConflictBlocked => vec![
            "reef".into(),
            "conflict".into(),
            "allow".into(),
            assignment.id.clone(),
        ],
        _ => Vec::new(),
    };
    Item {
        key: key(Object::Conflict, repo, &assignment.id),
        reason,
        headline: words::conflict(reason, &assignment),
        since: time(assignment.updated_at.as_ref()),
        change_id: assignment.change_id.clone(),
        commit_id: assignment.commit_id.clone(),
        next,
        record: v1::inbox_item::Record::Assignment(assignment),
    }
}

fn workspace_item(repo: RepoId, reason: Reason, workspace: v1::Workspace) -> Item {
    let since = time(
        workspace
            .last_snapshot_at
            .as_ref()
            .or(workspace.created_at.as_ref()),
    );
    Item {
        key: key(Object::Workspace, repo, &workspace.id),
        reason,
        headline: words::workspace(&workspace),
        since,
        change_id: workspace.change_id.clone(),
        commit_id: workspace.commit_id.clone(),
        next: vec![
            "reef".into(),
            "ws".into(),
            "inspect".into(),
            workspace.id.clone(),
        ],
        record: v1::inbox_item::Record::Workspace(workspace),
    }
}

/// The digest of what landed after `base`, with each newest landing's
/// subject from `subjects` (change id to description).
pub(crate) fn digest(
    base: DigestBase,
    base_seen: Timestamp,
    landed: Landed,
    subjects: &HashMap<String, String>,
) -> v1::LandedDigest {
    let (from_seq, from_mark) = (base.seq.unsigned_abs(), base.from_mark);
    v1::LandedDigest {
        count: landed.count,
        by_agent: landed
            .by_agent
            .into_iter()
            .map(|(agent, count)| v1::InboxAgentCount {
                agent: agent.unwrap_or_default(),
                count,
            })
            .collect(),
        newest: landed
            .newest
            .into_iter()
            .map(|row| v1::InboxLanding {
                subject: subjects
                    .get(&row.change_id)
                    .and_then(|text| text.lines().next())
                    .unwrap_or_default()
                    .to_owned(),
                change_id: row.change_id,
                agent: row.agent.unwrap_or_default(),
                landed_at: Some(timestamp(row.landed_at)),
                seq: row.seq.unsigned_abs(),
            })
            .collect(),
        from_seq,
        through_seq: landed.through_seq.map_or(from_seq, i64::unsigned_abs),
        since: Some(timestamp(base_seen)),
        from_mark,
    }
}

/// Every operation kind some object's items change on.
pub(crate) fn refresh_operations() -> Vec<i32> {
    let mut kinds: Vec<i32> = Object::ALL
        .iter()
        .flat_map(|object| object.triggers().0.iter().map(|kind| i32::from(*kind)))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// Every presence kind some object's items change on.
pub(crate) fn refresh_presence() -> Vec<i32> {
    let mut kinds: Vec<i32> = Object::ALL
        .iter()
        .flat_map(|object| object.triggers().1.iter().map(|kind| i32::from(*kind)))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}
