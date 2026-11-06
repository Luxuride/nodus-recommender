//! Entry point for the HTTP API that serves recommendations.
//!
//! The server is intentionally unimplemented: the API surface is being designed
//! in `docs/` first. When it is defined, this binary gains the HTTP framework,
//! configuration loading, and handlers; `nodus-core` stays untouched.

#![forbid(unsafe_code)]

fn main() {
    println!("nodus-api {}", env!("CARGO_PKG_VERSION"));
    println!("No API implemented yet — see docs/ for the design.");
}
