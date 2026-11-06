//! [`Verified::check`]: whether an envelope is evidence, in DSSE's order.
//!
//! 1. The envelope: at most [`MAX_ENVELOPE_BYTES`], JSON with no duplicate
//!    key and at most [`MAX_DEPTH`] levels, `payloadType`
//!    [`PAYLOAD_TYPE`], exactly one signature, base64 that decodes.
//! 2. The key: an ed25519 `SubjectPublicKeyInfo` that is neither small-order
//!    nor non-canonical, whose id is the signature's `keyid`.
//! 3. The signature, over [`pae`]`(payloadType, payload)`.
//! 4. Only then the payload: UTF-8 JSON with no duplicate key and at most
//!    [`MAX_DEPTH`] levels, a Statement v1 with one `gitTree` subject,
//!    [`PREDICATE_TYPE`], and the predicate's fields in their bounds.
//!
//! Junk that is unsigned, or signed by a malformed key, never reaches the
//! statement parser. The payload is kept as signed, and the envelope is
//! rewritten in canonical form, whose SHA-256 is the evidence id.

use std::collections::HashSet;
use std::fmt;
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::envelope::{Envelope, decode_base64};
use crate::keys::{KeyId, PublicKey};
use crate::statement::{Artifact, GIT_TREE, Outcome, Statement, TreeDigest, parse_time};
use crate::{
    Invalid, MAX_ARTIFACTS, MAX_DEPTH, MAX_ENVELOPE_BYTES, MAX_TARGET_BYTES, PAYLOAD_TYPE,
    PREDICATE_TYPE, STATEMENT_TYPE, pae,
};

/// The fields Reef indexes an evidence object by, read from its verified
/// payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The subject's git tree id.
    pub tree_id: String,
    /// The predicate's `kind`.
    pub kind: String,
    /// Its `result`.
    pub result: Outcome,
    /// Its `target`, if any.
    pub target: Option<String>,
    /// `startedOn`, as unix milliseconds.
    pub started_at_ms: i64,
    /// `finishedOn`, as unix milliseconds.
    pub finished_at_ms: i64,
    /// Its artifacts.
    pub artifacts: Vec<Artifact>,
}

/// An envelope that is evidence: its signature verifies with its key, and
/// its payload is a Statement v1 with Reef's predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct Verified {
    /// The evidence id: SHA-256 of [`Verified::canonical`], lowercase hex.
    pub id: String,
    /// The canonical envelope.
    pub canonical: Vec<u8>,
    /// The payload, as signed.
    pub payload: Vec<u8>,
    /// The payload, parsed.
    pub statement: Statement,
    /// The key that signed it.
    pub key: PublicKey,
    /// What Reef indexes it by.
    pub index: Index,
}

/// The envelope's JSON as received; unknown fields are dropped, as DSSE
/// says consumers ignore them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEnvelope {
    payload: String,
    payload_type: String,
    signatures: Vec<RawSignature>,
}

#[derive(Deserialize)]
struct RawSignature {
    #[serde(default)]
    keyid: Option<String>,
    sig: String,
}

/// The first half of every check, shared by evidence and session-key
/// certificates (R3.03): an envelope within its bounds, decoded, whose
/// signature is yet to be verified.
#[derive(Debug)]
pub(crate) struct Opened {
    /// The payload's bytes.
    pub(crate) payload: Vec<u8>,
    /// The signature's bytes.
    sig: Vec<u8>,
    /// The signature's `keyid`; empty when it has none.
    pub(crate) keyid: String,
}

