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

/// A simulation page's cost bound (R3.08, S-98): twice one evaluation's,
/// so a page's evaluations cost at most twice this.
pub const MAX_PAGE_COST: u64 = 2 * reef_policy::cost::MAX_EVALUATION_COST;

/// A simulation page's bound on the paths its landings touch: reading
/// diffs, not evaluating, is what large commits cost.
pub const MAX_PAGE_PATHS: u64 = 200_000;

/// A simulation page's bound on the evidence and provenance rows its
/// landings read, each checked again at the read.
pub const MAX_PAGE_ROWS: u64 = 50_000;

/// How many simulation pages a daemon reads at once; more wait their turn.
pub const CONCURRENT_SIMULATIONS: usize = 2;

/// Compiled policies by their bytes' SHA-256, at most [`CACHE_ENTRIES`],
/// the least recently used dropped first, and the evaluations' turns.
#[derive(Debug)]
pub struct Policies {
    cache: Mutex<VecDeque<(String, Arc<Loaded>)>>,
    compiles: AtomicU64,
    /// One permit per evaluation running. The permit is moved into the
    /// blocking task that evaluates, so a caller that goes away does not
    /// free it while the evaluation still runs.
    pub turns: Arc<tokio::sync::Semaphore>,
    /// One permit per simulation page being read (R3.08, S-98), held for
    /// the whole page; each of its evaluations takes a turn as well.
    pub simulations: Arc<tokio::sync::Semaphore>,
    /// A simulation page ends after the landing whose evaluations take its
    /// summed cost past this (R3.08, S-98). Only tests change it.
    #[doc(hidden)]
    pub page_cost: AtomicU64,
    /// A simulation page ends after the landing that takes the paths its
    /// landings touch past this. Only tests change it.
    #[doc(hidden)]
    pub page_paths: AtomicU64,
    /// A simulation page ends after the landing that takes the evidence
    /// and provenance rows its landings read past this. Only tests change
    /// it.
    #[doc(hidden)]
    pub page_rows: AtomicU64,
}

impl Default for Policies {
    fn default() -> Policies {
        Policies {
            cache: Mutex::default(),
            compiles: AtomicU64::default(),
            turns: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_EVALUATIONS)),
            simulations: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_SIMULATIONS)),
            page_cost: AtomicU64::new(MAX_PAGE_COST),
            page_paths: AtomicU64::new(MAX_PAGE_PATHS),
            page_rows: AtomicU64::new(MAX_PAGE_ROWS),
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
