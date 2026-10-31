//! Remote tokens (R2.08, ADR 0049): `reef_<id>_<secret>`, a 16-hex id that
//! finds the row and a 256-bit secret kept only as its blake3 digest.
//!
//! Every token is in memory, loaded from `remote_tokens` at start and
//! written through on create, rotate and revoke; last use is kept in
//! memory and flushed at most once a minute, so a request costs no
//! database write. Each live token has a cancellation token, cancelled
//! when it is revoked or expires, which ends the streams it holds.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt as _;
use tokio_util::sync::CancellationToken;

use super::{Limits, Refusal, RemoteError};
use crate::db::{Db, Digest, RemoteTokenRow, TokenScope};
use crate::model::Timestamp;

/// A token's prefix.
const PREFIX: &str = "reef_";
/// The id's length in hex characters.
const ID_LEN: usize = 16;
/// The secret's length in base64url characters: 32 bytes, no padding.
const SECRET_LEN: usize = 43;
/// A token's default life.
pub const DEFAULT_EXPIRY: Duration = Duration::from_hours(30 * 24);
/// The longest rotation overlap.
pub const MAX_OVERLAP: Duration = Duration::from_hours(7 * 24);

/// One token, as held in memory.
#[derive(Debug)]
struct Entry {
    row: RemoteTokenRow,
    /// Cancelled when the token is revoked or expires.
    ended: CancellationToken,
    /// Requests in flight, streams included.
    in_flight: usize,
    /// Streams open.
    streams: usize,
    /// The last use not yet written to the database.
    unflushed: Option<(Timestamp, String)>,
}

/// The tokens.
pub struct Tokens {
    db: Arc<Db>,
    limits: Limits,
    /// Every token, by id, live or ended.
    entries: Mutex<HashMap<String, Entry>>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").finish_non_exhaustive()
    }
}

/// A verified request's hold on its token: dropping it, when the response
/// body ends, ends the request's use.
#[derive(Debug)]
pub struct Use {
    tokens: Arc<Tokens>,
    id: String,
    stream: bool,
}

impl Drop for Use {
    fn drop(&mut self) {
        let mut entries = self.tokens.lock();
        if let Some(entry) = entries.get_mut(&self.id) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            if self.stream {
                entry.streams = entry.streams.saturating_sub(1);
            }
        }
    }
}

/// What [`Tokens::verify`] found: the token, for the request's [`super::Caller`].
#[derive(Debug)]
pub struct Verified {
    /// The token's id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its scope.
    pub scope: TokenScope,
    /// Cancelled when it is revoked or expires.
    pub ended: CancellationToken,
}

/// Why [`Tokens::verify`] or [`Tokens::hold`] refused.
#[derive(Debug)]
pub enum Refused {
    /// Not a live token.
    Token(Refusal),
    /// Over a per-token limit: `requests` or `streams`.
    Busy(&'static str),
}

/// `token`'s id and secret, if it has the token format.
fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(PREFIX)?;
    let (id, secret) = rest.split_at_checked(ID_LEN)?;
    let secret = secret.strip_prefix('_')?;
    let hex = id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let base64url = secret
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (hex && secret.len() == SECRET_LEN && base64url).then_some((id, secret))
}

fn digest(secret: &str) -> Digest {
    *blake3::hash(secret.as_bytes()).as_bytes()
}

/// Constant-time equality of two digests (`blake3::Hash`'s `PartialEq`).
fn same(a: &Digest, b: &Digest) -> bool {
    blake3::Hash::from_bytes(*a) == blake3::Hash::from_bytes(*b)
}

/// Whether `name` is `[a-z0-9][a-z0-9._-]{0,62}`.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
}

