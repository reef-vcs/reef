//! GC's sweep of session keys (R3.03): in the first report of a run, as
//! the stores step does for machine-wide stores, keys whose `not_after` is
//! before `min(now - 7 days, keep_newer)` and that no evidence in any
//! repository names, then the sessions left with no key. Identities and
//! authorities are never deleted: a certificate is checked against its
//! authority's public key for as long as evidence names its key.

use std::time::{Duration, SystemTime};

use crate::db::{Db, DbError};
use crate::model::Timestamp;

/// How long past its window a key is kept at the least.
pub const FLOOR: Duration = Duration::from_hours(7 * 24);

/// The SQL condition, on a `session_keys` row `k`, under which GC keeps a
/// key whatever its age: evidence names it. R3.04 extends it to keys that
/// provenance names.
pub fn pinned() -> &'static str {
    "k.key_id IN (SELECT key_id FROM evidence)"
}

/// What the sweep removed, or would have in a dry run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Session keys.
    pub keys: u32,
    /// Sessions left with no key.
    pub sessions: u32,
}

/// Sweeps the session keys that ended before `min(now - FLOOR,
/// keep_newer)` and that nothing pins.
pub fn sweep(db: &Db, keep_newer: SystemTime, dry_run: bool) -> Result<Swept, DbError> {
    let floor = SystemTime::now()
        .checked_sub(FLOOR)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let before = Timestamp::from(floor.min(keep_newer));
    let (keys, sessions) = db.sweep_session_keys(before, pinned(), dry_run)?;
    if keys + sessions > 0 {
        tracing::info!(
            session_key_count = keys,
            session_count = sessions,
            dry_run,
            "swept expired session keys no evidence names"
        );
    }
    Ok(Swept { keys, sessions })
}
