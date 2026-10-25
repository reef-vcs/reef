//! Generates the daemon API from `proto/` with `protox`, so building needs
//! no `protoc` (R1.03), and writes the compiled descriptor set to
//! `OUT_DIR/reef.daemon.v1.bin` for `reef_api::DESCRIPTOR_SET` (R2.01). protox
//! bundles the well-known types (`google/protobuf/timestamp.proto`) and
//! keeps source info, so the set carries every comment; `prost-types` is the
//! Rust side of the well-known types.

const PROTOS: &[&str] = &[
    "proto/reef/daemon/v1/daemon.proto",
    "proto/reef/daemon/v1/types.proto",
    "proto/reef/daemon/v1/error.proto",
    "proto/reef/daemon/v1/repositories.proto",
    "proto/reef/daemon/v1/workspaces.proto",
    "proto/reef/daemon/v1/changes.proto",
    "proto/reef/daemon/v1/operations.proto",
    "proto/reef/daemon/v1/events.proto",
    "proto/reef/daemon/v1/trees.proto",
    "proto/reef/daemon/v1/presence.proto",
    "proto/reef/daemon/v1/symbols.proto",
    "proto/reef/daemon/v1/queries.proto",
    "proto/reef/daemon/v1/remote.proto",
    "proto/reef/daemon/v1/maintenance.proto",
    "proto/reef/daemon/v1/stores.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(PROTOS, ["proto"])?;
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(
        out.join("reef.daemon.v1.bin"),
        prost::Message::encode_to_vec(&fds),
    )?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
