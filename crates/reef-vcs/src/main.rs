//! Entry point for the `reef` CLI. Later tickets add commands and `reef mcp`.

fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
