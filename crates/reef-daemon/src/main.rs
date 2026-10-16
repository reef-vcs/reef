//! Entry point for the Reef daemon. R1.03 replaces this with the real service.

#[allow(clippy::print_stdout, reason = "CLI output")]
fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
