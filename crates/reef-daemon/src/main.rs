//! Entry point for the Reef daemon. R1.03 replaces this with the real service.

fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
