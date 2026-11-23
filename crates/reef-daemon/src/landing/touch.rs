//! A web landing's touch (R3.31, ADR 0086 decision 2): through the web UI,
//! `LandChange` lands a commit only when its tree carries a `signoff`/`pass`
//! approved by a passkey that the destination's `reef-policy.toml` pins for
//! its identity, at the origin the touch ran at (`[passkeys]`, R3.43, #346),
//! and that is still live, whatever else the policy asks. Accept makes that sign-off before it asks. A leaked
//! session or a script in the page can register a passkey (an identity's
//! first needs no assertion), but it cannot pin one: the pin is a line in
//! the destination's tree, and changing that tree takes a landing. Nor can
//! it bring back a pinned passkey a removal or reset ended: its public key
//! is never registered again.
//!
//! The tree is the one of the commit asked for: the head of the stack
//! prefix that lands. A sign-off on it counts for the members below it
//! (R3.10), as Accept on a member's page accepts up to it. A web session
//! belongs to no identity, so any pinned person's touch counts; the policy
//! still decides whose sign-offs the landing needs.

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::files;
use reef_lib::jj::land::{self, LandError};
use reef_lib::jj::log::ChangeInfo;
use reef_policy::input::Evidence;

use super::LandingError;
use crate::daemon::AppState;
use crate::error::Advice;
use crate::identity::audit;
use crate::model::RepoId;
use crate::policy::Loaded;
use crate::service::policy as policy_input;

/// What a web landing's tree lacks: `VERIFIED_SIGNOFF_REQUIRED`'s
/// `missing`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The destination's policy pins no passkey, so no touch can count.
    Pin(Unpinned),
    /// No passing sign-off by a person's passkey-verified key is on it.
    Touch,
    /// Its touches are by passkeys the destination's policy does not pin
    /// for their identity, or that were removed since (a reset included).
    PinnedTouch,
    /// A live passkey the policy pins touched it, at a web origin no pin of
    /// it names (#346): the person moved the web UI's port, or a look-alike
    /// page ran the ceremony.
    PinnedOrigin {
        /// Where the touch ran.
        touched: String,
        /// Where the policy counts that passkey's touches.
        pinned: Vec<String>,
    },
}

impl Missing {
    /// Its metadata value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pin(_) => "pin",
            Self::Touch => "touch",
            Self::PinnedTouch => "pinned_touch",
            Self::PinnedOrigin { .. } => "pinned_origin",
        }
    }

    /// The end of the refusal's message.
    fn words(&self) -> String {
        match self {
            Self::Pin(_) => "that policy pins no passkey".to_owned(),
            Self::Touch => "its tree has none".to_owned(),
            Self::PinnedTouch => {
                "its tree's were approved with passkeys that policy does not pin, or that were removed".to_owned()
            }
            Self::PinnedOrigin { touched, pinned } => format!(
                "its tree's was approved at {touched}, where that policy does not count the passkey: it pins it at {}",
                pinned.join(" and ")
            ),
        }
    }
}

/// Why a destination pins no passkey: `VERIFIED_SIGNOFF_REQUIRED`'s
/// `policy`, with `missing` `pin`. A destination with no policy, or one
/// that does not load, lands nothing from a terminal either, so its first
/// or mended policy is committed by hand (`docs/policy.md`); one that loads
/// takes a change adding the pin through `reef land`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unpinned {
    /// It has no `reef-policy.toml`.
    NoPolicy,
    /// Its `reef-policy.toml` does not load.
    InvalidPolicy,
    /// Its policy loads and has no `[passkeys]`.
    NoPins,
}

