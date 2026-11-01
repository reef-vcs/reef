//! Per-address limits on the remote listener (R2.08): open connections per
//! address, and backoff for an address that keeps sending bad tokens.
//!
//! IPv4 addresses count one by one, IPv6 addresses by /64, since one host
//! usually holds a whole /64. A guess at a 256-bit secret cannot succeed
//! at any rate, so the backoff is for resources and log volume: an address
//! with `failures_before_backoff` refusals within `failure_window` has its
//! connections closed before TLS for `backoff`, doubling per repeat up to
//! `backoff_max`, forgotten after `backoff_reset` without a refusal.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::Limits;

/// An address as the limits count it: IPv4 whole, IPv6 by /64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key(u128);

impl From<IpAddr> for Key {
    fn from(ip: IpAddr) -> Key {
        match ip {
            IpAddr::V4(v4) => Key(u128::from(u32::from(v4))),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Key(u128::from(u32::from(v4))),
                None => Key((u128::from(v6) >> 64) | (1 << 127)),
            },
        }
    }
}

#[derive(Debug)]
struct Record {
    /// Refusals in the current window, and when it began.
    window: (Instant, u32),
    /// Until when the address is in backoff.
    until: Option<Instant>,
    /// The backoff its next period gets.
    next: Duration,
    last_failure: Instant,
    /// When a refusal was last logged, and how many since.
    logged: Option<(Instant, u32)>,
}

/// The per-address state.
#[derive(Debug)]
pub(crate) struct Failures {
    limits: Limits,
    records: Mutex<HashMap<Key, Record>>,
    open: Mutex<HashMap<Key, usize>>,
}

/// A connection counted against its address until dropped.
#[derive(Debug)]
pub(crate) struct Open {
    failures: Arc<Failures>,
    key: Key,
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut open = self
            .failures
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = open.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.key);
            }
        }
    }
}

/// What [`Failures::refused`] says to log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Logged {
    /// Refusals since the last line, this one included.
    pub(crate) count: u32,
    /// Whether this refusal put the address in backoff, and for how long.
    pub(crate) backoff: Option<Duration>,
}

impl Failures {
    pub(crate) fn new(limits: &Limits) -> Failures {
        Failures {
            limits: *limits,
            records: Mutex::new(HashMap::new()),
            open: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `ip` is in backoff now.
    pub(crate) fn blocked(&self, ip: IpAddr) -> bool {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records
            .get(&Key::from(ip))
            .and_then(|record| record.until)
            .is_some_and(|until| Instant::now() < until)
    }

    /// Counts a connection from `ip`, or `None` at the per-address limit.
    pub(crate) fn open(self: &Arc<Self>, ip: IpAddr) -> Option<Open> {
        let key = Key::from(ip);
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let count = open.entry(key).or_insert(0);
        if *count >= self.limits.max_connections_per_address {
            return None;
        }
        *count += 1;
        Some(Open {
            failures: Arc::clone(self),
            key,
        })
    }

    /// Records a token refusal from `ip`. Returns what to log, or `None`
    /// when a line for this address was logged within
    /// `refusal_log_every`.
    pub(crate) fn refused(&self, ip: IpAddr) -> Option<Logged> {
        let now = Instant::now();
        let limits = &self.limits;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let key = Key::from(ip);
        if !records.contains_key(&key) && records.len() >= limits.tracked_addresses {
            let oldest = records
                .iter()
                .min_by_key(|(_, record)| record.last_failure)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                records.remove(&oldest);
            }
        }
        let record = records.entry(key).or_insert_with(|| Record {
            window: (now, 0),
            until: None,
            next: limits.backoff,
            last_failure: now,
            logged: None,
        });
        if now.duration_since(record.last_failure) >= limits.backoff_reset {
            record.next = limits.backoff;
        }
        record.last_failure = now;
        if now.duration_since(record.window.0) >= limits.failure_window {
            record.window = (now, 0);
        }
        record.window.1 += 1;
        let mut backoff = None;
        if record.window.1 >= limits.failures_before_backoff {
            record.until = Some(now + record.next);
            backoff = Some(record.next);
            record.next = (record.next * 2).min(limits.backoff_max);
            record.window = (now, 0);
        }
        let due = record
            .logged
            .is_none_or(|(at, _)| now.duration_since(at) >= limits.refusal_log_every);
        if due || backoff.is_some() {
            let count = record.logged.map_or(0, |(_, count)| count) + 1;
            record.logged = Some((now, 0));
            Some(Logged { count, backoff })
        } else {
            if let Some((_, count)) = &mut record.logged {
                *count += 1;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            failures_before_backoff: 3,
            backoff: Duration::from_millis(50),
            backoff_max: Duration::from_millis(150),
            max_connections_per_address: 2,
            refusal_log_every: Duration::from_hours(1),
            ..Limits::default()
        }
    }

    #[test]
    fn keys() {
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff:2".parse().unwrap();
        let c: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        assert_eq!(Key::from(a), Key::from(b));
        assert_ne!(Key::from(a), Key::from(c));
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(Key::from(v4), Key::from(mapped));
    }

    #[test]
    fn backoff_doubles_and_logs_sparingly() {
        let failures = Failures::new(&limits());
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let first = failures.refused(ip).unwrap();
        assert_eq!(
            first,
            Logged {
                count: 1,
                backoff: None
            }
        );
        assert_eq!(failures.refused(ip), None);
        assert!(!failures.blocked(ip));
        let third = failures.refused(ip).unwrap();
        assert_eq!(third.backoff, Some(Duration::from_millis(50)));
        // The second refusal was not logged; this line counts it.
        assert_eq!(third.count, 2);
        assert!(failures.blocked(ip));
        assert!(!failures.blocked("192.0.2.2".parse().unwrap()));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!failures.blocked(ip));
        for _ in 0..2 {
            failures.refused(ip);
        }
        assert_eq!(
            failures.refused(ip).unwrap().backoff,
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn connections_per_address() {
        let failures = Arc::new(Failures::new(&limits()));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let a = failures.open(ip).unwrap();
        let _b = failures.open(ip).unwrap();
        assert!(failures.open(ip).is_none());
        assert!(failures.open("192.0.2.9".parse().unwrap()).is_some());
        drop(a);
        assert!(failures.open(ip).is_some());
    }
}
