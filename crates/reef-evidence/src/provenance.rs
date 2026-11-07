//! Provenance records (R3.04, ADR 0058): a daemon's signed statement that
//! it recorded one commit for a workspace, in a session, while a client had
//! declared a context for that session.
//!
//! A record is an in-toto Statement v1 in the same DSSE envelope as
//! evidence, with the same PAE, canonical form, size, depth, duplicate-key
//! and key rules (ADR 0055), signed once by a key the daemon holds for the
//! workspace's session (a session-key certificate with `custody: daemon`,
//! R3.03). Only its subject and predicate differ:
//!
//! ```json
//! {"_type":"https://in-toto.io/Statement/v1",
//!  "subject":[{"name":"commit","digest":{"gitCommit":"5d41402abc4b2a76b9719d911017c592ae2b6a3e"}}],
//!  "predicateType":"urn:reef-vcs:provenance:v1",
//!  "predicate":{"change":"8f3a0c2d91e4b6751c2d0e9f4a7b3c18",
//!               "tree":{"gitTree":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},
//!               "workspace":"0123456789abcdef","kind":"watcher",
//!               "session":"ses_41c9e07ab2d35f18","sequence":42,"agent":"builder-3",
//!               "recordedOn":"2026-11-06T10:12:03.418Z",
//!               "context":{"model":"example-provider/model-x-2026-10",
//!                          "harness":"example-harness/2.1.0",
//!                          "task":"https://tracker.example/T-123",
//!                          "instruction":{"sha256":"9f86d0…"},
//!                          "clientSession":"c0ffee00-1234",
//!                          "declaredOn":"2026-11-06T10:11:58.002Z"}}}
//! ```
//!
//! The commit, change, tree, workspace, kind, session, sequence and times
//! are what the daemon observed. The context and the agent label are what a
//! client declared: nothing here verifies them, and a reader must say so.
//! Who the signer is comes from the key's certificate, never from the
//! record.
//!
//! [`sign`] takes a [`Record`], never bytes, so a key that signs provenance
//! signs nothing else through this module.

use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::envelope::Envelope;
use crate::keys::{KeyId, PublicKey, SigningKey};
use crate::statement::{ResourceDescriptor, format_time, parse_time};
use crate::verify::{Opened, predicate_error, statement_value};
use crate::{Invalid, PAYLOAD_TYPE, STATEMENT_TYPE};

/// The record's predicate type.
pub const PROVENANCE_PREDICATE_TYPE: &str = "urn:reef-vcs:provenance:v1";

/// The subject's `name`, which must be exactly this.
pub const SUBJECT_NAME: &str = "commit";

/// The subject's digest algorithm: a git commit id.
pub const GIT_COMMIT: &str = "gitCommit";

/// The longest `kind`, in bytes.
pub const MAX_KIND_BYTES: usize = 32;

/// The longest declared `model`, `harness` or `clientSession`, in bytes.
pub const MAX_LABEL_BYTES: usize = 128;

/// The longest declared `task`, in bytes.
pub const MAX_TASK_BYTES: usize = 512;

/// The largest `sequence`: 2^53, so every reader's JSON numbers hold it.
pub const MAX_SEQUENCE: u64 = 1 << 53;

/// What a client declared for a session: every field optional and
/// unverified.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// The model and its version, in the client's own naming.
    pub model: Option<String>,
    /// The agent runtime and its version, `name/version`.
    pub harness: Option<String>,
    /// The originating task: a URI or an opaque reference.
    pub task: Option<String>,
    /// The SHA-256 of the instruction's exact bytes, lowercase hex.
    pub instruction_sha256: Option<String>,
    /// The harness's own session id.
    pub client_session: Option<String>,
    /// When the context was last declared, unix milliseconds.
    pub declared_on_ms: Option<i64>,
}

impl Context {
    /// Whether no field but `declared_on_ms` is set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.harness.is_none()
            && self.task.is_none()
            && self.instruction_sha256.is_none()
            && self.client_session.is_none()
    }
}