/// A new secret: 32 bytes from the thread's CSPRNG, base64url.
fn new_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A new id: 8 bytes from the thread's CSPRNG, hex.
fn new_id() -> String {
    use std::fmt::Write as _;
    let bytes: [u8; 8] = rand::rng().random();
    bytes.iter().fold(String::with_capacity(16), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn later(at: Timestamp, by: Duration) -> Timestamp {
    Timestamp(at.0.saturating_add(crate::model::duration_millis(by)))
}

impl Tokens {
    /// The tokens in `db`; expired ones are ended at once.
    pub fn load(db: Arc<Db>, limits: Limits) -> Result<Tokens, RemoteError> {
        let now = Timestamp::now();
        let entries = db
            .remote_tokens()?
            .into_iter()
            .map(|row| {
                let ended = CancellationToken::new();
                if !row.is_live(now) {
                    ended.cancel();
                }
                let entry = Entry {
                    row,
                    ended,
                    in_flight: 0,
                    streams: 0,
                    unflushed: None,
                };
                (entry.row.id.clone(), entry)
            })
            .collect();
        Ok(Tokens {
            db,
            limits,
            entries: Mutex::new(entries),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tokens neither revoked nor expired.
    pub fn live_count(&self) -> usize {
        let now = Timestamp::now();
        self.lock()
            .values()
            .filter(|entry| entry.row.is_live(now))
            .count()
    }

    /// The live token named `name`, by id.
    fn live_named<'a>(
        entries: &'a mut HashMap<String, Entry>,
        name: &str,
        now: Timestamp,
    ) -> Option<&'a mut Entry> {
        entries
            .values_mut()
            .find(|entry| entry.row.name == name && entry.row.is_live(now))
    }

    /// Issues a token; `expires_in` `None` is never. Returns its row and
    /// its bearer token.
    pub fn create(
        &self,
        name: &str,
        scope: TokenScope,
        expires_in: Option<Duration>,
    ) -> Result<(RemoteTokenRow, String), RemoteError> {
        if !valid_name(name) {
            return Err(RemoteError::InvalidTokenName {
                name: name.to_owned(),
            });
        }
        if expires_in.is_some_and(|life| life.is_zero()) {
            return Err(RemoteError::InvalidDuration {
                field: "expires_in",
                reason: "must be positive",
            });
        }
        let now = Timestamp::now();
        let mut entries = self.lock();
        if Self::live_named(&mut entries, name, now).is_some() {
            return Err(RemoteError::TokenExists {
                name: name.to_owned(),
            });
        }
        let live = entries.values().filter(|e| e.row.is_live(now)).count();
        if live >= self.limits.max_tokens {
            return Err(RemoteError::TooManyTokens {
                limit: self.limits.max_tokens,
            });
        }
        let id = loop {
            let id = new_id();
            if !entries.contains_key(&id) {
                break id;
            }
        };
        let secret = new_secret();
        let row = RemoteTokenRow {
            id: id.clone(),
            name: name.to_owned(),
            scope,
            digest: digest(&secret),
            previous_digest: None,
            previous_until: None,
            created_at: now,
            expires_at: expires_in.map(|life| later(now, life)),
            rotated_at: None,
            revoked_at: None,
            last_used_at: None,
            last_used_from: None,
        };
        self.db.insert_remote_token(&row)?;
        entries.insert(
            id.clone(),
            Entry {
                row: row.clone(),
                ended: CancellationToken::new(),
                in_flight: 0,
                streams: 0,
                unflushed: None,
            },
        );
        Ok((row, format!("{PREFIX}{id}_{secret}")))
    }

    /// Gives the live token `name` a new secret; the old one keeps working
    /// for `overlap`.
    pub fn rotate(
        &self,
        name: &str,
        overlap: Duration,
    ) -> Result<(RemoteTokenRow, String), RemoteError> {
        if overlap > MAX_OVERLAP {
            return Err(RemoteError::InvalidDuration {
                field: "overlap",
                reason: "must be at most 7 days",
            });
        }
        let now = Timestamp::now();
        let mut entries = self.lock();
        let Some(entry) = Self::live_named(&mut entries, name, now) else {
            return Err(RemoteError::TokenNotFound {
                name: name.to_owned(),
            });
        };
        let secret = new_secret();
        let mut row = entry.row.clone();
        row.previous_digest = (!overlap.is_zero()).then_some(row.digest);
        row.previous_until = (!overlap.is_zero()).then(|| later(now, overlap));
        row.digest = digest(&secret);
        row.rotated_at = Some(now);
        self.db.rotate_remote_token(&row)?;
        entry.row = row.clone();
        // Streams opened with the old secret end; their clients resume with
        // whichever secret they hold, so a rotation after a leak cuts the
        // leaked secret's streams, and one with an overlap only reconnects.
        entry.ended.cancel();
        entry.ended = CancellationToken::new();
        Ok((row.clone(), format!("{PREFIX}{}_{secret}", row.id)))
    }

    /// Ends the live token `name` at once, and its streams.
    pub fn revoke(&self, name: &str) -> Result<RemoteTokenRow, RemoteError> {
        let now = Timestamp::now();
        let mut entries = self.lock();
        let Some(entry) = Self::live_named(&mut entries, name, now) else {
            return Err(RemoteError::TokenNotFound {
                name: name.to_owned(),
            });
        };
        self.db.revoke_remote_token(&entry.row.id, now)?;
        entry.row.revoked_at = Some(now);
        entry.ended.cancel();
        Ok(entry.row.clone())
    }

    /// The token `bearer` names, if live, recording its use from `peer`.
    /// A refusal says `expired` or `revoked` only to a caller whose secret
    /// matches, so a guess learns nothing about which ids exist.
    pub fn verify(&self, bearer: Option<&str>, peer: SocketAddr) -> Result<Verified, Refusal> {
        let (id, secret) = bearer.and_then(parse).ok_or(Refusal::Unknown)?;
        let presented = digest(secret);
        let now = Timestamp::now();
        let mut entries = self.lock();
        let entry = entries.get_mut(id).ok_or(Refusal::Unknown)?;
        let row = &entry.row;
        let matches = same(&row.digest, &presented)
            || row
                .previous_digest
                .as_ref()
                .zip(row.previous_until)
                .is_some_and(|(previous, until)| until > now && same(previous, &presented));
        if !matches {
            return Err(Refusal::Unknown);
        }
        if row.revoked_at.is_some() {
            return Err(Refusal::Revoked);
        }
        if row.expires_at.is_some_and(|at| at <= now) {
            entry.ended.cancel();
            return Err(Refusal::Expired);
        }
        entry.unflushed = Some((now, peer.to_string()));
        Ok(Verified {
            id: row.id.clone(),
            name: row.name.clone(),
            scope: row.scope,
            ended: entry.ended.clone(),
        })
    }

    /// Why token `id`'s stream ended: `revoked`, `expired`, or `unknown`
    /// (its secret was rotated).
    pub fn ended_state(&self, id: &str) -> Refusal {
        match self.lock().get(id) {
            Some(entry) if entry.row.revoked_at.is_some() => Refusal::Revoked,
            Some(entry) if !entry.row.is_live(Timestamp::now()) => Refusal::Expired,
            Some(_) => Refusal::Unknown,
            None => Refusal::Revoked,
        }
    }

    /// Takes a request slot on token `id`, and a stream slot if `stream`,
    /// under the per-token limits.
    pub fn hold(self: &Arc<Self>, id: &str, stream: bool) -> Result<Use, Refused> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(id)
            .ok_or(Refused::Token(Refusal::Unknown))?;
        // Checked again under this lock: a revoke between `verify` and here
        // refuses the request.
        if entry.row.revoked_at.is_some() {
            return Err(Refused::Token(Refusal::Revoked));
        }
        if !entry.row.is_live(Timestamp::now()) {
            return Err(Refused::Token(Refusal::Expired));
        }
        if stream && entry.streams >= self.limits.max_streams_per_token {
            return Err(Refused::Busy("streams"));
        }
        if entry.in_flight >= self.limits.max_requests_per_token {
            return Err(Refused::Busy("requests"));
        }
        entry.in_flight += 1;
        if stream {
            entry.streams += 1;
        }
        Ok(Use {
            tokens: Arc::clone(self),
            id: id.to_owned(),
            stream,
        })
    }

    /// Every token, last use included, newest first.
    pub fn rows(&self) -> Vec<RemoteTokenRow> {
        let mut rows: Vec<RemoteTokenRow> = self
            .lock()
            .values()
            .map(|entry| {
                let mut row = entry.row.clone();
                if let Some((at, from)) = &entry.unflushed {
                    row.last_used_at = Some(*at);
                    row.last_used_from = Some(from.clone());
                }
                row
            })
            .collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        rows
    }

    /// Writes last uses to the database.
    pub fn flush(&self) -> Result<(), RemoteError> {
        let uses: Vec<(String, Timestamp, String)> = {
            let mut entries = self.lock();
            entries
                .values_mut()
                .filter_map(|entry| {
                    let (at, from) = entry.unflushed.take()?;
                    entry.row.last_used_at = Some(at);
                    entry.row.last_used_from = Some(from.clone());
                    Some((entry.row.id.clone(), at, from))
                })
                .collect()
        };
        self.db.touch_remote_tokens(&uses)?;
        Ok(())
    }

    /// Logs each live token that expires within 7 days, once a day.
    fn log_expiring(&self) {
        let now = Timestamp::now();
        let soon = later(now, Duration::from_hours(7 * 24));
        for entry in self.lock().values() {
            let row = &entry.row;
            if let Some(at) = row.expires_at.filter(|at| row.is_live(now) && *at <= soon) {
                tracing::info!(
                    token_id = %row.id,
                    token_name = %row.name,
                    expires_at = %at,
                    "a remote token expires within 7 days; rotate it or create another"
                );
            }
        }
    }

    /// Ends expired tokens' streams.
    pub fn sweep(&self) {
        let now = Timestamp::now();
        for entry in self.lock().values() {
            if !entry.row.is_live(now) && !entry.ended.is_cancelled() {
                entry.ended.cancel();
            }
        }
    }

    /// Flushes last uses every `last_used_flush` and ends expired tokens'
    /// streams every `expiry_sweep`, until `shutdown`, then flushes once
    /// more.
    pub fn run(self: &Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let tokens = Arc::clone(self);
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(tokens.limits.expiry_sweep);
            let mut flush = tokio::time::interval(tokens.limits.last_used_flush);
            let mut daily = tokio::time::interval(Duration::from_hours(24));
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = sweep.tick() => tokens.sweep(),
                    _ = daily.tick() => tokens.log_expiring(),
                    _ = flush.tick() => {
                        if let Err(error) = tokens.flush() {
                            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot record remote tokens' last use");
                        }
                    }
                }
            }
            if let Err(error) = tokens.flush() {
                tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot record remote tokens' last use");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(limits: &Limits) -> Arc<Tokens> {
        Arc::new(Tokens::load(Arc::new(Db::open_in_memory().unwrap()), *limits).unwrap())
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    #[test]
    fn names() {
        for name in ["laptop", "ci-runner-3", "a", "0.b_c-d", &"a".repeat(63)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "Laptop", "-a", ".a", "a b", "a/b", &"a".repeat(64), "é"] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn format_and_verify() {
        let tokens = tokens(&Limits::default());
        let (row, token) = tokens.create("laptop", TokenScope::Read, None).unwrap();
        assert_eq!(token.len(), 65, "{token}");
        assert!(token.starts_with(&format!("reef_{}_", row.id)));
        assert_eq!(parse(&token).map(|(id, _)| id), Some(row.id.as_str()));
        let verified = tokens.verify(Some(&token), peer()).unwrap();
        assert_eq!(verified.name, "laptop");
        assert_eq!(verified.scope, TokenScope::Read);
        // Only the digest is kept.
        let secret = parse(&token).unwrap().1;
        assert_ne!(row.digest.as_slice(), secret.as_bytes());
        // Wrong secret, unknown id, malformed: all unknown.
        let wrong = format!("reef_{}_{}", row.id, "A".repeat(43));
        let unknown = format!("reef_0000000000000000_{}", &token[22..]);
        for bad in [
            None,
            Some(""),
            Some("x"),
            Some(wrong.as_str()),
            Some(unknown.as_str()),
        ] {
            assert!(
                matches!(tokens.verify(bad, peer()), Err(Refusal::Unknown)),
                "{bad:?}"
            );
        }
        assert!(matches!(
            tokens.create("laptop", TokenScope::Write, None),
            Err(RemoteError::TokenExists { .. })
        ));
    }

    #[test]
    fn rotate_and_revoke() {
        let tokens = tokens(&Limits::default());
        let (_, old) = tokens.create("laptop", TokenScope::Write, None).unwrap();
        let (_, new) = tokens.rotate("laptop", Duration::ZERO).unwrap();
        assert!(tokens.verify(Some(&new), peer()).is_ok());
        assert!(matches!(
            tokens.verify(Some(&old), peer()),
            Err(Refusal::Unknown)
        ));
        let (_, newer) = tokens.rotate("laptop", Duration::from_hours(1)).unwrap();
        assert!(tokens.verify(Some(&new), peer()).is_ok());
        assert!(tokens.verify(Some(&newer), peer()).is_ok());
        let ended = tokens.verify(Some(&newer), peer()).unwrap().ended;
        tokens.revoke("laptop").unwrap();
        assert!(ended.is_cancelled());
        assert!(matches!(
            tokens.verify(Some(&newer), peer()),
            Err(Refusal::Revoked)
        ));
        assert!(matches!(
            tokens.revoke("laptop"),
            Err(RemoteError::TokenNotFound { .. })
        ));
        // The name is free again.
        tokens.create("laptop", TokenScope::Read, None).unwrap();
        assert!(matches!(
            tokens.rotate("laptop", MAX_OVERLAP + Duration::from_secs(1)),
            Err(RemoteError::InvalidDuration { .. })
        ));
    }

    #[test]
    fn limits() {
        let tokens = tokens(&Limits {
            max_tokens: 1,
            max_requests_per_token: 2,
            max_streams_per_token: 1,
            ..Limits::default()
        });
        let (row, _) = tokens.create("a", TokenScope::Read, None).unwrap();
        assert!(matches!(
            tokens.create("b", TokenScope::Read, None),
            Err(RemoteError::TooManyTokens { limit: 1 })
        ));
        let stream = tokens.hold(&row.id, true).unwrap();
        assert!(matches!(
            tokens.hold(&row.id, true),
            Err(Refused::Busy("streams"))
        ));
        let call = tokens.hold(&row.id, false).unwrap();
        assert!(matches!(
            tokens.hold(&row.id, false),
            Err(Refused::Busy("requests"))
        ));
        drop((stream, call));
        tokens.hold(&row.id, true).unwrap();
    }

    #[test]
    fn expiry_and_persistence() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tokens = Arc::new(Tokens::load(Arc::clone(&db), Limits::default()).unwrap());
        let (_, token) = tokens
            .create("short", TokenScope::Read, Some(Duration::from_millis(1)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            tokens.verify(Some(&token), peer()),
            Err(Refusal::Expired)
        ));
        let (_, kept) = tokens.create("kept", TokenScope::Write, None).unwrap();
        tokens.verify(Some(&kept), peer()).unwrap();
        tokens.flush().unwrap();
        let reloaded = Tokens::load(db, Limits::default()).unwrap();
        assert_eq!(reloaded.live_count(), 1);
        assert!(reloaded.verify(Some(&kept), peer()).is_ok());
        let kept_row = reloaded
            .rows()
            .into_iter()
            .find(|r| r.name == "kept")
            .unwrap();
        assert_eq!(kept_row.last_used_from.as_deref(), Some("127.0.0.1:5000"));
    }
}
