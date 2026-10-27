//! The broker's tests (R2.22, tests 1 to 6).

use super::*;

/// An item weighing `.0` bytes.
#[derive(Debug, PartialEq, Eq)]
struct Weighs(usize);

impl Item for Weighs {
    fn weight(&self) -> usize {
        self.0
    }
}

type TestBroker = Broker<&'static str, u32, Weighs>;

fn broker(limits: Limits) -> Arc<TestBroker> {
    Broker::new(limits)
}

/// Primed at 0, then positions 1 to `n` appended, each weighing 10.
fn filled(limits: Limits, n: u32) -> Arc<TestBroker> {
    let broker = broker(limits);
    broker.start(&"t", 0);
    append(&broker, "t", 1..=n, 10);
    broker
}

fn append(
    broker: &TestBroker,
    topic: &'static str,
    positions: impl IntoIterator<Item = u32>,
    weight: usize,
) {
    broker.append(
        &topic,
        positions
            .into_iter()
            .map(|at| (Arc::new(Weighs(weight)), at))
            .collect(),
    );
}

/// The positions after each entry of a hit, and its `more`; `None` for a
/// miss, `Some((vec![], false))` never confused with one.
fn positions(lookup: Lookup<u32, Weighs>) -> Result<(Vec<u32>, bool), &'static str> {
    match lookup {
        Lookup::Hit { entries, more } => {
            Ok((entries.iter().map(|entry| entry.after).collect(), more))
        }
        Lookup::Miss => Err("miss"),
        Lookup::Unknown => Err("unknown"),
    }
}

// 1
#[test]
fn reads_after_a_position() {
    let broker = filled(Limits::default(), 5);
    assert_eq!(
        positions(broker.read(&"t", &2, None, 100)),
        Ok((vec![3, 4, 5], false))
    );
    assert_eq!(
        positions(broker.read(&"t", &2, None, 2)),
        Ok((vec![3, 4], true))
    );
    assert_eq!(
        positions(broker.read(&"t", &5, None, 100)),
        Ok((vec![], false))
    );
    assert_eq!(
        positions(broker.read(&"t", &0, None, 100)),
        Ok((vec![1, 2, 3, 4, 5], false))
    );
    assert_eq!(positions(broker.read(&"t", &9, None, 100)), Err("miss"));
    assert_eq!(positions(broker.read(&"u", &2, None, 100)), Err("unknown"));
    let stats = broker.stats();
    assert_eq!((stats.hits, stats.misses), (4, 2));
    // Followed but not primed.
    let _following = broker.follow("u");
    assert_eq!(positions(broker.read(&"u", &2, None, 100)), Err("unknown"));
    // Sequence numbers run on from the start's.
    let Lookup::Hit { entries, .. } = broker.read(&"t", &0, None, 1) else {
        panic!("a hit");
    };
    assert_eq!(entries[0].seq, 1);
    assert_eq!(broker.head(&"t"), Some((5, 5)));
}

// 2
#[test]
fn upto_bounds_a_read() {
    let broker = filled(Limits::default(), 5);
    assert_eq!(
        positions(broker.read(&"t", &2, Some(&4), 100)),
        Ok((vec![3, 4], false))
    );
    assert_eq!(
        positions(broker.read(&"t", &2, Some(&4), 1)),
        Ok((vec![3], true))
    );
    assert_eq!(positions(broker.read(&"t", &2, Some(&9), 100)), Err("miss"));
    assert_eq!(
        positions(broker.read(&"t", &2, Some(&2), 100)),
        Ok((vec![], false))
    );
    assert_eq!(
        positions(broker.read(&"t", &4, Some(&2), 100)),
        Ok((vec![], false))
    );
}