/// What a record says, as values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The commit the daemon recorded: 40 or 64 lowercase hex digits.
    pub commit_id: String,
    /// Its change id, 32 lowercase hex digits.
    pub change_id: String,
    /// Its tree, absent when the tree is conflicted.
    pub tree_id: Option<String>,
    /// The Reef workspace, 16 lowercase hex digits.
    pub workspace: String,
    /// What recorded it: a snapshot kind, or `edit`. An open vocabulary.
    pub kind: String,
    /// The workspace's provenance session, `ses_` and 16 hex digits.
    pub session: String,
    /// This record's number in its session, from 1.
    pub sequence: u64,
    /// The session's agent label, declared.
    pub agent: Option<String>,
    /// When the daemon recorded it, unix milliseconds.
    pub recorded_on_ms: i64,
    /// The declared context in effect.
    pub context: Context,
}

/// The record's Statement, as JSON reads and writes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "evidence.ProvenanceStatement")]
pub struct ProvenanceStatement {
    /// `https://in-toto.io/Statement/v1`.
    #[serde(rename = "_type")]
    #[schemars(extend("const" = "https://in-toto.io/Statement/v1"))]
    pub type_: String,
    /// Exactly one: `{"name": "commit", "digest": {"gitCommit": <commit
    /// id>}}`.
    #[schemars(length(min = 1, max = 1))]
    pub subject: Vec<ResourceDescriptor>,
    /// `urn:reef-vcs:provenance:v1`.
    #[serde(rename = "predicateType")]
    #[schemars(extend("const" = "urn:reef-vcs:provenance:v1"))]
    pub predicate_type: String,
    /// What the daemon recorded, and what was declared.
    pub predicate: ProvenancePredicate,
}

