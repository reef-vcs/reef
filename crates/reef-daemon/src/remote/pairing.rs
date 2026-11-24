//! Pairing a device (R3.40, ADR 0095): a code `reef remote pair` prints, typed
//! once on a phone, which trades it for its token.
//!
//! The code is 8 characters of Crockford's base32, 40 bits from the thread's
//! CSPRNG. Memory holds its blake3 digest, at most `max_pairings` at once,
//! `pairing_life` each, and a restart drops them. A wrong code counts against
//! its address as a wrong token does (`super::gate`), and
//! `pairing_wrong_codes` of them, from any addresses, end every code waiting.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rand::RngExt as _;

use super::{Limits, RemoteError};
use crate::db::Digest;
use crate::model::Timestamp;

/// The one method a pairing code calls, and the only credential it takes.
pub(crate) const REDEEM: &str = "/reef.api.v1.RemoteService/RedeemPairing";

/// What a pairing code's bearer starts with. No token does: a token's id is
/// hex.
const PREFIX: &str = "reef_pair_";

/// Crockford's base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;

/// A paired device's default life.
pub(crate) const DEFAULT_EXPIRY: Duration = Duration::from_hours(90 * 24);

/// A paired device's shortest and longest life.
const EXPIRY: std::ops::RangeInclusive<Duration> =
    Duration::from_mins(1)..=Duration::from_hours(366 * 24);

/// What a code makes: a device's token, for a person, at an address. The gate
/// puts it in the redeem's extensions once the code is taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pairing {
    pub(crate) name: String,
    pub(crate) identity_id: String,
    pub(crate) origin: String,
    /// How long the token lives.
    pub(crate) expires_in: Duration,
}

#[derive(Debug)]
struct Pending {
    digest: Digest,
    until: Timestamp,
    pairing: Pairing,
}

#[derive(Debug, Default)]
struct Waiting {
    pending: Vec<Pending>,
    /// Wrong codes since no code waited.
    wrong: u32,
}

/// Why a code was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wrong {
    /// Whether this one ended every code waiting.
    pub(crate) ended_all: bool,
}

/// The codes waiting to be typed.
#[derive(Debug)]
pub(crate) struct Pairings {
    limits: Limits,
    waiting: Mutex<Waiting>,
}

/// The pairing code a bearer holds, if it is one.
pub(crate) fn code_of(bearer: &str) -> Option<&str> {
    bearer.strip_prefix(PREFIX)
}

/// The life asked for, or the default.
pub(crate) fn expiry(asked: Option<Duration>) -> Result<Duration, RemoteError> {
    let life = asked.unwrap_or(DEFAULT_EXPIRY);
    if EXPIRY.contains(&life) {
        Ok(life)
    } else {
        Err(RemoteError::InvalidDuration {
            field: "expires_in",
            reason: "a device stays paired for 1 minute to 366 days",
        })
    }
}

fn digest(code: &str) -> Digest {
    *blake3::hash(code.as_bytes()).as_bytes()
}

/// `typed` as the daemon drew it: upper case, without hyphens and spaces, and
/// with the letters Crockford's base32 leaves out read as the digits they
/// look like.
fn canonical(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

fn new_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_LEN)
        .map(|_| char::from(ALPHABET[usize::from(rng.random::<u8>() % 32)]))
        .collect()
}

