use reef_api::v1;

use super::*;

fn repo() -> RepoId {
    "aaaa000000000001".parse().unwrap()
}

fn at(ms: i64) -> prost_types::Timestamp {
    timestamp(Timestamp(ms))
}

fn status(state: v1::AutomaticState, missing: &[&str]) -> v1::AutomaticStatus {
    let mut status = v1::AutomaticStatus {
        missing: missing
            .iter()
            .map(|kind| v1::PolicyEvidenceNeed {
                kind: (*kind).to_owned(),
                attestors: vec!["release".to_owned()],
                ..v1::PolicyEvidenceNeed::default()
            })
            .collect(),
        missing_total: u32::try_from(missing.len()).unwrap(),
        ..v1::AutomaticStatus::default()
    };
    status.set_state(state);
    status
}

fn proposal(
    change: &str,
    automatic: Option<v1::AutomaticStatus>,
    recorded: i64,
) -> v1::ProposalEntry {
    let mut last = v1::ChangeTransition {
        recorded_at: Some(at(recorded)),
        ..v1::ChangeTransition::default()
    };
    last.set_to(v1::LifecycleState::Proposed);
    last.set_cause(v1::TransitionCause::Request);
    let mut lifecycle = v1::ChangeLifecycle {
        last: Some(last),
        commit_id: format!("commit-{change}"),
        ..v1::ChangeLifecycle::default()
    };
    lifecycle.set_state(v1::LifecycleState::Proposed);
    v1::ProposalEntry {
        change_id: change.to_owned(),
        lifecycle: Some(lifecycle),
        automatic,
        ..v1::ProposalEntry::default()
    }
}

fn returned(entry: &mut v1::ProposalEntry, amended: bool) {
    let lifecycle = entry.lifecycle.as_mut().unwrap();
    lifecycle.amended = amended;
    let last = lifecycle.last.as_mut().unwrap();
    last.set_cause(v1::TransitionCause::Landing);
    last.reason = "checks: test failed on the batch's tree".to_owned();
}

fn help(id: &str, created: i64) -> v1::Comment {
    let mut help = v1::HelpRequest::default();
    help.set_state(v1::HelpState::Open);
    help.set_reason(v1::HelpReason::Decision);
    let mut comment = v1::Comment {
        id: id.to_owned(),
        change_id: "c1".to_owned(),
        help: Some(help),
        created_at: Some(at(created)),
        author: Some(v1::CommentAuthor {
            agent: "claude@a".to_owned(),
            ..v1::CommentAuthor::default()
        }),
        ..v1::Comment::default()
    };
    comment.set_kind(v1::CommentKind::HelpRequest);
    comment
}

fn instruction(id: &str, state: v1::InstructionState, actor: v1::TransitionActor) -> v1::Comment {
    let mut instruction = v1::Instruction::default();
    instruction.set_state(state);
    let mut author = v1::CommentAuthor::default();
    author.set_actor(actor);
    let mut comment = v1::Comment {
        id: id.to_owned(),
        instruction: Some(instruction),
        author: Some(author),
        ..v1::Comment::default()
    };
    comment.set_kind(v1::CommentKind::Instruction);
    comment
}

fn assignment(id: &str, state: v1::ConflictAssignmentState) -> v1::ConflictAssignment {
    let mut assignment = v1::ConflictAssignment {
        id: id.to_owned(),
        ..v1::ConflictAssignment::default()
    };
    assignment.set_state(state);
    assignment
}

fn reason_of(entry: &v1::ProposalEntry) -> Option<Reason> {
    rules::place(rules::PROPOSAL, entry)
}

#[test]
fn every_reason_is_in_exactly_one_table() {
    let tables = rules::tables();
    for reason in Reason::ALL {
        let count = tables
            .iter()
            .flatten()
            .filter(|named| **named == reason)
            .count();
        assert_eq!(count, 1, "{reason:?}");
    }
}

#[test]
fn needs_rows_come_before_watching_rows() {
    for table in rules::tables() {
        assert!(rules::needs_first(&table), "{table:?}");
    }
}

