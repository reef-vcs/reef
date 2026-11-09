//! What a policy sees about one change: the input the daemon builds, and
//! `reef policy check --input` reads back as JSON.
//!
//! CEL sees [`Input::change`], [`Input::evidence`],
//! [`Input::provenance`] and [`Input::landing`] as the variables `change`,
//! `evidence`, `provenance` and `landing`, and the policy's own groups as `groups`. Strings for
//! enumerations, integers (`int`, never `uint`) for counts and times in
//! milliseconds since the Unix epoch, and no `null`: an absent value is an
//! empty string, zero or an empty list.
//!
//! The shapes here are the one source the static check
//! ([`crate::check`]) and the JSON Schema ([`crate::schema`]) are derived
//! from, so neither can drift from what an expression is given.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One change, its evidence and its provenance, as the policy judges them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Input")]
pub struct Input {
    /// The change judged.
    pub change: Change,
    /// The evidence on `change.tree` whose signature verifies. The evaluator
    /// sorts it by witness time then id, fills each object's `self` and
    /// `attestors`, and keeps what the policy can use (see
    /// [`crate::EVIDENCE_TRUSTED_MAX`]).
    pub evidence: Vec<Evidence>,
    /// Every provenance record of the change (R3.04), by recorded time.
    pub provenance: Vec<Provenance>,
    /// Evidence on the tree left out before the evaluator saw it: objects
    /// whose signature does not verify. The evaluator adds the untrusted
    /// objects it drops over its cap.
    pub evidence_ignored: i64,
    /// Provenance records of the change the daemon did not read, past its
    /// cap: `builtin:input` blocks when there are any, since a rule over
    /// every record would not see them.
    pub provenance_ignored: i64,
    /// The change's own `reef-policy.toml`, when the change touches it:
    /// whether it is still present and what loading it reported.
    /// `builtin:policy-file` reads it; CEL does not.
    pub proposed_policy: ProposedPolicy,
    /// Where the change would land (R3.13): the destination and what
    /// rebasing onto it does. Empty outside a landing or its preview.
    pub landing: Landing,
}

/// What landing the change does (R3.13, ADR 0064): the destination it
/// lands onto, whether the change is rebased onto its head, and the paths
/// both the change and the destination touched since the change's parents.
/// A policy that trusts only fast-forward landings says `!landing.rebased`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Landing")]
pub struct Landing {
    /// The destination bookmark; empty when nothing was previewed.
    pub destination: String,
    /// Its head: the commit the change lands on.
    pub destination_commit: String,
    /// Whether the change is rebased onto `destination_commit`; false for
    /// a fast-forward, whose landed tree is the judged one.
    pub rebased: bool,
    /// Paths both the change and the destination changed since the
    /// change's parents, sorted, at most [`crate::MAX_OVERLAP_PATHS`].
    pub overlap: Vec<String>,
    /// How many there are; more than `overlap`'s length when it was cut.
    pub overlap_total: i64,
}

/// The change judged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Change")]
pub struct Change {
    /// The change id, as hex.
    pub id: String,
    /// The commit judged.
    pub commit: String,
    /// Its tree: the one evidence must be about.
    pub tree: String,
    /// Its parents' commit ids.
    pub parents: Vec<String>,
    /// Its full description.
    pub description: String,
    /// Its author.
    pub author: Author,
    /// Whether the change has no diff against its parents.
    pub empty: bool,
    /// Whether its tree has a conflict.
    pub conflicted: bool,
    /// Every touched path, sorted: a rename contributes its old and new
    /// path.
    pub paths: Vec<String>,
    /// Each touched file.
    pub files: Vec<File>,
    /// Lines added over `files`.
    pub lines_added: i64,
    /// Lines removed over `files`.
    pub lines_removed: i64,
    /// Its lifecycle state (R3.09): `draft`, `proposed`, `queued`,
    /// `landed`, `declined`, `withdrawn` or `superseded`; `queued` inside a
    /// landing (R3.13).
    pub state: String,
}

/// A commit's author.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Author")]
pub struct Author {
    /// The name.
    pub name: String,
    /// The email address.
    pub email: String,
}

/// One touched file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.File")]
pub struct File {
    /// Its path after the change.
    pub path: String,
    /// Its path before, for a rename or a copy; empty otherwise.
    pub old_path: String,
    /// `added`, `modified`, `deleted`, `renamed` or `copied`.
    pub status: String,
    /// Whether either side is binary; a binary file counts no lines.
    pub binary: bool,
    /// Whether the diff was too large to count; such a file counts no
    /// lines.
    pub too_large: bool,
    /// Lines added.
    pub lines_added: i64,
    /// Lines removed.
    pub lines_removed: i64,
    /// The new side's size in bytes; 0 for a deletion.
    pub size: i64,
}