impl Opened {
    /// Step 1 of the module documentation: the size, the JSON (no duplicate
    /// key, at most [`MAX_DEPTH`] levels), the payload type, exactly one
    /// signature, and base64 that decodes.
    pub(crate) fn parse(envelope: &[u8]) -> Result<Opened, Invalid> {
        if envelope.len() > MAX_ENVELOPE_BYTES {
            return Err(Invalid::Size(envelope.len()));
        }
        strict_json(envelope).map_err(|problem| Invalid::Envelope(problem.to_string()))?;
        let raw: RawEnvelope = serde_json::from_slice(envelope)
            .map_err(|error| Invalid::Envelope(error.to_string()))?;
        if raw.payload_type != PAYLOAD_TYPE {
            return Err(Invalid::PayloadType(raw.payload_type));
        }
        let [signature] = <[RawSignature; 1]>::try_from(raw.signatures)
            .map_err(|signatures| Invalid::Signatures(signatures.len()))?;
        let payload = decode_base64(&raw.payload)
            .ok_or_else(|| Invalid::Envelope("the payload is not base64".into()))?;
        let sig = decode_base64(&signature.sig)
            .ok_or_else(|| Invalid::Envelope("the signature is not base64".into()))?;
        Ok(Opened {
            payload,
            sig,
            keyid: signature.keyid.unwrap_or_default(),
        })
    }

    /// Steps 2 and 3: the `keyid` is `key`'s id, and the signature over
    /// [`pae`]`(payloadType, payload)` verifies with it.
    pub(crate) fn verify(&self, key: &PublicKey) -> Result<(), Invalid> {
        if self.keyid != key.id().as_str() {
            return Err(Invalid::KeyId {
                found: self.keyid.clone(),
                expected: key.id().to_string(),
            });
        }
        if !key.verify(&pae(PAYLOAD_TYPE, &self.payload), &self.sig) {
            return Err(Invalid::Signature {
                key_id: key.id().to_string(),
            });
        }
        Ok(())
    }

    /// The envelope in canonical form, signed by `key`.
    pub(crate) fn canonical(&self, key: &PublicKey) -> Vec<u8> {
        Envelope::new(PAYLOAD_TYPE, &self.payload, key.id().as_str(), &self.sig).to_bytes()
    }
}

impl Verified {
    /// Checks `envelope` (a DSSE envelope as JSON) against `public_key` (the
    /// signer's DER `SubjectPublicKeyInfo`), in the order of the module
    /// documentation.
    pub fn check(envelope: &[u8], public_key: &[u8]) -> Result<Verified, Invalid> {
        let opened = Opened::parse(envelope)?;
        let key = PublicKey::from_spki(public_key)?;
        opened.verify(&key)?;
        let statement = parse_statement(&opened.payload)?;
        let index = index_of(&statement)?;
        let canonical = opened.canonical(&key);
        Ok(Verified {
            id: crate::sha256_hex(&canonical),
            canonical,
            payload: opened.payload,
            statement,
            key,
            index,
        })
    }

    /// The signer's key id.
    pub fn key_id(&self) -> &KeyId {
        self.key.id()
    }
}

/// The payload as a Statement, checked field by field.
pub(crate) fn parse_statement(payload: &[u8]) -> Result<Statement, Invalid> {
    let value = statement_value(payload)?;
    let Value::Object(root) = &value else {
        return Err(Invalid::Statement("it is not a JSON object".into()));
    };
    match root.get("predicateType") {
        Some(Value::String(t)) if t == PREDICATE_TYPE => {}
        Some(Value::String(t)) => return Err(Invalid::PredicateType(t.clone())),
        _ => {
            return Err(Invalid::Statement(
                "predicateType is missing or not a string".into(),
            ));
        }
    }
    check_subject(root.get("subject"))?;
    match root.get("predicate") {
        Some(Value::Object(predicate)) => check_predicate(predicate)?,
        _ => {
            return Err(Invalid::Predicate {
                field: String::new(),
                message: "the predicate is missing or not a JSON object".into(),
            });
        }
    }
    serde_json::from_value(value).map_err(|error| Invalid::Statement(error.to_string()))
}

