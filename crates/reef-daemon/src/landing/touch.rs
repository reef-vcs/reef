//! A web landing's touch (R3.31, ADR 0086 decision 2): through the web UI,
//! `LandChange` lands a commit only when its tree carries a `signoff`/`pass`
//! approved by a passkey that the destination's `reef-policy.toml` pins for
//! its identity (`[passkeys]`, R3.43) and that is still live, whatever else
//! the policy asks. Accept makes that sign-off before it asks. A leaked
//! session or a script in the page can register a passkey (an identity's
//! first needs no assertion), but it cannot pin one: the pin is a line in
//! the destination's tree, and changing that tree takes a landing.
//!
//! The tree is the one of the commit asked for: the head of the stack
//! prefix that lands. A sign-off on it counts for the members below it
//! (R3.10), as Accept on a member's page accepts up to it. A web session
//! belongs to no identity, so any pinned person's touch counts; the policy
//! still decides whose sign-offs the landing needs.

use std::sync::Arc;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::files;
use reef_lib::jj::land::{self, LandError};
use reef_lib::jj::log::ChangeInfo;
use reef_policy::input::Evidence;
use reef_policy::{Policy, Presence};

use super::LandingError;
use crate::daemon::AppState;
use crate::error::Advice;
use crate::model::RepoId;
use crate::policy::Loaded;
use crate::service::policy as policy_input;

/// What a web landing's tree lacks: `VERIFIED_SIGNOFF_REQUIRED`'s
/// `missing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The destination's policy pins no passkey (or it has no policy, or
    /// its policy does not load), so no touch can count.
    Pin,
    /// No passing sign-off by a person's passkey-verified key is on it.
    Touch,
    /// Its touches are by passkeys the destination's policy does not pin
    /// for their identity, or that were removed since (a reset included).
    PinnedTouch,
}

impl Missing {
    /// Its metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Touch => "touch",
            Self::PinnedTouch => "pinned_touch",
        }
    }

    /// The end of the refusal's message.
    fn words(self) -> &'static str {
        match self {
            Self::Pin => "that policy pins no passkey",
            Self::Touch => "its tree has none",
            Self::PinnedTouch => {
                "its tree's were approved with passkeys that policy does not pin, or that were removed"
            }
        }
    }
}

/// A web landing refused for want of a touch: `VERIFIED_SIGNOFF_REQUIRED`.
#[derive(Debug)]
pub struct Untouched {
    /// What it lacks.
    pub missing: Missing,
    /// The change.
    pub change: String,
    /// The commit asked for.
    pub commit: String,
    /// Its tree; empty when conflicted.
    pub tree: String,
    /// The bookmark it is proposed for.
    pub destination: String,
}

impl std::fmt::Display for Untouched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "through the web UI, commit {} lands only with a sign-off on its tree approved by a passkey {}'s reef-policy.toml pins, and {}",
            self.commit,
            self.destination,
            self.missing.words()
        )
    }
}

impl Untouched {
    /// Its advice.
    pub(crate) fn advice(&self) -> Advice {
        let Self {
            missing,
            change,
            commit,
            destination,
            ..
        } = self;
        let sign_off = [
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
        ];
        match missing {
            Missing::Pin => Advice::run(
                format!(
                    "Through the web UI, a landing counts only a touch of a passkey the destination's reef-policy.toml pins, and {destination}'s pins none. Add the [passkeys] line `reef identity passkey list` prints to reef-policy.toml and land that from a terminal, or land this change from a terminal: `reef land {change}`."
                ),
                ["reef", "identity", "passkey", "list"],
            ),
            Missing::Touch => Advice::run(
                format!(
                    "A landing asked for through the web UI needs a person's touch with a passkey {destination}'s reef-policy.toml pins: Accept on the proposal page signs the tree with one touch and then asks to land it, or sign it off from a terminal and ask again."
                ),
                sign_off,
            ),
            Missing::PinnedTouch => Advice::run(
                format!(
                    "The tree's sign-offs were approved with passkeys {destination}'s reef-policy.toml does not pin, or that were removed since. Sign it off again with a pinned passkey; `reef identity passkey list` shows each passkey and the [passkeys] line that pins it."
                ),
                sign_off,
            ),
        }
    }