/// One evidence object on the change's tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Evidence")]
pub struct Evidence {
    /// The evidence id.
    pub id: String,
    /// What was checked: `test`, `build`, `lint`, `signoff`, ...
    pub kind: String,
    /// `pass`, `fail` or `error` (no verdict).
    pub result: String,
    /// The target covered; empty for the whole check.
    pub target: String,
    /// When the check started, as the attestor claims.
    pub started_at: i64,
    /// When it finished, as the attestor claims.
    pub finished_at: i64,
    /// When this daemon witnessed it: the one time a signer cannot claim.
    pub added_at: i64,
    /// The `reef-agent` of the caller that added it, unverified.
    pub added_by_agent: String,
    /// The artifacts it names.
    pub artifacts: Vec<Artifact>,
    /// The attestor's own data, unverified: whatever its producer wrote.
    #[schemars(with = "BTreeMap<String, serde_json::Value>")]
    pub annotations: BTreeMap<String, serde_json::Value>,
    /// The signer's key and the identity bound to it at witness time.
    pub key: Key,
    /// Whether an executor job accepted it fresh (R3.05).
    pub fresh: bool,
    /// The job that accepted it, if any.
    pub job: String,
    /// Whether it came from the change itself: from a session or an agent
    /// that recorded one of the change's provenance records. Computed by the
    /// evaluator; a value in the input is replaced.
    #[serde(rename = "self")]
    pub from_self: bool,
    /// The names of the policy's attestor sets it satisfies. Computed by the
    /// evaluator; a value in the input is replaced.
    pub attestors: Vec<String>,
}

/// One artifact an evidence object names.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Artifact")]
pub struct Artifact {
    /// Its name.
    pub name: String,
    /// Its SHA-256, as hex.
    pub sha256: String,
    /// Its media type.
    pub media_type: String,
    /// `stored`, `collected` or `absent` (R3.02).
    pub state: String,
    /// Its size in bytes, when stored.
    pub size: i64,
}

/// A signer's key and its identity (R3.03's `KeyIdentity`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Key")]
pub struct Key {
    /// The key id.
    pub id: String,
    /// `unbound`, `bound`, `expired`, `revoked` or `invalid`, at witness
    /// time.
    pub status: String,
    /// The identity's name, from the certificate.
    pub identity: String,
    /// The identity's id (`idn_` and 16 hex).
    pub identity_id: String,
    /// `person` or `ci`.
    pub identity_kind: String,
    /// `human`, `agent` or `executor`.
    pub actor: String,
    /// The agent label the key was certified with.
    pub agent: String,
    /// The session the key was certified for.
    pub session: String,
    /// `none` or `user_verified`.
    pub presence: String,
    /// `client` or `daemon`.
    pub custody: String,
    /// The certifying authority's key id: the daemon's, or for a keyless
    /// key the Fulcio CA's.
    pub authority: String,
    /// Who certified it: `reef-daemon`, or `sigstore` for a keyless key
    /// (R3.42).
    pub issuer_kind: String,
    /// A keyless key's OIDC issuer; empty otherwise.
    pub oidc_issuer: String,
    /// A keyless key's OIDC subject (an email or a URI); empty otherwise.
    pub oidc_subject: String,
}

/// One provenance record of the change (R3.04).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Provenance")]
pub struct Provenance {
    /// The record id.
    pub id: String,
    /// The commit it is about.
    pub commit: String,
    /// That commit's tree.
    pub tree: String,
    /// The workspace that recorded it.
    pub workspace: String,
    /// The workspace's provenance session.
    pub session: String,
    /// Its place in the session.
    pub sequence: i64,
    /// What recorded it: `snapshot` or `edit`.
    pub kind: String,
    /// The workspace's agent label.
    pub agent: String,
    /// When it was recorded.
    pub recorded_at: i64,
    /// The declared model.
    pub model: String,
    /// The declared harness.
    pub harness: String,
    /// The declared task reference.
    pub task: String,
    /// The declared instruction's SHA-256.
    pub instruction_sha256: String,
    /// The client's own session id.
    pub client_session: String,
    /// `valid` or `invalid`.
    pub signature: String,
    /// The key that signed it.
    pub key: Key,
}

/// The change's own `reef-policy.toml`, for `builtin:policy-file`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.ProposedPolicy")]
pub struct ProposedPolicy {
    /// Whether the change's tree has the file; false when the change does
    /// not touch it, or deletes it.
    pub present: bool,
    /// Its SHA-256, when present.
    pub sha256: String,
    /// What loading it reported; empty when it loads.
    pub diagnostics: Vec<crate::Diagnostic>,
}

/// What `[[conflicts.rule]]` sees about one conflict (R3.25 to R3.28 fill
/// it from conflict objects and may add fields).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.ConflictInput")]
pub struct ConflictInput {
    /// The conflicted change.
    pub change_id: String,
    /// The conflicted paths.
    pub paths: Vec<String>,
    /// Each side.
    pub sides: Vec<ConflictSide>,
}

/// One side of a conflict.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.ConflictSide")]
pub struct ConflictSide {
    /// The side's change.
    pub change_id: String,
    /// The identity that recorded it.
    pub identity: String,
    /// The agent that recorded it.
    pub agent: String,
}

/// What `reef policy check --input` reads: an input to evaluate, or a
/// conflict to look a tier up for (an object whose only key is
/// `conflict`).
#[derive(Clone, Debug, PartialEq)]
pub enum Case {
    /// A change's input.
    Change(Box<Input>),
    /// A conflict.
    Conflict(ConflictInput),
}

impl Case {
    /// Reads a case from JSON.
    pub fn from_json(bytes: &[u8]) -> Result<Case, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Conflict {
            conflict: ConflictInput,
        }
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| format!("not JSON: {error}"))?;
        if value.get("conflict").is_some() {
            serde_json::from_value::<Conflict>(value)
                .map(|case| Case::Conflict(case.conflict))
                .map_err(|error| format!("not a conflict input: {error}"))
        } else {
            serde_json::from_value::<Input>(value)
                .map(|input| Case::Change(Box::new(input)))
                .map_err(|error| {
                    format!("not a policy input (`reef schema policy input`): {error}")
                })
        }
    }
}
