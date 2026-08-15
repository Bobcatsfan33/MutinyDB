//! Print the C9 bounds artifact (I-10).
//!
//! ```text
//! cargo run --release -p schweep-server --bin c9-costs > testing/evidence/c9-bounds.json
//! ```
//!
//! Deterministic, so `testing/differential/tests/evidence.rs` recomputes what this prints and compares it.
//! Regenerating is therefore a way of *recording* a change that already happened, never a way of making a
//! test pass.

#![allow(clippy::print_stdout)]

fn main() {
    print!("{}", schweep_server::costs::measure().to_json());
}
