//! Enforces the workspace's crate dependency direction (ADR 0033, which
//! supersedes 0007): `reef-daemon` → `reef-lib` → `reef-error`, and the
//! `reef` binary (`reef-vcs`) → `reef-client` → `reef-api` and
//! `reef-error`, with nothing that links jj-lib anywhere in `reef`'s
//! dependency graph (R2.05). `reef-testing` stands apart so any crate can
//! dev-depend on it, `reef-api`, the API schema, depends on no Reef crate
//! (ADR 0029), and `reef-broker`, the subscription broker the store reuses,
//! depends on none either and links neither jj-lib nor gRPC (R2.22, ADR
//! 0038).

#![allow(
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const MEMBERS: [&str; 9] = [
    "reef-api",
    "reef-broker",
    "reef-client",
    "reef-daemon",
    "reef-error",
    "reef-lib",
    "reef-symbols",
    "reef-testing",
    "reef-vcs",
];

/// What the `reef` binary and the thin client must never link: the daemon,
/// the jj layer, and what only they need (R2.05).
const NOT_IN_THE_CLIENT: [&str; 9] = [
    "jj-lib",
    "reef-lib",
    "reef-daemon",
    "gix",
    "rusqlite",
    "notify",
    "watchman_client",
    "reef-symbols",
    "tree-sitter",
];

fn metadata() -> Value {
    run_metadata(&["--no-deps"])
}

/// `cargo metadata` with the resolve graph, every feature of every member
/// on, so no feature can pull the daemon in unseen.
fn resolved_metadata() -> Value {
    run_metadata(&["--all-features"])
}

fn run_metadata(extra: &[&str]) -> Value {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .args(extra)
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("failed to run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata output is not JSON")
}

fn package<'a>(metadata: &'a Value, name: &str) -> &'a Value {
    metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("package {name} not found"))
}

/// Dependencies of `name` as `(dependency name, kind)`, where kind is
/// `"normal"`, `"dev"`, or `"build"`.
fn dependencies(metadata: &Value, name: &str) -> Vec<(String, String)> {
    package(metadata, name)["dependencies"]
        .as_array()
        .expect("dependencies is an array")
        .iter()
        .map(|d| {
            let dep = d["name"].as_str().expect("dependency name").to_owned();
            let kind = d["kind"].as_str().unwrap_or("normal").to_owned();
            (dep, kind)
        })
        .collect()
}

fn assert_no_dependency(metadata: &Value, from: &str, to: &str) {
    let found: Vec<_> = dependencies(metadata, from)
        .into_iter()
        .filter(|(dep, _)| dep == to)
        .collect();
    assert!(
        found.is_empty(),
        "{from} must not depend on {to}, found {found:?}"
    );
}

/// What the broker must never link (R2.22): it is the store's as much as
/// the daemon's, so nothing of jj, gRPC or the daemon's database.
const NOT_IN_THE_BROKER: [&str; 6] = ["jj-lib", "tonic", "prost", "rusqlite", "gix", "hyper"];

/// What the symbol extractor must not link (R2.14, ADR 0042): it takes bytes
/// and returns symbols, for the daemon, the store and structural merge alike.
const NOT_IN_SYMBOLS: [&str; 6] = ["jj-lib", "tonic", "rusqlite", "gix", "hyper", "tokio"];

#[test]
fn workspace_members_are_the_nine_crates() {
    let metadata = metadata();
    let mut members: Vec<&str> = metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .map(|p| p["name"].as_str().expect("package name"))
        .collect();
    members.sort_unstable();
    assert_eq!(members, MEMBERS);
}

