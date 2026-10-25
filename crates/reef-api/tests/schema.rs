//! The v1 schema as a whole (R2.01): every service and RPC the Phase 2
//! README lists, a comment on every element, and field numbers frozen.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use prost::Message as _;
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorProto, FileDescriptorSet};

/// The README's "API v1" row, `Service.Rpc`.
const RPCS: &[&str] = &[
    "Daemon.Status",
    "Repositories.Register",
    "Repositories.List",
    "Repositories.Forget",
    "Workspaces.Create",
    "Workspaces.List",
    "Workspaces.Inspect",
    "Workspaces.Release",
    "Workspaces.Snapshot",
    "Workspaces.Heartbeat",
    "Workspaces.Move",
    "Workspaces.Status",
    "Changes.Log",
    "Changes.Get",
    "Changes.Diff",
    "Changes.Describe",
    "Changes.Abandon",
    "Changes.Squash",
    "Operations.Log",
    "Operations.Get",
    "Operations.Undo",
    "Operations.Subscribe",
    "Operations.Replay",
    "Trees.List",
    "Trees.Read",
    "Trees.Apply",
    "Presence.Subscribe",
    "Presence.Report",
    "Symbols.Index",
    "Symbols.Ingest",
    "Symbols.Lookup",
    "Queries.List",
    "Queries.Run",
    "Remote.IssueToken",
    "Remote.ListTokens",
    "Remote.RevokeToken",
    "Maintenance.Gc",
    "Maintenance.Backup",
    "Maintenance.Quiesce",
    "Maintenance.Resume",
    "Stores.Build",
    "Stores.Prepare",
    "Stores.Seal",
    "Stores.Abort",
    "Stores.List",
];

/// The files under `reef/daemon/v1/`, in the order `build.rs` compiles
/// them.
const FILES: [&str; 15] = [
    "daemon",
    "types",
    "error",
    "repositories",
    "workspaces",
    "changes",
    "operations",
    "events",
    "trees",
    "presence",
    "symbols",
    "queries",
    "remote",
    "maintenance",
    "stores",
];

const FIXTURE: &str = "tests/fixtures/v1-fields.txt";

fn v1_files() -> Vec<FileDescriptorProto> {
    let set = FileDescriptorSet::decode(reef_api::DESCRIPTOR_SET).unwrap();
    set.file
        .into_iter()
        .filter(|file| file.package() == "reef.daemon.v1")
        .collect()
}

#[test]
fn schema_lists_every_readme_service() {
    let files = v1_files();
    let names: BTreeSet<String> = files.iter().map(|file| file.name().to_owned()).collect();
    let expected: BTreeSet<String> = FILES
        .iter()
        .map(|name| format!("reef/daemon/v1/{name}.proto"))
        .collect();
    assert_eq!(names, expected);
    let rpcs: BTreeSet<String> = files
        .iter()
        .flat_map(|file| &file.service)
        .flat_map(|service| {
            service
                .method
                .iter()
                .map(move |method| format!("{}.{}", service.name(), method.name()))
        })
        .collect();
    let expected: BTreeSet<String> = RPCS.iter().map(|rpc| (*rpc).to_owned()).collect();
    assert_eq!(rpcs, expected);
}

