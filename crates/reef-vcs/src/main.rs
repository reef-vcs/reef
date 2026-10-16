//! Entry point for the `reef` CLI. Later tickets add commands and `reef mcp`.

use anyhow::Context as _;
use reef_daemon::logging::{self, LogOptions};

fn main() -> anyhow::Result<()> {
    let _logging = LogOptions::from_env()
        .and_then(|opts| logging::init(&opts))
        .context("initialize logging")?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    // R1.22 replaces the rest.
    Ok(())
}
