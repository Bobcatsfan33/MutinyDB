//! Regenerates the receipt for the scenario generator's tuned constants (I-10).
//!
//! ```text
//! cargo run -p schweep-differential --bin generator-coverage \
//!   > testing/evidence/c0-generator-coverage.json
//! ```
//!
//! `tests/evidence.rs` recomputes the same numbers and fails if the committed file has drifted,
//! so the ledger's receipt cannot quietly stop describing the generator it justifies.

#![allow(clippy::print_stdout)]

use std::process::ExitCode;

use schweep_differential::coverage::{measure, ARTIFACT_SEEDS};

fn main() -> ExitCode {
    // ARTIFACT_SEEDS lives in the library so this binary and `tests/evidence.rs` can never
    // disagree about what was measured.
    match measure(ARTIFACT_SEEDS) {
        Ok(coverage) => {
            print!("{}", coverage.to_json());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("measuring generator coverage failed: {e}");
            ExitCode::FAILURE
        }
    }
}