#[test]
fn only_sign_offs_missing_is_needs_you_now() {
    let entry = proposal(
        "c1",
        Some(status(v1::AutomaticState::Needs, &["signoff"])),
        1,
    );
    assert_eq!(reason_of(&entry), Some(Reason::Signoff));
    assert_eq!(
        words::proposal(Reason::Signoff, &entry),
        "Needs a sign-off from release"
    );
}

#[test]
fn a_failed_check_beside_a_sign_off_is_the_agents() {
    let entry = proposal(
        "c1",
        Some(status(v1::AutomaticState::Needs, &["signoff", "test"])),
        1,
    );
    assert_eq!(reason_of(&entry), Some(Reason::InReview));
    let cut = {
        let mut status = status(v1::AutomaticState::Needs, &["signoff"]);
        status.missing_total = 25;
        proposal("c1", Some(status), 1)
    };
    assert_eq!(reason_of(&cut), Some(Reason::InReview));
}

#[test]
fn returned_until_it_changes() {
    let mut entry = proposal("c1", None, 1);
    returned(&mut entry, false);
    assert_eq!(reason_of(&entry), Some(Reason::Returned));
    assert_eq!(
        words::proposal(Reason::Returned, &entry),
        "Came back from landing: a check failed"
    );
    returned(&mut entry, true);
    assert_eq!(reason_of(&entry), Some(Reason::InReview));
    let held = proposal("c2", Some(status(v1::AutomaticState::Returned, &[])), 1);
    assert_eq!(reason_of(&held), Some(Reason::Returned));
}

#[test]
fn a_returned_landing_the_service_retries_is_watched() {
    let mut retried = status(v1::AutomaticState::Returned, &[]);
    retried.reason = "busy".to_owned();
    retried.retrying = true;
    let mut entry = proposal("c1", Some(retried.clone()), 1);
    returned(&mut entry, false);
    assert_eq!(reason_of(&entry), Some(Reason::InReview));
    retried.retrying = false;
    let mut given_up = proposal("c2", Some(retried), 1);
    returned(&mut given_up, false);
    assert_eq!(reason_of(&given_up), Some(Reason::Returned));
}

#[test]
fn a_busy_return_the_service_has_not_seen_yet_is_watched() {
    let busy = |state: Option<v1::AutomaticState>| {
        let mut entry = proposal("c1", state.map(|state| status(state, &[])), 1);
        returned(&mut entry, false);
        entry
            .lifecycle
            .as_mut()
            .unwrap()
            .last
            .as_mut()
            .unwrap()
            .reason = "busy: main moved under 3 batches in a row".to_owned();
        reason_of(&entry)
    };
    assert_eq!(
        busy(Some(v1::AutomaticState::Queued)),
        Some(Reason::InReview),
        "the service queued it and will again"
    );
    assert_eq!(
        busy(Some(v1::AutomaticState::Ready)),
        Some(Reason::Returned),
        "a landing asked for by hand, on a destination that lands on request"
    );
    assert_eq!(busy(None), Some(Reason::Returned));
}

#[test]
fn a_rule_beside_a_sign_off_is_the_agents() {
    let entry = proposal(
        "c1",
        Some(status(v1::AutomaticState::Needs, &["signoff", "rule"])),
        1,
    );
    assert_eq!(reason_of(&entry), Some(Reason::InReview));
}

#[test]
fn no_executor_is_needs_you_now_and_off_is_watched() {
    let entry = proposal("c1", Some(status(v1::AutomaticState::NoExecutor, &[])), 1);
    assert_eq!(reason_of(&entry), Some(Reason::NoExecutor));
    let off = proposal("c2", Some(status(v1::AutomaticState::Off, &[])), 1);
    assert_eq!(reason_of(&off), Some(Reason::InReview));
    assert_eq!(
        words::proposal(Reason::InReview, &off),
        "In review; no policy judges it here"
    );
}

#[test]
fn instructions_by_person_and_agent() {
    use v1::InstructionState as S;
    use v1::TransitionActor as A;
    let place = |comment: &v1::Comment| rules::place(rules::INSTRUCTION, comment);
    assert_eq!(
        place(&instruction("i1", S::AwaitingConfirmation, A::Human)),
        Some(Reason::Confirm)
    );
    assert_eq!(
        place(&instruction("i2", S::AwaitingConfirmation, A::Agent)),
        None
    );
    assert_eq!(
        place(&instruction("i3", S::Running, A::Agent)),
        Some(Reason::InFlight)
    );
    assert_eq!(place(&instruction("i4", S::Done, A::Human)), None);
}