impl Pairings {
    pub(crate) fn new(limits: &Limits) -> Pairings {
        Pairings {
            limits: *limits,
            waiting: Mutex::new(Waiting::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Timestamp::now();
        waiting.pending.retain(|pending| pending.until > now);
        if waiting.pending.is_empty() {
            waiting.wrong = 0;
        }
        waiting
    }

    /// A new code for `pairing`, and when it stops working. One waiting under
    /// the same name gives way to it.
    pub(crate) fn start(&self, pairing: Pairing) -> Result<(String, Timestamp), RemoteError> {
        let mut waiting = self.lock();
        waiting
            .pending
            .retain(|pending| pending.pairing.name != pairing.name);
        if waiting.pending.len() >= self.limits.max_pairings {
            return Err(RemoteError::TooManyPairings {
                limit: self.limits.max_pairings,
            });
        }
        let code = new_code();
        let until = super::tokens::later(Timestamp::now(), self.limits.pairing_life);
        waiting.pending.push(Pending {
            digest: digest(&code),
            until,
            pairing,
        });
        Ok((code, until))
    }

    /// Takes the pairing `typed` is the code of: a second call finds nothing.
    pub(crate) fn take(&self, typed: &str) -> Result<Pairing, Wrong> {
        let presented = digest(&canonical(typed));
        let mut waiting = self.lock();
        let found = waiting
            .pending
            .iter()
            .position(|pending| super::tokens::same(&pending.digest, &presented));
        if let Some(index) = found {
            return Ok(waiting.pending.swap_remove(index).pairing);
        }
        if waiting.pending.is_empty() {
            return Err(Wrong { ended_all: false });
        }
        waiting.wrong += 1;
        let ended_all = waiting.wrong >= self.limits.pairing_wrong_codes;
        if ended_all {
            waiting.pending.clear();
        }
        Err(Wrong { ended_all })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(name: &str) -> Pairing {
        Pairing {
            name: name.to_owned(),
            identity_id: "idn_1".to_owned(),
            origin: "https://mac.example:7470".to_owned(),
            expires_in: DEFAULT_EXPIRY,
        }
    }

    #[test]
    fn a_code_is_drawn_from_the_alphabet_and_taken_once() {
        let pairings = Pairings::new(&Limits::default());
        let (code, _) = pairings.start(pairing("iphone")).unwrap();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        let typed = format!("{}-{} ", code[..4].to_lowercase(), &code[4..]);
        assert_eq!(pairings.take(&typed).unwrap(), pairing("iphone"));
        assert_eq!(pairings.take(&code), Err(Wrong { ended_all: false }));
    }

    // Review R6: every character may be each of the 32, and codes do not repeat.
    #[test]
    fn codes_are_40_bits() {
        use std::collections::HashSet;
        let codes: HashSet<String> = (0..2_000).map(|_| new_code()).collect();
        assert_eq!(codes.len(), 2_000);
        for position in 0..CODE_LEN {
            let seen: HashSet<u8> = codes.iter().map(|code| code.as_bytes()[position]).collect();
            assert_eq!(seen, HashSet::from(*ALPHABET), "character {position}");
        }
        assert!(codes.iter().all(|code| code.len() == CODE_LEN));
    }

    #[test]
    fn letters_a_person_mistakes_are_read_as_digits() {
        assert_eq!(canonical("oO-iI lL-9z"), "0011119Z");
    }

    #[test]
    fn a_second_code_under_one_name_replaces_the_first() {
        let pairings = Pairings::new(&Limits::default());
        let (first, _) = pairings.start(pairing("iphone")).unwrap();
        let (second, _) = pairings.start(pairing("iphone")).unwrap();
        assert!(pairings.take(&first).is_err());
        assert!(pairings.take(&second).is_ok());
    }

    #[test]
    fn limits() {
        let pairings = Pairings::new(&Limits {
            max_pairings: 2,
            pairing_wrong_codes: 3,
            ..Limits::default()
        });
        let (a, _) = pairings.start(pairing("a")).unwrap();
        pairings.start(pairing("b")).unwrap();
        assert!(matches!(
            pairings.start(pairing("c")),
            Err(RemoteError::TooManyPairings { limit: 2 })
        ));
        for ended_all in [false, false, true] {
            assert_eq!(pairings.take("ZZZZZZZZ"), Err(Wrong { ended_all }));
        }
        assert!(pairings.take(&a).is_err());
        // The count starts over with the next code.
        let (c, _) = pairings.start(pairing("c")).unwrap();
        assert_eq!(pairings.take("ZZZZZZZZ"), Err(Wrong { ended_all: false }));
        assert!(pairings.take(&c).is_ok());
    }

    #[test]
    fn a_code_expires() {
        let pairings = Pairings::new(&Limits {
            pairing_life: Duration::from_millis(1),
            ..Limits::default()
        });
        let (code, _) = pairings.start(pairing("iphone")).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(pairings.take(&code).is_err());
    }
}