/// Reef's provenance predicate (R3.04, ADR 0058). Fields are only ever
/// added (ADR 0055's rules); a reader keeps the ones it does not know in
/// [`ProvenancePredicate::unknown`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.ProvenancePredicate")]
pub struct ProvenancePredicate {
    /// The commit's change id: 32 lowercase hex digits. Observed.
    #[schemars(regex(pattern = r"^[0-9a-f]{32}$"))]
    pub change: String,
    /// The commit's tree, `{"gitTree": <hex>}`; absent when it is
    /// conflicted. Observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<BTreeMap<String, String>>,
    /// The Reef workspace id: 16 lowercase hex digits. Observed.
    #[schemars(regex(pattern = r"^[0-9a-f]{16}$"))]
    pub workspace: String,
    /// What recorded the commit: a snapshot kind (`watcher`, `explicit`,
    /// `final`, `move`, `shutdown`) or `edit`. An open vocabulary.
    #[schemars(regex(pattern = r"^[a-z][a-z0-9-]{0,31}$"))]
    pub kind: String,
    /// The workspace's provenance session, which the signing key's
    /// certificate names.
    #[schemars(regex(pattern = r"^ses_[0-9a-f]{16}$"))]
    pub session: String,
    /// The record's number in its session, from 1.
    #[schemars(range(min = 1, max = 9_007_199_254_740_992_u64))]
    pub sequence: u64,
    /// The session's agent label: declared, never verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub agent: Option<String>,
    /// When the daemon recorded the commit: RFC 3339 in UTC with `Z`, to
    /// the millisecond.
    pub recorded_on: String,
    /// What a client declared for the session; `{}` when nothing was.
    pub context: ProvenanceContext,
    /// Fields a later Reef added that this one does not know.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// The declared context, as the predicate writes it. Every field is a
/// client's claim.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "evidence.ProvenanceContext")]
pub struct ProvenanceContext {
    /// The model and its version: 1 to 128 bytes of visible ASCII.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub model: Option<String>,
    /// The agent runtime and its version, `name/version`: 1 to 128 bytes of
    /// visible ASCII.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub harness: Option<String>,
    /// The originating task: 1 to 512 bytes, no control characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub task: Option<String>,
    /// The instruction's digest, `{"sha256": <hex>}`: of its exact bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<BTreeMap<String, String>>,
    /// The harness's own session id: 1 to 128 bytes of visible ASCII.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub client_session: Option<String>,
    /// When the context was last declared: RFC 3339 in UTC with `Z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_on: Option<String>,
    /// Fields a later Reef added that this one does not know.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// `ms` as the predicate writes a time.
fn time_text(field: &str, ms: i64) -> Result<String, Invalid> {
    let ms = u64::try_from(ms).map_err(|_| predicate_error(field, "before 1970"))?;
    format_time(field, UNIX_EPOCH + Duration::from_millis(ms))
}

/// Whether `kind` is a record kind: `^[a-z][a-z0-9-]{0,31}$`.
pub fn is_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    (1..=MAX_KIND_BYTES).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Whether `id` is a git object id: 40 or 64 lowercase hex digits.
pub fn is_object_id(id: &str) -> bool {
    crate::is_lower_hex(id, 40) || crate::is_lower_hex(id, 64)
}

/// Why `value` is not a declared label (`model`, `harness`,
/// `clientSession`), if it is not: 1 to [`MAX_LABEL_BYTES`] bytes of
/// visible ASCII.
pub fn label_problem(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Some(format!(
            "{} bytes; it is 1 to {MAX_LABEL_BYTES} bytes",
            value.len()
        ));
    }
    (!value.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| "it is visible ASCII, with no spaces".to_owned())
}

/// Why `value` is not a task reference, if it is not: 1 to
/// [`MAX_TASK_BYTES`] bytes of UTF-8 with no control characters.
pub fn task_problem(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_TASK_BYTES {
        return Some(format!(
            "{} bytes; a task is 1 to {MAX_TASK_BYTES} bytes",
            value.len()
        ));
    }
    value
        .chars()
        .any(char::is_control)
        .then(|| "a task has no control characters".to_owned())
}

/// Why `value` is not an instruction digest, if it is not: 64 lowercase hex
/// digits.
pub fn instruction_problem(value: &str) -> Option<String> {
    (!crate::is_lower_hex(value, 64)).then(|| {
        "the instruction is its SHA-256: 64 lowercase hex digits, never the text".to_owned()
    })
}

/// Why `context` is not one a record may carry, as `(field, problem)`, the
/// field named as the API names it (`model`, `harness`, `task`,
/// `instruction_sha256`, `client_session`).
pub fn context_problem(context: &Context) -> Option<(&'static str, String)> {
    type Check<'a> = (&'static str, Option<&'a String>, fn(&str) -> Option<String>);
    let checks: [Check<'_>; 5] = [
        ("model", context.model.as_ref(), label_problem),
        ("harness", context.harness.as_ref(), label_problem),
        ("task", context.task.as_ref(), task_problem),
        (
            "instruction_sha256",
            context.instruction_sha256.as_ref(),
            instruction_problem,
        ),
        (
            "client_session",
            context.client_session.as_ref(),
            label_problem,
        ),
    ];
    checks.into_iter().find_map(|(field, value, check)| {
        value.and_then(|value| check(value).map(|problem| (field, problem)))
    })
}

impl Context {
    /// The context as the predicate writes it.
    fn json(&self) -> Result<ProvenanceContext, Invalid> {
        Ok(ProvenanceContext {
            model: self.model.clone(),
            harness: self.harness.clone(),
            task: self.task.clone(),
            instruction: self
                .instruction_sha256
                .as_ref()
                .map(|sha256| BTreeMap::from([("sha256".to_owned(), sha256.clone())])),
            client_session: self.client_session.clone(),
            declared_on: self
                .declared_on_ms
                .map(|ms| time_text("context", ms))
                .transpose()?,
            unknown: BTreeMap::new(),
        })
    }
}

/// The SHA-256, in lowercase hex, of the canonical JSON of `context`
/// without `declaredOn`: equal declared fields give equal digests, however
/// they were written. GC groups records into runs by it.
pub fn context_digest(context: &Context) -> String {
    let mut json = context.json().unwrap_or_default();
    json.declared_on = None;
    crate::sha256_hex(&serde_json::to_vec(&json).unwrap_or_default())
}

impl Record {
    /// The record's Statement.
    pub fn statement(&self) -> Result<ProvenanceStatement, Invalid> {
        Ok(ProvenanceStatement {
            type_: STATEMENT_TYPE.to_owned(),
            subject: vec![ResourceDescriptor {
                name: Some(SUBJECT_NAME.to_owned()),
                uri: None,
                digest: BTreeMap::from([(GIT_COMMIT.to_owned(), self.commit_id.clone())]),
                media_type: None,
                annotations: None,
            }],
            predicate_type: PROVENANCE_PREDICATE_TYPE.to_owned(),
            predicate: ProvenancePredicate {
                change: self.change_id.clone(),
                tree: self.tree_id.as_ref().map(|tree| {
                    BTreeMap::from([(crate::statement::GIT_TREE.to_owned(), tree.clone())])
                }),
                workspace: self.workspace.clone(),
                kind: self.kind.clone(),
                session: self.session.clone(),
                sequence: self.sequence,
                agent: self.agent.clone(),
                recorded_on: time_text("recordedOn", self.recorded_on_ms)?,
                context: self.context.json()?,
                unknown: BTreeMap::new(),
            },
        })
    }

