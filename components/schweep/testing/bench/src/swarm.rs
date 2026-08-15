//! The deterministic overlapping-query population used by C10's swarm benchmark and its gate.
//!
//! Every query has a private output name and the same scan/filter prefix. The differing root prevents
//! the memo from collapsing the registrations into aliases of one answer, while the common prefix gives
//! it exactly the sharing opportunity whose marginal cost the product is built around (I-8).

/// Build `count` distinct, near-duplicate standing queries.
#[must_use]
pub fn near_duplicate_queries(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("SELECT t.n AS q_{index:05} FROM t WHERE t.k > 1"))
        .collect()
}

/// The next distinct member of a swarm, used to measure marginal registration.
#[must_use]
pub fn marginal_query(existing: usize) -> String {
    format!("SELECT t.n AS q_{existing:05} FROM t WHERE t.k > 1")
}
