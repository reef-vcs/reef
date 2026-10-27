//! A broker between a log and the streams that follow it (R2.22, ADR
//! 0038): bounded in-memory rings, one per topic, that a producer appends
//! to once and any number of readers read from.
//!
//! - A **topic** `K` is what readers follow: a repository in the daemon.
//! - A **position** `P` sits between two items, as a reader's place does:
//!   the daemon's are operation-log frontiers. Each item is appended with
//!   the position after it, and the ring indexes them, so a reader asks for
//!   "what follows my position" without the broker knowing what a position
//!   means.
//! - An **item** `I` is what readers send, converted once: the daemon's
//!   are operations with their effects and messages.
//!
//! The producer primes a topic with [`Broker::start`], then [`Broker::append`]s
//! what it reads from the log. A reader [`Broker::follow`]s a topic, waits on
//! [`Broker::changes`], and [`Broker::read`]s after its position: a
//! [`Lookup::Hit`] is served from memory; a [`Lookup::Miss`] (a position
//! the ring no longer holds or never did) or [`Lookup::Unknown`] (a topic
//! not primed) sends the reader back to the log, which the broker never
//! reads itself.
//!
//! Rings are evicted by their [`Limits`] only, never held for a slow
//! reader, so a stalled reader pins no memory and holds back no one: it
//! catches up from the log. A topic nobody follows is dropped after
//! [`Limits::linger`].
//!
//! The daemon's adapter (`reef-daemon`'s `subscribe::broker`) is the worked
//! example of a producer and a reader; the phase-four store brings its own
//! over the same types.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// What a ring holds.
pub trait Item: Send + Sync + 'static {
    /// Roughly the bytes this item costs while held, for the byte bounds.
    fn weight(&self) -> usize;
}

/// A broker's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Items one topic's ring holds at most.
    pub items: usize,
    /// Bytes ([`Item::weight`]) one topic's ring holds at most. An item
    /// heavier than this stands alone in its ring until the next append.
    pub bytes: usize,
    /// Bytes every ring together holds at most; past it, the ring holding
    /// the most gives up its oldest item.
    pub total_bytes: usize,
    /// How long a topic nobody follows is kept before [`Broker::sweep`]
    /// drops it.
    pub linger: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            items: 4_096,
            bytes: 8 << 20,
            total_bytes: 64 << 20,
            linger: Duration::from_mins(1),
        }
    }
}

/// One appended item and the position after it.
#[derive(Debug)]
pub struct Entry<P, I> {
    /// Its sequence number in its topic: one more than the previous
    /// entry's, never reused, not even across [`Broker::start`].
    pub seq: u64,
    /// The item.
    pub item: Arc<I>,
    /// The position after it.
    pub after: P,
}

/// What [`Broker::read`] found.
#[derive(Debug)]
pub enum Lookup<P, I> {
    /// `entries` follow the position, in order.
    Hit {
        /// The entries, at most the read's `max`.
        entries: Vec<Arc<Entry<P, I>>>,
        /// `max` cut them before `upto` or the head.
        more: bool,
    },
    /// The position, or `upto`, is not one the ring holds: read the log.
    Miss,
    /// No such topic, or it is not primed: read the log.
    Unknown,
}

/// A broker's counters and sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Topics, primed or not.
    pub topics: usize,
    /// Items held in every ring.
    pub items: usize,
    /// Bytes held in every ring.
    pub bytes: usize,
    /// Items appended since the broker was made.
    pub appended: u64,
    /// Items evicted by the bounds.
    pub evicted: u64,
    /// Reads served from a ring.
    pub hits: u64,
    /// Reads sent back to the log ([`Lookup::Miss`] or [`Lookup::Unknown`]).
    pub misses: u64,
    /// [`Broker::start`]s of a topic already primed (rewrites and
    /// restarts) and [`Broker::unprime`]s.
    pub restarts: u64,
}

/// One topic's ring.
struct Ring<P, I> {
    entries: VecDeque<Arc<Entry<P, I>>>,
    /// Every position the ring holds, the base included, to the sequence
    /// number of the entry it follows (the base's is one before the first
    /// entry's).
    index: HashMap<P, u64>,
    /// The position before the first entry, and its sequence number;
    /// `None` until primed.
    base: Option<(P, u64)>,
    bytes: usize,
    followers: usize,
    /// When the last follower left.
    left_at: Option<Instant>,
    /// From the broker's one counter, so a topic dropped and created again
    /// never repeats a version a reader holds.
    version: u64,
    /// Which incarnation of the topic this ring is, for its followers.
    generation: u64,
    /// The head's sequence number when last cleared, so sequence numbers
    /// run on across an unprimed spell.
    cleared_at: u64,
    changes: watch::Sender<u64>,
}

