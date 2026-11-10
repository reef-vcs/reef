//! The policy file, the input and the evaluation as JSON Schema, which
//! `reef schema policy` prints.
//!
//! The file's schema describes `reef-policy.toml` read as JSON: the same
//! keys and types, with every table closed to unknown keys. It is
//! documentation and editor support; [`crate::Policy::load`] is what
//! checks a file.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ConflictDecision, Evaluation, Input};

/// `reef-policy.toml`, format version 1.
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "reef-policy.toml", rename = "policy.PolicyFile")]
struct File {
    /// The format version: 1.
    version: u32,
    /// Who signs off a change to `reef-policy.toml` and `reef-checks.toml`.
    owners: Owners,
    /// Named sets of identities, by name or by id (`idn_…`).
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
    /// Named sets of signers whose evidence counts.
    #[serde(default)]
    attestors: BTreeMap<String, Attestor>,
    /// Evidence a change needs.
    #[serde(default)]
    require: Vec<Require>,
    /// Size limits.
    limits: Option<Limits>,
    /// Paths no change may touch.
    #[serde(default)]
    forbid: Vec<Forbid>,
    /// Areas that need a person's sign-off.
    #[serde(default)]
    signoff: Vec<SignoffArea>,
    /// Conflict resolution tiers and attempt caps (R3.28 applies them).
    conflicts: Option<Conflicts>,
    /// How the landing service batches and checks landings (R3.14).
    landing: Option<LandingTable>,
    /// Free rules in CEL.
    #[serde(default)]
    rule: Vec<Rule>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Owners")]
struct Owners {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    identities: Vec<String>,
    /// Distinct identities needed; default 1.
    count: Option<u32>,
    /// `none` or `user_verified`; default `none`.
    presence: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Attestor")]
struct Attestor {
    #[serde(default)]
    identities: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
    /// `person` or `ci`; default either.
    #[serde(default)]
    identity_kinds: Vec<String>,
    /// `human`, `agent` or `executor`; default `["executor"]`.
    actors: Option<Vec<String>>,
    /// Authority key ids (64 hex); empty for any this daemon verifies.
    #[serde(default)]
    authorities: Vec<String>,
    /// The least presence accepted; default `none`.
    presence: Option<String>,
    /// `exclude` (default) or `allow`: evidence from the change's own
    /// session or agent.
    #[serde(rename = "self")]
    self_: Option<String>,
    /// A CEL condition over `e`, one evidence object, and `groups`.
    when: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Require")]
struct Require {
    name: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    except: Vec<String>,
    kinds: Vec<String>,
    #[serde(default)]
    targets: Vec<String>,
    attestors: Vec<String>,
    when: Option<String>,
    message: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Limits")]
struct Limits {
    max_files: Option<u64>,
    max_lines: Option<u64>,
    /// `"5MiB"`, `"512KiB"`, or a number of bytes.
    max_file_size: Option<serde_json::Value>,
    #[serde(default)]
    except: Vec<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Forbid")]
struct Forbid {
    name: String,
    paths: Vec<String>,
    #[serde(default)]
    except: Vec<String>,
    when: Option<String>,
    message: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.SignoffArea")]
struct SignoffArea {
    name: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    except: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    identities: Vec<String>,
    count: Option<u32>,
    presence: Option<String>,
    /// Evidence kinds that count as a sign-off; default `["signoff"]`.
    kinds: Option<Vec<String>>,
    when: Option<String>,
    message: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Landing")]
struct LandingTable {
    /// `batch` (default): `[[require]]` evidence must also pass on the
    /// batch's tested tree; `proposal`: only on the submitted tree.
    checks: Option<String>,
    /// The most changes in a batch, 1 to 32; default 8.
    batch_max: Option<u32>,
    /// How long a batch waits to fill, `0s` to `10m`; default `0s`.
    batch_wait: Option<String>,
    /// Path patterns whose changes land in a batch of their own.
    #[serde(default)]
    alone: Vec<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Conflicts")]
struct Conflicts {
    /// `land`, `approve` (default) or `ask`.
    tier: Option<String>,
    /// 1 to 20; default 3.
    attempts: Option<u32>,
    #[serde(default)]
    rule: Vec<ConflictRule>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.ConflictRule")]
struct ConflictRule {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    except: Vec<String>,
    tier: Option<String>,
    attempts: Option<u32>,
    /// A CEL condition over `conflict` and `groups`.
    when: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.file.Rule")]
struct Rule {
    name: String,
    when: Option<String>,
    /// A CEL expression over `change`, `evidence`, `provenance` and
    /// `groups` that must be true.
    expr: String,
    message: Option<String>,
}

/// The four members of `reef schema policy`, built in `generator`, whose
/// definitions the caller takes: `file`, `input`, `evaluation` and
/// `conflict_decision`.
pub fn members(generator: &mut schemars::SchemaGenerator) -> [(&'static str, schemars::Schema); 4] {
    [
        ("file", generator.subschema_for::<File>()),
        ("input", generator.subschema_for::<Input>()),
        ("evaluation", generator.subschema_for::<Evaluation>()),
        (
            "conflict_decision",
            generator.subschema_for::<ConflictDecision>(),
        ),
    ]
}

/// The policy file's schema.
pub fn file() -> serde_json::Value {
    schemars::schema_for!(File).to_value()
}

/// The input's schema.
pub fn input() -> serde_json::Value {
    schemars::schema_for!(Input).to_value()
}

/// The evaluation's schema.
pub fn evaluation() -> serde_json::Value {
    schemars::schema_for!(Evaluation).to_value()
}

/// A conflict decision's schema.
pub fn conflict_decision() -> serde_json::Value {
    schemars::schema_for!(ConflictDecision).to_value()
}
