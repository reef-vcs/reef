//! Compiles `proto/` with `protox`, so building needs no `protoc` and no
//! `buf` (R1.03, R2.01), and generates messages, clients, and servers with
//! `tonic-prost-build`. Also writes, to `OUT_DIR`:
//!
//! - `access.rs`: every `reef.api.v1` method's `(reef.api.v1.access)`
//!   option, sorted by path. The build fails if a method has none.
//! - `reef_api_v1.fds`: the v1 files and their imports, with comments, as a
//!   `FileDescriptorSet` for SDK generators and reflection (R2.24).
//! - `digest.rs`: blake3 of the v1 descriptors without source info, which
//!   the `api_digest_matches_revision` test compares with `API_DIGEST`.

use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

use prost_reflect::Value;

const V1: &[&str] = &[
    "proto/reef/api/v1/options.proto",
    "proto/reef/api/v1/error.proto",
    "proto/reef/api/v1/operations.proto",
    "proto/reef/api/v1/changes.proto",
    "proto/reef/api/v1/files.proto",
    "proto/reef/api/v1/diff.proto",
    "proto/reef/api/v1/workspaces.proto",
    "proto/reef/api/v1/repositories.proto",
    "proto/reef/api/v1/status.proto",
    "proto/reef/api/v1/presence.proto",
    "proto/reef/api/v1/daemon.proto",
    "proto/reef/api/v1/maintenance.proto",
    "proto/reef/api/v1/stores.proto",
    "proto/reef/api/v1/symbols.proto",
];

/// Frozen at 0.1.0 and served only for version detection.
const V0: &[&str] = &[
    "proto/reef/daemon/v0/daemon.proto",
    "proto/reef/daemon/v0/error.proto",
];

const PACKAGE: &str = "reef.api.v1";

fn compiler(
    files: &[&str],
    imports: bool,
    source_info: bool,
) -> Result<protox::Compiler, Box<dyn Error>> {
    let mut compiler = protox::Compiler::new(["proto"])?;
    compiler
        .include_imports(imports)
        .include_source_info(source_info)
        .open_files(files)?;
    Ok(compiler)
}

/// `access.rs`: `(path, access)` for every v1 method, sorted by path.
fn access_table(pool: &prost_reflect::DescriptorPool) -> Result<String, Box<dyn Error>> {
    let extension = pool
        .get_extension_by_name("reef.api.v1.access")
        .ok_or("reef.api.v1.access is not defined")?;
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    for service in pool.services().filter(|s| s.package_name() == PACKAGE) {
        for method in service.methods() {
            let path = format!("/{}/{}", service.full_name(), method.name());
            let options = method.options();
            let access = match options.get_extension(&extension).as_ref() {
                Value::EnumNumber(number) if options.has_extension(&extension) && *number != 0 => {
                    *number
                }
                _ => {
                    missing.push(path);
                    continue;
                }
            };
            rows.push((path, access));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "these methods set no `(reef.api.v1.access)` (ADR 0029): {}",
            missing.join(", ")
        )
        .into());
    }
    rows.sort();
    let mut out = String::from("&[\n");
    for (path, access) in rows {
        writeln!(out, "    ({path:?}, {access}),")?;
    }
    out.push(']');
    Ok(out)
}

fn main() -> Result<(), Box<dyn Error>> {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);

    let all: Vec<&str> = V1.iter().chain(V0).copied().collect();
    let everything = compiler(&all, true, true)?;
    std::fs::write(
        out.join("access.rs"),
        access_table(&everything.descriptor_pool())?,
    )?;

    let published = compiler(V1, true, true)?;
    std::fs::write(
        out.join("reef_api_v1.fds"),
        published.encode_file_descriptor_set(),
    )?;

    // Comments do not count: only the v1 files, without source info. The
    // encoding keeps extension options, so changing an RPC's access changes
    // the digest too.
    let bare = compiler(V1, false, false)?;
    let digest = blake3::hash(&bare.encode_file_descriptor_set());
    std::fs::write(
        out.join("digest.rs"),
        format!("{:?}", digest.to_hex().as_str()),
    )?;

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .btree_map(".")
        .compile_fds(everything.file_descriptor_set())?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