impl<P: Clone + Eq + Hash, I: Item> Ring<P, I> {
    fn new(version: u64, generation: u64) -> Ring<P, I> {
        Ring {
            entries: VecDeque::new(),
            index: HashMap::new(),
            base: None,
            bytes: 0,
            followers: 0,
            left_at: None,
            version,
            generation,
            cleared_at: 0,
            changes: watch::Sender::new(version),
        }
    }

    /// The newest sequence number and position, if primed.
    fn head(&self) -> Option<(u64, &P)> {
        match self.entries.back() {
            Some(entry) => Some((entry.seq, &entry.after)),
            None => self.base.as_ref().map(|(position, seq)| (*seq, position)),
        }
    }

    /// Takes the broker's next version.
    fn changed(&mut self, versions: &mut u64) {
        *versions += 1;
        self.version = *versions;
        self.changes.send_replace(self.version);
    }

    /// Empties the ring, unprimed: every read is [`Lookup::Unknown`] until
    /// the next [`Broker::start`]. Returns the bytes it held.
    fn clear(&mut self) -> usize {
        if let Some((seq, _)) = self.head() {
            self.cleared_at = seq;
        }
        self.entries.clear();
        self.index.clear();
        self.base = None;
        std::mem::take(&mut self.bytes)
    }

    /// Drops the oldest entry: its position becomes the base.
    fn evict_oldest(&mut self) -> bool {
        let Some(oldest) = self.entries.pop_front() else {
            return false;
        };
        if let Some((base, _)) = self.base.take() {
            self.index.remove(&base);
        }
        self.bytes -= oldest.item.weight();
        self.base = Some((oldest.after.clone(), oldest.seq));
        true
    }
}

struct State<K, P, I> {
    topics: HashMap<K, Ring<P, I>>,
    bytes: usize,
    stats: Stats,
    /// The last version given to any ring; 0 is never one.
    versions: u64,
    /// The last generation given to a ring.
    generations: u64,
}

/// `topic`'s ring in `topics`, created unprimed with the next version and
/// generation if it does not exist, and whether it was.
fn ring_of<'a, K: Clone + Eq + Hash, P: Clone + Eq + Hash, I: Item>(
    topics: &'a mut HashMap<K, Ring<P, I>>,
    versions: &mut u64,
    generations: &mut u64,
    topic: &K,
) -> (&'a mut Ring<P, I>, bool) {
    let mut created = false;
    let ring = topics.entry(topic.clone()).or_insert_with(|| {
        created = true;
        *versions += 1;
        *generations += 1;
        Ring::new(*versions, *generations)
    });
    (ring, created)
}

/// Rings of items per topic `K`, between positions `P`; see the crate
/// documentation.
pub struct Broker<K, P, I> {
    state: Mutex<State<K, P, I>>,
    limits: Limits,
    /// Bumped after every append, start and removal, and by `notify`.
    changes: watch::Sender<u64>,
    /// Bumped when a topic is created or dropped.
    wanted: watch::Sender<u64>,
}

