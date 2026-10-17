//! Generates the daemon API from `proto/` with `protox`, so building needs
//! no `protoc` (R1.03). Later services add their files to the one
//! `compile` call. protox bundles the well-known types
//! (`google/protobuf/timestamp.proto`); `prost-types` is their Rust side.

const PROTOS: &[&str] = &[
    "proto/reef/daemon/v0/daemon.proto",
    "proto/reef/daemon/v0/types.proto",
    "proto/reef/daemon/v0/error.proto",
    "proto/reef/daemon/v0/repositories.proto",
    "proto/reef/daemon/v0/workspaces.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(PROTOS, ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
