//! The claim: an in-toto Statement v1 about one git tree, with Reef's v1
//! predicate.
//!
//! ```json
//! {"_type":"https://in-toto.io/Statement/v1",
//!  "subject":[{"name":"tree","digest":{"gitTree":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"}}],
//!  "predicateType":"urn:reef-vcs:evidence:v1",
//!  "predicate":{"kind":"test","result":"pass","target":"crates/reef-lib:jj_writes",
//!               "startedOn":"2026-11-05T10:00:00Z","finishedOn":"2026-11-05T10:01:12Z",
//!               "artifacts":[{"name":"test.log","digest":{"sha256":"9f86d0…"},"mediaType":"text/plain"}],
//!               "annotations":{"runner":"ci-7"}}}
//! ```
//!
//! The types here are what the JSON is; [`crate::Verified::check`] is what
//! decides whether a payload is evidence. [`Claim`] is the producer's side:
//! the predicate's fields as Rust values, written as compact JSON in the
//! order above, so the same claim is always the same bytes.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::envelope::Envelope;
use crate::keys::SigningKey;
use crate::{Invalid, PAYLOAD_TYPE, PREDICATE_TYPE, STATEMENT_TYPE};

/// The subject's `name`, which must be exactly this: fixed so that two
/// producers of the same claim write the same bytes, and checked so that a
/// stored subject never says anything else.
pub const SUBJECT_NAME: &str = "tree";

/// The subject's `DigestSet` key for a git tree id.
pub const GIT_TREE: &str = "gitTree";

/// An in-toto Statement v1 carrying Reef's predicate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.Statement")]
pub struct Statement {
    /// `https://in-toto.io/Statement/v1`.
    #[serde(rename = "_type")]
    #[schemars(extend("const" = "https://in-toto.io/Statement/v1"))]
    pub type_: String,
    /// Exactly one: the tree, `{"name": "tree", "digest": {"gitTree": <hex>}}`.
    #[schemars(length(min = 1, max = 1))]
    pub subject: Vec<ResourceDescriptor>,
    /// `urn:reef-vcs:evidence:v1`.
    #[serde(rename = "predicateType")]
    #[schemars(extend("const" = "urn:reef-vcs:evidence:v1"))]
    pub predicate_type: String,
    /// What was checked and how it came out.
    pub predicate: Predicate,
}

impl Statement {
    /// The subject's git tree id: the first subject's `gitTree`, which a
    /// checked statement has exactly one of.
    pub fn tree_id(&self) -> Option<&str> {
        self.subject
            .first()
            .and_then(|subject| subject.digest.get(GIT_TREE))
            .map(String::as_str)
    }
}

/// An in-toto `ResourceDescriptor`: the subject, or an artifact a check
/// produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.ResourceDescriptor")]
pub struct ResourceDescriptor {
    /// The subject's is `tree`; an artifact's is required, such as the file
    /// name of a log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where an artifact can be fetched; Reef does not read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Algorithm to lowercase hex digest: `gitTree` for the subject, and
    /// `sha256` for an artifact.
    pub digest: BTreeMap<String, String>,
    /// An artifact's media type, such as `text/plain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// The producer's own data about the resource; Reef never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Map<String, Value>>,
}

/// How a check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "evidence.Outcome")]
pub enum Outcome {
    /// The check passed.
    Pass,
    /// The check failed: the tree does not meet it.
    Fail,
    /// The check reached no verdict: the runner crashed, timed out, or ran
    /// out of budget. Not a failure of the tree.
    Error,
}

impl Outcome {
    /// Every outcome.
    pub const ALL: [Outcome; 3] = [Outcome::Pass, Outcome::Fail, Outcome::Error];

    /// The predicate's value: `pass`, `fail` or `error`.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Error => "error",
        }
    }

    /// The outcome the predicate's value names.
    pub fn parse(text: &str) -> Option<Outcome> {
        Outcome::ALL.into_iter().find(|o| o.as_str() == text)
    }
}

/// Reef's v1 predicate: what was checked on the tree, and how it came out.
/// Fields are only ever added, as optional ones (ADR 0055); a reader keeps
/// the ones it does not know in [`Predicate::unknown`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.Predicate")]
pub struct Predicate {
    /// What was checked: `test`, `build`, `lint`, `policy`, or another kind
    /// in the open vocabulary.
    #[schemars(regex(pattern = r"^[a-z][a-z0-9-]{0,62}$"))]
    pub kind: String,
    /// `pass`, `fail`, or `error` (no verdict).
    pub result: Outcome,
    /// The test or build target covered, in the executor's naming; absent
    /// for the whole check of that kind on the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub target: Option<String>,
    /// When the check started, as the attestor claims: RFC 3339 in UTC with
    /// `Z`.
    pub started_on: String,
    /// When it finished, not before `startedOn`.
    pub finished_on: String,
    /// What the check produced, by SHA-256: at most 64, each with a `name`
    /// and a `sha256` digest. The bytes need not be stored anywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub artifacts: Vec<ResourceDescriptor>,
    /// The producer's own data. Reef never reads it and never defines a
    /// field in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Map<String, Value>>,
    /// Fields a later Reef added that this one does not know.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// A git tree id, the evidence's subject: 40 (SHA-1) or 64 (SHA-256)
/// lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreeDigest(String);

impl TreeDigest {
    /// The git tree `hex`; [`Invalid::Subject`] when it is not 40 or 64
    /// lowercase hex digits.
    pub fn git(hex: &str) -> Result<TreeDigest, Invalid> {
        if crate::is_lower_hex(hex, 40) || crate::is_lower_hex(hex, 64) {
            Ok(TreeDigest(hex.to_owned()))
        } else {
            Err(Invalid::Subject(format!(
                "gitTree {hex:?} is not 40 or 64 lowercase hex digits"
            )))
        }
    }