    /// The payload: the statement as compact JSON in the order of the
    /// module documentation, so one record is always the same bytes.
    pub fn payload(&self) -> Result<Vec<u8>, Invalid> {
        // Strings, numbers and maps of strings always serialize.
        Ok(serde_json::to_vec(&self.statement()?).unwrap_or_default())
    }
}

/// Signs `record` with `key`, the session's held key. A record that
/// [`check`] would refuse is refused here, so a key never signs one.
pub fn sign(key: &SigningKey, record: &Record) -> Result<Envelope, Invalid> {
    let payload = record.payload()?;
    parse_record(&payload)?;
    Ok(key.sign(PAYLOAD_TYPE, &payload))
}

/// A record whose signature verifies with its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// What it says.
    pub record: Record,
    /// The record's id: SHA-256 of [`Checked::canonical`], lowercase hex.
    pub id: String,
    /// The canonical envelope.
    pub canonical: Vec<u8>,
    /// The payload, as signed.
    pub payload: Vec<u8>,
    /// The key that signed it.
    pub key: PublicKey,
}

impl Checked {
    /// The signer's key id.
    pub fn key_id(&self) -> &KeyId {
        self.key.id()
    }
}

/// Checks a record in ADR 0055's order: the size and the envelope; the key
/// (`public_key`, a DER `SubjectPublicKeyInfo`) and its id against the
/// `keyid`; the signature over the PAE; and only then the statement and the
/// predicate.
pub fn check(envelope: &[u8], public_key: &[u8]) -> Result<Checked, Invalid> {
    let opened = Opened::parse(envelope)?;
    let key = PublicKey::from_spki(public_key)?;
    opened.verify(&key)?;
    let record = parse_record(&opened.payload)?;
    let canonical = opened.canonical(&key);
    Ok(Checked {
        record,
        id: crate::sha256_hex(&canonical),
        canonical,
        payload: opened.payload,
        key,
    })
}

/// The record in a payload, checked field by field.
fn parse_record(payload: &[u8]) -> Result<Record, Invalid> {
    let value = statement_value(payload)?;
    let Value::Object(root) = &value else {
        return Err(Invalid::Statement("it is not a JSON object".into()));
    };
    match root.get("predicateType") {
        Some(Value::String(t)) if t == PROVENANCE_PREDICATE_TYPE => {}
        Some(Value::String(t)) => {
            return Err(Invalid::Statement(format!(
                "predicateType is {t:?}, not {PROVENANCE_PREDICATE_TYPE:?}"
            )));
        }
        _ => {
            return Err(Invalid::Statement(
                "predicateType is missing or not a string".into(),
            ));
        }
    }
    let commit_id = subject_commit(root.get("subject"))?;
    let Some(Value::Object(predicate)) = root.get("predicate") else {
        return Err(Invalid::Predicate {
            field: String::new(),
            message: "the predicate is missing or not a JSON object".into(),
        });
    };
    record_of(commit_id, predicate)
}

