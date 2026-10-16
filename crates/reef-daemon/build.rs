//! Generates the daemon API from `proto/` with `protox`, so building needs
//! no `protoc` (R1.03). Later services add their files to the one
//! `compile` call.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["proto/reef/daemon/v0/daemon.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
