//! Enforces the workspace's crate dependency direction:
//! `reef-vcs` → `reef-daemon` → `reef-lib` → `reef-api`, with `reef-testing`
//! standing apart so any crate can dev-depend on it. See ADR 0007 and ADR
//! 0029.

#![allow(
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const MEMBERS: [&str; 5] = [
    "reef-api",
    "reef-daemon",
    "reef-lib",
    "reef-testing",
    "reef-vcs",
];

fn metadata() -> Value {
    cargo_metadata(&["--no-deps"])
}

/// `cargo metadata` with `args` added, parsed.
fn cargo_metadata(args: &[&str]) -> Value {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .args(args)
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

#[test]
fn workspace_members_are_the_five_crates() {
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

#[test]
fn reef_lib_depends_on_no_higher_crate() {
    let metadata = metadata();
    assert_no_dependency(&metadata, "reef-lib", "reef-daemon");
    assert_no_dependency(&metadata, "reef-lib", "reef-vcs");
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

#[test]
fn reef_lib_depends_on_reef_api() {
    let metadata = metadata();
    assert!(
        dependencies(&metadata, "reef-lib")
            .iter()
            .any(|(dep, kind)| dep == "reef-api" && kind == "normal"),
        "reef-lib re-exports reef_api::error"
    );
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
}

/// `reef-api`'s whole normal dependency graph, not just its direct
/// dependencies: a client must reach the daemon without linking jj-lib or
/// SQLite (ADR 0029).
#[test]
fn reef_api_links_neither_jj_lib_nor_sqlite() {
    let metadata = cargo_metadata(&[]);
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes is an array");
    let node = |id: &str| {
        nodes
            .iter()
            .find(|n| n["id"] == id)
            .unwrap_or_else(|| panic!("node {id} not found"))
    };
    let root = package(&metadata, "reef-api")["id"]
        .as_str()
        .expect("package id")
        .to_owned();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for dep in node(&id)["deps"].as_array().expect("deps is an array") {
            let normal = dep["dep_kinds"]
                .as_array()
                .expect("dep_kinds is an array")
                .iter()
                .any(|kind| kind["kind"].is_null());
            if normal {
                stack.push(dep["pkg"].as_str().expect("dep pkg").to_owned());
            }
        }
    }
    let names: Vec<&str> = seen
        .iter()
        .map(|id| {
            metadata["packages"]
                .as_array()
                .expect("packages is an array")
                .iter()
                .find(|p| p["id"] == id.as_str())
                .and_then(|p| p["name"].as_str())
                .expect("package name")
        })
        .collect();
    for banned in ["jj-lib", "rusqlite", "libsqlite3-sys", "gix", "reef-lib"] {
        assert!(
            !names.contains(&banned),
            "reef-api's normal dependencies include {banned}"
        );
    }
    assert!(names.contains(&"tonic"), "the walk found {names:?}");
}
