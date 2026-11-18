//! Resolution tiers and attempt caps (R3.28, ADR 0082; `docs/conflicts.md`,
//! "Tiers and attempts").
//!
//! Policy gives each assignment's conflict a tier (`land`, `approve`,
//! `ask`) and an attempt cap ([`TierColumns`], looked up by the follower).
//! An attempt is one resolver's turn: it starts when a resolver is handed
//! the conflict (its default owner, a claim, an assignment, a reopen), and
//! the same agent carrying on (followed, returned) is the same attempt.
//! A hand-off that would start an attempt the tier or the cap does not
//! allow leaves the conflict `blocked` for a person ([`blocked`]); a
//! person's go-ahead ([`grant`]) lets it go on. When the conflict goes,
//! [`approval`] says whether landing the resolution needs a person's
//! sign-off, which `builtin:resolution` enforces at the gate.

use reef_policy::{DEFAULT_ATTEMPTS, DEFAULT_TIER, Tier};

use super::{Reason, State};
use crate::db::{AssignmentRow, OwnerColumns, TierColumns};
use crate::model::Timestamp;

/// The most attempts one grant adds.
pub const MAX_GRANT: u32 = 20;

/// Why a new attempt cannot start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// The tier is `ask`, and no person gave the go-ahead.
    Ask,
    /// The attempts are used up.
    Attempts,
}

impl Block {
    /// The reason a blocked row records.
    pub fn reason(self) -> Reason {
        match self {
            Block::Ask => Reason::Ask,
            Block::Attempts => Reason::Attempts,
        }
    }

    /// Its name in errors: `ask` or `attempts`.
    pub fn as_str(self) -> &'static str {
        match self {
            Block::Ask => "ask",
            Block::Attempts => "attempts",
        }
    }
}

/// `row`'s tier: as looked up, else the default a policy without
/// `[conflicts]` gives.
pub fn tier_of(row: &AssignmentRow) -> Tier {
    row.tier
        .tier
        .as_deref()
        .and_then(Tier::parse)
        .unwrap_or(DEFAULT_TIER)
}

/// The attempts `row` allows: the policy's cap plus what people granted.
pub fn limit(row: &AssignmentRow) -> u32 {
    let cap = row
        .tier
        .cap
        .unwrap_or(u32::try_from(DEFAULT_ATTEMPTS).unwrap_or(3));
    cap.saturating_add(row.granted)
}

/// Why `row` cannot start a new attempt, if it cannot.
pub fn blocked(row: &AssignmentRow) -> Option<Block> {
    if tier_of(row) == Tier::Ask && row.go_ahead_at.is_none() {
        Some(Block::Ask)
    } else if row.attempts >= limit(row) {
        Some(Block::Attempts)
    } else {
        None
    }
}

/// Whether `row` moving to `to` for `reason`, owned by `owner`, starts an
/// attempt: a resolver handed the conflict. The same session keeping it (a
/// priority change) and the same agent carrying on (followed, returned)
/// do not.
pub fn starts_attempt(
    row: &AssignmentRow,
    to: State,
    reason: Reason,
    owner: &OwnerColumns,
) -> bool {
    to == State::Assigned
        && matches!(
            reason,
            Reason::Default | Reason::Claimed | Reason::Assigned | Reason::Allowed
        )
        && !(row.state == State::Assigned.as_str() && row.owner.session_id == owner.session_id)
}

/// Where the follower's hand-off of `row` to `to` for `reason` lands: the
/// same, or `blocked` when it would start an attempt, or fill the queue
/// with one, that the tier or the cap does not allow. The owner it would
/// have gone to is kept either way.
pub fn hand_off(
    row: &AssignmentRow,
    to: State,
    reason: Reason,
    owner: &OwnerColumns,
) -> (State, Reason) {
    let gated = starts_attempt(row, to, reason, owner) || to == State::Queued;
    match blocked(row) {
        Some(block) if gated => (State::Blocked, block.reason()),
        _ => (to, reason),
    }
}

