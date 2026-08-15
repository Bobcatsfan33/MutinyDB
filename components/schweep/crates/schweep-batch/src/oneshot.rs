//! One-shot queries through ephemeral circuits (`ARCHITECTURE.md` §5.8, §6 C7).
//!
//! A one-shot query is asked once and forgotten. Current answers it with **the same machinery** a
//! standing query uses — the same binder, the same incrementalizer, the same operators — by building a
//! circuit, feeding it the accumulated input as one delta, reading the answer, and dropping it.
//!
//! ## Why not a separate execution path
//!
//! Because a second path is a second set of answers. Every dialect rule, every null case, every
//! aggregate cliff would have to be right twice, and the differential harness would have to be run
//! against both to know that it was. The cost of reusing the incremental path is that a one-shot pays
//! for machinery it does not need — state backends it will throw away, an integral it reads once. That
//! cost is real and it is the *right* cost: it is paid in one query's latency rather than in
//! correctness everywhere.
//!
//! §6 C10 is where that trade gets measured, and §6 C10 also says what to expect: losing one-shot
//! throughput to DuckDB is "their game", stated rather than chased.
//!
//! ## What "one big delta" means for the answer
//!
//! Exactly what it means for bootstrap (`docs/DURABILITY.md` B2): an operator's state is a function of
//! the accumulated input, and an answer is the integral of its sink's output deltas, so feeding the
//! whole input at once produces the answer feeding it epoch by epoch produces. The one-shot path is
//! therefore not an approximation of the standing path — it is the standing path, with one epoch.

use std::collections::BTreeMap;

use schweep_plan::bind::Catalog;
use schweep_plan::plan::Query;
use schweep_zset::{Canonical, EpochDeltas, Schema, ZSetBatch};

use crate::error::Result;
use crate::hydrate;

/// Answer a query once, over an accumulated input given as one delta.
///
/// The circuit is built, stepped once, read, and dropped before this function returns. Nothing is
/// registered, nothing is shared, and nothing survives.
pub fn answer(catalog: &Catalog, query: &Query, input: &EpochDeltas) -> Result<Canonical> {
    let plan = schweep_sql::incrementalize_typed(query, catalog)?;
    let mut circuit = schweep_sql::instantiate(&plan)?;
    circuit.step(input)?;
    Ok(circuit.answer()?)
}

/// The same, from SQL text.
pub fn answer_sql(catalog: &Catalog, sql: &str, input: &EpochDeltas) -> Result<Canonical> {
    let plan = schweep_sql::compile(sql, catalog)?;
    let mut circuit = schweep_sql::instantiate(&plan)?;
    circuit.step(input)?;
    Ok(circuit.answer()?)
}

/// Answer a query once over whatever the log currently holds — snapshot plus suffix, or the whole log.
///
/// The one-shot door a client would reach: it does not care whether a compaction has happened, and
/// cannot tell.
pub fn answer_over_log(
    log: &schweep_log::Log,
    catalog: &Catalog,
    query: &Query,
) -> Result<Canonical> {
    let tables: BTreeMap<String, Schema> = catalog.clone();
    let input = hydrate::one_delta_for(log, &tables)?;
    answer(catalog, query, &input)
}

/// Answer a query once over integrals already in hand.
pub fn answer_over_integrals(
    catalog: &Catalog,
    query: &Query,
    integrals: &BTreeMap<String, ZSetBatch>,
) -> Result<Canonical> {
    answer(catalog, query, &hydrate::as_one_delta(integrals)?)
}