/// Every element's `SourceCodeInfo` path and its name, for the comment
/// check: path numbers are `FileDescriptorProto`'s and `DescriptorProto`'s
/// field numbers (message 4, nested message 3, field 2, enum 5 at file
/// level and 4 nested, enum value 2, service 6, method 2).
fn elements(file: &FileDescriptorProto) -> Vec<(Vec<i32>, String)> {
    fn enums(
        out: &mut Vec<(Vec<i32>, String)>,
        path: &[i32],
        scope: &str,
        e: &EnumDescriptorProto,
    ) {
        let name = format!("{scope}.{}", e.name());
        out.push((path.to_vec(), name.clone()));
        for (i, value) in e.value.iter().enumerate() {
            out.push((
                [path, &[2, index(i)]].concat(),
                format!("{name}.{}", value.name()),
            ));
        }
    }
    fn message(out: &mut Vec<(Vec<i32>, String)>, path: &[i32], scope: &str, m: &DescriptorProto) {
        let name = format!("{scope}.{}", m.name());
        // Map entries are synthesized; their fields have no source.
        if m.options
            .as_ref()
            .is_some_and(prost_types::MessageOptions::map_entry)
        {
            return;
        }
        out.push((path.to_vec(), name.clone()));
        for (i, field) in m.field.iter().enumerate() {
            out.push((
                [path, &[2, index(i)]].concat(),
                format!("{name}.{}", field.name()),
            ));
        }
        for (i, nested) in m.nested_type.iter().enumerate() {
            message(out, &[path, &[3, index(i)]].concat(), &name, nested);
        }
        for (i, e) in m.enum_type.iter().enumerate() {
            enums(out, &[path, &[4, index(i)]].concat(), &name, e);
        }
    }
    let mut out = Vec::new();
    for (i, m) in file.message_type.iter().enumerate() {
        message(&mut out, &[4, index(i)], "", m);
    }
    for (i, e) in file.enum_type.iter().enumerate() {
        enums(&mut out, &[5, index(i)], "", e);
    }
    for (i, service) in file.service.iter().enumerate() {
        out.push((vec![6, index(i)], service.name().to_owned()));
        for (j, method) in service.method.iter().enumerate() {
            out.push((
                vec![6, index(i), 2, index(j)],
                format!("{}.{}", service.name(), method.name()),
            ));
        }
    }
    out
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap()
}

#[test]
fn every_field_and_rpc_is_documented() {
    let mut undocumented = Vec::new();
    let mut checked = 0;
    for file in v1_files() {
        let locations = &file.source_code_info.as_ref().unwrap().location;
        for (path, name) in elements(&file) {
            checked += 1;
            let documented = locations
                .iter()
                .find(|location| location.path == path)
                .is_some_and(|location| !location.leading_comments().trim().is_empty());
            if !documented {
                undocumented.push(format!("{}: {name}", file.name()));
            }
        }
    }
    // Every message, field, enum value, service, and method of 15 files.
    assert!(checked > 700, "walked only {checked} elements");
    assert!(
        undocumented.is_empty(),
        "no leading comment on:\n{}",
        undocumented.join("\n")
    );
}

/// `<message> <field> <number>` for every field, nested messages by their
/// dotted name, sorted.
fn field_listing() -> String {
    fn walk(out: &mut BTreeSet<String>, scope: &str, m: &DescriptorProto) {
        let name = if scope.is_empty() {
            m.name().to_owned()
        } else {
            format!("{scope}.{}", m.name())
        };
        for field in &m.field {
            out.insert(format!("{name} {} {}", field.name(), field.number()));
        }
        for nested in &m.nested_type {
            walk(out, &name, nested);
        }
    }
    let mut lines = BTreeSet::new();
    for file in v1_files() {
        for m in &file.message_type {
            walk(&mut lines, "", m);
        }
    }
    let mut text = String::new();
    for line in lines {
        writeln!(text, "{line}").unwrap();
    }
    text
}

/// A field renumbered or renamed fails here; regenerate the fixture with
/// `REEF_UPDATE_FIXTURES=1 cargo test -p reef-api --test schema` only for an
/// addition. Within v1 a field is never renumbered, renamed, or removed.
#[test]
fn field_numbers_are_frozen() {
    let listing = field_listing();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    if std::env::var_os("REEF_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        std::fs::write(&fixture, &listing).unwrap();
    }
    let frozen = std::fs::read_to_string(&fixture).unwrap();
    let now: BTreeSet<&str> = listing.lines().collect();
    let then: BTreeSet<&str> = frozen.lines().collect();
    let missing: Vec<&&str> = then.difference(&now).collect();
    assert!(
        missing.is_empty(),
        "fields removed, renamed, or renumbered: {missing:?}"
    );
    let added: Vec<&&str> = now.difference(&then).collect();
    assert!(
        added.is_empty(),
        "fields added; regenerate {FIXTURE} with REEF_UPDATE_FIXTURES=1:\n{added:?}"
    );
}

#[test]
fn schema_hash_is_the_descriptor_sets_blake3() {
    assert_eq!(reef_api::SCHEMA_HASH.len(), 64);
    assert_eq!(
        *reef_api::SCHEMA_HASH,
        blake3::hash(reef_api::DESCRIPTOR_SET).to_hex().to_string()
    );
}
