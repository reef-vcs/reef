//! Reading provenance records (R3.04): every read checks each record again,
//! in ADR 0055's order, and takes every field it reports from the verified
//! payload, never from the columns, as evidence reads do.
//!
//! 1. The envelope, its key from `evidence_keys`, the signature, the
//!    statement; then every index column must equal the payload, the id
//!    must be the canonical envelope's SHA-256. All of that is
//!    `SIGNATURE_CHECK_VALID`; anything else `INVALID`.
//! 2. The key's identity: R3.03's certificate check and status at the
//!    record's `recorded_at`, the daemon being its own witness. On top of
//!    R3.03's rules, a certificate whose custody is not `daemon`, or whose
//!    session is not the record's, makes the identity `INVALID`: a
//!    client's key cannot sign provenance that reads as the daemon's, and a
//!    held key cannot vouch for another session.
//!
//! The keys of a page are looked up in one query, and each distinct
//! certificate is checked once.

use std::collections::{BTreeSet, HashMap};

use reef_api::v1;
use reef_evidence::provenance::{Checked, check, context_digest};
use reef_evidence::session::Custody;

use super::session::context_message;
use crate::db::{Db, DbError, DeclaredColumns, StoredProvenance};
use crate::identity::read::{ReadKey, key_identity, read_keys, unverified_identity};
use crate::model::Timestamp;
use crate::service::convert::timestamp;

/// Whether `checked` is `stored`'s record: its canonical form, its id, and
/// every index column.
fn matches(stored: &StoredProvenance, checked: &Checked) -> bool {
    let row = &stored.row;
    let record = &checked.record;
    let context = &record.context;
    checked.canonical == row.envelope
        && checked.id == row.id
        && row.format == 1
        && row.commit_id == record.commit_id
        && row.change_id == record.change_id
        && row.tree_id == record.tree_id
        && row.workspace_id == record.workspace
        && row.session_id == record.session
        && u64::try_from(row.sequence).ok() == Some(record.sequence)
        && row.kind == record.kind
        && row.key_id == checked.key_id().as_str()
        && row.agent == record.agent
        && row.declared
            == DeclaredColumns {
                model: context.model.clone(),
                harness: context.harness.clone(),
                task: context.task.clone(),
                instruction_sha256: context.instruction_sha256.clone(),
                client_session: context.client_session.clone(),
            }
        && row.context_digest == context_digest(context)
        && row.recorded_at.0 == record.recorded_on_ms
}

/// `rows` read as the API's records; with `with_envelope`, each carries its
/// canonical envelope. Blocking: the checks are CPU work.
pub fn read_records(
    db: &Db,
    rows: Vec<StoredProvenance>,
    with_envelope: bool,
) -> Result<Vec<v1::ProvenanceRecord>, DbError> {
    let checked: Vec<(StoredProvenance, Option<Checked>)> = rows
        .into_iter()
        .map(|stored| {
            let checked = stored
                .public_key
                .as_deref()
                .and_then(|key| check(&stored.row.envelope, key).ok());
            (stored, checked)
        })
        .collect();
    let key_ids: BTreeSet<&str> = checked
        .iter()
        .filter_map(|(_, checked)| checked.as_ref().map(|c| c.key_id().as_str()))
        .collect();
    let keys: HashMap<String, ReadKey> = if key_ids.is_empty() {
        HashMap::new()
    } else {
        let rows = db.session_keys_by_id(&key_ids.into_iter().collect::<Vec<_>>())?;
        // Provenance is signed by daemon-held keys, never keyless ones.
        read_keys(db, rows, None)?
            .into_iter()
            .map(|read| (read.row.key_id.clone(), read))
            .collect()
    };
    Ok(checked
        .into_iter()
        .map(|(stored, checked)| record_message(&stored, checked.as_ref(), &keys, with_envelope))
        .collect())
}

fn record_message(
    stored: &StoredProvenance,
    checked: Option<&Checked>,
    keys: &HashMap<String, ReadKey>,
    with_envelope: bool,
) -> v1::ProvenanceRecord {
    let row = &stored.row;
    let recorded_at = Timestamp(row.recorded_at.0);
    let valid = checked.is_some_and(|checked| matches(stored, checked));
    let mut message = v1::ProvenanceRecord {
        id: row.id.clone(),
        envelope: if with_envelope {
            row.envelope.clone()
        } else {
            Vec::new()
        },
        ..v1::ProvenanceRecord::default()
    };
    message.set_signature(if valid {
        v1::SignatureCheck::Valid
    } else {
        v1::SignatureCheck::Invalid
    });
    let Some(checked) = checked else {
        if !valid {
            tracing::warn!(
                provenance_id = %row.id,
                "a stored provenance record does not verify; it reads as invalid"
            );
        }
        message.commit_id.clone_from(&row.commit_id);
        message.recorded_at = Some(timestamp(recorded_at));
        message.identity = Some(unverified_identity(recorded_at));
        return message;
    };
    let record = &checked.record;
    message.commit_id.clone_from(&record.commit_id);
    message.change_id.clone_from(&record.change_id);
    message.tree_id = record.tree_id.clone().unwrap_or_default();
    message.workspace_id.clone_from(&record.workspace);
    message.session_id.clone_from(&record.session);
    message.sequence = record.sequence;
    message.kind.clone_from(&record.kind);
    message.agent = record.agent.clone().unwrap_or_default();
    let recorded_at = Timestamp(record.recorded_on_ms);
    message.recorded_at = Some(timestamp(recorded_at));
    let context = &record.context;
    message.context = Some(context_message(
        &DeclaredColumns {
            model: context.model.clone(),
            harness: context.harness.clone(),
            task: context.task.clone(),
            instruction_sha256: context.instruction_sha256.clone(),
            client_session: context.client_session.clone(),
        },
        context.declared_on_ms.map(Timestamp),
    ));
    message.key_id = checked.key_id().to_string();
    if !valid {
        tracing::warn!(
            provenance_id = %row.id,
            "a stored provenance record's index differs from its payload; it reads as invalid"
        );
        message.identity = Some(unverified_identity(recorded_at));
        return message;
    }
    let read = keys.get(checked.key_id().as_str());
    let mut identity = key_identity(read, recorded_at);
    let foreign = read
        .and_then(|read| read.certified.as_ref())
        .is_some_and(|certified| {
            certified.binding.custody != Custody::Daemon
                || certified.binding.session != record.session
        });
    if foreign {
        identity.set_status(v1::IdentityStatus::Invalid);
    }
    message.identity = Some(identity);
    message
}
