//! Enforces the workspace's crate dependency direction:
//! `reef-vcs` → `reef-daemon` → `reef-lib`, with `reef-testing` standing
//! apart so any crate can dev-depend on it. See ADR 0007.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const MEMBERS: [&str; 4] = ["reef-daemon", "reef-lib", "reef-testing", "reef-vcs"];

fn metadata() -> Value {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
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
fn workspace_members_are_the_four_crates() {
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
fn reef_testing_has_no_normal_reef_dependency() {
    let metadata = metadata();
    let reef_deps: Vec<_> = dependencies(&metadata, "reef-testing")
        .into_iter()
        .filter(|(dep, kind)| dep.starts_with("reef-") && kind == "normal")
        .collect();
    assert!(
        reef_deps.is_empty(),
        "reef-testing must not depend on other reef crates, found {reef_deps:?}"
    );
}