impl Unpinned {
    /// Its metadata value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoPolicy => "no_policy",
            Self::InvalidPolicy => "invalid_policy",
            Self::NoPins => "no_pins",
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
        let web = "Through the web UI, a landing counts only a touch of a passkey the destination's reef-policy.toml pins";
        match missing {
            Missing::Pin(Unpinned::NoPolicy) => Advice::run(
                format!(
                    "{web}, and {destination} has no reef-policy.toml, so nothing lands on it, from the web UI or with `reef land`, until a first one is committed to it by hand: `reef policy init` writes a starter; add the [passkeys] line `reef identity passkey list` prints, check it with `reef policy check`, commit it to {destination} and Accept again."
                ),
                ["reef", "policy", "init"],
            ),
            Missing::Pin(Unpinned::InvalidPolicy) => Advice::run(
                format!(
                    "{web}, and {destination}'s reef-policy.toml does not load, so nothing lands on it, from the web UI or with `reef land`, until a fix is committed to it by hand. Evaluating it names each problem; mend them, add the [passkeys] line `reef identity passkey list` prints, commit it to {destination} and Accept again."
                ),
                [
                    "reef",
                    "policy",
                    "eval",
                    "--policy-revision",
                    destination,
                    "-r",
                    commit,
                ],
            ),
            Missing::Pin(Unpinned::NoPins) => Advice::run(
                format!(
                    "{web}, and {destination}'s pins none. Land a change that adds the [passkeys] line `reef identity passkey list` prints to it, with its owners' sign-off, from a terminal (`reef land`), then Accept again; or land this change from a terminal: `reef land {change}`."
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
            Missing::PinnedOrigin { touched, pinned } => Advice::run(
                format!(
                    "A pinned passkey touched this at {touched}, and {destination}'s reef-policy.toml pins it at {}. If you moved the web UI's port (`[ui] listen`), pin the new origin with the line `reef identity passkey list` prints. If you did not, a page that is not Reef's asked for that touch: open Reef with `reef ui`, check the address, and sign this off again.",
                    pinned.join(" and ")
                ),
                sign_off,
            ),
        }
    }

    /// Its reason and metadata.
    pub(crate) fn reason(&self) -> (&'static str, Vec<(&'static str, String)>) {
        (
            reef_api::error::VERIFIED_SIGNOFF_REQUIRED,
            [
                ("missing", self.missing.as_str().to_owned()),
                ("change_id", self.change.clone()),
                ("commit_id", self.commit.clone()),
                ("tree_id", self.tree.clone()),
                ("destination", self.destination.clone()),
            ]
            .into_iter()
            .chain(match &self.missing {
                Missing::Pin(unpinned) => Some(("policy", unpinned.as_str().to_owned())),
                Missing::PinnedOrigin { touched, .. } => Some(("origin", touched.clone())),
                Missing::Touch | Missing::PinnedTouch => None,
            })
            .collect(),
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
    let loaded = match destination_policy(state, repo, destination).await? {
        None => return refuse(Missing::Pin(Unpinned::NoPolicy)),
        Some(Err(_)) => return refuse(Missing::Pin(Unpinned::InvalidPolicy)),
        Some(Ok(bytes)) => state.policy.load(&bytes),
    };
    let policy = match &*loaded {
        Loaded::Policy(policy) if policy.pins_passkeys() => policy,
        Loaded::Policy(_) => return refuse(Missing::Pin(Unpinned::NoPins)),
        Loaded::Invalid(_) => return refuse(Missing::Pin(Unpinned::InvalidPolicy)),
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
    let mut elsewhere = None;
    for evidence in touches {
        let key = &evidence.key;
        match policy.unpinned(key) {
            Some(reef_policy::Unpinned::Passkey) => {}
            _ if !live(state, evidence)? => {}
            None => return Ok(()),
            Some(reef_policy::Unpinned::Origin) => {
                elsewhere = Some(evidence);
            }
        }
    }
    let Some(evidence) = elsewhere else {
        return refuse(Missing::PinnedTouch);
    };
    audit::elsewhere(&evidence.key, &evidence.id);
    refuse(Missing::PinnedOrigin {
        touched: evidence.key.passkey_origin.clone(),
        pinned: policy
            .pins_of(&evidence.key)
            .map(|pin| pin.origin.to_string())
            .collect(),
    })
}

/// The bytes of the policy at `destination`'s head: `None` when it has
/// none, an error when the file cannot be read whole.
async fn destination_policy(
    state: &AppState,
    repo: RepoId,
    destination: &str,
) -> Result<Option<Result<Vec<u8>, String>>, LandingError> {
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
    Ok(file.map(|file| file.bytes))
}

/// Whether the passkey that approved `evidence`'s key, as the daemon
/// recorded when it certified the key, is still live: a removal or a reset
/// after the touch stops it counting, whatever is registered since.
fn live(state: &AppState, evidence: &Evidence) -> Result<bool, LandingError> {
    let key = &evidence.key;
    let approver = state
        .db
        .session_keys_by_id(&[key.id.as_str()])?
        .into_iter()
        .find_map(|row| row.passkey_id);
    let Some(approver) = approver else {
        return Ok(false);
    };
    Ok(state.db.passkey(&approver)?.is_some_and(|row| {
        row.removed_at.is_none()
            && row.identity_id == key.identity_id
            && row.fingerprint == key.passkey
    }))
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
