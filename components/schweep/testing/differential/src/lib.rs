//! # schweep-differential — the oracle harness
//!
//! **The differential harness is the product's credibility** (`ARCHITECTURE.md` §7). Every
//! correctness claim Current will ever make routes through this crate: a seeded scenario is
//! generated, fed epoch by epoch to two implementations, and their answers are compared byte for
//! byte at every sealed epoch. That is invariant I-1, executed.
//!
//! ## What C0 proves, and what it does not
//!
//! In C0 the oracle is on **both** sides. That does not test the oracle — it tests the harness:
//! that scenarios generate reproducibly, that epochs seal in order, that answers are read at the
//! right epoch, that comparison actually detects a difference, and that a seed re-creates a run
//! exactly. The [`SaboteurEngine`] is the proof of the middle one: a deliberately wrong
//! implementation the harness must catch.
//!
//! There is no incremental engine yet, so **nothing here proves anything about incremental
//! evaluation**. From C1, one side becomes `schweep-circuit` and the same code starts earning
//! its keep.
//!
//! ## Layout
//!
//! - [`rng`] — the only randomness in the repository, seeded and value-stable (D-6, I-2).
//! - [`scenario`] — the generator: tables, a query, and epochs of deltas that always include
//!   retractions (§7).
//! - [`engine`] — [`EngineUnderTest`], the seam C1 attaches to.
//! - [`oracle_engine`] — the oracle as an implementation, plus the saboteur.
//! - [`harness`] — the comparison itself.
//! - [`sql_render`] — a typed query rendered back to SQL, so the fuzzer can drive the SQL door with
//!   the population the generator already produces (C5).
//! - [`sql_engine`] — the SQL door as an implementation: text in, circuit out.
//! - [`memo_engine`] — one query registered into a memo, so the registry's plumbing is under I-1 (C6).
//! - [`network_engine`] — a `schweepd` on loopback: the harness over a socket (C9).
//! - [`oneshot_engine`] — a query answered by recomputation through an ephemeral circuit (C7).
//!
//! ## Reproducing a failure
//!
//! Every failure prints its seed. `Scenario::generate(seed)` re-creates the run exactly:
//!
//! ```
//! use schweep_differential::{compare, OracleEngine, Scenario};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let scenario = Scenario::generate(42)?;
//! let report = compare::<OracleEngine, OracleEngine>(&scenario)
//!     .map_err(|d| d.to_string())?;
//! assert_eq!(report.seed, 42);
//! assert_eq!(report.comparisons, report.epochs + 1); // epoch 0 is compared too
//! # Ok(())
//! # }
//! ```

pub mod circuit_engine;
pub mod coverage;
pub mod engine;
pub mod harness;
pub mod memo_engine;
pub mod network_engine;
pub mod oneshot_engine;
pub mod oracle_engine;
pub mod rng;
pub mod scenario;
pub mod sql_engine;
pub mod sql_render;

pub use circuit_engine::CircuitEngine;
pub use engine::EngineUnderTest;
pub use harness::{
    compare, sweep, sweep_matching, Divergence, DivergenceKind, Report, SweepReport,
};
pub use memo_engine::MemoEngine;
pub use network_engine::NetworkEngine;
pub use oneshot_engine::OneShotEngine;
pub use oracle_engine::{OracleEngine, SaboteurEngine};
pub use rng::Rng;
pub use scenario::{Family, Operation, Scenario};
pub use sql_engine::SqlEngine;
pub use sql_render::{sql_form, NoSqlForm};