/// A payload as a Statement v1's JSON object: no duplicate key, at most
/// [`MAX_DEPTH`] levels, and `_type` [`STATEMENT_TYPE`]. A problem inside
/// `predicate` is [`Invalid::Predicate`] for the field it is under.
pub(crate) fn statement_value(payload: &[u8]) -> Result<Value, Invalid> {
    strict_json(payload).map_err(|problem| match problem.predicate_field() {
        Some(field) => Invalid::Predicate {
            field,
            message: problem.to_string(),
        },
        None => Invalid::Statement(problem.to_string()),
    })?;
    let value: Value =
        serde_json::from_slice(payload).map_err(|error| Invalid::Statement(error.to_string()))?;
    let Value::Object(root) = &value else {
        return Err(Invalid::Statement("it is not a JSON object".into()));
    };
    match root.get("_type") {
        Some(Value::String(t)) if t == STATEMENT_TYPE => {}
        Some(Value::String(t)) => {
            return Err(Invalid::Statement(format!(
                "_type is {t:?}, not {STATEMENT_TYPE:?}"
            )));
        }
        _ => {
            return Err(Invalid::Statement(
                "_type is missing or not a string".into(),
            ));
        }
    }
    Ok(value)
}

fn check_subject(subject: Option<&Value>) -> Result<(), Invalid> {
    let bad = |message: String| Err(Invalid::Subject(message));
    let Some(Value::Array(subjects)) = subject else {
        return bad("subject is missing or not an array".into());
    };
    let [Value::Object(only)] = subjects.as_slice() else {
        return bad(format!(
            "{} subjects; evidence is about exactly one tree",
            subjects.len()
        ));
    };
    match only.get("name") {
        Some(Value::String(name)) if name == crate::statement::SUBJECT_NAME => {}
        _ => return bad("the subject's name must be \"tree\"".into()),
    }
    for field in ["uri", "mediaType"] {
        if only.get(field).is_some_and(|value| !value.is_string()) {
            return bad(format!("the subject's {field} is not a string"));
        }
    }
    if only
        .get("annotations")
        .is_some_and(|value| !value.is_object())
    {
        return bad("the subject's annotations is not an object".into());
    }
    let Some(Value::Object(digest)) = only.get("digest") else {
        return bad("the subject has no digest object".into());
    };
    if let Some(other) = digest.keys().find(|key| *key != GIT_TREE) {
        return bad(format!(
            "the subject's digest has {other:?}; format 1 has only gitTree"
        ));
    }
    match digest.get(GIT_TREE) {
        Some(Value::String(tree)) => TreeDigest::git(tree).map(drop),
        Some(_) => bad("gitTree is not a string".into()),
        None => bad("the subject's digest has no gitTree".into()),
    }
}

pub(crate) fn predicate_error(field: &str, message: impl Into<String>) -> Invalid {
    Invalid::Predicate {
        field: field.to_owned(),
        message: message.into(),
    }
}

/// Whether `kind` is in the open vocabulary's form: `^[a-z][a-z0-9-]{0,62}$`.
pub fn is_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Why `target` is not one, if it is not: 1 to 512 bytes, no control
/// characters.
pub fn target_problem(target: &str) -> Option<String> {
    if target.is_empty() || target.len() > MAX_TARGET_BYTES {
        return Some(format!(
            "{} bytes; a target is 1 to {MAX_TARGET_BYTES} bytes",
            target.len()
        ));
    }
    target
        .chars()
        .any(char::is_control)
        .then(|| "a target has no control characters".to_owned())
}

