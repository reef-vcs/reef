//! Compiles the vendored `proto/scip.proto` with `protox`, so building needs
//! no `protoc` (as `reef-api`'s build does), and generates its messages with
//! `prost-build`.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=proto/scip.proto");
    let descriptors = protox::compile(["proto/scip.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    Ok(())
}
