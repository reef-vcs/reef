//! The evidence format's cost (R3.01): building and signing a claim with
//! three artifacts, checking that envelope, and checking one of about
//! 64 KiB (large `annotations`). Results are in `docs/benchmarks.md`,
//! "Evidence (R3.01)".
//!
//! ```sh
//! cargo bench -p reef-evidence --bench evidence
//! ```

#![allow(missing_docs, reason = "a criterion harness")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a benchmark: a panic is the failure report"
)]

use std::hint::black_box;
use std::time::{Duration, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use reef_evidence::{
    Artifact, Claim, MAX_ENVELOPE_BYTES, Outcome, SigningKey, TreeDigest, Verified,
};
use serde_json::{Map, json};

fn claim(annotation_bytes: usize) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    let mut annotations = Map::new();
    annotations.insert("runner".into(), json!("ci-7"));
    if annotation_bytes > 0 {
        annotations.insert("padding".into(), json!("x".repeat(annotation_bytes)));
    }
    Claim {
        kind: "test".into(),
        result: Outcome::Pass,
        target: Some("crates/reef-lib:jj_writes".into()),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: ["test.log", "junit.xml", "coverage.json"]
            .into_iter()
            .map(|name| Artifact {
                name: name.into(),
                sha256: reef_evidence::sha256_hex(name.as_bytes()),
                uri: None,
                media_type: Some("text/plain".into()),
            })
            .collect(),
        annotations,
    }
}

fn evidence(c: &mut Criterion) {
    let key = SigningKey::from_seed(&[1; 32]).unwrap();
    let tree = TreeDigest::git("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let spki = key.public_key().spki().to_vec();
    let small = claim(0);
    let mut group = c.benchmark_group("evidence");
    group.bench_function("sign", |b| {
        b.iter(|| black_box(small.sign(&tree, &key).unwrap().to_bytes()));
    });
    let envelope = small.sign(&tree, &key).unwrap().to_bytes();
    group.bench_function("check", |b| {
        b.iter(|| black_box(Verified::check(&envelope, &spki).unwrap()));
    });
    // Base64 grows the payload by a third: about 47 KiB of annotations
    // makes an envelope just under the bound.
    let large = claim(MAX_ENVELOPE_BYTES * 3 / 4 - 1_500)
        .sign(&tree, &key)
        .unwrap()
        .to_bytes();
    assert!(large.len() <= MAX_ENVELOPE_BYTES && large.len() > 60 * 1024);
    group.bench_function("check_64k", |b| {
        b.iter(|| black_box(Verified::check(&large, &spki).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, evidence);
criterion_main!(benches);
