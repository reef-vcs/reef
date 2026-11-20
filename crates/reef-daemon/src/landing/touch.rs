//! A web landing's touch (R3.31, ADR 0086 decision 2): through the web UI,
//! `LandChange` lands a commit only when its tree carries a `signoff`/`pass`
//! by a person's passkey-verified key (R3.43), whatever the destination's
//! policy asks. Accept makes that sign-off before it asks, so a leaked
//! session or a script in the page cannot land without a person's touch.
//!
//! The tree is the one of the commit asked for: the head of the stack
//! prefix that lands. A sign-off on it counts for the members below it
//! (R3.10), as Accept on a member's page accepts up to it. A web session
//! belongs to no identity, so any person's verified sign-off counts; the
//! policy still decides whose it needs, and whether their passkey is pinned.

use reef_lib::jj::log::ChangeInfo;
use reef_policy::input::Evidence;

use super::LandingError;
use crate::daemon::AppState;
use crate::error::Advice;
use crate::model::RepoId;
use crate::service::policy as policy_input;

/// Refuses `info` unless its tree carries a person's touch
/// ([`touched`]): `VERIFIED_SIGNOFF_REQUIRED`.
pub(crate) async fn require(
    state: &AppState,
    repo: RepoId,
    info: &ChangeInfo,
) -> Result<(), LandingError> {
    let found = match &info.tree_id {
        Some(tree) => {
            policy_input::read_evidence(state, repo, tree)
                .await
                .map_err(|status| LandingError::Input(status.message().to_owned()))?
                .evidence
        }
        None => Vec::new(),
    };
    if found.iter().any(touched) {
        return Ok(());
    }
    Err(LandingError::VerifiedSignoffRequired {
        change: info.change_id.as_str().to_owned(),
        commit: info.commit_id.as_str().to_owned(),
        tree: info.tree_id.clone().unwrap_or_default(),
    })
}

/// [`LandingError::VerifiedSignoffRequired`]'s advice: how to sign off
/// `commit`'s tree.
pub(crate) fn advice(commit: &str) -> Advice {
    Advice::run(
        "A landing asked for through the web UI needs a person's touch: Accept on the proposal page signs the tree with one passkey touch and then asks to land it, or sign it off from a terminal and ask again.",
        [
            "reef",
            "evidence",
            "add",
            "--kind",
            "signoff",
            "--result",
            "pass",
            "--actor",
            "human",
            "--verified",
            "-r",
            commit,
        ],
    )
}

/// [`LandingError::VerifiedSignoffRequired`]'s reason and metadata.
pub(crate) fn reason(
    change: &str,
    commit: &str,
    tree: &str,
) -> (&'static str, Vec<(&'static str, String)>) {
    (
        reef_api::error::VERIFIED_SIGNOFF_REQUIRED,
        vec![
            ("change_id", change.to_owned()),
            ("commit_id", commit.to_owned()),
            ("tree_id", tree.to_owned()),
        ],
    )
}

/// Whether `evidence`, read with its signature valid, is a person's touch:
/// a passing sign-off by a person's key, bound when it was witnessed, whose
/// certificate a passkey approved for this statement and change. A read
/// that finds the approval does not match reports the presence as `none`.
fn touched(evidence: &Evidence) -> bool {
    let key = &evidence.key;
    evidence.kind == "signoff"
        && evidence.result == "pass"
        && key.presence == "user_verified"
        && key.status == "bound"
        && key.identity_kind == "person"
        && key.actor == "human"
}

#[cfg(test)]
mod tests {
    use reef_policy::input::Key;

    use super::*;

    fn verified() -> Evidence {
        Evidence {
            kind: "signoff".to_owned(),
            result: "pass".to_owned(),
            key: Key {
                status: "bound".to_owned(),
                identity_kind: "person".to_owned(),
                actor: "human".to_owned(),
                presence: "user_verified".to_owned(),
                ..Key::default()
            },
            ..Evidence::default()
        }
    }

    #[test]
    fn only_a_persons_verified_passing_signoff_is_a_touch() {
        assert!(touched(&verified()));
        let refused: [fn(&mut Evidence); 6] = [
            |e| "test".clone_into(&mut e.kind),
            |e| "fail".clone_into(&mut e.result),
            |e| "none".clone_into(&mut e.key.presence),
            |e| "revoked".clone_into(&mut e.key.status),
            |e| "ci".clone_into(&mut e.key.identity_kind),
            |e| "agent".clone_into(&mut e.key.actor),
        ];
        for change in refused {
            let mut evidence = verified();
            change(&mut evidence);
            assert!(!touched(&evidence), "{evidence:?}");
        }
    }
}
