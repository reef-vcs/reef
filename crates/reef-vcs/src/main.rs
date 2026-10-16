//! Entry point for the `reef` CLI. Later tickets add commands and `reef mcp`.

#[allow(clippy::print_stdout, reason = "CLI output")]
fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