    /// The id, in lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TreeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An artifact a check produced, recorded by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    /// Its name, such as `test.log`.
    pub name: String,
    /// SHA-256 of its bytes, 64 lowercase hex digits.
    pub sha256: String,
    /// Where it can be fetched, if anywhere.
    pub uri: Option<String>,
    /// Its media type.
    pub media_type: Option<String>,
}

/// A claim about a tree, as its producer builds it: the predicate's fields
/// as Rust values.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    /// What was checked.
    pub kind: String,
    /// How it came out.
    pub result: Outcome,
    /// The target covered; `None` for the whole check.
    pub target: Option<String>,
    /// When the check started.
    pub started_on: SystemTime,
    /// When it finished.
    pub finished_on: SystemTime,
    /// What it produced.
    pub artifacts: Vec<Artifact>,
    /// The producer's own data; empty for none.
    pub annotations: Map<String, Value>,
}

/// The latest time the format writes: 9999-12-31T23:59:59.999Z, in
/// milliseconds after the Unix epoch. RFC 3339 has four-digit years.
pub const MAX_TIME_MS: u128 = 253_402_300_799_999;

/// `at` as the predicate writes it: RFC 3339 in UTC to the millisecond,
/// with `Z`. [`Invalid::Predicate`] for `field` when `at` is before
/// 1970-01-01 or after 9999-12-31, which the format does not write.
pub fn format_time(field: &str, at: SystemTime) -> Result<String, Invalid> {
    let out_of_range = || Invalid::Predicate {
        field: field.to_owned(),
        message: "a time from 1970-01-01T00:00:00Z to 9999-12-31T23:59:59.999Z".into(),
    };
    let since = at.duration_since(UNIX_EPOCH).map_err(|_| out_of_range())?;
    if since.as_millis() > MAX_TIME_MS {
        return Err(out_of_range());
    }
    Ok(humantime::format_rfc3339_millis(at).to_string())
}

/// Whether `text` has the form `YYYY-MM-DDTHH:MM:SS[.fraction]Z`, with one
/// to nine fraction digits, seconds 00 to 59 (a leap second, `:60`, is
/// refused), and nothing else. Ranges of months, days and hours are
/// humantime's to check.
fn is_time_form(text: &str) -> bool {
    let b = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    if b.len() < 20 || b[b.len() - 1] != b'Z' {
        return false;
    }
    let fixed = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19)
        && b[17] <= b'5';
    if !fixed {
        return false;
    }
    match &b[19..b.len() - 1] {
        [] => true,
        [b'.', fraction @ ..] => {
            (1..=9).contains(&fraction.len()) && fraction.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    }
}

/// A predicate time as unix milliseconds: `YYYY-MM-DDTHH:MM:SS[.f]Z` in
/// UTC, one to nine fraction digits, no leap second, truncated to the
/// millisecond; `None` when it is not one.
pub fn parse_time(text: &str) -> Option<i64> {
    if !is_time_form(text) {
        return None;
    }
    let at = humantime::parse_rfc3339(text).ok()?;
    let since = at.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

impl Claim {
    /// The Statement of this claim about `tree`; [`Invalid::Predicate`]
    /// when a field is outside the format's bounds (a time before 1970, a
    /// kind or target the checker would refuse), so a claim never signs
    /// what [`crate::Verified::check`] refuses.
    pub fn statement(&self, tree: &TreeDigest) -> Result<Statement, Invalid> {
        let statement = self.unchecked_statement(tree)?;
        let payload = serde_json::to_vec(&statement).unwrap_or_default();
        crate::verify::parse_statement(&payload)?;
        Ok(statement)
    }

    fn unchecked_statement(&self, tree: &TreeDigest) -> Result<Statement, Invalid> {
        Ok(Statement {
            type_: STATEMENT_TYPE.to_owned(),
            subject: vec![ResourceDescriptor {
                name: Some(SUBJECT_NAME.to_owned()),
                uri: None,
                digest: BTreeMap::from([(GIT_TREE.to_owned(), tree.as_str().to_owned())]),
                media_type: None,
                annotations: None,
            }],
            predicate_type: PREDICATE_TYPE.to_owned(),
            predicate: Predicate {
                kind: self.kind.clone(),
                result: self.result,
                target: self.target.clone(),
                started_on: format_time("startedOn", self.started_on)?,
                finished_on: format_time("finishedOn", self.finished_on)?,
                artifacts: self
                    .artifacts
                    .iter()
                    .map(|artifact| ResourceDescriptor {
                        name: Some(artifact.name.clone()),
                        uri: artifact.uri.clone(),
                        digest: BTreeMap::from([("sha256".to_owned(), artifact.sha256.clone())]),
                        media_type: artifact.media_type.clone(),
                        annotations: None,
                    })
                    .collect(),
                annotations: (!self.annotations.is_empty()).then(|| self.annotations.clone()),
                unknown: BTreeMap::new(),
            },
        })
    }

    /// The payload: the statement as compact JSON, keys in the order of the
    /// format's example, with no trailing newline.
    pub fn payload(&self, tree: &TreeDigest) -> Result<Vec<u8>, Invalid> {
        // A struct of strings, maps of strings and JSON values always
        // serializes.
        Ok(serde_json::to_vec(&self.statement(tree)?).unwrap_or_default())
    }

    /// The claim about `tree`, signed by `key`.
    pub fn sign(&self, tree: &TreeDigest, key: &SigningKey) -> Result<Envelope, Invalid> {
        Ok(key.sign(PAYLOAD_TYPE, &self.payload(tree)?))
    }
}