/// A person's go-ahead on `row` (`identity`, at `at`): records it, and adds
/// `attempts` (when given), or one when the row is out of attempts, so the
/// next hand-off may start one.
pub fn grant(
    row: &mut AssignmentRow,
    identity: Option<&str>,
    attempts: Option<u32>,
    at: Timestamp,
) {
    row.go_ahead_identity_id = identity.map(str::to_owned);
    row.go_ahead_at = Some(at);
    let add = match attempts {
        Some(n) => n,
        None if row.attempts >= limit(row) => 1,
        None => 0,
    };
    if row.attempts > limit(row) {
        // Attempts started past the limit (a person's assignments) are
        // granted first, so `add` is room for new ones.
        row.granted = row.granted.saturating_add(row.attempts - limit(row));
    }
    row.granted = row.granted.saturating_add(add);
}

/// Whether landing a resolution of `row` (as it was when the conflict
/// went) needs a person's sign-off: not when a person resolved it (the
/// resolving session's actor is `human`, or the conflict was taken and no
/// agent's session wrote the resolution); otherwise when the tier is not
/// `land`, or the conflict was blocked when it was resolved.
pub fn approval(row: &AssignmentRow, resolver_actor: Option<&str>) -> bool {
    let by_person = resolver_actor == Some("human")
        || (row.state == State::Taken.as_str() && resolver_actor != Some("agent"));
    !by_person && (tier_of(row) != Tier::Land || row.state == State::Blocked.as_str())
}

/// How strict a tier is.
fn rank(tier: Option<&str>) -> u8 {
    match tier.and_then(Tier::parse) {
        Some(Tier::Land) => 0,
        Some(Tier::Approve) | None => 1,
        Some(Tier::Ask) => 2,
    }
}

/// `looked`, a fresh lookup, kept against `old`, what the row held. Under
/// the same policy, a conflict that narrows (the resolver fixed the files a
/// strict rule covers first) never lowers its tier nor raises its cap: the
/// resolver's own progress must not lift a requirement (S-230). A first
/// lookup, another policy, or one replacing a failed lookup takes `looked`.
pub fn merged(old: &TierColumns, looked: TierColumns) -> TierColumns {
    if old.tier.is_none() || old.error.is_some() || old.policy_sha256 != looked.policy_sha256 {
        return looked;
    }
    let mut kept = if rank(looked.tier.as_deref()) > rank(old.tier.as_deref()) {
        looked.clone()
    } else {
        old.clone()
    };
    kept.cap = match (old.cap, looked.cap) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    kept.conflict_id = looked.conflict_id;
    kept.policy_commit = looked.policy_commit;
    kept.at = looked.at;
    kept
}