fn check_predicate(predicate: &Map<String, Value>) -> Result<(), Invalid> {
    let required = |field: &str| {
        predicate
            .get(field)
            .ok_or_else(|| predicate_error(field, "required"))
    };
    match required("kind")? {
        Value::String(kind) if is_kind(kind) => {}
        _ => {
            return Err(predicate_error(
                "kind",
                "lowercase letters, digits and '-', starting with a letter, at most 63",
            ));
        }
    }
    match required("result")? {
        Value::String(result) if Outcome::parse(result).is_some() => {}
        _ => return Err(predicate_error("result", "pass, fail or error")),
    }
    match predicate.get("target") {
        None => {}
        Some(Value::String(target)) => {
            if let Some(problem) = target_problem(target) {
                return Err(predicate_error("target", problem));
            }
        }
        Some(_) => return Err(predicate_error("target", "not a string")),
    }
    let time = |field: &str| -> Result<SystemTime, Invalid> {
        match required(field)? {
            Value::String(text) if parse_time(text).is_some() => humantime::parse_rfc3339(text)
                .map_err(|error| predicate_error(field, error.to_string())),
            _ => Err(predicate_error(
                field,
                "an RFC 3339 time in UTC ending in Z, such as 2026-11-05T10:00:00Z",
            )),
        }
    };
    let started = time("startedOn")?;
    let finished = time("finishedOn")?;
    if finished < started {
        return Err(predicate_error("finishedOn", "before startedOn"));
    }
    if let Some(artifacts) = predicate.get("artifacts") {
        check_artifacts(artifacts)?;
    }
    match predicate.get("annotations") {
        None | Some(Value::Object(_)) => {}
        Some(_) => return Err(predicate_error("annotations", "not a JSON object")),
    }
    Ok(())
}

fn check_artifacts(artifacts: &Value) -> Result<(), Invalid> {
    let bad = |message: String| Err(predicate_error("artifacts", message));
    let Value::Array(artifacts) = artifacts else {
        return bad("not an array".into());
    };
    if artifacts.len() > MAX_ARTIFACTS {
        return bad(format!(
            "{} artifacts; at most {MAX_ARTIFACTS}",
            artifacts.len()
        ));
    }
    for (index, artifact) in artifacts.iter().enumerate() {
        let Value::Object(artifact) = artifact else {
            return bad(format!("artifact {index} is not an object"));
        };
        match artifact.get("name") {
            Some(Value::String(name)) if !name.is_empty() => {}
            _ => return bad(format!("artifact {index} has no name")),
        }
        let Some(Value::Object(digest)) = artifact.get("digest") else {
            return bad(format!("artifact {index} has no digest object"));
        };
        match digest.get("sha256") {
            Some(Value::String(sha256)) if crate::is_lower_hex(sha256, 64) => {}
            Some(_) => {
                return bad(format!(
                    "artifact {index}'s sha256 is not 64 lowercase hex digits"
                ));
            }
            None => return bad(format!("artifact {index} has no sha256 digest")),
        }
        if digest.values().any(|value| !value.is_string()) {
            return bad(format!(
                "artifact {index}'s digest has a value that is not a string"
            ));
        }
        for field in ["uri", "mediaType"] {
            if artifact.get(field).is_some_and(|value| !value.is_string()) {
                return bad(format!("artifact {index}'s {field} is not a string"));
            }
        }
        if artifact
            .get("annotations")
            .is_some_and(|value| !value.is_object())
        {
            return bad(format!("artifact {index}'s annotations is not an object"));
        }
    }
    Ok(())
}

/// The index fields of a statement, as [`Verified::check`] reads them;
/// also for a stored payload that no longer verifies.
pub fn index_of(statement: &Statement) -> Result<Index, Invalid> {
    let predicate = &statement.predicate;
    let time = |field: &str, text: &str| {
        parse_time(text).ok_or_else(|| predicate_error(field, "not an RFC 3339 time"))
    };
    Ok(Index {
        tree_id: statement
            .tree_id()
            .ok_or_else(|| Invalid::Subject("no gitTree".into()))?
            .to_owned(),
        kind: predicate.kind.clone(),
        result: predicate.result,
        target: predicate.target.clone(),
        started_at_ms: time("startedOn", &predicate.started_on)?,
        finished_at_ms: time("finishedOn", &predicate.finished_on)?,
        artifacts: predicate
            .artifacts
            .iter()
            .map(|artifact| Artifact {
                name: artifact.name.clone().unwrap_or_default(),
                sha256: artifact.digest.get("sha256").cloned().unwrap_or_default(),
                uri: artifact.uri.clone(),
                media_type: artifact.media_type.clone(),
            })
            .collect(),
    })
}