// 3
#[test]
fn eviction_keeps_the_bounds() {
    // By count: the base moves up and stays a hit.
    let limits = Limits {
        items: 3,
        ..Limits::default()
    };
    let broker = filled(limits, 5);
    assert_eq!(broker.size_of(&"t"), Some((3, 30)));
    assert_eq!(
        positions(broker.read(&"t", &2, None, 100)),
        Ok((vec![3, 4, 5], false))
    );
    assert_eq!(positions(broker.read(&"t", &1, None, 100)), Err("miss"));
    assert_eq!(positions(broker.read(&"t", &0, None, 100)), Err("miss"));
    assert_eq!(broker.stats().evicted, 2);

    // By bytes: the oldest by weight.
    let broker = broker_with_bytes(25, 1000);
    append(&broker, "t", [1, 2, 3], 10);
    assert_eq!(broker.size_of(&"t"), Some((2, 20)));
    assert_eq!(
        positions(broker.read(&"t", &1, None, 100)),
        Ok((vec![2, 3], false))
    );

    // One item over the bound stands alone until the next append.
    append(&broker, "t", [4], 100);
    assert_eq!(broker.size_of(&"t"), Some((1, 100)));
    assert_eq!(
        positions(broker.read(&"t", &3, None, 100)),
        Ok((vec![4], false))
    );
    append(&broker, "t", [5], 10);
    assert_eq!(broker.size_of(&"t"), Some((1, 10)));
    assert_eq!(
        positions(broker.read(&"t", &4, None, 100)),
        Ok((vec![5], false))
    );

    // In total: from the topic holding the most.
    let broker = broker_with_bytes(1000, 50);
    broker.start(&"a", 0);
    broker.start(&"b", 0);
    append(&broker, "a", 1..=3, 10);
    append(&broker, "b", 1..=2, 5);
    assert_eq!(broker.stats().bytes, 40);
    append(&broker, "b", [3], 20);
    // 60 > 50: whichever of the two holding 30 gives up its oldest, and
    // the next largest after that, until the total is within 50.
    let stats = broker.stats();
    assert!(stats.bytes <= 50, "{stats:?}");
    assert_eq!(
        stats.bytes,
        broker.size_of(&"a").unwrap().1 + broker.size_of(&"b").unwrap().1
    );

    // The index never holds an evicted position other than the base.
    let broker = filled(limits, 100);
    for at in 0..97 {
        assert_eq!(
            positions(broker.read(&"t", &at, None, 100)),
            Err("miss"),
            "{at}"
        );
    }
    assert_eq!(
        positions(broker.read(&"t", &97, None, 100)),
        Ok((vec![98, 99, 100], false))
    );
}

fn broker_with_bytes(bytes: usize, total_bytes: usize) -> Arc<TestBroker> {
    let broker = broker(Limits {
        bytes,
        total_bytes,
        ..Limits::default()
    });
    broker.start(&"t", 0);
    broker
}

// 4
#[test]
fn start_resets() {
    let broker = filled(Limits::default(), 5);
    broker.start(&"t", 50);
    for at in 0..=5 {
        assert_eq!(
            positions(broker.read(&"t", &at, None, 100)),
            Err("miss"),
            "{at}"
        );
    }
    assert_eq!(
        positions(broker.read(&"t", &50, None, 100)),
        Ok((vec![], false))
    );
    assert_eq!(
        broker.head(&"t"),
        Some((5, 50)),
        "sequence numbers are not reused"
    );
    append(&broker, "t", [51], 10);
    assert_eq!(broker.head(&"t"), Some((6, 51)));
    assert_eq!(broker.stats().restarts, 1);
    assert_eq!(broker.size_of(&"t"), Some((1, 10)));
    assert_eq!(broker.stats().bytes, 10);
}

// 5
#[tokio::test(start_paused = true)]
async fn followers_and_linger() {
    let broker = broker(Limits {
        linger: Duration::from_secs(10),
        ..Limits::default()
    });
    let mut wanted = broker.wanted_changes();
    let following = broker.follow("t");
    assert!(wanted.has_changed().unwrap(), "a new topic is announced");
    wanted.borrow_and_update();
    assert_eq!(broker.wanted(), vec!["t"]);
    broker.start(&"t", 0);
    append(&broker, "t", [1], 10);

    // A followed topic is never swept.
    tokio::time::advance(Duration::from_mins(1)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), vec!["t"]);

    // It lingers after its last reader, and a new one within the linger
    // keeps its ring.
    drop(following);
    tokio::time::advance(Duration::from_secs(5)).await;
    broker.sweep(Instant::now());
    let again = broker.follow("t");
    assert!(!wanted.has_changed().unwrap(), "not a new topic");
    assert_eq!(
        positions(broker.read(&"t", &0, None, 100)),
        Ok((vec![1], false))
    );
    drop(again);
    tokio::time::advance(Duration::from_secs(9)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), vec!["t"]);
    tokio::time::advance(Duration::from_secs(1)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), Vec::<&str>::new());
    assert!(
        wanted.has_changed().unwrap(),
        "a dropped topic is announced"
    );
    assert_eq!(
        broker.stats(),
        Stats {
            appended: 1,
            hits: 1,
            ..Stats::default()
        }
    );

    // A topic only started, never followed, lingers too.
    broker.start(&"u", 0);
    tokio::time::advance(Duration::from_secs(10)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), Vec::<&str>::new());
}

