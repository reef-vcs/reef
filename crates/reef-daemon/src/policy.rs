//! Repository policy (R3.07, ADR 0061): compiled policies, cached by the
//! bytes they were loaded from. `PolicyService` (`crate::service::policy`)
//! builds the input and evaluates.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use reef_policy::{Diagnostic, Policy};

/// How many compiled policies a daemon keeps.
pub const CACHE_ENTRIES: usize = 16;

/// A policy file, loaded or not.
#[derive(Debug)]
pub enum Loaded {
    /// It loads.
    Policy(Box<Policy>),
    /// It does not, for these reasons.
    Invalid(Vec<Diagnostic>),
}

/// How many evaluations run at once; more wait their turn, so callers
/// sending costly policy text cannot hold every blocking thread (review
/// finding 3).
pub const CONCURRENT_EVALUATIONS: usize = 4;

/// Compiled policies by their bytes' SHA-256, at most [`CACHE_ENTRIES`],
/// the least recently used dropped first, and the evaluations' turns.
#[derive(Debug)]
pub struct Policies {
    cache: Mutex<VecDeque<(String, Arc<Loaded>)>>,
    compiles: AtomicU64,
    /// One permit per evaluation running.
    pub turns: tokio::sync::Semaphore,
}

impl Default for Policies {
    fn default() -> Policies {
        Policies {
            cache: Mutex::default(),
            compiles: AtomicU64::default(),
            turns: tokio::sync::Semaphore::new(CONCURRENT_EVALUATIONS),
        }
    }
}

impl Policies {
    /// `bytes` loaded, from the cache when the same bytes were loaded
    /// before.
    pub fn load(&self, bytes: &[u8]) -> Arc<Loaded> {
        let key = reef_policy::sha256_hex(bytes);
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(at) = cache.iter().position(|(k, _)| *k == key) {
                let entry = cache.remove(at).unwrap_or_else(|| unreachable!());
                let loaded = Arc::clone(&entry.1);
                cache.push_front(entry);
                return loaded;
            }
        }
        self.compiles.fetch_add(1, Ordering::Relaxed);
        let loaded = Arc::new(match Policy::load(bytes) {
            Ok(policy) => Loaded::Policy(Box::new(policy)),
            Err(diagnostics) => Loaded::Invalid(diagnostics),
        });
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|(k, _)| *k != key);
        cache.push_front((key, Arc::clone(&loaded)));
        cache.truncate(CACHE_ENTRIES);
        loaded
    }

    /// How many policies this daemon has compiled: what a cache hit does
    /// not add to.
    pub fn compiles(&self) -> u64 {
        self.compiles.load(Ordering::Relaxed)
    }
}