/// The subject's commit id: exactly one subject, named `commit`, whose
/// digest is only a `gitCommit` of 40 or 64 lowercase hex digits.
fn subject_commit(subject: Option<&Value>) -> Result<String, Invalid> {
    let bad = |message: String| Err(Invalid::Subject(message));
    let Some(Value::Array(subjects)) = subject else {
        return bad("subject is missing or not an array".into());
    };
    let [Value::Object(only)] = subjects.as_slice() else {
        return bad(format!(
            "{} subjects; a provenance record is about exactly one commit",
            subjects.len()
        ));
    };
    match only.get("name") {
        Some(Value::String(name)) if name == SUBJECT_NAME => {}
        _ => return bad(format!("the subject's name must be {SUBJECT_NAME:?}")),
    }
    let Some(Value::Object(digest)) = only.get("digest") else {
        return bad("the subject has no digest object".into());
    };
    if let Some(other) = digest.keys().find(|key| *key != GIT_COMMIT) {
        return bad(format!(
            "the subject's digest has {other:?}; it has only {GIT_COMMIT}"
        ));
    }
    match digest.get(GIT_COMMIT) {
        Some(Value::String(commit)) if is_object_id(commit) => Ok(commit.clone()),
        _ => bad(format!(
            "the subject's {GIT_COMMIT} is not 40 or 64 lowercase hex digits"
        )),
    }
}

/// A required string field.
fn string<'a>(object: &'a Map<String, Value>, key: &str, field: &str) -> Result<&'a str, Invalid> {
    match object.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(predicate_error(field, format!("{key} is not a string"))),
        None => Err(predicate_error(field, format!("{key} is required"))),
    }
}

/// An optional string field, checked by `problem`.
fn optional(
    object: &Map<String, Value>,
    key: &str,
    field: &str,
    problem: fn(&str) -> Option<String>,
) -> Result<Option<String>, Invalid> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => match problem(text) {
            Some(problem) => Err(predicate_error(field, problem)),
            None => Ok(Some(text.clone())),
        },
        Some(_) => Err(predicate_error(field, format!("{key} is not a string"))),
    }
}

/// A time field's milliseconds.
fn time(object: &Map<String, Value>, key: &str, field: &str) -> Result<i64, Invalid> {
    let text = string(object, key, field)?;
    parse_time(text).ok_or_else(|| {
        predicate_error(
            field,
            format!("{key}: an RFC 3339 time in UTC ending in Z, such as 2026-11-06T10:00:00.000Z"),
        )
    })
}

/// The predicate's fields as a record about `commit_id`.
fn record_of(commit_id: String, predicate: &Map<String, Value>) -> Result<Record, Invalid> {
    let change = string(predicate, "change", "change")?;
    if !crate::is_lower_hex(change, 32) {
        return Err(predicate_error("change", "32 lowercase hex digits"));
    }
    let tree_id = match predicate.get("tree") {
        None => None,
        Some(Value::Object(tree)) => {
            if let Some(other) = tree.keys().find(|key| *key != crate::statement::GIT_TREE) {
                return Err(predicate_error(
                    "tree",
                    format!("{other:?}; the tree has only gitTree"),
                ));
            }
            let id = string(tree, crate::statement::GIT_TREE, "tree")?;
            if !is_object_id(id) {
                return Err(predicate_error(
                    "tree",
                    "gitTree is 40 or 64 lowercase hex digits",
                ));
            }
            Some(id.to_owned())
        }
        Some(_) => return Err(predicate_error("tree", "not a JSON object")),
    };
    let workspace = string(predicate, "workspace", "workspace")?;
    if !crate::is_lower_hex(workspace, 16) {
        return Err(predicate_error("workspace", "16 lowercase hex digits"));
    }
    let kind = string(predicate, "kind", "kind")?;
    if !is_kind(kind) {
        return Err(predicate_error(
            "kind",
            "lowercase letters, digits and '-', starting with a letter, at most 32",
        ));
    }
    let session = string(predicate, "session", "session")?;
    if !crate::session::is_session_id(session) {
        return Err(predicate_error(
            "session",
            format!("{session:?} is not ses_ and 16 lowercase hex digits"),
        ));
    }
    let sequence =
        match predicate.get("sequence") {
            Some(Value::Number(number)) => number
                .as_u64()
                .filter(|n| (1..=MAX_SEQUENCE).contains(n))
                .ok_or_else(|| predicate_error("sequence", "an integer from 1 to 2^53"))?,
            Some(_) => return Err(predicate_error("sequence", "not a number")),
            None => return Err(predicate_error("sequence", "required")),
        };
    let agent = optional(predicate, "agent", "agent", crate::session::agent_problem)?;
    let recorded_on_ms = time(predicate, "recordedOn", "recordedOn")?;
    let context = match predicate.get("context") {
        Some(Value::Object(context)) => context_of(context)?,
        Some(_) => return Err(predicate_error("context", "not a JSON object")),
        None => return Err(predicate_error("context", "required")),
    };
    Ok(Record {
        commit_id,
        change_id: change.to_owned(),
        tree_id,
        workspace: workspace.to_owned(),
        kind: kind.to_owned(),
        session: session.to_owned(),
        sequence,
        agent,
        recorded_on_ms,
        context,
    })
}

