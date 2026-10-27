//! What the broker costs per operation (R2.22): an append to a full ring,
//! one read, and the in-memory fan-out of one operation to 500 readers.
//! The end-to-end figures are the subscription load test's
//! (`docs/load-tests.md`); these are in `docs/benchmarks.md`, "Broker
//! (R2.22)".
//!
//! ```sh
//! cargo bench -p reef-broker --bench broker
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    reason = "a benchmark: a panic is the failure report"
)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use reef_broker::{Broker, Item, Limits, Lookup};

/// An operation as the daemon's ring holds it, roughly: an encoded message
/// of about 1.5 KiB and a few fields a filter reads.
#[derive(Clone)]
struct Operation {
    kind: u8,
    workspace: u64,
    message: Vec<u8>,
}

impl Item for Operation {
    fn weight(&self) -> usize {
        2 * self.message.len() + 256
    }
}

fn operation(n: u64) -> Arc<Operation> {
    Arc::new(Operation {
        kind: (n % 4) as u8,
        workspace: n % 20,
        message: vec![b'x'; 1536],
    })
}

/// A ring at its default bound of 4,096 operations, positions 0 to 4,096.
fn full() -> (Arc<Broker<u64, u64, Operation>>, u64) {
    let broker = Broker::new(Limits::default());
    broker.start(&0, 0);
    let n = Limits::default().items as u64;
    broker.append(&0, (1..=n).map(|at| (operation(at), at)).collect());
    (broker, n)
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("broker");

    // An append past the bound: an eviction and two index updates.
    let (broker, mut head) = full();
    group.bench_function("append_1", |b| {
        b.iter(|| {
            head += 1;
            broker.append(&0, vec![(operation(head), head)]);
        });
    });

    // One entry after a position one behind the head.
    let (broker, head) = full();
    group.bench_function("read_1", |b| {
        b.iter(
            || match broker.read(&0, black_box(&(head - 1)), None, 1_000) {
                Lookup::Hit { entries, .. } => black_box(entries),
                _ => unreachable!(),
            },
        );
    });

    // 500 readers take the newest operation, filter it as the daemon's
    // classes do (8 in 10 keep everything, 1 a workspace, 1 a kind), and
    // clone what they keep: the in-memory cost per operation at the phase
    // target.
    let (broker, head) = full();
    group.bench_function("fanout_500", |b| {
        b.iter(|| {
            let mut sent = 0;
            for reader in 0..500_u64 {
                let Lookup::Hit { entries, .. } = broker.read(&0, &(head - 1), None, 1_000) else {
                    unreachable!();
                };
                for entry in entries {
                    let keep = match reader % 10 {
                        8 => entry.item.workspace == reader % 20,
                        9 => entry.item.kind == 1,
                        _ => true,
                    };
                    if keep {
                        sent += black_box(entry.item.message.clone()).len();
                    }
                }
            }
            sent
        });
    });
    group.finish();
}

criterion_group!(broker, benches);
criterion_main!(broker);