// 6
#[test]
fn changes() {
    let broker = broker(Limits::default());
    let mut all = broker.changes();
    let _following = broker.follow("t");
    let mut one = broker.changes_of(&"t").unwrap();
    assert!(broker.version(&"t").unwrap() > 0, "0 is never a version");
    broker.start(&"t", 0);
    assert!(all.has_changed().unwrap());
    assert!(one.has_changed().unwrap());
    all.borrow_and_update();
    one.borrow_and_update();
    let version = broker.version(&"t").unwrap();

    append(&broker, "t", [1], 10);
    assert!(all.has_changed().unwrap());
    assert!(one.has_changed().unwrap());
    assert!(broker.version(&"t").unwrap() > version);
    all.borrow_and_update();
    one.borrow_and_update();

    // Nothing to append is no change.
    broker.append(&"t", Vec::new());
    assert!(!all.has_changed().unwrap());

    broker.notify();
    assert!(all.has_changed().unwrap());
    assert!(
        !one.has_changed().unwrap(),
        "notify wakes only the daemon-wide receiver"
    );
    all.borrow_and_update();

    // Another topic's append is not this topic's change.
    broker.start(&"u", 0);
    append(&broker, "u", [1], 10);
    assert!(!one.has_changed().unwrap());

    broker.remove(&"t");
    assert!(all.has_changed().unwrap());
    assert_eq!(broker.version(&"t"), None);
    assert!(broker.changes_of(&"t").is_none());
}

// R2.22 review: a topic dropped and created again never repeats a
// version a reader holds, and a follower of the old ring does not count
// against the new one.
#[tokio::test(start_paused = true)]
async fn incarnations_are_told_apart() {
    let broker = broker(Limits {
        linger: Duration::from_secs(1),
        ..Limits::default()
    });
    let old = broker.follow("t");
    broker.start(&"t", 0);
    append(&broker, "t", 1..=5, 10);
    let seen = broker.version(&"t").unwrap();
    broker.remove(&"t");

    let new = broker.follow("t");
    let created = broker.version(&"t").unwrap();
    assert!(created > seen, "{created} after {seen}");
    broker.start(&"t", 5);
    append(&broker, "t", 6..=10, 10);
    assert!(broker.version(&"t").unwrap() > seen);

    // The old follower leaving does not leave the new ring unfollowed.
    drop(old);
    tokio::time::advance(Duration::from_secs(2)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), vec!["t"]);
    drop(new);
    tokio::time::advance(Duration::from_secs(2)).await;
    broker.sweep(Instant::now());
    assert_eq!(broker.wanted(), Vec::<&str>::new());
}

// R2.22 review: a producer that cannot read unprimes its ring; readers
// are sent to the log and woken, and sequence numbers run on after it.
#[test]
fn unprime_sends_readers_to_the_log() {
    let broker = filled(Limits::default(), 5);
    let mut all = broker.changes();
    all.borrow_and_update();
    let version = broker.version(&"t").unwrap();
    broker.unprime(&"t");
    assert!(all.has_changed().unwrap());
    assert!(broker.version(&"t").unwrap() > version);
    assert_eq!(positions(broker.read(&"t", &5, None, 100)), Err("unknown"));
    assert_eq!(broker.head(&"t"), None);
    assert_eq!(broker.stats().bytes, 0);
    assert_eq!(broker.stats().restarts, 1);
    broker.start(&"t", 9);
    append(&broker, "t", [10], 10);
    assert_eq!(
        broker.head(&"t"),
        Some((6, 10)),
        "sequence numbers are not reused"
    );
    // Unpriming an unprimed or missing topic does nothing.
    broker.unprime(&"u");
    let _following = broker.follow("u");
    broker.unprime(&"u");
    assert_eq!(broker.stats().restarts, 1);
}