/// The predicate's `context`.
fn context_of(context: &Map<String, Value>) -> Result<Context, Invalid> {
    let instruction_sha256 = match context.get("instruction") {
        None => None,
        Some(Value::Object(digest)) => {
            if let Some(other) = digest.keys().find(|key| *key != "sha256") {
                return Err(predicate_error(
                    "context",
                    format!("instruction has {other:?}; it has only sha256"),
                ));
            }
            let sha256 = string(digest, "sha256", "context")?;
            if let Some(problem) = instruction_problem(sha256) {
                return Err(predicate_error("context", problem));
            }
            Some(sha256.to_owned())
        }
        Some(_) => {
            return Err(predicate_error(
                "context",
                "instruction is not a JSON object",
            ));
        }
    };
    let declared_on_ms = match context.get("declaredOn") {
        None => None,
        Some(_) => Some(time(context, "declaredOn", "context")?),
    };
    Ok(Context {
        model: optional(context, "model", "context", label_problem)?,
        harness: optional(context, "harness", "context", label_problem)?,
        task: optional(context, "task", "context", task_problem)?,
        instruction_sha256,
        client_session: optional(context, "clientSession", "context", label_problem)?,
        declared_on_ms,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests: a panic is the failure report")]

    use super::*;

    fn context() -> Context {
        Context {
            model: Some("example-provider/model-x-2026-10".into()),
            harness: Some("example-harness/2.1.0".into()),
            task: Some("https://tracker.example/T-123".into()),
            instruction_sha256: Some(crate::sha256_hex(b"test")),
            client_session: Some("c0ffee00-1234".into()),
            declared_on_ms: Some(1_793_959_918_002),
        }
    }

    #[test]
    fn context_digest_ignores_declared_on() {
        let a = context();
        let mut b = context();
        b.declared_on_ms = None;
        assert_eq!(context_digest(&a), context_digest(&b));
        b.task = Some("T-124".into());
        assert_ne!(context_digest(&a), context_digest(&b));
        assert_ne!(
            context_digest(&Context::default()),
            context_digest(&a),
            "the empty context has its own digest"
        );
    }

    #[test]
    fn declared_field_bounds() {
        assert_eq!(
            label_problem("model x"),
            Some("it is visible ASCII, with no spaces".into())
        );
        assert!(label_problem(&"m".repeat(128)).is_none());
        assert!(label_problem(&"m".repeat(129)).is_some());
        assert!(task_problem("T-1\n").is_some());
        assert!(task_problem("tâche 1").is_none());
        assert!(instruction_problem(&"A".repeat(64)).is_some());
        let mut bad = context();
        bad.client_session = Some(String::new());
        assert_eq!(
            context_problem(&bad).map(|(f, _)| f),
            Some("client_session")
        );
        assert!(context_problem(&context()).is_none());
    }

    #[test]
    fn kinds() {
        assert!(is_kind("watcher"));
        assert!(is_kind("edit"));
        assert!(is_kind(&"a".repeat(32)));
        assert!(!is_kind(&"a".repeat(33)));
        assert!(!is_kind("Watcher"));
        assert!(!is_kind("1st"));
    }
}
