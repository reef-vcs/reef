//! Entry point for the Reef daemon. R1.03 replaces this with the real service.

use anyhow::Context as _;
use reef_daemon::logging::{self, LogOptions};

fn main() -> anyhow::Result<()> {
    LogOptions::from_env()
        .and_then(|opts| logging::init(&opts))
        .context("initialize logging")?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    // R1.03 replaces the rest.
    Ok(())
}