/// `columns` for `tier` and `attempts` from rule `rule` of the policy
/// `sha256` at `commit`, looked up for `conflict_id` at `at`.
pub fn decided(
    decision: Result<reef_policy::ConflictDecision, String>,
    commit: &str,
    sha256: Option<&str>,
    conflict_id: &str,
    at: Timestamp,
) -> TierColumns {
    let (tier, cap, rule, error) = match decision {
        Ok(decision) => (
            decision.tier,
            u32::try_from(decision.attempts.max(0)).unwrap_or(u32::MAX),
            decision.rule,
            None,
        ),
        Err(error) => (
            Tier::Ask,
            u32::try_from(DEFAULT_ATTEMPTS).unwrap_or(3),
            None,
            Some(error),
        ),
    };
    TierColumns {
        tier: Some(tier.as_str().to_owned()),
        cap: Some(cap),
        rule,
        conflict_id: Some(conflict_id.to_owned()),
        policy_commit: Some(commit.to_owned()),
        policy_sha256: sha256.map(str::to_owned),
        error,
        at: Some(at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepoId;

    fn row(state: State, tier: &str, attempts: u32) -> AssignmentRow {
        AssignmentRow {
            repo_id: RepoId::generate(),
            id: "asg_0000000000000001".to_owned(),
            change_id: "c".to_owned(),
            kind: "tree".to_owned(),
            commit_id: "1".to_owned(),
            conflict_id: "cfl".to_owned(),
            first_conflict_id: "cfl".to_owned(),
            introduced: 1,
            files_total: 1,
            state: state.as_str().to_owned(),
            reason: Reason::Default.as_str().to_owned(),
            owner: owner("ses_a"),
            priority: 0,
            queued_at: None,
            claims: 0,
            events: 1,
            opened_at: Timestamp(0),
            updated_at: Timestamp(0),
            conflict_changed_at: Timestamp(0),
            closed_at: None,
            closed_commit_id: None,
            resolved_session_id: None,
            tier: TierColumns {
                tier: Some(tier.to_owned()),
                cap: Some(2),
                ..TierColumns::default()
            },
            attempts,
            granted: 0,
            go_ahead_identity_id: None,
            go_ahead_at: None,
            approval: false,
        }
    }

    fn owner(session: &str) -> OwnerColumns {
        OwnerColumns {
            session_id: Some(session.to_owned()),
            ..OwnerColumns::default()
        }
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one row per case of the table")]
    fn hand_off_table() {
        use Reason as R;
        use State as S;
        // (state, tier, attempts, go-ahead, to, reason, owner) -> (state, reason, starts)
        let rows = [
            // The default owner of a land conflict starts attempt 1.
            (
                S::Closed,
                "land",
                0,
                false,
                S::Assigned,
                R::Default,
                "ses_b",
                (S::Assigned, R::Default, true),
            ),
            // An ask conflict waits for a person, with no attempt started.
            (
                S::Closed,
                "ask",
                0,
                false,
                S::Assigned,
                R::Default,
                "ses_b",
                (S::Blocked, R::Ask, true),
            ),
            (
                S::Closed,
                "ask",
                0,
                true,
                S::Assigned,
                R::Default,
                "ses_b",
                (S::Assigned, R::Default, true),
            ),
            // A claim within the cap starts one; at the cap it is blocked.
            (
                S::Queued,
                "approve",
                1,
                false,
                S::Assigned,
                R::Claimed,
                "ses_b",
                (S::Assigned, R::Claimed, true),
            ),
            (
                S::Queued,
                "approve",
                2,
                false,
                S::Assigned,
                R::Claimed,
                "ses_b",
                (S::Blocked, R::Attempts, true),
            ),
            // Following and returning carry the attempt on, even at the cap.
            (
                S::Assigned,
                "approve",
                2,
                false,
                S::Assigned,
                R::Followed,
                "ses_b",
                (S::Assigned, R::Followed, false),
            ),
            (
                S::Queued,
                "approve",
                2,
                false,
                S::Assigned,
                R::Returned,
                "ses_b",
                (S::Assigned, R::Returned, false),
            ),
            (
                S::Queued,
                "approve",
                1,
                false,
                S::Assigned,
                R::Returned,
                "ses_b",
                (S::Assigned, R::Returned, false),
            ),
            // A priority change keeps the session: no attempt.
            (
                S::Assigned,
                "approve",
                2,
                false,
                S::Assigned,
                R::Assigned,
                "ses_a",
                (S::Assigned, R::Assigned, false),
            ),
            // The last attempt ending leaves it for a person, not the queue.
            (
                S::Assigned,
                "land",
                2,
                false,
                S::Queued,
                R::OwnerEnded,
                "ses_a",
                (S::Blocked, R::Attempts, false),
            ),
            (
                S::Assigned,
                "land",
                1,
                false,
                S::Queued,
                R::Released,
                "ses_a",
                (S::Queued, R::Released, false),
            ),
            // A person taking it over is not an attempt.
            (
                S::Blocked,
                "ask",
                0,
                false,
                S::Taken,
                R::Taken,
                "",
                (S::Taken, R::Taken, false),
            ),
        ];
        for (state, tier, attempts, go_ahead, to, reason, session, expected) in rows {
            let mut r = row(state, tier, attempts);
            if go_ahead {
                r.go_ahead_at = Some(Timestamp(1));
            }
            let o = owner(session);
            let (got_state, got_reason) = hand_off(&r, to, reason, &o);
            let starts = starts_attempt(&r, to, reason, &o);
            assert_eq!(
                (got_state, got_reason, starts),
                expected,
                "{state:?} {tier} {attempts} {go_ahead} -> {to:?} {reason:?}"
            );
        }
    }

    #[test]
    fn grants_make_room() {
        let mut r = row(State::Blocked, "land", 2);
        assert_eq!(blocked(&r), Some(Block::Attempts));
        grant(&mut r, Some("idn_1"), None, Timestamp(5));
        assert_eq!((r.granted, limit(&r), blocked(&r)), (1, 3, None));
        assert_eq!(r.go_ahead_at, Some(Timestamp(5)));
        // An ask conflict under its cap: the go-ahead alone unblocks it.
        let mut ask = row(State::Blocked, "ask", 0);
        assert_eq!(blocked(&ask), Some(Block::Ask));
        grant(&mut ask, Some("idn_1"), None, Timestamp(5));
        assert_eq!((ask.granted, blocked(&ask)), (0, None));
        // Explicit attempts are added; attempts past the limit first.
        let mut over = row(State::Assigned, "land", 4);
        grant(&mut over, None, Some(2), Timestamp(5));
        assert_eq!((limit(&over), over.attempts), (6, 4));
    }

    #[test]
    fn approval_table() {
        // (state at close, tier, resolver actor) -> needs approval
        let rows = [
            (State::Assigned, "land", Some("agent"), false),
            (State::Assigned, "land", None, false),
            (State::Assigned, "approve", Some("agent"), true),
            (State::Assigned, "approve", None, true),
            (State::Assigned, "ask", Some("agent"), true),
            (State::Blocked, "land", Some("agent"), true),
            (State::Queued, "land", None, false),
            (State::Assigned, "approve", Some("human"), false),
            (State::Taken, "ask", None, false),
            (State::Taken, "approve", Some("agent"), true),
            (State::Blocked, "ask", Some("human"), false),
        ];
        for (state, tier, actor, expected) in rows {
            assert_eq!(
                approval(&row(state, tier, 1), actor),
                expected,
                "{state:?} {tier} {actor:?}"
            );
        }
    }

    #[test]
    fn a_narrowing_conflict_keeps_its_strictest_tier() {
        let columns = |tier: &str, cap: u32, sha: &str, error: Option<&str>| TierColumns {
            tier: Some(tier.to_owned()),
            cap: Some(cap),
            policy_sha256: Some(sha.to_owned()),
            error: error.map(str::to_owned),
            ..TierColumns::default()
        };
        let pick = |old: TierColumns, new: TierColumns| {
            let got = merged(&old, new);
            (got.tier.unwrap(), got.cap.unwrap())
        };
        // Same policy: never less strict, never more attempts.
        assert_eq!(
            pick(
                columns("approve", 3, "p", None),
                columns("land", 5, "p", None)
            ),
            ("approve".to_owned(), 3)
        );
        assert_eq!(
            pick(columns("land", 5, "p", None), columns("ask", 2, "p", None)),
            ("ask".to_owned(), 2)
        );
        // Another policy, or a failed lookup before: the new lookup.
        assert_eq!(
            pick(columns("ask", 3, "p", None), columns("land", 5, "q", None)),
            ("land".to_owned(), 5)
        );
        assert_eq!(
            pick(
                columns("ask", 3, "p", Some("x")),
                columns("land", 5, "p", None)
            ),
            ("land".to_owned(), 5)
        );
        assert_eq!(
            pick(TierColumns::default(), columns("land", 5, "p", None)),
            ("land".to_owned(), 5)
        );
    }

    #[test]
    fn a_failed_lookup_is_ask() {
        let columns = decided(Err("bad".to_owned()), "c0", Some("ab"), "cfl", Timestamp(1));
        assert_eq!(columns.tier.as_deref(), Some("ask"));
        assert_eq!(columns.error.as_deref(), Some("bad"));
        let ok = decided(
            Ok(reef_policy::ConflictDecision {
                tier: Tier::Land,
                attempts: 5,
                rule: Some(0),
            }),
            "c0",
            None,
            "cfl",
            Timestamp(1),
        );
        assert_eq!(
            (ok.tier.as_deref(), ok.cap, ok.rule, ok.policy_sha256),
            (Some("land"), Some(5), Some(0), None)
        );
    }
}