#[test]
fn classify_places_keys_and_orders_newest_first() {
    let mut workspace = v1::Workspace {
        id: "ws1".to_owned(),
        owner: "claude@b".to_owned(),
        last_snapshot_at: Some(at(5)),
        ..v1::Workspace::default()
    };
    workspace.set_attention(v1::Attention::WatchFailed);
    workspace.set_state(v1::WorkspaceState::Active);
    let buckets = classify(Sources {
        repo: repo(),
        proposals: vec![
            proposal(
                "c1",
                Some(status(v1::AutomaticState::Needs, &["signoff"])),
                3,
            ),
            proposal("c2", Some(status(v1::AutomaticState::Waiting, &[])), 4),
        ],
        help: vec![help("cmt1", 6)],
        instructions: vec![],
        assignments: vec![
            assignment("asg1", v1::ConflictAssignmentState::Unowned),
            assignment("asg2", v1::ConflictAssignmentState::Blocked),
        ],
        workspaces: vec![workspace],
        now: Timestamp(10),
    });
    let keys: Vec<&str> = buckets
        .needs_you_now
        .iter()
        .map(|item| item.key.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "help:aaaa000000000001:cmt1",
            "workspace:aaaa000000000001:ws1",
            "proposal:aaaa000000000001:c1",
            "conflict:aaaa000000000001:asg1",
            "conflict:aaaa000000000001:asg2",
        ]
    );
    assert_eq!(buckets.needs_you_now_total, 5);
    assert_eq!(buckets.watching.len(), 1);
    assert_eq!(buckets.watching[0].key, "proposal:aaaa000000000001:c2");
    assert_eq!(buckets.watching[0].headline, "Waiting for its checks");
    assert_eq!(
        buckets.needs_you_now[0].headline,
        "claude@a asks for help with a decision"
    );
    assert_eq!(
        buckets.needs_you_now[1].headline,
        "Workspace claude@b: its file watch could not be attached"
    );
    assert_eq!(
        buckets.needs_you_now[3].next,
        ["reef", "conflict", "assign", "asg1", "--queue"]
    );
    assert_eq!(
        buckets.needs_you_now[4].next,
        ["reef", "conflict", "allow", "asg2"]
    );
}

#[test]
fn a_bucket_is_cut_with_its_total() {
    let help: Vec<v1::Comment> = (0..250).map(|n| help(&format!("cmt{n}"), n)).collect();
    let buckets = classify(Sources {
        repo: repo(),
        help,
        ..Sources::empty(repo(), Timestamp(0))
    });
    assert_eq!(buckets.needs_you_now.len(), BUCKET_CAP);
    assert_eq!(buckets.needs_you_now_total, 250);
    assert_eq!(buckets.needs_you_now[0].key, "help:aaaa000000000001:cmt249");
    assert_eq!(buckets.keys.len(), 250, "the raise ledger keeps every key");
    assert!(
        buckets.keys.contains("help:aaaa000000000001:cmt0"),
        "past the cap"
    );
}

#[test]
fn refresh_kinds_are_the_triggers_union() {
    let operations = refresh_operations();
    assert!(operations.contains(&i32::from(OperationKind::Comment)));
    assert!(!operations.contains(&i32::from(OperationKind::Snapshot)));
    assert_eq!(
        refresh_presence(),
        [
            i32::from(PresenceKind::Automatic),
            i32::from(PresenceKind::Attention)
        ]
    );
}

#[test]
fn every_landing_reason_has_words() {
    let all = [
        "policy",
        "conflicted",
        "unlanded_parents",
        "amended",
        "destination",
        "busy",
        "failed",
        "checks",
        "unavailable",
        "empty",
        "alone",
        "dependency",
    ];
    for text in all {
        let reason = crate::landing::Reason::parse(text).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(reason.as_str(), text);
        assert!(!words::landing_words(reason).contains('_'), "{text}");
    }
}