    /// Its reason and metadata.
    pub(crate) fn reason(&self) -> (&'static str, Vec<(&'static str, String)>) {
        (
            reef_api::error::VERIFIED_SIGNOFF_REQUIRED,
            vec![
                ("missing", self.missing.as_str().to_owned()),
                ("change_id", self.change.clone()),
                ("commit_id", self.commit.clone()),
                ("tree_id", self.tree.clone()),
                ("destination", self.destination.clone()),
            ],
        )
    }
}

/// Refuses `info`, proposed for `destination`, unless its tree carries a
/// touch ([`touched`]) by a live passkey the destination's policy pins:
/// `VERIFIED_SIGNOFF_REQUIRED`.
pub(crate) async fn require(
    state: &AppState,
    repo: RepoId,
    info: &ChangeInfo,
    destination: &str,
) -> Result<(), LandingError> {
    let refuse = |missing| {
        Err(LandingError::VerifiedSignoffRequired(Box::new(Untouched {
            missing,
            change: info.change_id.as_str().to_owned(),
            commit: info.commit_id.as_str().to_owned(),
            tree: info.tree_id.clone().unwrap_or_default(),
            destination: destination.to_owned(),
        })))
    };
    let loaded = destination_policy(state, repo, destination).await?;
    let policy = match loaded.as_deref() {
        Some(Loaded::Policy(policy)) if policy.pins_passkeys() => policy,
        _ => return refuse(Missing::Pin),
    };
    let found = match &info.tree_id {
        Some(tree) => {
            policy_input::read_evidence(state, repo, tree)
                .await
                .map_err(|status| LandingError::Input(status.message().to_owned()))?
                .evidence
        }
        None => Vec::new(),
    };
    let mut touches = found.iter().filter(|evidence| touched(evidence)).peekable();
    if touches.peek().is_none() {
        return refuse(Missing::Touch);
    }
    for evidence in touches {
        if pinned(policy, evidence) && live(state, evidence)? {
            return Ok(());
        }
    }
    refuse(Missing::PinnedTouch)
}

/// The policy at `destination`'s head, loaded; `None` when it has none or
/// the file cannot be read whole.
async fn destination_policy(
    state: &AppState,
    repo: RepoId,
    destination: &str,
) -> Result<Option<Arc<Loaded>>, LandingError> {
    let bookmark = destination.to_owned();
    let file = state
        .manager
        .handle(repo)?
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await?;
                let commit = match land::destination_head(&repo, &bookmark) {
                    Ok(commit) => commit,
                    Err(LandError::DestinationNotFound(_)) => {
                        return Err(LandingError::DestinationNotFound(bookmark));
                    }
                    Err(LandError::DestinationConflicted(_)) => {
                        return Err(LandingError::DestinationConflicted(bookmark));
                    }
                    Err(error) => return Err(LandingError::Input(ErrorChain(&error).to_string())),
                };
                let tree = files::commit_tree(&repo, &commit)
                    .await
                    .map_err(|error| LandingError::Input(ErrorChain(&error).to_string()))?;
                policy_input::policy_file(&tree)
                    .await
                    .map_err(|status| LandingError::Input(status.message().to_owned()))
            }
            .boxed_local()
        })
        .await??;
    Ok(file
        .and_then(|file| file.bytes.ok())
        .map(|bytes| state.policy.load(&bytes)))
}

/// Whether `policy` pins the passkey that approved `evidence`'s key, for
/// the key's identity: its presence counts as `user_verified` there.
fn pinned(policy: &Policy, evidence: &Evidence) -> bool {
    policy.presence(&evidence.key) == Some(Presence::UserVerified)
}

/// Whether the passkey that approved `evidence`'s key is still one of its
/// identity's live passkeys: a removal or a reset after the touch stops it
/// counting.
fn live(state: &AppState, evidence: &Evidence) -> Result<bool, LandingError> {
    let key = &evidence.key;
    if key.identity_id.is_empty() || key.passkey.is_empty() {
        return Ok(false);
    }
    Ok(state
        .db
        .passkeys(Some(&key.identity_id), false)?
        .iter()
        .any(|row| row.fingerprint == key.passkey))
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