/// What [`strict_json`] found wrong.
#[derive(Debug)]
enum Problem {
    /// Not JSON, or trailing bytes after it.
    Syntax(String),
    /// A key repeated in one object, at `path`.
    Duplicate { path: Vec<String>, key: String },
    /// An array or object deeper than [`MAX_DEPTH`], at `path`.
    TooDeep { path: Vec<String> },
}

impl Problem {
    /// The predicate field a payload problem is inside, if any: the field
    /// it is under, or the duplicated field itself.
    fn predicate_field(&self) -> Option<String> {
        let (path, key) = match self {
            Problem::Syntax(_) => return None,
            Problem::Duplicate { path, key } => (path, Some(key)),
            Problem::TooDeep { path } => (path, None),
        };
        if path.first().map(String::as_str) != Some("predicate") {
            return None;
        }
        path.get(1).or(key).cloned()
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = |path: &[String]| {
            if path.is_empty() {
                "the top level".to_owned()
            } else {
                path.join(".")
            }
        };
        match self {
            Problem::Syntax(message) => f.write_str(message),
            Problem::Duplicate { path, key } => {
                write!(f, "the key {key:?} appears twice in {}", at(path))
            }
            Problem::TooDeep { path } => write!(
                f,
                "it nests more than {MAX_DEPTH} levels deep, at {}",
                at(path)
            ),
        }
    }
}

/// Walks `bytes` once as JSON, refusing a key repeated in one object and
/// nesting deeper than [`MAX_DEPTH`]. serde's derived types refuse a
/// duplicated struct field but not a duplicated map key, and two parsers
/// that keep different copies of one can disagree about what was signed.
fn strict_json(bytes: &[u8]) -> Result<(), Problem> {
    let mut walk = Walk {
        path: Vec::new(),
        problem: None,
    };
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let result = Node {
        walk: &mut walk,
        depth: 0,
    }
    .deserialize(&mut de)
    .and_then(|()| de.end());
    match (result, walk.problem) {
        (_, Some(problem)) => Err(problem),
        (Err(error), None) => Err(Problem::Syntax(error.to_string())),
        (Ok(()), None) => Ok(()),
    }
}

struct Walk {
    path: Vec<String>,
    problem: Option<Problem>,
}

struct Node<'w> {
    walk: &'w mut Walk,
    depth: usize,
}

impl Node<'_> {
    fn enter<E: serde::de::Error>(&mut self) -> Result<usize, E> {
        let depth = self.depth + 1;
        if depth > MAX_DEPTH {
            self.walk.problem = Some(Problem::TooDeep {
                path: self.walk.path.clone(),
            });
            return Err(E::custom("too deep"));
        }
        Ok(depth)
    }
}

impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<(), A::Error> {
        let depth = self.enter()?;
        let mut index = 0usize;
        loop {
            self.walk.path.push(index.to_string());
            let more = seq.next_element_seed(Node {
                walk: &mut *self.walk,
                depth,
            })?;
            self.walk.path.pop();
            if more.is_none() {
                return Ok(());
            }
            index += 1;
        }
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
        let depth = self.enter()?;
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                self.walk.problem = Some(Problem::Duplicate {
                    path: self.walk.path.clone(),
                    key,
                });
                return Err(A::Error::custom("duplicate key"));
            }
            self.walk.path.push(key);
            map.next_value_seed(Node {
                walk: &mut *self.walk,
                depth,
            })?;
            self.walk.path.pop();
        }
        Ok(())
    }
}