/// The Reef crates `name` depends on with `kind` (`normal` or `build`).
fn reef_dependencies(metadata: &Value, name: &str, kinds: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = dependencies(metadata, name)
        .into_iter()
        .filter(|(dep, kind)| dep.starts_with("reef-") && kinds.contains(&kind.as_str()))
        .map(|(dep, _)| dep)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// R2.05 test 1: the edges between Reef crates that keep the client thin.
#[test]
fn crate_edges() {
    let metadata = metadata();
    let shipped = ["normal", "build"];
    assert_eq!(
        reef_dependencies(&metadata, "reef-error", &shipped),
        Vec::<String>::new()
    );
    assert_eq!(
        reef_dependencies(&metadata, "reef-client", &shipped),
        ["reef-api", "reef-error"]
    );
    assert_eq!(
        reef_dependencies(&metadata, "reef-lib", &shipped),
        ["reef-error"]
    );
    assert_eq!(
        reef_dependencies(&metadata, "reef-vcs", &shipped),
        ["reef-api", "reef-client", "reef-error"]
    );
    assert_eq!(
        reef_dependencies(&metadata, "reef-broker", &shipped),
        Vec::<String>::new()
    );
    assert!(
        reef_dependencies(&metadata, "reef-daemon", &shipped).contains(&"reef-broker".to_owned()),
        "the daemon serves subscriptions through the broker"
    );
    assert_eq!(
        reef_dependencies(&metadata, "reef-symbols", &shipped),
        Vec::<String>::new()
    );
    assert!(
        reef_dependencies(&metadata, "reef-daemon", &shipped).contains(&"reef-symbols".to_owned()),
        "the daemon extracts symbols"
    );
}

/// R2.05 test 2: nothing the daemon or the jj layer links is reachable from
/// the `reef` binary or the thin client over normal and build edges, on any
/// target. Dev-dependencies (tests) are free to use them.
#[test]
fn reef_binary_has_no_jj_lib() {
    let metadata = resolved_metadata();
    for root in ["reef-vcs", "reef-client"] {
        let reached = assert_unreachable(&metadata, root, &NOT_IN_THE_CLIENT, false);
        assert!(reached > 20, "{root}: only {reached} packages reached");
    }
}

/// R2.22 test 18: the broker links nothing of jj, gRPC or the daemon's
/// database, and no Reef crate, over normal and build edges.
#[test]
fn reef_symbols_links_no_jj_grpc_or_sqlite() {
    let metadata = resolved_metadata();
    let reached = assert_unreachable(&metadata, "reef-symbols", &NOT_IN_SYMBOLS, true);
    assert!(reached > 5, "reef-symbols: only {reached} packages reached");
}

#[test]
fn reef_broker_links_no_jj_or_grpc() {
    let metadata = resolved_metadata();
    let reached = assert_unreachable(&metadata, "reef-broker", &NOT_IN_THE_BROKER, true);
    assert!(reached > 1, "reef-broker: only {reached} packages reached");
}

/// Walks `root`'s normal and build edges on every target, failing with the
/// path to the first package in `forbidden` it reaches, or with
/// `no_reef_crates` the first Reef crate; returns how many packages it
/// reached.
fn assert_unreachable(
    metadata: &Value,
    root: &str,
    forbidden: &[&str],
    no_reef_crates: bool,
) -> usize {
    let names: BTreeMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .map(|p| {
            (
                p["id"].as_str().expect("id"),
                p["name"].as_str().expect("name"),
            )
        })
        .collect();
    let nodes: BTreeMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes is an array")
        .iter()
        .map(|node| (node["id"].as_str().expect("node id"), node))
        .collect();
    let id_of = |name: &str| {
        names
            .iter()
            .find_map(|(id, n)| (*n == name).then_some(*id))
            .unwrap_or_else(|| panic!("package {name} not found"))
    };
    // Breadth first, remembering how each package was reached.
    let mut via: BTreeMap<&str, &str> = BTreeMap::new();
    let start = id_of(root);
    let mut queue = VecDeque::from([start]);
    via.insert(start, start);
    while let Some(id) = queue.pop_front() {
        let name = names[id];
        let reef_crate = id != start && name.starts_with("reef-");
        if forbidden.contains(&name) || (no_reef_crates && reef_crate) {
            let mut path = vec![name];
            let mut at = id;
            while via[at] != at {
                at = via[at];
                path.push(names[at]);
            }
            path.reverse();
            panic!("{root} reaches {name}: {}", path.join(" -> "));
        }
        for dep in nodes[id]["deps"].as_array().expect("deps is an array") {
            let shipped = dep["dep_kinds"]
                .as_array()
                .expect("dep_kinds is an array")
                .iter()
                .any(|kind| kind["kind"].is_null() || kind["kind"] == "build");
            let pkg = dep["pkg"].as_str().expect("dep pkg");
            if shipped && !via.contains_key(pkg) {
                via.insert(pkg, id);
                queue.push_back(pkg);
            }
        }
    }
    via.len()
}

#[test]
fn reef_lib_depends_on_no_higher_crate() {
    let metadata = metadata();
    for higher in ["reef-daemon", "reef-vcs", "reef-client"] {
        assert_no_dependency(&metadata, "reef-lib", higher);
    }
}

#[test]
fn reef_api_depends_on_no_reef_crate() {
    let metadata = metadata();
    let reef_deps: Vec<_> = dependencies(&metadata, "reef-api")
        .into_iter()
        .filter(|(dep, _)| dep.starts_with("reef-"))
        .collect();
    assert!(
        reef_deps.is_empty(),
        "reef-api must not depend on other reef crates, found {reef_deps:?}"
    );
    assert_no_dependency(&metadata, "reef-lib", "reef-api");
}

#[test]
fn the_daemon_and_the_cli_depend_on_reef_api() {
    let metadata = metadata();
    for crate_name in ["reef-daemon", "reef-vcs"] {
        assert!(
            dependencies(&metadata, crate_name)
                .iter()
                .any(|(dep, kind)| dep == "reef-api" && kind == "normal"),
            "{crate_name} must depend on reef-api"
        );
    }
}

#[test]
fn reef_daemon_does_not_depend_on_reef_vcs() {
    let metadata = metadata();
    assert_no_dependency(&metadata, "reef-daemon", "reef-vcs");
}

#[test]
fn reef_testing_has_no_non_dev_reef_dependency() {
    let metadata = metadata();
    let reef_deps: Vec<_> = dependencies(&metadata, "reef-testing")
        .into_iter()
        .filter(|(dep, kind)| dep.starts_with("reef-") && kind != "dev")
        .collect();
    assert!(
        reef_deps.is_empty(),
        "reef-testing must not depend on other reef crates, found {reef_deps:?}"
    );
}