impl<K, P, I> fmt::Debug for Broker<K, P, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Broker")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<K, P, I> Broker<K, P, I>
where
    K: Clone + Eq + Hash + Send + 'static,
    P: Clone + Eq + Hash + Send + 'static,
    I: Item,
{
    /// An empty broker.
    pub fn new(limits: Limits) -> Arc<Broker<K, P, I>> {
        Arc::new(Broker {
            state: Mutex::new(State {
                topics: HashMap::new(),
                bytes: 0,
                stats: Stats::default(),
                versions: 0,
                generations: 0,
            }),
            limits,
            changes: watch::Sender::new(0),
            wanted: watch::Sender::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State<K, P, I>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changes.send_modify(|version| *version += 1);
    }

    /// The bounds.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// A receiver that sees every append, start and removal in any topic,
    /// and every [`Broker::notify`].
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// A receiver that sees one topic's appends and starts only, if it
    /// exists: for a reader that follows few of many topics.
    pub fn changes_of(&self, topic: &K) -> Option<watch::Receiver<u64>> {
        self.lock()
            .topics
            .get(topic)
            .map(|ring| ring.changes.subscribe())
    }

    /// A receiver that sees topics created and dropped: the producer's cue
    /// to start or stop reading one.
    pub fn wanted_changes(&self) -> watch::Receiver<u64> {
        self.wanted.subscribe()
    }

    /// Wakes every [`Broker::changes`] receiver with no data: the producer
    /// saw something readers must check for themselves.
    pub fn notify(&self) {
        self.bump();
    }

    /// Counts a reader of `topic` until the guard drops, creating the topic
    /// unprimed if it does not exist.
    pub fn follow(self: &Arc<Self>, topic: K) -> Following<K, P, I> {
        let (created, generation) = {
            let mut state = self.lock();
            let state = &mut *state;
            let (ring, created) = ring_of(
                &mut state.topics,
                &mut state.versions,
                &mut state.generations,
                &topic,
            );
            ring.followers += 1;
            ring.left_at = None;
            (created, ring.generation)
        };
        if created {
            self.wanted.send_modify(|version| *version += 1);
        }
        Following {
            broker: Arc::clone(self),
            topic,
            generation,
        }
    }

    /// One follower of `generation` of `topic` left; a ring that replaced
    /// it since is not this follower's.
    fn unfollow(&self, topic: &K, generation: u64) {
        let mut state = self.lock();
        if let Some(ring) = state.topics.get_mut(topic)
            && ring.generation == generation
        {
            ring.followers = ring.followers.saturating_sub(1);
            if ring.followers == 0 {
                ring.left_at = Some(Instant::now());
            }
        }
    }

    /// The topics: followed, or lingering after their last reader left.
    pub fn wanted(&self) -> Vec<K> {
        self.lock().topics.keys().cloned().collect()
    }

    /// (Re)starts `topic` empty at `at`, creating it if needed: its first
    /// position, or where the producer goes on after a rewrite of the log
    /// or when it cannot continue from the head. Every earlier position
    /// becomes a [`Lookup::Miss`].
    pub fn start(&self, topic: &K, at: P) {
        {
            let mut state = self.lock();
            let state = &mut *state;
            let (ring, created) = ring_of(
                &mut state.topics,
                &mut state.versions,
                &mut state.generations,
                topic,
            );
            let restarted = ring.base.is_some();
            let seq = ring.head().map_or(ring.cleared_at, |(seq, _)| seq);
            let freed = ring.clear();
            ring.index.insert(at.clone(), seq);
            ring.base = Some((at, seq));
            ring.changed(&mut state.versions);
            state.bytes -= freed;
            if restarted {
                state.stats.restarts += 1;
            }
            if created {
                ring.left_at = Some(Instant::now());
                self.wanted.send_modify(|version| *version += 1);
            }
        }
        self.bump();
    }

    /// Empties `topic`'s ring and leaves it unprimed, keeping its
    /// followers: every read is [`Lookup::Unknown`], so readers go to the
    /// log, until the producer [`Broker::start`]s it again. For a producer
    /// that cannot read: its readers then meet the log's own error rather
    /// than wait on a ring that no longer moves.
    pub fn unprime(&self, topic: &K) {
        {
            let mut state = self.lock();
            let state = &mut *state;
            let Some(ring) = state.topics.get_mut(topic) else {
                return;
            };
            if ring.base.is_none() {
                return;
            }
            let freed = ring.clear();
            ring.changed(&mut state.versions);
            state.bytes -= freed;
            state.stats.restarts += 1;
        }
        self.bump();
    }

    /// `topic`'s newest sequence number and position, if it is primed: where
    /// its producer reads on from.
    pub fn head(&self, topic: &K) -> Option<(u64, P)> {
        self.lock()
            .topics
            .get(topic)
            .and_then(|ring| ring.head().map(|(seq, at)| (seq, at.clone())))
    }

    /// A number that changes on every append to and start of `topic`, if
    /// it exists: what a reader compares to know whether to read again.
    pub fn version(&self, topic: &K) -> Option<u64> {
        self.lock().topics.get(topic).map(|ring| ring.version)
    }

    /// Appends `items` to `topic`, each with the position after it, then
    /// evicts to the bounds. Does nothing to a topic that does not exist or
    /// is not primed.
    pub fn append(&self, topic: &K, items: Vec<(Arc<I>, P)>) {
        if items.is_empty() {
            return;
        }
        {
            let mut state = self.lock();
            let state = &mut *state;
            let Some(ring) = state.topics.get_mut(topic) else {
                return;
            };
            let Some((mut seq, _)) = ring.head() else {
                return;
            };
            for (item, after) in items {
                seq += 1;
                let weight = item.weight();
                ring.index.insert(after.clone(), seq);
                ring.entries.push_back(Arc::new(Entry { seq, item, after }));
                ring.bytes += weight;
                state.bytes += weight;
                state.stats.appended += 1;
            }
            while ring.entries.len() > self.limits.items
                || (ring.bytes > self.limits.bytes && ring.entries.len() > 1)
            {
                let before = ring.bytes;
                ring.evict_oldest();
                state.bytes -= before - ring.bytes;
                state.stats.evicted += 1;
            }
            ring.changed(&mut state.versions);
            while state.bytes > self.limits.total_bytes {
                let Some(largest) = state
                    .topics
                    .values_mut()
                    .filter(|ring| ring.entries.len() > 1)
                    .max_by_key(|ring| ring.bytes)
                else {
                    break;
                };
                let before = largest.bytes;
                largest.evict_oldest();
                state.bytes -= before - largest.bytes;
                state.stats.evicted += 1;
            }
        }
        self.bump();
    }

    /// Drops `topic` and its ring.
    pub fn remove(&self, topic: &K) {
        let removed = {
            let mut state = self.lock();
            let removed = state.topics.remove(topic);
            if let Some(ring) = &removed {
                state.bytes -= ring.bytes;
            }
            removed.is_some()
        };
        if removed {
            self.wanted.send_modify(|version| *version += 1);
            self.bump();
        }
    }

    /// The entries after `after` in `topic`, up to `upto` if given or else
    /// the head, at most `max`. Copies their `Arc`s under the lock; the
    /// caller filters and converts outside it.
    pub fn read(&self, topic: &K, after: &P, upto: Option<&P>, max: usize) -> Lookup<P, I> {
        let mut state = self.lock();
        let state = &mut *state;
        let lookup = match state.topics.get(topic) {
            Some(ring) if ring.base.is_some() => Self::lookup(ring, after, upto, max),
            _ => Lookup::Unknown,
        };
        match lookup {
            Lookup::Hit { .. } => state.stats.hits += 1,
            Lookup::Miss | Lookup::Unknown => state.stats.misses += 1,
        }
        lookup
    }

    fn lookup(ring: &Ring<P, I>, after: &P, upto: Option<&P>, max: usize) -> Lookup<P, I> {
        let (Some(from), Some((_, base_seq))) = (ring.index.get(after), &ring.base) else {
            return Lookup::Miss;
        };
        let head = ring.head().map_or(*base_seq, |(seq, _)| seq);
        let to = match upto {
            Some(upto) => match ring.index.get(upto) {
                Some(to) => *to,
                None => return Lookup::Miss,
            },
            None => head,
        };
        if to <= *from {
            return Lookup::Hit {
                entries: Vec::new(),
                more: false,
            };
        }
        let skip = usize::try_from(from - base_seq).unwrap_or(usize::MAX);
        let wanted = usize::try_from(to - from).unwrap_or(usize::MAX);
        let entries: Vec<Arc<Entry<P, I>>> = ring
            .entries
            .iter()
            .skip(skip)
            .take(wanted.min(max))
            .cloned()
            .collect();
        Lookup::Hit {
            more: wanted > entries.len(),
            entries,
        }
    }

    /// Drops topics nobody has followed for [`Limits::linger`] by `now`.
    pub fn sweep(&self, now: Instant) {
        let swept = {
            let mut state = self.lock();
            let state = &mut *state;
            let linger = self.limits.linger;
            let lapsed: Vec<K> = state
                .topics
                .iter()
                .filter(|(_, ring)| {
                    ring.followers == 0
                        && ring
                            .left_at
                            .is_some_and(|left| now.saturating_duration_since(left) >= linger)
                })
                .map(|(topic, _)| topic.clone())
                .collect();
            for topic in &lapsed {
                if let Some(ring) = state.topics.remove(topic) {
                    state.bytes -= ring.bytes;
                }
            }
            !lapsed.is_empty()
        };
        if swept {
            self.wanted.send_modify(|version| *version += 1);
        }
    }

    /// The counters and sizes now.
    pub fn stats(&self) -> Stats {
        let state = self.lock();
        Stats {
            topics: state.topics.len(),
            items: state.topics.values().map(|ring| ring.entries.len()).sum(),
            bytes: state.bytes,
            ..state.stats
        }
    }

    /// `topic`'s items and bytes, if it exists.
    pub fn size_of(&self, topic: &K) -> Option<(usize, usize)> {
        self.lock()
            .topics
            .get(topic)
            .map(|ring| (ring.entries.len(), ring.bytes))
    }
}

/// A reader following a topic; see [`Broker::follow`].
pub struct Following<K, P, I>
where
    K: Clone + Eq + Hash + Send + 'static,
    P: Clone + Eq + Hash + Send + 'static,
    I: Item,
{
    broker: Arc<Broker<K, P, I>>,
    topic: K,
    generation: u64,
}

impl<K, P, I> fmt::Debug for Following<K, P, I>
where
    K: Clone + Eq + Hash + Send + 'static,
    P: Clone + Eq + Hash + Send + 'static,
    I: Item,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Following").finish_non_exhaustive()
    }
}

impl<K, P, I> Drop for Following<K, P, I>
where
    K: Clone + Eq + Hash + Send + 'static,
    P: Clone + Eq + Hash + Send + 'static,
    I: Item,
{
    fn drop(&mut self) {
        self.broker.unfollow(&self.topic, self.generation);
    }
}

#[cfg(test)]
mod tests;
